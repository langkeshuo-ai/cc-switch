use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::{
    atomic_write, atomic_write_private, delete_file, get_home_dir, path_is_within, read_json_file,
    write_json_file, write_json_file_private, write_text_file_private,
};
use crate::error::AppError;
use crate::model_capabilities::{image_input_capability_from_modalities, ImageInputCapability};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::process::{Command, Stdio};
use toml_edit::DocumentMut;

pub const CC_SWITCH_CODEX_MODEL_PROVIDER_ID: &str = "custom";
/// Temporary model-provider id used while the built-in `codex-official`
/// provider is routed through CC Switch.  A dedicated id is an ownership
/// marker: unlike a generic localhost `base_url`, it can be detected and
/// cleaned up without mistaking a user's own local provider for takeover.
pub const CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID: &str = "cc-switch-official";
pub const CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME: &str = "cc-switch-model-catalog.json";
const CODEX_PROXY_AUTH_PLACEHOLDER: &str = "PROXY_MANAGED";

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

// Generating a ProxyChat catalog only needs one stable Codex model template per
// process. Without this cache every provider switch/takeover can start the
// Codex CLI again, which is especially expensive for npm-installed `codex.cmd`
// on Windows. Tests deliberately bypass the global cache because they isolate
// CODEX_HOME and seed different model templates.
#[cfg(not(test))]
static CODEX_MODEL_CATALOG_TEMPLATE_CACHE: OnceCell<Value> = OnceCell::new();

/// Top-level `config.toml` key that controls Codex's built-in web-search tool.
pub(crate) const CODEX_WEB_SEARCH_FIELD: &str = "web_search";
/// Value that disables the web-search tool. Some native `/responses` gateways
/// reject a `web_search` tool with `responses_feature_not_supported` ("tool type
/// 'web_search' is not supported by this gateway phase"), so for those we write
/// this per the vendors' official Codex docs. Also doubles as cc-switch's
/// ownership sentinel: we only ever remove a `web_search` key whose value equals
/// this string, never a user's own setting.
pub(crate) const CODEX_WEB_SEARCH_DISABLED: &str = "disabled";

/// Native `/responses` gateways whose first-party models do NOT support the Codex
/// `web_search` hosted tool. A BLACKLIST (default-on): everything not listed keeps
/// Codex's default, so relays/aggregators fronting real GPT — and any unknown
/// provider — are never touched. This avoids a whitelist's dangerous failure mode
/// (a fragile "is this GPT?" heuristic wrongly keeping web_search ON → hard 400);
/// the blacklist's failure mode is the safe, recoverable one (a not-yet-listed
/// broken gateway errors once → add it here).
///
/// Matched two ways so an aggregator (e.g. SiliconFlow) fronting these vendors'
/// models is also caught:
/// - `base_url` host substring, and
/// - the model id's brand prefix (after stripping any `vendor/` path segment).
///
/// Verified 2026-06-28 doc audit — reject: MiMo (hard 400), LongCat (official
/// config ships `web_search = "disabled"`), MiniMax (tool-type enum `['function']`
/// only), and Qwen3-Coder models (百炼 marks built-in tools unsupported for
/// the coder series). Deliberately NOT listed by host: 火山方舟豆包, general
/// 阿里百炼 Qwen models that support built-in web_search, and GPT-native relays.
const CODEX_WEB_SEARCH_REJECT_HOSTS: &[&str] = &[
    "xiaomimimo.com", // Xiaomi MiMo (api.xiaomimimo.com, token-plan-cn.xiaomimimo.com)
    "longcat.chat",   // Meituan LongCat (api.longcat.chat)
    "minimax.io",     // MiniMax global (api.minimax.io)
    "minimax.cn",     // MiniMax CN (current official endpoint)
    "minimaxi.com",   // MiniMax CN (legacy endpoint)
    // StepFun Responses API currently supports only `function` tools:
    // platform.stepfun.com/docs/zh/api-reference/responses/responses-create
    "stepfun.com",
    "stepfun.ai",
    // Conservative (unverified, not a confirmed reject): Baidu Qianfan's
    // pay-as-you-go Responses guide documents only `function` / `mcp` tools
    // (cloud.baidu.com/doc/qianfan-docs/s/4mi400l1m). Host-exact; Qianfan's
    // Chat plans on the same domain are ProxyChat and never consult this list.
    "qianfan.baidubce.com",
    // Conservative (unverified): iFlytek Astron Coding Plan fronts third-party
    // models behind one Responses gateway with no documented hosted-tool
    // support (www.xfyun.cn/doc/spark/CodingPlan.html).
    "xf-yun.com",
    // Zhipu GLM CN / global (open.bigmodel.cn, api.z.ai): the native Responses
    // gateway's tool-type enum is `function | web_search_preview |
    // code_interpreter | mcp` (verbatim from the #6944 400 body) — Codex's
    // `web_search` hosted tool is not in it. Matched on host labels (see
    // `codex_url_host_matches_any`), so `xyz.ai` never collides with `z.ai`.
    "bigmodel.cn",
    "z.ai",
];

/// Brand prefixes of models whose native gateways reject `web_search`, matched
/// against the model id's last `/`-segment so aggregator ids like
/// `MiniMaxAI/MiniMax-M3` are caught. Exact brand names (not a fuzzy heuristic),
/// so a supporting gateway is never wrongly matched.
const CODEX_WEB_SEARCH_REJECT_MODEL_PREFIXES: &[&str] =
    &["mimo", "longcat", "minimax", "qwen3-coder", "glm"];

/// Host component of a base URL (or a bare host), lowercased, without scheme,
/// userinfo, port, path or query. Tolerates the loose forms users paste into
/// the provider form (`example.com`, `https://user@Example.com:8443/v1`).
pub(crate) fn codex_url_host(url_or_host: &str) -> String {
    let trimmed = url_or_host.trim();
    let rest = trimmed
        .split_once("://")
        .map_or(trimmed, |(_scheme, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(ipv6) = host_port.strip_prefix('[') {
        ipv6.split(']').next().unwrap_or(ipv6)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether the URL's host IS one of `hosts` or a subdomain of it, matched on
/// DNS label boundaries. Vendor host lists must go through this rather than a
/// substring `contains`: a 4-char entry like `z.ai` would otherwise also match
/// `api.xyz.ai` / `viz.ai` and silently push an unrelated provider onto a
/// vendor-specific code path.
pub(crate) fn codex_url_host_matches_any(url_or_host: &str, hosts: &[&str]) -> bool {
    let host = codex_url_host(url_or_host);
    if host.is_empty() {
        return false;
    }
    hosts.iter().any(|candidate| {
        let candidate = candidate.trim_start_matches('.').to_ascii_lowercase();
        host == candidate || host.ends_with(&format!(".{candidate}"))
    })
}

/// Top-level `model` id from a Codex `config.toml`.
fn codex_top_level_model(config_text: &str) -> Option<String> {
    let doc = config_text.parse::<toml::Value>().ok()?;
    doc.get("model")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Whether a native `/responses` provider's gateway is known to reject the Codex
/// `web_search` hosted tool — by `base_url` host OR by the active model's brand
/// (so an aggregator fronting a reject vendor's model is caught too). Driven by
/// the live `config.toml`, so it applies to existing providers without a re-save.
fn codex_native_gateway_rejects_web_search(config_text: &str) -> bool {
    if let Some(base_url) = extract_codex_base_url(config_text) {
        if codex_url_host_matches_any(&base_url, CODEX_WEB_SEARCH_REJECT_HOSTS) {
            return true;
        }
    }
    if let Some(model) = codex_top_level_model(config_text) {
        let model = model.to_ascii_lowercase();
        // Strip any aggregator "vendor/" prefix, e.g. "MiniMaxAI/MiniMax-M3"
        // or "qwen/qwen3-coder-plus".
        let model = model.rsplit('/').next().unwrap_or(model.as_str());
        if CODEX_WEB_SEARCH_REJECT_MODEL_PREFIXES
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            return true;
        }
    }
    false
}
const CODEX_MODEL_CATALOG_TEMPLATE_SLUG: &str = "gpt-5.5";
const CODEX_MANAGED_OAUTH_LIVE_AUTH_MARKER_FILENAME: &str = "codex_managed_oauth_live_auth.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexManagedOAuthLiveAuthMarker {
    version: u32,
    /// cc-switch 本地托管账号 ID，用于区分同一 ChatGPT workspace 下的登录。
    account_id: String,
    /// 原生 auth.json 的 `tokens.account_id`，即 ChatGPT workspace ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chatgpt_account_id: Option<String>,
    /// id_token 中跨刷新稳定的用户身份，防止同 workspace 的原生登录串号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_identity: Option<String>,
}

pub(crate) struct CodexManagedLiveRefresh {
    pub(crate) refresh_token: String,
    pub(crate) id_token: Option<String>,
    pub(crate) last_refresh_ms: Option<i64>,
    pub(crate) chatgpt_account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexLiveFileState {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    #[cfg(unix)]
    mode: Option<u32>,
}

impl CodexLiveFileState {
    fn capture(path: PathBuf) -> Result<Self, AppError> {
        if !path.exists() {
            return Ok(Self {
                path,
                contents: None,
                #[cfg(unix)]
                mode: None,
            });
        }

        let contents = fs::read(&path).map_err(|error| AppError::io(&path, error))?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(
                fs::metadata(&path)
                    .map_err(|error| AppError::io(&path, error))?
                    .permissions()
                    .mode(),
            )
        };

        Ok(Self {
            path,
            contents: Some(contents),
            #[cfg(unix)]
            mode,
        })
    }

    fn restore(&self) -> Result<(), AppError> {
        match self.contents.as_deref() {
            Some(contents) => {
                atomic_write(&self.path, contents)?;
                #[cfg(unix)]
                if let Some(mode) = self.mode {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&self.path, fs::Permissions::from_mode(mode))
                        .map_err(|error| AppError::io(&self.path, error))?;
                }
                Ok(())
            }
            None => delete_file(&self.path),
        }
    }
}

/// Rollback point for the cc-switch-owned model catalog. Catalog projection
/// writes this file before the caller commits `config.toml`, so guarded restore
/// paths use this snapshot when a concurrently changing `auth.json` cancels the
/// commit.
pub(crate) struct CodexModelCatalogFileSnapshot(CodexLiveFileState);

impl CodexModelCatalogFileSnapshot {
    pub(crate) fn capture() -> Result<Self, AppError> {
        CodexLiveFileState::capture(get_codex_model_catalog_path()).map(Self)
    }

    pub(crate) fn restore(&self) -> Result<(), AppError> {
        self.0.restore()
    }
}

/// Exact rollback state for a managed Codex live write. The generated catalog
/// and ownership marker are part of the same logical commit as auth/config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexLiveStateSnapshot {
    auth: CodexLiveFileState,
    config: CodexLiveFileState,
    catalog: CodexLiveFileState,
    managed_marker: CodexLiveFileState,
}

impl CodexLiveStateSnapshot {
    pub(crate) fn capture() -> Result<Self, AppError> {
        Ok(Self {
            auth: CodexLiveFileState::capture(get_codex_auth_path())?,
            config: CodexLiveFileState::capture(get_codex_config_path())?,
            catalog: CodexLiveFileState::capture(get_codex_model_catalog_path())?,
            managed_marker: CodexLiveFileState::capture(
                get_codex_managed_oauth_live_auth_marker_path(),
            )?,
        })
    }

    /// Roll back config/catalog exactly while retaining a demonstrably newer
    /// ChatGPT auth generation for the same account. OAuth refresh can advance
    /// auth.json after a provider transaction captures its snapshot; restoring
    /// that snapshot blindly would invalidate the CLI's newly rotated token.
    ///
    /// Cross-account writes are still rolled back exactly: an A -> B transaction
    /// that fails must restore A even if B refreshed while it was briefly live.
    /// The marker follows auth as one generation bundle.
    pub(crate) fn restore_preserving_newer_same_account_auth(&self) -> Result<(), AppError> {
        let mut failures = Vec::new();
        let current_auth = match CodexLiveFileState::capture(get_codex_auth_path()) {
            Ok(state) => Some(state),
            Err(error) => {
                // Inspection failure must not prevent config/catalog and the
                // remaining rollback files from being attempted.
                failures.push(format!("inspect current auth: {error}"));
                None
            }
        };
        let current_marker =
            match CodexLiveFileState::capture(get_codex_managed_oauth_live_auth_marker_path()) {
                Ok(state) => Some(state),
                Err(error) => {
                    failures.push(format!("inspect current managed marker: {error}"));
                    None
                }
            };
        let snapshot_generation = Self::chatgpt_auth_generation(&self.auth, &self.managed_marker);
        let current_generation = current_auth
            .as_ref()
            .zip(current_marker.as_ref())
            .and_then(|(auth, marker)| Self::chatgpt_auth_generation(auth, marker));
        let preserve_current_auth = match (snapshot_generation, current_generation) {
            (Some((snapshot_account, snapshot_time)), Some((current_account, current_time)))
                if snapshot_account == current_account =>
            {
                match (snapshot_time, current_time) {
                    (Some(snapshot_time), Some(current_time)) => current_time > snapshot_time,
                    (None, Some(_)) => true,
                    _ => false,
                }
            }
            _ => false,
        };

        for (label, state) in [("catalog", &self.catalog), ("config", &self.config)] {
            if let Err(error) = state.restore() {
                failures.push(format!("{label}: {error}"));
            }
        }
        if !preserve_current_auth {
            for (label, state) in [
                ("auth", &self.auth),
                ("managed marker", &self.managed_marker),
            ] {
                if let Err(error) = state.restore() {
                    failures.push(format!("{label}: {error}"));
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppError::Message(format!(
                "恢复 Codex Live 状态失败: {}",
                failures.join("; ")
            )))
        }
    }

    fn chatgpt_auth_generation(
        auth_state: &CodexLiveFileState,
        marker_state: &CodexLiveFileState,
    ) -> Option<(String, Option<i64>)> {
        let auth: Value = serde_json::from_slice(auth_state.contents.as_deref()?).ok()?;
        let chatgpt_account_id = extract_codex_managed_oauth_account_id(&auth)?;
        let user_identity = extract_codex_auth_user_identity(&auth);
        let marker = marker_state.contents.as_deref().and_then(|contents| {
            serde_json::from_slice::<CodexManagedOAuthLiveAuthMarker>(contents).ok()
        });
        let generation_id = match marker {
            Some(marker)
                if matches!(marker.version, 1 | 2)
                    && marker.account_id == chatgpt_account_id
                    && user_identity.is_some() =>
            {
                format!(
                    "managed:{}:{}",
                    marker.account_id,
                    user_identity.as_deref().expect("checked above")
                )
            }
            Some(marker)
                if marker.version == 3
                    && marker.chatgpt_account_id.as_deref()
                        == Some(chatgpt_account_id.as_str())
                    && marker
                        .user_identity
                        .as_deref()
                        .is_some_and(|identity| user_identity.as_deref() == Some(identity)) =>
            {
                format!(
                    "managed:{}:{}",
                    marker.account_id,
                    marker.user_identity.as_deref().expect("checked above")
                )
            }
            _ => format!(
                "native:{}",
                user_identity.as_deref().unwrap_or(&chatgpt_account_id)
            ),
        };
        let last_refresh_ms = auth
            .get("last_refresh")
            .and_then(Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.timestamp_millis());
        Some((generation_id, last_refresh_ms))
    }
}

/// Which Codex tool surface the generated model catalog should target.
///
/// - `ProxyChat`: cc-switch's proxy takes over and converts Responses<->Chat,
///   so the catalog keeps Codex's default tool set (incl. the freeform
///   `apply_patch` custom tool, which the proxy rewrites to a function tool).
/// - `NativeResponses`: Codex talks directly to a provider's native
///   `/responses` endpoint (no proxy). Such gateways (e.g. Xiaomi MiMo,
///   MiniMax) reject `type=="custom"` tools, so the catalog must suppress the
///   freeform `apply_patch` and rely on `shell_type="shell_command"` for edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexCatalogToolProfile {
    ProxyChat,
    NativeResponses,
    /// Codex talks (through cc-switch's proxy) to a native Anthropic Messages
    /// gateway. Like `NativeResponses` it must suppress Codex's freeform custom
    /// tools — the Responses→Anthropic transform keeps only `function` tools.
    /// Additionally the Codex `web_search` hosted tool is unusable on this path
    /// (the transform drops it), so it is always disabled — see
    /// `prepare_codex_config_text_with_model_catalog`.
    Anthropic,
}

impl CodexCatalogToolProfile {
    /// Pick the catalog tool profile from a provider's `apiFormat` meta value.
    ///
    /// Prefer [`crate::proxy::providers::codex::resolve_codex_catalog_tool_profile`],
    /// which also honors settings-level `apiFormat` and the TOML `wire_api` (matching
    /// the proxy router). This string-only mapping is the fallback for non-Anthropic
    /// cases.
    pub fn from_api_format(api_format: Option<&str>) -> Self {
        match api_format {
            Some("anthropic") => CodexCatalogToolProfile::Anthropic,
            // Native (direct) Responses gateways reject Codex's freeform custom
            // tools (apply_patch, etc.); strip them via the NativeResponses profile.
            Some("openai_responses") => CodexCatalogToolProfile::NativeResponses,
            _ => CodexCatalogToolProfile::ProxyChat,
        }
    }
}

/// Reserved built-in provider IDs from OpenAI Codex's config/model-provider
/// catalog. Keep in sync with Codex `RESERVED_MODEL_PROVIDER_IDS` (0.149:
/// exactly these five; 0.148 is the same minus `amazon-bedrock-runtime`).
/// `oss` / `ollama-chat` are NOT reserved on 0.148/0.149 — both load as
/// ordinary custom tables — so listing them here would strand their bearer
/// token in the ignored top level. Mirror: providerConfigUtils.ts.
const CODEX_RESERVED_MODEL_PROVIDER_IDS: &[&str] = &[
    "amazon-bedrock",
    "amazon-bedrock-runtime",
    "openai",
    "ollama",
    "lmstudio",
];

/// 获取 Codex 配置目录路径
pub fn get_codex_config_dir() -> PathBuf {
    if let Some(custom) = crate::settings::get_codex_override_dir() {
        return custom;
    }

    get_home_dir().join(".codex")
}

/// 获取 Codex auth.json 路径
pub fn get_codex_auth_path() -> PathBuf {
    get_codex_config_dir().join("auth.json")
}

fn get_codex_managed_oauth_live_auth_marker_path() -> PathBuf {
    crate::config::get_app_config_dir().join(CODEX_MANAGED_OAUTH_LIVE_AUTH_MARKER_FILENAME)
}

#[cfg(test)]
pub(crate) fn codex_managed_oauth_live_auth_marker_exists() -> bool {
    get_codex_managed_oauth_live_auth_marker_path().exists()
}

/// 从 live/备份的 Codex `auth` 中提取上游 ChatGPT workspace ID。
///
/// 仅接受 ChatGPT 登录形状（`auth_mode == "chatgpt"`、`OPENAI_API_KEY` 可清空）。
/// 托管账号写入的完整 bundle 会额外带 `tokens.refresh_token` 与顶层 `last_refresh`，
/// 这里一并容忍。Codex CLI 自刷新会轮换 access_token，因此短期 token 指纹不能
/// 作为稳定的所有权谓词；cc-switch 的本地账号 ID 单独记录在 marker 中。
fn extract_codex_managed_oauth_account_id(auth: &Value) -> Option<String> {
    let auth_obj = auth.as_object()?;

    if auth_obj.keys().any(|key| {
        !matches!(
            key.as_str(),
            "auth_mode" | "OPENAI_API_KEY" | "tokens" | "last_refresh"
        )
    }) {
        return None;
    }

    if auth.get("auth_mode").and_then(|value| value.as_str()) != Some("chatgpt") {
        return None;
    }

    let api_key_is_clearable = auth
        .get("OPENAI_API_KEY")
        .is_none_or(|value| value.is_null() || value.as_str() == Some("PROXY_MANAGED"));
    if !api_key_is_clearable {
        return None;
    }

    let tokens = auth.get("tokens").and_then(|value| value.as_object())?;

    if tokens.keys().any(|key| {
        !matches!(
            key.as_str(),
            "access_token" | "account_id" | "id_token" | "refresh_token"
        )
    }) {
        return None;
    }

    let account_id = tokens
        .get("account_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    tokens
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())?;

    Some(account_id.to_string())
}

/// 从原生 auth.json 的 id_token 提取跨刷新稳定的用户身份。
fn extract_codex_auth_user_identity(auth: &Value) -> Option<String> {
    let id_token = auth.pointer("/tokens/id_token")?.as_str()?;
    extract_codex_id_token_user_identity(id_token)
}

pub(crate) fn extract_codex_id_token_user_identity(id_token: &str) -> Option<String> {
    extract_codex_id_token_subject(id_token).map(|subject| format!("sub:{subject}"))
}

pub(crate) fn extract_codex_id_token_subject(id_token: &str) -> Option<String> {
    let mut segments = id_token.split('.');
    let header = segments.next()?;
    let payload = segments.next()?;
    segments.next()?;
    if segments.next().is_some() {
        return None;
    }

    let header: Value = URL_SAFE_NO_PAD
        .decode(header)
        .ok()
        .and_then(|decoded| serde_json::from_slice(&decoded).ok())?;
    header
        .get("alg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;

    let claims: Value = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|decoded| serde_json::from_slice(&decoded).ok())?;
    claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
pub(crate) fn test_codex_id_token(subject: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = URL_SAFE_NO_PAD.encode(json!({ "sub": subject }).to_string());
    format!("{header}.{payload}.")
}

/// Build the native-shaped ChatGPT auth bundle shared by cc-switch and Codex CLI.
pub fn codex_managed_oauth_auth_value(
    account_id: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: &str,
    last_refresh: &str,
) -> Value {
    let mut tokens = serde_json::Map::new();
    if let Some(id_token) = id_token {
        tokens.insert("id_token".to_string(), Value::String(id_token.to_string()));
    }
    tokens.insert(
        "access_token".to_string(),
        Value::String(access_token.to_string()),
    );
    tokens.insert(
        "refresh_token".to_string(),
        Value::String(refresh_token.to_string()),
    );
    tokens.insert(
        "account_id".to_string(),
        Value::String(account_id.to_string()),
    );
    json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": Value::Object(tokens),
        "last_refresh": last_refresh,
    })
}

pub fn record_codex_managed_oauth_live_auth(
    auth: &Value,
    managed_account_id: &str,
) -> Result<(), AppError> {
    let managed_account_id = managed_account_id.trim();
    let Some(chatgpt_account_id) = extract_codex_managed_oauth_account_id(auth) else {
        return Ok(());
    };
    if managed_account_id.is_empty() {
        return Ok(());
    }
    let user_identity = extract_codex_auth_user_identity(auth).ok_or_else(|| {
        AppError::Message(
            "Codex 托管 OAuth auth.json 的 id_token 缺少稳定用户身份，无法安全记录账号所有权"
                .to_string(),
        )
    })?;

    let marker = CodexManagedOAuthLiveAuthMarker {
        version: 3,
        account_id: managed_account_id.to_string(),
        chatgpt_account_id: Some(chatgpt_account_id),
        user_identity: Some(user_identity),
    };
    crate::config::write_json_file(&get_codex_managed_oauth_live_auth_marker_path(), &marker)
}

fn migrate_legacy_codex_managed_oauth_live_auth_marker(
    auth: &Value,
    managed_account_id: &str,
    managed_id_token: Option<&str>,
) -> Result<(), AppError> {
    let marker_path = get_codex_managed_oauth_live_auth_marker_path();
    if !marker_path.exists() {
        return Ok(());
    }
    let marker: CodexManagedOAuthLiveAuthMarker = read_json_file(&marker_path)?;
    if !matches!(marker.version, 1 | 2) || marker.account_id != managed_account_id {
        return Ok(());
    }

    let auth_account_id = extract_codex_managed_oauth_account_id(auth);
    let auth_user_identity = extract_codex_auth_user_identity(auth);
    let managed_user_identity = managed_id_token.and_then(extract_codex_id_token_user_identity);
    if auth_account_id.as_deref() != Some(managed_account_id)
        || auth_user_identity.as_deref() != managed_user_identity.as_deref()
        || managed_user_identity.is_none()
    {
        return Err(AppError::Message(format!(
            "旧版 Codex OAuth 账号 {managed_account_id} 无法通过稳定用户身份确认磁盘凭据所有权；为避免覆盖或串用 auth.json，本次操作已取消，请在认证中心重新登录该账号"
        )));
    }

    record_codex_managed_oauth_live_auth(auth, managed_account_id)
}

/// Before removing a manager record, make any legacy live-auth ownership
/// provable with the manager's persisted user identity. Failure is surfaced so
/// callers keep the manager record and marker instead of orphaning auth.json.
pub(crate) fn prepare_codex_live_auth_for_managed_account_removal(
    managed_account_id: &str,
    managed_id_token: Option<&str>,
) -> Result<(), AppError> {
    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return Ok(());
    }
    let auth: Value = read_json_file(&auth_path)?;
    migrate_legacy_codex_managed_oauth_live_auth_marker(&auth, managed_account_id, managed_id_token)
}

pub fn codex_auth_matches_recorded_managed_oauth(
    auth: &Value,
    account_id: &str,
) -> Result<bool, AppError> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Ok(false);
    }

    let Some(auth_account_id) = extract_codex_managed_oauth_account_id(auth) else {
        return Ok(false);
    };
    let auth_user_identity = extract_codex_auth_user_identity(auth);
    let marker_path = get_codex_managed_oauth_live_auth_marker_path();
    let marker: CodexManagedOAuthLiveAuthMarker = match read_json_file(&marker_path) {
        Ok(marker) => marker,
        Err(err) => {
            log::warn!(
                "Failed to read Codex managed OAuth auth marker at {}: {err}",
                marker_path.display()
            );
            return Ok(false);
        }
    };

    // v1/v2 markers do not carry a stable user identity. Since multiple users
    // can share one workspace, those markers cannot safely authorize adopting
    // or deleting credentials. The next explicit activation replaces them
    // with a v3 marker.
    Ok(marker.account_id == account_id
        && match marker.version {
            3 => {
                marker.chatgpt_account_id.as_deref() == Some(auth_account_id.as_str())
                    && marker
                        .user_identity
                        .as_deref()
                        .is_some_and(|identity| auth_user_identity.as_deref() == Some(identity))
            }
            _ => false,
        })
}

/// Verify that a proxied Codex request still uses the exact live access token
/// owned by the selected local account. Workspace IDs alone are not sufficient:
/// different Team users can share one value.
pub(crate) fn codex_live_auth_matches_managed_request(
    account_id: &str,
    request_access_token: &str,
) -> Result<bool, AppError> {
    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return Ok(false);
    }
    let auth: Value = read_json_file(&auth_path)?;
    if !codex_auth_matches_recorded_managed_oauth(&auth, account_id)? {
        return Ok(false);
    }
    let live_access_token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty());
    Ok(live_access_token == Some(request_access_token.trim()))
}

pub(crate) fn clear_codex_managed_oauth_live_auth_marker_for_account(
    account_id: &str,
) -> Result<(), AppError> {
    let marker_path = get_codex_managed_oauth_live_auth_marker_path();
    if !marker_path.exists() {
        return Ok(());
    }
    let marker: CodexManagedOAuthLiveAuthMarker = match read_json_file(&marker_path) {
        Ok(marker) => marker,
        Err(error) => {
            log::warn!(
                "Failed to read Codex managed OAuth auth marker at {} while cleaning account {}: {error}",
                marker_path.display(),
                account_id
            );
            // A malformed marker cannot establish ownership for any account
            // and is unusable for rollback/synchronization; remove the stale
            // bookkeeping file while leaving non-matching live auth untouched.
            return delete_file(&marker_path);
        }
    };
    if marker.account_id == account_id.trim() {
        delete_file(&marker_path)?;
    }
    Ok(())
}

/// 切走托管 provider 或从认证中心删除账号时，清理其残留在
/// `~/.codex/auth.json` 的 ChatGPT 登录。
///
/// 删除谓词同时校验 cc-switch marker 中的本地账号 ID 与原生 auth.json 中的
/// workspace ID，不依赖会被 Codex CLI 自刷新破坏的 access-token 指纹。切换路径必须
/// 先把盘上轮换后的 refresh token 采纳回 manager，再调用本函数。
pub fn clear_codex_live_auth_for_managed_account(account_id: &str) -> Result<(), AppError> {
    clear_codex_live_auth_for_managed_account_if_unchanged(account_id, None)
}

/// Verify that the outgoing account's live refresh generation has not changed
/// since it was adopted into the OAuth manager.
pub fn ensure_codex_live_auth_unchanged_for_managed_account(
    account_id: &str,
    expected_refresh_token: &str,
) -> Result<(), AppError> {
    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return Err(AppError::Message(format!(
            "Codex CLI 账号 {account_id} 的 live auth 已在切换期间被移除，请重试"
        )));
    }
    let auth: Value = read_json_file(&auth_path)?;
    let current_refresh_token = auth
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .map(str::trim);
    if !codex_live_auth_is_managed_chatgpt_login(&auth, account_id)
        || current_refresh_token != Some(expected_refresh_token.trim())
    {
        return Err(AppError::Message(format!(
            "Codex CLI 账号 {account_id} 的 live 凭据在切换期间已刷新；为避免覆盖新 refresh token，本次操作已取消，请重试"
        )));
    }
    Ok(())
}

/// Content-based cleanup with an optional compare-before-delete guard.
pub fn clear_codex_live_auth_for_managed_account_if_unchanged(
    account_id: &str,
    expected_refresh_token: Option<&str>,
) -> Result<(), AppError> {
    let auth_path = get_codex_auth_path();
    let mut removed_matching_auth = false;
    if auth_path.exists() {
        let auth: Value = read_json_file(&auth_path)?;
        if codex_live_auth_is_managed_chatgpt_login(&auth, account_id) {
            if let Some(expected_refresh_token) = expected_refresh_token {
                let current_refresh_token = auth
                    .pointer("/tokens/refresh_token")
                    .and_then(Value::as_str)
                    .map(str::trim);
                if current_refresh_token != Some(expected_refresh_token.trim()) {
                    return Err(AppError::Message(format!(
                        "Codex CLI 账号 {account_id} 的 live 凭据在切换期间已刷新；为避免删除新 refresh token，本次操作已取消，请重试"
                    )));
                }
            }
            delete_file(&auth_path)?;
            removed_matching_auth = true;
        }
    }

    if removed_matching_auth {
        // Once the matching live file is gone, any marker is stale regardless
        // of version or parseability.
        delete_file(&get_codex_managed_oauth_live_auth_marker_path())?;
    } else {
        clear_codex_managed_oauth_live_auth_marker_for_account(account_id)?;
    }
    Ok(())
}

/// 判断给定的 Codex `auth` 是否属于指定的 cc-switch 本地托管账号。
///
/// 原生 `tokens.account_id` 是 workspace ID，可能被多个本地账号共享；因此必须同时
/// 命中 cc-switch marker 中的本地账号 ID，不能只按 auth.json 内容判断。
///
/// 用于 Live 备份剥离：避免把托管账号的可刷新 token 持久化进备份配置。
pub fn codex_live_auth_is_managed_chatgpt_login(auth: &Value, account_id: &str) -> bool {
    codex_auth_matches_recorded_managed_oauth(auth, account_id).unwrap_or(false)
}

/// 读回 Codex CLI 当前 `~/.codex/auth.json` 中属于 `account_id` 的 refresh_token /
/// id_token（仅当磁盘上的登录账号与之一致时）。
///
/// 用于切换回托管 provider 前，采纳 CLI 自行刷新时轮换出的最新 refresh_token，避免
/// 用陈腐 token 覆盖 CLI 的有效登录（“裸跑 codex” 反复切换场景）。
pub fn read_codex_live_auth_refresh_for_account(
    account_id: &str,
) -> Option<(String, Option<String>, Option<i64>)> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return None;
    }
    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return None;
    }
    let auth: Value = read_json_file(&auth_path).ok()?;
    // 仅在磁盘上确是「该 account_id 的 ChatGPT 登录」时才采纳其 refresh_token，
    // 避免从非 chatgpt/异常 auth 里误取 token。
    if !codex_live_auth_is_managed_chatgpt_login(&auth, account_id) {
        return None;
    }
    let tokens = auth.get("tokens")?.as_object()?;
    let refresh_token = tokens.get("refresh_token")?.as_str()?.trim().to_string();
    if refresh_token.is_empty() {
        return None;
    }
    let id_token = tokens
        .get("id_token")
        .and_then(|value| value.as_str())
        .map(|token| token.to_string());
    let last_refresh_ms = auth
        .get("last_refresh")
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis());
    Some((refresh_token, id_token, last_refresh_ms))
}

/// Read a managed live credential after safely upgrading a legacy marker.
/// v1/v2 markers only identify a workspace, so the manager's persisted
/// id_token must prove the live user's identity before the marker can become
/// authoritative again.
pub(crate) fn read_codex_live_auth_refresh_for_managed_account(
    account_id: &str,
    managed_id_token: Option<&str>,
) -> Result<Option<CodexManagedLiveRefresh>, AppError> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Ok(None);
    }
    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return Ok(None);
    }
    let auth: Value = read_json_file(&auth_path)?;
    migrate_legacy_codex_managed_oauth_live_auth_marker(&auth, account_id, managed_id_token)?;
    if !codex_auth_matches_recorded_managed_oauth(&auth, account_id)? {
        return Ok(None);
    }
    let Some((refresh_token, id_token, last_refresh_ms)) =
        read_codex_live_auth_refresh_for_account(account_id)
    else {
        return Ok(None);
    };
    let chatgpt_account_id = extract_codex_managed_oauth_account_id(&auth)
        .ok_or_else(|| AppError::Message("Codex live auth 缺少 workspace ID".to_string()))?;
    Ok(Some(CodexManagedLiveRefresh {
        refresh_token,
        id_token,
        last_refresh_ms,
        chatgpt_account_id,
    }))
}

/// Keep Codex CLI's live auth in the same refresh-token generation after the
/// manager refreshes a managed account.
///
/// The write is compare-and-swap-like: immediately before replacing auth.json,
/// it verifies that the file still contains the refresh token used for the
/// network request. Codex CLI does not share cc-switch's process lock, so this
/// is a best-effort guard that narrows (but cannot make atomic) the cross-process
/// check-to-replace window.
/// Ownership is local-account scoped through the marker, while auth.json keeps
/// the upstream workspace ID required by Codex.
pub fn sync_codex_managed_oauth_live_auth_after_refresh(
    account_id: &str,
    expected_refresh_token: &str,
    refreshed_auth: &Value,
) -> Result<bool, AppError> {
    let account_id = account_id.trim();
    let expected_refresh_token = expected_refresh_token.trim();
    if account_id.is_empty() || expected_refresh_token.is_empty() {
        return Ok(false);
    }

    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return Ok(false);
    }
    let current_auth: Value = read_json_file(&auth_path)?;
    if !codex_live_auth_is_managed_chatgpt_login(&current_auth, account_id) {
        return Ok(false);
    }
    let current_refresh_token = current_auth
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .map(str::trim);
    if current_refresh_token != Some(expected_refresh_token) {
        return Ok(false);
    }

    let marker_path = get_codex_managed_oauth_live_auth_marker_path();
    let was_recorded_managed = marker_path.exists()
        && codex_auth_matches_recorded_managed_oauth(&current_auth, account_id)?;

    write_json_file_private(&auth_path, refreshed_auth)?;
    if was_recorded_managed {
        record_codex_managed_oauth_live_auth(refreshed_auth, account_id)?;
    }
    Ok(true)
}

/// 获取 Codex config.toml 路径
pub fn get_codex_config_path() -> PathBuf {
    get_codex_config_dir().join("config.toml")
}

pub fn get_codex_model_catalog_path() -> PathBuf {
    get_codex_config_dir().join(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)
}

/// 原子写 Codex 的 `auth.json` 与 `config.toml`，在第二步失败时回滚第一步
pub fn write_codex_live_atomic(
    auth: &Value,
    config_text_opt: Option<&str>,
) -> Result<(), AppError> {
    let auth_path = get_codex_auth_path();
    let config_path = get_codex_config_path();

    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    // 读取旧内容用于回滚
    let old_auth = if auth_path.exists() {
        Some(fs::read(&auth_path).map_err(|e| AppError::io(&auth_path, e))?)
    } else {
        None
    };
    let _old_config = if config_path.exists() {
        Some(fs::read(&config_path).map_err(|e| AppError::io(&config_path, e))?)
    } else {
        None
    };

    // 准备写入内容
    let cfg_text = match config_text_opt {
        Some(s) => s.to_string(),
        None => String::new(),
    };
    if !cfg_text.trim().is_empty() {
        toml::from_str::<toml::Table>(&cfg_text).map_err(|e| AppError::toml(&config_path, e))?;
    }

    // 第一步：写 auth.json（含 OAuth 凭证，权限收紧）
    write_json_file_private(&auth_path, auth)?;

    // 第二步：写 config.toml（失败则回滚 auth.json）
    if let Err(e) = write_text_file_private(&config_path, &cfg_text) {
        // 回滚 auth.json —— 同样必须用 private，否则回滚瞬间又写出 0644 文件
        if let Some(bytes) = old_auth {
            if let Err(rollback_err) = atomic_write_private(&auth_path, &bytes) {
                log::error!(
                    "回滚 Codex auth.json 失败，磁盘可能停在半写入状态: path={} err={rollback_err}",
                    auth_path.display()
                );
            }
        } else if let Err(rollback_err) = delete_file(&auth_path) {
            log::error!(
                "回滚删除 Codex auth.json 失败: path={} err={rollback_err}",
                auth_path.display()
            );
        }
        return Err(e);
    }

    Ok(())
}

/// 读取 `~/.codex/config.toml`，若不存在返回空字符串
pub fn read_codex_config_text() -> Result<String, AppError> {
    let path = get_codex_config_path();
    if path.exists() {
        std::fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))
    } else {
        Ok(String::new())
    }
}

/// 对非空的 TOML 文本进行语法校验
pub fn validate_config_toml(text: &str) -> Result<(), AppError> {
    if text.trim().is_empty() {
        return Ok(());
    }
    toml::from_str::<toml::Table>(text)
        .map(|_| ())
        .map_err(|e| AppError::toml(Path::new("config.toml"), e))
}

/// 读取并校验 `~/.codex/config.toml`，返回文本（可能为空）
pub fn read_and_validate_codex_config_text() -> Result<String, AppError> {
    let s = read_codex_config_text()?;
    validate_config_toml(&s)?;
    Ok(s)
}

fn active_codex_model_provider_id(doc: &DocumentMut) -> Option<String> {
    doc.get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(crate) fn is_custom_codex_model_provider_id(id: &str) -> bool {
    // Exact match, mirroring upstream: both the built-in provider lookup and
    // validate_reserved_model_provider_ids are case-sensitive, so `OpenAI`
    // etc. are legitimate custom ids whose tables must receive the token.
    // Keep in sync with the frontend list in src/utils/providerConfigUtils.ts.
    let id = id.trim();
    !id.is_empty() && !CODEX_RESERVED_MODEL_PROVIDER_IDS.contains(&id)
}

/// Write only Codex `config.toml` for provider switching.
///
/// Codex login state lives in `auth.json`; provider routing, endpoint, model,
/// and provider-scoped bearer tokens live in `config.toml`. Provider switches
/// should not overwrite the user's ChatGPT login cache.
pub fn write_codex_live_config_atomic(config_text_opt: Option<&str>) -> Result<(), AppError> {
    let config_path = get_codex_config_path();
    let cfg_text = match config_text_opt {
        Some(config_text) => config_text.to_string(),
        None => String::new(),
    };

    if !cfg_text.trim().is_empty() {
        toml::from_str::<toml::Table>(&cfg_text).map_err(|e| AppError::toml(&config_path, e))?;
    }

    write_text_file_private(&config_path, &cfg_text)
}

// ==================== config.toml 外科手术式合并 ====================
//
// 供应商切换/接管写入的 config 文本是 cc-switch 生成的"稀疏"模板：只有当前
// 供应商需要的键。历史上写盘会整份替换 config.toml，把用户手写的注释、键顺序、
// 自定义段落（profiles、mcp_servers 等）一并抹掉。这里的合并以新文本的键为
// 权威覆盖/插入，其余既有内容原样保留（magpie 式 surgical editing）。
//
// 仅两类例外会被移除：
// - `model_catalog_json` 指向 cc-switch 生成的目录文件（陈旧指针会让读取端
//   误报上一个供应商的目录）——要求值携带 ownership 哨兵，用户手写的同名键
//   绝不受影响；
// - `web_search = "disabled"`（cc-switch 自己写入的哨兵值，切换后残留即垃圾，
//   同样要求哨兵值）；顶层 `experimental_bearer_token` 无论值一律随切换剥离
//   （新配置缺席时）：它是供应商级凭证，残留会让下一个供应商的端点收到上一个
//   供应商的 token——安全属性优先于配置保留，用户注释/自定义段落不受影响。

/// 判断该键当前值是否为 cc-switch 拥有的哨兵值（merge 时允许在新配置缺席时移除）。
fn codex_config_value_is_cc_switch_owned(key: &str, item: &toml_edit::Item) -> bool {
    let value = match item.as_str() {
        Some(value) => value,
        None => return false,
    };
    match key {
        "model_catalog_json" => {
            Path::new(value).file_name().and_then(|name| name.to_str())
                == Some(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)
        }
        "web_search" => value == CODEX_WEB_SEARCH_DISABLED,
        // 供应商级凭证：任何值都不得跨切换残留（泄漏 = A 的 token 发到 B 的端点）
        "experimental_bearer_token" => true,
        _ => false,
    }
}

/// 新文本标量是否自带注释（自带注释时整体替换，不保留旧侧 decor）。
fn codex_config_item_has_comment(item: &toml_edit::Item) -> bool {
    item.as_value()
        .map(|value| {
            let has = |decor: Option<&toml_edit::RawString>| {
                decor
                    .and_then(|text| text.as_str())
                    .is_some_and(|text| text.contains('#'))
            };
            has(value.decor().prefix()) || has(value.decor().suffix())
        })
        .unwrap_or(false)
}

/// 让合并输出的换行风格跟随既有文件（CRLF 为主 → 统一 CRLF，否则统一 LF）。
///
/// 输出含 TOML 多行字符串字面量（`"""` / `'''`）时跳过统一：全局 `\n` ↔ `\r\n`
/// 改写会把字面量内部的真实换行一并改写，直接改变其语义内容；而混合换行是
/// 合法 TOML，只是美观问题。
fn codex_config_merge_harmonize_newlines(output: &str, existing_text: &str) -> String {
    if output.contains("\"\"\"") || output.contains("'''") {
        return output.to_string();
    }
    let crlf = existing_text.matches("\r\n").count();
    let lf_total = existing_text.matches('\n').count();
    let lf_only = lf_total.saturating_sub(crlf);
    if crlf > lf_only {
        output.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        output.replace("\r\n", "\n")
    }
}

/// 外科手术式合并：`new_text` 中出现的顶层键覆盖/插入（表与数组整体替换、
/// 标量替换值并保留旧侧注释 decor），`existing_text` 独有的键/表/注释原样保留。
///
/// 任一侧解析失败或 `existing_text` 为空时，回退为直接返回 `new_text`
/// （与既有整份替换行为一致，保注释绝不以写入失败为代价）。
fn merge_codex_config_surgical(existing_text: &str, new_text: &str) -> String {
    if new_text.trim().is_empty() {
        return new_text.to_string();
    }
    let new_doc: DocumentMut = match new_text.parse() {
        Ok(doc) => doc,
        Err(_) => return new_text.to_string(),
    };
    if existing_text.trim().is_empty() {
        return new_text.to_string();
    }
    let mut doc: DocumentMut = match existing_text.parse() {
        Ok(doc) => doc,
        Err(_) => return new_text.to_string(),
    };

    // 1) cc-switch 拥有、新配置不再携带的哨兵键 → 移除（陈旧值不得残留）。
    for key in [
        "model_catalog_json",
        "web_search",
        "experimental_bearer_token",
    ] {
        let stale = doc
            .get(key)
            .is_some_and(|item| codex_config_value_is_cc_switch_owned(key, item))
            && new_doc.get(key).is_none();
        if stale {
            doc.as_table_mut().remove(key);
        }
    }

    // 2) 新配置的每个顶层键：覆盖/插入。
    let root = doc.as_table_mut();
    for (key, src_item) in new_doc.as_table().iter() {
        let replace_scalar_keep_decor = root.get(key).is_some_and(|dest| {
            dest.is_value() && src_item.is_value() && !codex_config_item_has_comment(src_item)
        });
        if replace_scalar_keep_decor {
            if let Some(toml_edit::Item::Value(dest_value)) = root.get_mut(key) {
                let decor = dest_value.decor().clone();
                *dest_value = src_item
                    .clone()
                    .into_value()
                    .expect("checked is_value above");
                *dest_value.decor_mut() = decor;
            }
        } else {
            root.insert(key, src_item.clone());
        }
    }

    codex_config_merge_harmonize_newlines(&doc.to_string(), existing_text)
}

/// 读取盘上 `~/.codex/config.toml` 并与 `new_text` 做外科手术式合并。
///
/// 仅供应商切换/接管写入路径调用（文本来自 cc-switch 生成的稀疏模板）；
/// verbatim 恢复与接管清理路径写入的是完整文本、键缺席即删除意图，
/// 不得经过本函数。盘读取失败时按空文件处理（回退整份替换）。
pub(crate) fn merge_codex_live_config_with_disk(new_text: &str) -> String {
    let existing_text = read_codex_config_text().unwrap_or_default();
    merge_codex_config_surgical(&existing_text, new_text)
}

pub fn extract_codex_auth_api_key(auth: &Value) -> Option<String> {
    auth.get("OPENAI_API_KEY")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
}

pub fn extract_codex_api_key(auth: Option<&Value>, config_text: Option<&str>) -> Option<String> {
    auth.and_then(extract_codex_auth_api_key)
        .or_else(|| config_text.and_then(extract_codex_experimental_bearer_token))
}

/// Extract the upstream base URL from a Codex `config.toml` string.
///
/// Prefers the active `[model_providers.<model_provider>].base_url`, falling
/// back to a top-level `base_url`. Deliberately never reads a non-active
/// `[model_providers.*]` section — the frontend `extractCodexBaseUrl`
/// (`getRecoverableBaseUrlAssignments`) excludes those too, and a leftover
/// section unrelated to the active provider must not leak into `{{baseUrl}}`.
pub fn extract_codex_base_url(config_text: &str) -> Option<String> {
    let doc = config_text.parse::<toml::Value>().ok()?;

    if let Some(active_provider) = doc.get("model_provider").and_then(|v| v.as_str()) {
        if let Some(base_url) = doc
            .get("model_providers")
            .and_then(|providers| providers.get(active_provider))
            .and_then(|provider| provider.get("base_url"))
            .and_then(|v| v.as_str())
        {
            return Some(base_url.to_string());
        }
    }

    doc.get("base_url")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub fn codex_auth_has_login_material(auth: &Value) -> bool {
    let Some(obj) = auth.as_object() else {
        return false;
    };

    obj.iter().any(|(key, value)| {
        if key == "auth_mode" {
            return false;
        }

        if key == "OPENAI_API_KEY" {
            return value
                .as_str()
                .map(str::trim)
                .is_some_and(|token| !token.is_empty());
        }

        match value {
            Value::Null => false,
            Value::String(text) => !text.trim().is_empty(),
            Value::Array(items) => !items.is_empty(),
            Value::Object(map) => !map.is_empty(),
            _ => true,
        }
    })
}

pub fn codex_auth_has_oauth_login_material(auth: &Value) -> bool {
    let Some(obj) = auth.as_object() else {
        return false;
    };

    obj.iter().any(|(key, value)| {
        if key == "auth_mode" || key == "OPENAI_API_KEY" {
            return false;
        }

        match value {
            Value::Null => false,
            Value::String(text) => !text.trim().is_empty(),
            Value::Array(items) => !items.is_empty(),
            Value::Object(map) => !map.is_empty(),
            _ => true,
        }
    })
}

/// The auth mode Codex resolves for an `auth.json` payload
/// (`AuthDotJson::resolved_mode`, `codex-rs/login/src/auth/manager.rs`,
/// 0.153.2): an explicit `auth_mode` wins outright; otherwise presence
/// decides in this order — `personal_access_token`, `bedrock_api_key`,
/// `bedrock_access_keys`, `OPENAI_API_KEY` — and everything else is
/// ChatGPT. Presence is `Option::is_some`, i.e. any non-null value, even an
/// empty one; the material itself is checked afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexResolvedAuthMode {
    ApiKey,
    Chatgpt,
    ChatgptAuthTokens,
    Headers,
    AgentIdentity,
    PersonalAccessToken,
    BedrockApiKey,
    BedrockAccessKeys,
    /// An `auth_mode` string Codex's serde rejects (`rename_all =
    /// "lowercase"` plus explicit camelCase renames, exact match): the whole
    /// file fails to load, which Codex reports as signed out.
    Unrecognized,
}

fn codex_auth_resolved_mode(auth: &serde_json::Map<String, Value>) -> CodexResolvedAuthMode {
    let present = |key: &str| auth.get(key).is_some_and(|value| !value.is_null());

    // `auth_mode: null` deserializes to `None` (serde default) and falls
    // through to the implicit precedence below.
    if let Some(mode) = auth.get("auth_mode").filter(|value| !value.is_null()) {
        return match mode.as_str() {
            Some("apikey") => CodexResolvedAuthMode::ApiKey,
            Some("chatgpt") => CodexResolvedAuthMode::Chatgpt,
            Some("chatgptAuthTokens") => CodexResolvedAuthMode::ChatgptAuthTokens,
            Some("headers") => CodexResolvedAuthMode::Headers,
            Some("agentIdentity") => CodexResolvedAuthMode::AgentIdentity,
            Some("personalAccessToken") => CodexResolvedAuthMode::PersonalAccessToken,
            Some("bedrockApiKey") => CodexResolvedAuthMode::BedrockApiKey,
            Some("bedrockAccessKeys") => CodexResolvedAuthMode::BedrockAccessKeys,
            _ => CodexResolvedAuthMode::Unrecognized,
        };
    }
    if present("personal_access_token") {
        return CodexResolvedAuthMode::PersonalAccessToken;
    }
    if present("bedrock_api_key") {
        return CodexResolvedAuthMode::BedrockApiKey;
    }
    if present("bedrock_access_keys") {
        return CodexResolvedAuthMode::BedrockAccessKeys;
    }
    if present("OPENAI_API_KEY") {
        return CodexResolvedAuthMode::ApiKey;
    }
    CodexResolvedAuthMode::Chatgpt
}

/// True when Codex would load `auth` as a signed-in OpenAI account for a
/// `requires_openai_auth` provider — the state its login screen and
/// `ConfiguredModelProvider::account_state` (0.149+) go by. The auth mode is
/// resolved exactly as Codex does (`codex_auth_resolved_mode`) and only then
/// is the matching credential checked, so a Bedrock credential outranks a
/// stale `OPENAI_API_KEY` sitting next to it just as it does in Codex, where
/// that probe returns `UnsupportedBedrockApiKeyAuth` and fails TUI startup.
/// Modes Codex cannot load from storage (`headers`, unrecognized) are signed
/// out. The credential must be non-blank (stricter than Codex's `is_some`,
/// erring toward "signed out"); metadata such as `last_refresh` never counts.
pub fn codex_auth_has_openai_account_material(auth: &Value) -> bool {
    let Some(obj) = auth.as_object() else {
        return false;
    };

    let value_present = |value: &Value| match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        _ => true,
    };
    let has = |key: &str| obj.get(key).is_some_and(value_present);

    match codex_auth_resolved_mode(obj) {
        CodexResolvedAuthMode::ApiKey => extract_codex_auth_api_key(auth).is_some(),
        CodexResolvedAuthMode::PersonalAccessToken => has("personal_access_token"),
        CodexResolvedAuthMode::AgentIdentity => has("agent_identity"),
        CodexResolvedAuthMode::Chatgpt | CodexResolvedAuthMode::ChatgptAuthTokens => obj
            .get("tokens")
            .and_then(Value::as_object)
            .is_some_and(|tokens| {
                ["id_token", "access_token", "refresh_token"]
                    .iter()
                    .any(|key| tokens.get(*key).is_some_and(value_present))
            }),
        CodexResolvedAuthMode::Headers
        | CodexResolvedAuthMode::BedrockApiKey
        | CodexResolvedAuthMode::BedrockAccessKeys
        | CodexResolvedAuthMode::Unrecognized => false,
    }
}

/// Where Codex keeps CLI auth, per the top-level `cli_auth_credentials_store`
/// key (`codex-rs/config/src/types.rs`, serde lowercase; unset = `file`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexAuthStoreMode {
    /// `auth.json` is the only store — the file decides login state.
    File,
    /// Keyring only; `auth.json` is never read and is deleted after a save.
    Keyring,
    /// Keyring first, `auth.json` as fallback for both load and save.
    Auto,
    /// In-process only; nothing on disk is ever a login.
    Ephemeral,
    /// Unparsable config or a value Codex would reject.
    Unknown,
}

pub(crate) fn codex_config_auth_store_mode(config_text: &str) -> CodexAuthStoreMode {
    if !config_text.contains("cli_auth_credentials_store") {
        return CodexAuthStoreMode::File;
    }
    let Ok(doc) = config_text.parse::<DocumentMut>() else {
        return CodexAuthStoreMode::Unknown;
    };
    match doc
        .get("cli_auth_credentials_store")
        .and_then(|item| item.as_str())
    {
        None => CodexAuthStoreMode::File,
        Some("file") => CodexAuthStoreMode::File,
        Some("keyring") => CodexAuthStoreMode::Keyring,
        Some("auto") => CodexAuthStoreMode::Auto,
        Some("ephemeral") => CodexAuthStoreMode::Ephemeral,
        Some(_) => CodexAuthStoreMode::Unknown,
    }
}

/// True only when the auth carries material Codex itself authenticates with
/// ahead of the API-key fallback: OAuth tokens or another first-class login
/// carrier. Unlike `codex_auth_has_oauth_login_material`, pure metadata such
/// as `last_refresh` or `tokens.account_id` does NOT count — metadata must not
/// shield a stale third-party `OPENAI_API_KEY` from post-switch cleanup.
pub fn codex_auth_has_credential_login_material(auth: &Value) -> bool {
    let Some(obj) = auth.as_object() else {
        return false;
    };

    let value_present = |value: &Value| match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        _ => true,
    };

    if ["personal_access_token", "agent_identity", "bedrock_api_key"]
        .iter()
        .any(|key| obj.get(*key).is_some_and(value_present))
    {
        return true;
    }

    obj.get("tokens")
        .and_then(Value::as_object)
        .is_some_and(|tokens| {
            ["id_token", "access_token", "refresh_token"]
                .iter()
                .any(|key| tokens.get(*key).is_some_and(value_present))
        })
}

/// True when live `auth.json` is the shape a preserve-off third-party switch
/// leaves behind: an `OPENAI_API_KEY` (possibly alongside metadata like
/// `auth_mode` / `last_refresh`) with no real login credential next to it.
pub fn codex_live_auth_is_stale_third_party_residue(live_auth: &Value) -> bool {
    if codex_auth_has_credential_login_material(live_auth) {
        return false;
    }
    live_auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|key| !key.is_empty())
}

/// After a normal switch to an official provider that carries no login
/// material of its own, delete a live `auth.json` that only holds a stale
/// third-party API key, so Codex shows its login screen instead of sending
/// the wrong key to the official endpoint (401 with no way to re-login).
///
/// Deleting the file — not writing `{}` — is deliberate: Codex resolves an
/// empty object to ChatGPT mode without tokens and errors at bootstrap,
/// while a missing file yields NotAuthenticated and the login screen,
/// matching Codex's own logout.
///
/// Callers must only invoke this after the outgoing provider was
/// successfully backfilled into the DB — that backfill holds the only other
/// copy of the third-party key. The switch backfill intentionally lacks the
/// proxy-side "no credentials in the builtin official row" guard
/// (`services/proxy.rs` `sync_live_config_to_provider`): that asymmetry is
/// what heals official API-key logins into the DB row, and this cleanup's
/// safety depends on it — do not align the two guards.
///
/// Returns Ok(true) when the file was deleted.
pub fn clear_stale_codex_live_auth_after_official_switch(
    db_auth: &Value,
) -> Result<bool, AppError> {
    if codex_auth_has_login_material(db_auth) {
        // A material-carrying official provider gets a full auth write;
        // nothing stale can remain.
        return Ok(false);
    }
    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return Ok(false);
    }
    let live_auth: Value = read_json_file(&auth_path)?;
    if !codex_live_auth_is_stale_third_party_residue(&live_auth) {
        return Ok(false);
    }
    delete_file(&auth_path)?;
    Ok(true)
}

pub fn should_restore_codex_provider_token_for_backfill(
    category: Option<&str>,
    template_settings: &Value,
) -> bool {
    if category == Some("official") {
        return false;
    }

    let Some(auth) = template_settings.get("auth") else {
        return true;
    };

    let has_provider_api_key = extract_codex_auth_api_key(auth).is_some();
    let has_oauth_login = codex_auth_has_oauth_login_material(auth);
    !has_oauth_login || has_provider_api_key
}

fn parse_codex_positive_u64(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(n)) => n.as_u64().filter(|v| *v > 0),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok().filter(|v| *v > 0),
        _ => None,
    }
}

fn extract_codex_top_level_u64(config_text: &str, field: &str) -> Option<u64> {
    let doc = config_text.parse::<toml::Value>().ok()?;
    doc.get(field)
        .and_then(|value| value.as_integer())
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
}

fn codex_catalog_input_modalities(
    model: &str,
    declared_modalities: Option<&[String]>,
) -> Vec<String> {
    let modalities = match image_input_capability_from_modalities(model, declared_modalities) {
        ImageInputCapability::Unsupported => &["text"][..],
        ImageInputCapability::Supported | ImageInputCapability::Unknown => &["text", "image"][..],
    };
    modalities.iter().map(|item| (*item).to_string()).collect()
}

/// Canonical reasoning effort levels Codex understands, with the same
/// descriptions the official gpt-5.5 template uses. `none` disables thinking.
const CODEX_REASONING_LEVEL_DESCRIPTIONS: &[(&str, &str)] = &[
    ("none", "Disable Thinking"),
    ("minimal", "Minimal reasoning"),
    ("low", "Fast responses with lighter reasoning"),
    (
        "medium",
        "Balances speed and reasoning depth for everyday tasks",
    ),
    ("high", "Greater reasoning depth for complex problems"),
    ("xhigh", "Extra high reasoning depth for complex problems"),
    ("max", "Maximum reasoning depth for the hardest problems"),
    ("ultra", "Ultra reasoning depth"),
];

fn codex_reasoning_level_description(effort: &str) -> Option<&'static str> {
    CODEX_REASONING_LEVEL_DESCRIPTIONS
        .iter()
        .find(|(candidate, _)| *candidate == effort)
        .map(|(_, description)| *description)
}

/// User-declared levels reduced to the canonical efforts Codex understands,
/// in canonical (lowest → highest) order regardless of declaration order.
/// Unknown efforts are dropped so a typo can never produce an entry Codex
/// would reject.
fn codex_canonical_efforts(levels: &[String]) -> Vec<&str> {
    CODEX_REASONING_LEVEL_DESCRIPTIONS
        .iter()
        .filter(|(effort, _)| levels.iter().any(|candidate| candidate == effort))
        .map(|(effort, _)| *effort)
        .collect()
}

/// Build a `supported_reasoning_levels` array from user-declared effort values.
fn codex_supported_reasoning_levels(levels: &[String]) -> Value {
    let entries: Vec<Value> = codex_canonical_efforts(levels)
        .into_iter()
        .map(|effort| {
            let description = codex_reasoning_level_description(effort)
                .expect("canonical effort always has a description");
            json!({ "effort": effort, "description": description })
        })
        .collect();
    json!(entries)
}

/// Apply a per-model reasoning-level override onto a catalog entry. Returns
/// true when the override was applied (so callers can skip further work).
/// `template_default` is the base entry's `default_reasoning_level` (from the
/// profile template or an official vendor entry) used as the fallback when the
/// user did not declare one explicitly.
fn apply_codex_reasoning_level_override(
    entry_obj: &mut serde_json::Map<String, Value>,
    template_default: Option<&str>,
    spec: &CodexCatalogModelSpec,
) -> bool {
    let Some(levels) = spec.reasoning_levels.as_deref() else {
        return false;
    };
    let canonical = codex_canonical_efforts(levels);
    if canonical.is_empty() {
        return false;
    }
    let supported = codex_supported_reasoning_levels(levels);
    entry_obj.insert("supported_reasoning_levels".to_string(), supported);

    // Default: explicit user value wins; otherwise keep the base default when
    // it is still supported; otherwise fall back to the highest supported
    // level in canonical order. All candidates are validated against the
    // canonical set so the default can never reference a dropped effort.
    let default_level = spec
        .default_reasoning_level
        .as_deref()
        .filter(|level| canonical.contains(level))
        .or_else(|| template_default.filter(|level| canonical.contains(level)))
        .or_else(|| canonical.last().copied());
    if let Some(default_level) = default_level {
        entry_obj.insert("default_reasoning_level".to_string(), json!(default_level));
    }
    true
}

fn codex_catalog_model_entry(
    template: &Value,
    spec: &CodexCatalogModelSpec,
    priority: usize,
    profile: CodexCatalogToolProfile,
    default_context_window: u64,
) -> Value {
    let mut entry = template.clone();
    let Some(entry_obj) = entry.as_object_mut() else {
        return json!({});
    };

    let display_name = spec.display_name.as_deref().unwrap_or(&spec.model);
    let context_window = spec.context_window.unwrap_or(default_context_window);
    entry_obj.insert("slug".to_string(), json!(spec.model));
    entry_obj.insert("display_name".to_string(), json!(display_name));
    entry_obj.insert("description".to_string(), json!(display_name));
    entry_obj.insert("context_window".to_string(), json!(context_window));
    entry_obj.insert("max_context_window".to_string(), json!(context_window));
    entry_obj.insert("priority".to_string(), json!(1000 + priority));
    entry_obj.insert("additional_speed_tiers".to_string(), json!([]));
    entry_obj.insert("service_tiers".to_string(), json!([]));
    entry_obj.insert("availability_nux".to_string(), Value::Null);
    entry_obj.insert("upgrade".to_string(), Value::Null);

    // Image support is a model capability, not a tool-profile capability.
    // Trust hidden preset metadata first, then the confirmed text-only registry;
    // every unknown model fails open so GPT/relay aliases are never declared
    // text-only merely because a template had a conservative default.
    entry_obj.insert(
        "input_modalities".to_string(),
        json!(codex_catalog_input_modalities(
            &spec.model,
            spec.input_modalities.as_deref(),
        )),
    );

    if profile != CodexCatalogToolProfile::ProxyChat {
        // Native `/responses` and Anthropic gateways reject / drop Codex's freeform
        // `apply_patch` (type=="custom") tool. Strip any key that would make Codex
        // emit a custom/freeform tool, and rely on shell_type="shell_command" for
        // edits. Defensive even though the native template is already clean
        // (guards against template drift / an accidental gpt-5.5 clone).
        //
        // NOTE: `base_instructions` is NOT stripped — Codex's catalog parser
        // treats it as a REQUIRED field and refuses to load the file without
        // it ("missing field `base_instructions`"). The template carries a
        // neutral identity default; per-vendor official text overrides below.
        for key in [
            "apply_patch_tool_type",
            "web_search_tool_type",
            "tools",
            "model_messages",
        ] {
            entry_obj.remove(key);
        }
        entry_obj.insert("shell_type".to_string(), json!("shell_command"));

        if let Some(base_instructions) = spec
            .base_instructions
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            entry_obj.insert("base_instructions".to_string(), json!(base_instructions));
        }
        if let Some(parallel) = spec.supports_parallel_tool_calls {
            entry_obj.insert("supports_parallel_tool_calls".to_string(), json!(parallel));
        }
    }

    if profile == CodexCatalogToolProfile::ProxyChat {
        // Codex's `original` image detail (full-resolution) is rejected by
        // strict Chat gateways with `400 invalid_request_error`, param
        // `messages.N.content`. Never advertise the capability on the
        // ProxyChat contract so Codex keeps to auto/high.
        entry_obj.insert("supports_image_detail_original".to_string(), json!(false));
    }

    // Per-model reasoning levels override the template's conservative
    // none/high default (e.g. a LiteLLM gateway serving a model that accepts
    // low/medium/high/xhigh/max). Applies to every profile.
    let template_default = template
        .get("default_reasoning_level")
        .and_then(|value| value.as_str());
    apply_codex_reasoning_level_override(entry_obj, template_default, spec);

    entry
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexCatalogModelSpec {
    model: String,
    /// Explicit user value only. Entries fall back to the model id — except
    /// official vendor catalog entries, which keep the vendor's display name.
    display_name: Option<String>,
    /// Explicit user value only. Entries fall back to the config's
    /// `model_context_window` (or 128k) — except official vendor catalog
    /// entries, which keep the vendor's declared window.
    context_window: Option<u64>,
    /// Per-row override for the native template's `supports_parallel_tool_calls`
    /// (e.g. MiniMax=true, MiMo=false). Only consulted for `NativeResponses`.
    supports_parallel_tool_calls: Option<bool>,
    /// Hidden per-row capability declaration from built-in provider metadata.
    /// When omitted, all catalog profiles consult the shared text-only model
    /// registry and otherwise default to `["text", "image"]`.
    input_modalities: Option<Vec<String>>,
    /// Per-row override for the native template's `base_instructions` (the
    /// model identity / system preamble). Carries each vendor's OFFICIAL value
    /// (e.g. MiMo "developed by Xiaomi", MiniMax "based on MiniMax-M3"); falls
    /// back to the template default when absent. Only consulted for
    /// `NativeResponses`.
    base_instructions: Option<String>,
    /// Per-row override for the generated catalog's `supported_reasoning_levels`
    /// (e.g. ["none", "low", "medium", "high", "xhigh", "max"]). When omitted
    /// the template's conservative default (none/high) is kept. Consulted for
    /// every profile; the vendor-catalog path applies it on top of the
    /// official entry.
    reasoning_levels: Option<Vec<String>>,
    /// Per-row override for the generated catalog's `default_reasoning_level`.
    /// Only meaningful together with `reasoning_levels`; when absent the
    /// template default is kept if it is still in the list, otherwise the last
    /// (highest) declared level wins.
    default_reasoning_level: Option<String>,
}

fn codex_catalog_model_specs(settings: &Value) -> Vec<CodexCatalogModelSpec> {
    let Some(models) = settings
        .get("modelCatalog")
        .and_then(|catalog| catalog.get("models"))
        .and_then(|models| models.as_array())
    else {
        return Vec::new();
    };

    let mut seen = std::collections::HashSet::new();
    let mut specs = Vec::new();

    for model_config in models {
        let Some(model) = model_config
            .get("model")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|model| !model.is_empty())
        else {
            continue;
        };

        if !seen.insert(model.to_string()) {
            continue;
        }

        let display_name = model_config
            .get("displayName")
            .or_else(|| model_config.get("display_name"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let context_window = parse_codex_positive_u64(
            model_config
                .get("contextWindow")
                .or_else(|| model_config.get("context_window")),
        );

        let supports_parallel_tool_calls = model_config
            .get("supportsParallelToolCalls")
            .or_else(|| model_config.get("supports_parallel_tool_calls"))
            .and_then(|value| value.as_bool());
        let input_modalities = model_config
            .get("inputModalities")
            .or_else(|| model_config.get("input_modalities"))
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|items| !items.is_empty());

        let base_instructions = model_config
            .get("baseInstructions")
            .or_else(|| model_config.get("base_instructions"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);

        let reasoning_levels = model_config
            .get("reasoningLevels")
            .or_else(|| model_config.get("reasoning_levels"))
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::trim)
                    .filter(|level| !level.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|levels| !levels.is_empty());
        let default_reasoning_level = model_config
            .get("defaultReasoningLevel")
            .or_else(|| model_config.get("default_reasoning_level"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|level| !level.is_empty())
            .map(str::to_string);

        specs.push(CodexCatalogModelSpec {
            model: model.to_string(),
            display_name,
            context_window,
            supports_parallel_tool_calls,
            input_modalities,
            base_instructions,
            reasoning_levels,
            default_reasoning_level,
        });
    }

    specs
}

fn find_codex_model_template(catalog: &Value) -> Option<Value> {
    catalog
        .get("models")
        .and_then(|models| models.as_array())
        .and_then(|models| {
            models.iter().find(|model| {
                model.get("slug").and_then(|slug| slug.as_str())
                    == Some(CODEX_MODEL_CATALOG_TEMPLATE_SLUG)
            })
        })
        .cloned()
}

fn load_codex_model_template_from_cache() -> Result<Option<Value>, AppError> {
    let path = get_codex_config_dir().join("models_cache.json");
    if !path.exists() {
        return Ok(None);
    }

    let text = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    let catalog: Value = serde_json::from_str(&text).map_err(|e| AppError::json(&path, e))?;
    Ok(find_codex_model_template(&catalog))
}

/// Fixed candidates for locating the `codex` CLI when it is not on the process
/// PATH (common in GUI apps launched outside a terminal).
const CODEX_CLI_FIXED_CANDIDATES: &[&str] = &[
    "codex",                                // PATH (all platforms)
    "/opt/homebrew/bin/codex",              // macOS Apple Silicon Homebrew
    "/usr/local/bin/codex",                 // macOS Intel Homebrew / Linux
    "/home/linuxbrew/.linuxbrew/bin/codex", // Linux Homebrew
];

fn push_codex_cli_candidate(
    candidates: &mut Vec<PathBuf>,
    seen: &mut HashSet<String>,
    candidate: PathBuf,
) {
    let key = candidate.to_string_lossy().into_owned();
    if seen.insert(key) {
        candidates.push(candidate);
    }
}

fn push_existing_codex_cli_candidate(
    candidates: &mut Vec<PathBuf>,
    seen: &mut HashSet<String>,
    candidate: PathBuf,
) {
    if candidate.exists() {
        push_codex_cli_candidate(candidates, seen, candidate);
    }
}

fn push_codex_cli_candidates_from_version_dirs(
    candidates: &mut Vec<PathBuf>,
    seen: &mut HashSet<String>,
    versions_dir: PathBuf,
    suffix: &[&str],
) {
    let Ok(entries) = fs::read_dir(versions_dir) else {
        return;
    };

    let mut discovered = entries
        .filter_map(Result::ok)
        .map(|entry| {
            let mut candidate = entry.path();
            for component in suffix {
                candidate.push(component);
            }
            candidate
        })
        .filter(|candidate| candidate.exists())
        .collect::<Vec<_>>();

    // Prefer newer-looking version directories before older global installs.
    discovered.sort_by(|a, b| b.cmp(a));
    for candidate in discovered {
        push_codex_cli_candidate(candidates, seen, candidate);
    }
}

fn push_home_codex_cli_candidates(
    candidates: &mut Vec<PathBuf>,
    seen: &mut HashSet<String>,
    home: &Path,
) {
    for relative in [
        ".nvm/current/bin/codex",
        ".volta/bin/codex",
        ".asdf/shims/codex",
        ".local/share/mise/shims/codex",
        ".config/mise/shims/codex",
        ".local/bin/codex",
        ".npm-global/bin/codex",
        ".npm-packages/bin/codex",
        ".local/share/pnpm/codex",
        "Library/pnpm/codex",
    ] {
        push_existing_codex_cli_candidate(candidates, seen, home.join(relative));
    }

    push_codex_cli_candidates_from_version_dirs(
        candidates,
        seen,
        home.join(".nvm/versions/node"),
        &["bin", "codex"],
    );
    push_codex_cli_candidates_from_version_dirs(
        candidates,
        seen,
        home.join(".local/share/fnm/node-versions"),
        &["installation", "bin", "codex"],
    );
    push_codex_cli_candidates_from_version_dirs(
        candidates,
        seen,
        home.join("Library/Application Support/fnm/node-versions"),
        &["installation", "bin", "codex"],
    );
}

fn push_env_codex_cli_candidates(candidates: &mut Vec<PathBuf>, seen: &mut HashSet<String>) {
    for (env_key, suffix) in [
        ("NPM_CONFIG_PREFIX", &["bin", "codex"][..]),
        ("VOLTA_HOME", &["bin", "codex"][..]),
        ("ASDF_DATA_DIR", &["shims", "codex"][..]),
        ("MISE_DATA_DIR", &["shims", "codex"][..]),
        ("PNPM_HOME", &["codex"][..]),
    ] {
        let Some(prefix) = std::env::var_os(env_key) else {
            continue;
        };
        let mut candidate = PathBuf::from(prefix);
        for component in suffix {
            candidate.push(component);
        }
        push_existing_codex_cli_candidate(candidates, seen, candidate);
    }

    if let Some(nvm_dir) = std::env::var_os("NVM_DIR") {
        push_codex_cli_candidates_from_version_dirs(
            candidates,
            seen,
            PathBuf::from(nvm_dir).join("versions/node"),
            &["bin", "codex"],
        );
    }

    if let Some(fnm_dir) = std::env::var_os("FNM_DIR") {
        push_codex_cli_candidates_from_version_dirs(
            candidates,
            seen,
            PathBuf::from(fnm_dir).join("node-versions"),
            &["installation", "bin", "codex"],
        );
    }

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let npm_dir = PathBuf::from(appdata).join("npm");
            for name in ["codex.cmd", "codex.exe", "codex"] {
                push_existing_codex_cli_candidate(candidates, seen, npm_dir.join(name));
            }
        }
    }
}

fn codex_cli_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for candidate in CODEX_CLI_FIXED_CANDIDATES {
        push_codex_cli_candidate(&mut candidates, &mut seen, PathBuf::from(candidate));
    }

    push_env_codex_cli_candidates(&mut candidates, &mut seen);
    push_home_codex_cli_candidates(&mut candidates, &mut seen, &get_home_dir());

    candidates
}

fn codex_bundled_models_command(candidate: &Path) -> Command {
    let mut command = Command::new(candidate);
    command
        .args(["debug", "models", "--bundled"])
        .stdin(Stdio::null());

    // A release build uses the Windows GUI subsystem, so a console child that
    // is created without this flag gets its own transient console window. npm
    // installs Codex as `codex.cmd`, which Windows launches through cmd.exe.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
}

fn load_codex_model_template_from_bundled() -> Result<Option<Value>, AppError> {
    for candidate in codex_cli_candidates() {
        let candidate_label = candidate.to_string_lossy();
        let output = match codex_bundled_models_command(&candidate).output() {
            Ok(output) => output,
            Err(err) => {
                log::debug!("failed to run `{candidate_label} debug models --bundled`: {err}");
                continue;
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!("`{candidate_label} debug models --bundled` failed: {stderr}");
            continue;
        }

        let catalog: Value = match serde_json::from_slice(&output.stdout) {
            Ok(catalog) => catalog,
            Err(e) => {
                log::debug!(
                    "Failed to parse `{candidate_label} debug models --bundled` output: {e}"
                );
                continue;
            }
        };
        if let Some(template) = find_codex_model_template(&catalog) {
            return Ok(Some(template));
        }
    }

    Ok(None)
}

fn load_codex_model_template_static() -> Option<Value> {
    let text = include_str!("resources/gpt5_5_template.json");
    match serde_json::from_str(text) {
        Ok(template) => Some(template),
        Err(e) => {
            log::warn!("Failed to parse bundled gpt-5.5 template: {e}");
            None
        }
    }
}

/// Bundled clean template for native `/responses` providers. Unlike the
/// gpt-5.5 template it carries NO freeform `apply_patch` / `web_search` tool
/// declarations and no GPT-5 base_instructions, so Codex never emits a
/// `type=="custom"` tool that native gateways (MiMo/MiniMax/…) reject. Edits
/// flow through `shell_type="shell_command"` instead. We deliberately do NOT
/// fall back to `models_cache.json` here (that would reintroduce gpt-5.5's
/// freeform apply_patch).
fn load_codex_native_responses_template() -> Value {
    let text = include_str!("resources/codex_native_responses_template.json");
    serde_json::from_str(text).expect("bundled codex native responses template must be valid JSON")
}

/// Hosts whose native `/responses` gateway publishes an OFFICIAL Codex model
/// catalog (models.json) that cc-switch mirrors verbatim. Matched against
/// `base_url` ONLY — deliberately NOT by model brand, unlike
/// `CODEX_WEB_SEARCH_REJECT_MODEL_PREFIXES`: the official entries GRANT
/// capabilities (freeform `apply_patch`, vendor harness), and an aggregator
/// merely hosting the same model may not honor them. The safe failure
/// direction for aggregators is the neutral template (degraded but working);
/// wrongly granting freeform apply_patch would reintroduce the custom-tool
/// rejection bug.
const CODEX_DEEPSEEK_OFFICIAL_CATALOG_HOSTS: &[&str] = &["deepseek.com"];

/// Bundled copy of DeepSeek's official Codex models.json — the exact file
/// their one-click integration script writes (api-docs.deepseek.com →
/// quick_start/agent_integrations/codex): freeform apply_patch, GPT-5 harness
/// base_instructions, low/high/max reasoning levels, web_search supported,
/// 1m context. Declares `minimal_client_version` 0.144.0.
fn load_codex_deepseek_official_catalog_models() -> Vec<Value> {
    let text = include_str!("resources/codex_deepseek_catalog_template.json");
    let catalog: Value =
        serde_json::from_str(text).expect("bundled DeepSeek official catalog must be valid JSON");
    catalog
        .get("models")
        .and_then(|models| models.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Official vendor catalog entries for the provider in `config_text`, if its
/// gateway ships one. Only the `NativeResponses` profile qualifies: ProxyChat
/// runs through cc-switch's converter (gpt-5.5 template contract) and the
/// Anthropic transform drops custom tools, so both must keep their existing
/// templates. Host-driven like the web_search blacklist, so existing providers
/// pick it up on their next switch without a re-save.
fn codex_official_vendor_catalog_models(
    config_text: &str,
    profile: CodexCatalogToolProfile,
) -> Option<Vec<Value>> {
    if profile != CodexCatalogToolProfile::NativeResponses {
        return None;
    }
    let base_url = extract_codex_base_url(config_text)?.to_ascii_lowercase();
    if CODEX_DEEPSEEK_OFFICIAL_CATALOG_HOSTS
        .iter()
        .any(|host| base_url.contains(host))
    {
        let models = load_codex_deepseek_official_catalog_models();
        if !models.is_empty() {
            return Some(models);
        }
    }
    None
}

/// Build one catalog entry from an official vendor catalog: match the user's
/// model id against the vendor entries by slug; an unknown id clones the
/// vendor's first (flagship) entry so it keeps the gateway's capability
/// profile without impersonating the flagship. The official entry is
/// authoritative — no tool-profile stripping — but explicit per-row user
/// overrides still win.
fn codex_vendor_catalog_model_entry(
    vendor_models: &[Value],
    spec: &CodexCatalogModelSpec,
    priority: usize,
) -> Value {
    let matched = vendor_models.iter().find(|entry| {
        entry
            .get("slug")
            .and_then(|slug| slug.as_str())
            .is_some_and(|slug| slug.eq_ignore_ascii_case(&spec.model))
    });
    let mut entry = match matched {
        Some(found) => found.clone(),
        None => vendor_models.first().cloned().unwrap_or_else(|| json!({})),
    };
    // Capture before the mutable borrow: the vendor entry's own default is the
    // fallback when the user declares reasoning levels without a default.
    let vendor_default = entry
        .get("default_reasoning_level")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let Some(entry_obj) = entry.as_object_mut() else {
        return json!({});
    };

    if matched.is_none() {
        let display_name = spec.display_name.as_deref().unwrap_or(&spec.model);
        entry_obj.insert("slug".to_string(), json!(spec.model));
        entry_obj.insert("display_name".to_string(), json!(display_name));
        entry_obj.insert("description".to_string(), json!(display_name));
        entry_obj.insert("priority".to_string(), json!(1000 + priority));
        // Unknown model: don't inherit the flagship entry's modalities —
        // resolve from the registry/fail-open logic instead, so a vision
        // variant (e.g. deepseek-v4-flash-vision-exp) is not declared
        // text-only merely because the flagship is.
        entry_obj.insert(
            "input_modalities".to_string(),
            json!(codex_catalog_input_modalities(
                &spec.model,
                spec.input_modalities.as_deref(),
            )),
        );
    }

    // Explicit user overrides win over the official entry; absent values keep
    // the vendor's declarations (context window, modalities, harness, ...).
    if let Some(display_name) = spec.display_name.as_deref() {
        entry_obj.insert("display_name".to_string(), json!(display_name));
    }
    if let Some(context_window) = spec.context_window {
        entry_obj.insert("context_window".to_string(), json!(context_window));
        entry_obj.insert("max_context_window".to_string(), json!(context_window));
    }
    if let Some(parallel) = spec.supports_parallel_tool_calls {
        entry_obj.insert("supports_parallel_tool_calls".to_string(), json!(parallel));
    }
    if let Some(modalities) = spec.input_modalities.as_deref() {
        entry_obj.insert("input_modalities".to_string(), json!(modalities));
    }
    if let Some(base_instructions) = spec
        .base_instructions
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        entry_obj.insert("base_instructions".to_string(), json!(base_instructions));
    }

    // Per-model reasoning levels win over the official vendor entry too.
    // The vendor file is the base (its own levels stay when no override is
    // declared); its default_reasoning_level is the fallback.
    apply_codex_reasoning_level_override(entry_obj, vendor_default.as_deref(), spec);

    // Defensive: if a future codex parser requires a field the vendor file
    // predates, backfill only whitelisted parser-required keys.
    fill_template_fields_from_static(&mut entry);
    entry
}

/// Fields Codex's external-catalog parser REQUIRES (no serde default): when
/// one is missing Codex rejects the whole catalog file at startup ("missing
/// field ..."). `base_instructions` is the other known required field; the
/// templates always carry it and `codex_catalog_model_entry` handles it.
/// When Codex requires a new field, add it here AND to the static templates.
const CODEX_CATALOG_PARSER_REQUIRED_FIELDS: &[&str] = &[
    "supports_reasoning_summaries",
    // codex 0.148.0 rejects the catalog without it (#6661); a models_cache.json
    // written by an older build can lack it.
    "supports_parallel_tool_calls",
];

/// `models_cache.json` is shared by every Codex install on the machine (npm
/// CLI, desktop-bundled binary, ...), and each version serializes its own
/// `ModelInfo` shape — the cache's field set follows whichever process wrote
/// it last, so it cannot be assumed to satisfy the current external-catalog
/// schema (observed live: 0.144.5 requires `supports_reasoning_summaries`
/// while a coexisting build kept rewriting the cache without it). Backfill
/// ONLY parser-required fields from the bundled static template: optional
/// capability fields keep their missing-means-default semantics, and existing
/// values always win.
fn fill_template_fields_from_static(template: &mut Value) {
    let Some(static_template) = load_codex_model_template_static() else {
        return;
    };
    let (Some(template_obj), Some(static_obj)) =
        (template.as_object_mut(), static_template.as_object())
    else {
        return;
    };
    for key in CODEX_CATALOG_PARSER_REQUIRED_FIELDS {
        if !template_obj.contains_key(*key) {
            if let Some(value) = static_obj.get(*key) {
                template_obj.insert((*key).to_string(), value.clone());
            }
        }
    }
}

fn load_codex_model_catalog_template_uncached() -> Result<Value, AppError> {
    // ① models_cache.json (created by Codex when it connects to OpenAI)
    if let Some(mut template) = load_codex_model_template_from_cache()? {
        fill_template_fields_from_static(&mut template);
        return Ok(template);
    }
    // ② codex CLI (PATH + platform-specific common paths)
    if let Some(mut template) = load_codex_model_template_from_bundled()? {
        fill_template_fields_from_static(&mut template);
        return Ok(template);
    }
    // ③ Static fallback bundled at compile time
    if let Some(template) = load_codex_model_template_static() {
        return Ok(template);
    }

    Err(AppError::Message(format!(
        "Codex model catalog template `{CODEX_MODEL_CATALOG_TEMPLATE_SLUG}` not found. Please start Codex once so models_cache.json is available, or ensure the `codex` CLI is on PATH."
    )))
}

fn get_or_load_codex_model_catalog_template<F>(
    cache: &OnceCell<Value>,
    loader: F,
) -> Result<Value, AppError>
where
    F: FnOnce() -> Result<Value, AppError>,
{
    cache.get_or_try_init(loader).cloned()
}

#[cfg(not(test))]
fn load_codex_model_catalog_template() -> Result<Value, AppError> {
    get_or_load_codex_model_catalog_template(
        &CODEX_MODEL_CATALOG_TEMPLATE_CACHE,
        load_codex_model_catalog_template_uncached,
    )
}

#[cfg(test)]
fn load_codex_model_catalog_template() -> Result<Value, AppError> {
    load_codex_model_catalog_template_uncached()
}

fn codex_model_catalog_from_specs(
    specs: &[CodexCatalogModelSpec],
    template: &Value,
    profile: CodexCatalogToolProfile,
    default_context_window: u64,
) -> Value {
    let entries: Vec<Value> = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            codex_catalog_model_entry(template, spec, index, profile, default_context_window)
        })
        .collect();

    json!({ "models": entries })
}

fn codex_model_catalog_from_settings(
    settings: &Value,
    config_text: &str,
    profile: CodexCatalogToolProfile,
) -> Result<Option<Value>, AppError> {
    let specs = codex_catalog_model_specs(settings);
    if specs.is_empty() {
        return Ok(None);
    }

    // Vendors that publish an OFFICIAL Codex models.json for their native
    // `/responses` gateway get it mirrored verbatim instead of the neutral
    // template: its freeform apply_patch, vendor harness base_instructions and
    // reasoning levels are load-bearing (the harness tells the model to use
    // apply_patch, so catalog and harness must stay consistent).
    if let Some(vendor_models) = codex_official_vendor_catalog_models(config_text, profile) {
        let entries: Vec<Value> = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| codex_vendor_catalog_model_entry(&vendor_models, spec, index))
            .collect();
        return Ok(Some(json!({ "models": entries })));
    }

    let default_context_window =
        extract_codex_top_level_u64(config_text, "model_context_window").unwrap_or(128_000);

    // Native providers use the bundled clean template (no freeform apply_patch,
    // no cache dependency); proxy-chat providers keep cloning Codex's gpt-5.5
    // entry so the proxy can rewrite custom<->function tools as before.
    let template = match profile {
        CodexCatalogToolProfile::NativeResponses | CodexCatalogToolProfile::Anthropic => {
            load_codex_native_responses_template()
        }
        CodexCatalogToolProfile::ProxyChat => load_codex_model_catalog_template()?,
    };
    Ok(Some(codex_model_catalog_from_specs(
        &specs,
        &template,
        profile,
        default_context_window,
    )))
}

fn set_codex_model_catalog_json_field(
    config_text: &str,
    catalog_path: Option<&Path>,
) -> Result<String, AppError> {
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    match catalog_path {
        Some(_) => {
            // Only claim the pointer when it is absent or already cc-switch-owned.
            // A user-managed external catalog file (custom filename or path) is
            // left untouched, mirroring the None arm's ownership rule that
            // `resolve_cc_switch_catalog_path` relies on.
            let is_cc_switch_owned = doc
                .get("model_catalog_json")
                .and_then(|item| item.as_str())
                .map(|path| {
                    Path::new(path).file_name().and_then(|name| name.to_str())
                        == Some(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)
                })
                .unwrap_or(true);
            if is_cc_switch_owned {
                doc["model_catalog_json"] =
                    toml_edit::value(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME);
            }
        }
        None => {
            let should_remove = doc
                .get("model_catalog_json")
                .and_then(|item| item.as_str())
                .map(|path| {
                    Path::new(path).file_name().and_then(|name| name.to_str())
                        == Some(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)
                })
                .unwrap_or(false);
            if should_remove {
                doc.as_table_mut().remove("model_catalog_json");
            }
        }
    }

    Ok(doc.to_string())
}

/// Pure toggle for the top-level `web_search` field that turns Codex's built-in
/// web-search tool off. When `disable` is true we write `web_search = "disabled"`
/// (the catalog's `supports_search_tool` does NOT gate this — the request-time
/// tool comes from the config, defaulting on). When false we *remove* the field,
/// but only when it carries cc-switch's own `"disabled"` sentinel, so switching
/// back to a web-search-capable provider re-enables it without clobbering a
/// user's manual setting.
///
/// The caller decides `disable` (see `codex_native_gateway_rejects_web_search`);
/// lifecycle is bound to the cc-switch catalog pointer so the field is set/cleaned
/// up wherever the native catalog is written/removed.
fn set_codex_native_web_search_field(config_text: &str, disable: bool) -> Result<String, AppError> {
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    if disable {
        doc[CODEX_WEB_SEARCH_FIELD] = toml_edit::value(CODEX_WEB_SEARCH_DISABLED);
    } else {
        let owned = doc
            .get(CODEX_WEB_SEARCH_FIELD)
            .and_then(|item| item.as_str())
            == Some(CODEX_WEB_SEARCH_DISABLED);
        if owned {
            doc.as_table_mut().remove(CODEX_WEB_SEARCH_FIELD);
        }
    }

    Ok(doc.to_string())
}

/// Generate Codex `model_catalog_json` from provider settings and inject/remove
/// the top-level TOML field that points Codex to the generated file.
pub fn prepare_codex_config_text_with_model_catalog(
    settings: &Value,
    config_text: &str,
    profile: CodexCatalogToolProfile,
) -> Result<String, AppError> {
    let catalog_path = get_codex_model_catalog_path();

    if let Some(catalog) = codex_model_catalog_from_settings(settings, config_text, profile)? {
        let config_text = set_codex_model_catalog_json_field(config_text, Some(&catalog_path))?;
        // Disable web_search only for native gateways on the reject blacklist
        // (MiMo/LongCat/MiniMax by host or model brand; Qwen3-Coder by model).
        // Everything else — relays, DouBao, web-search-capable Qwen models,
        // unknown providers — keeps Codex's default.
        let disable_web_search = match profile {
            // The Responses→Anthropic transform silently drops the Codex web_search
            // hosted tool, so always disable it here rather than present a dead tool.
            CodexCatalogToolProfile::Anthropic => true,
            CodexCatalogToolProfile::NativeResponses => {
                codex_native_gateway_rejects_web_search(&config_text)
            }
            CodexCatalogToolProfile::ProxyChat => false,
        };
        let config_text = set_codex_native_web_search_field(&config_text, disable_web_search)?;
        write_json_file(&catalog_path, &catalog)?;
        Ok(config_text)
    } else {
        let config_text = set_codex_model_catalog_json_field(config_text, None)?;
        // Even without a generated catalog, the Responses→Anthropic transform drops the
        // Codex web_search hosted tool, so keep the invariant that an Anthropic provider
        // never presents it as a dead tool.
        let disable_web_search = profile == CodexCatalogToolProfile::Anthropic;
        set_codex_native_web_search_field(&config_text, disable_web_search)
    }
}

/// Reverse of `prepare_codex_config_text_with_model_catalog`: read the
/// cc-switch–maintained catalog file referenced by `~/.codex/config.toml` and
/// convert it back into the simplified shape the frontend table uses:
/// `{ "models": [{ "model", "displayName"?, "contextWindow"?, hidden overrides... }, ...] }`.
///
/// We only reverse-parse catalogs whose `model_catalog_json` path is the
/// cc-switch–generated file (identified by filename
/// `cc-switch-model-catalog.json`). A user-managed external catalog file is
/// left alone — surfacing its richer structure as the simplified table would
/// be a downgrade we can't safely round-trip.
///
/// `displayName`, `contextWindow`, and `inputModalities` are omitted from the
/// returned entry when the on-disk value matches the fallback that
/// `codex_model_catalog_from_settings` injects for unset inputs (slug for
/// display_name, `model_context_window` or 128_000 for context_window, and the
/// shared confirmed-text-only inference for input modalities). This preserves
/// the "user left it blank" intent across round-trip; an unavoidable edge case
/// is that a user-typed value that happens to equal the fallback also collapses
/// to blank, but the next save writes the same fallback so behavior is stable.
///
/// All failure modes (missing file, parse error, no `model_catalog_json`,
/// entries without `slug`) collapse to `Ok(None)` so callers can treat this
/// as best-effort enrichment without making `read_live_settings` brittle.
/// 模型目录文件读取上限（32 MiB）。目录 JSON 正常只有几百 KiB；超过则视为异常，
/// 避免指向外部大文件时耗尽内存。
const MAX_CODEX_CATALOG_BYTES: u64 = 32 * 1024 * 1024;

pub fn read_codex_model_catalog_simplified_from_live() -> Result<Option<Value>, AppError> {
    let config_text = read_codex_config_text()?;
    let config_dir = get_codex_config_dir();
    let Some(catalog_path) = resolve_cc_switch_catalog_path(&config_text, &config_dir) else {
        return Ok(None);
    };
    if !catalog_path.exists() {
        return Ok(None);
    }
    let catalog_text = match read_limited_string(&catalog_path, MAX_CODEX_CATALOG_BYTES) {
        Ok(text) => text,
        Err(error) => {
            log::warn!(
                "拒绝读取越界或过大的 Codex 模型目录 {}: {error}",
                catalog_path.display()
            );
            return Ok(None);
        }
    };
    Ok(build_simplified_catalog_from_texts(
        &config_text,
        &catalog_text,
    ))
}

/// 安全地读取文件为字符串，并在超过字节上限时返回错误。
pub(crate) fn read_limited_string(path: &Path, max_bytes: u64) -> Result<String, AppError> {
    let metadata = fs::metadata(path).map_err(|error| AppError::io(path, error))?;
    if metadata.len() > max_bytes {
        return Err(AppError::Config(format!(
            "文件 {} 超过大小上限 {} 字节",
            path.display(),
            max_bytes
        )));
    }
    fs::read_to_string(path).map_err(|error| AppError::io(path, error))
}

/// Read the cc-switch Codex model catalog file with a size cap.
pub(crate) fn read_codex_model_catalog_text(path: &Path) -> Result<String, AppError> {
    read_limited_string(path, MAX_CODEX_CATALOG_BYTES)
}

/// Given `config.toml` text, resolve the on-disk path of the cc-switch–owned
/// catalog file (returns `None` if `model_catalog_json` is absent or points at
/// a file we don't own). Relative paths are resolved under `base_dir`;
/// absolute paths must still be inside `base_dir`.
pub(crate) fn resolve_cc_switch_catalog_path(
    config_text: &str,
    base_dir: &Path,
) -> Option<PathBuf> {
    if config_text.trim().is_empty() {
        return None;
    }
    let doc = config_text.parse::<DocumentMut>().ok()?;
    let catalog_path_str = doc
        .get("model_catalog_json")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;

    let referenced_path = Path::new(catalog_path_str);
    let is_cc_switch_owned = referenced_path.file_name().and_then(|name| name.to_str())
        == Some(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME);
    if !is_cc_switch_owned {
        return None;
    }

    // 注意（有意的行为变更）：Windows 上 `/…` 形式的旧 WSL 风格 Linux 路径也会
    // 被视为绝对路径，从而在下方的包含性校验中失败——此前这类路径会因无法匹配
    // 生成文件名而回退为按文件名解析、碰巧能工作。可接受：下一次切换供应商时
    // 写入侧会重新落一个裸文件名，配置自愈（见
    // `set_catalog_json_none_removes_cc_switch_owned_by_filename` 的场景注释）。
    let is_unix_absolute = catalog_path_str.starts_with('/');
    let resolved = if referenced_path.is_absolute() || is_unix_absolute {
        referenced_path.to_path_buf()
    } else {
        base_dir.join(referenced_path)
    };

    if !path_is_within(base_dir, &resolved) {
        log::warn!(
            "Codex model_catalog_json 指向配置目录外: {}（允许目录: {}）",
            resolved.display(),
            base_dir.display()
        );
        return None;
    }

    // 词法包含不等于运行时包含：配置目录内的符号链接（如 ~/.codex/link ->
    // /etc）能让 `link/cc-switch-model-catalog.json` 通过上面的检查，读取却
    // 落到目录外。文件存在时把真实路径 canonicalize 出来再校验一次，并把
    // canonical 路径返回给调用方——后续读取不再经过 symlink 组件。
    if resolved.exists() {
        let canonical = match fs::canonicalize(&resolved) {
            Ok(path) => path,
            Err(error) => {
                log::warn!(
                    "Codex model_catalog_json canonicalize 失败: {}: {error}",
                    resolved.display()
                );
                return None;
            }
        };
        // base 同样 canonicalize，保证两侧前缀一致（Windows \\?\、
        // macOS /tmp -> /private/tmp）；base 失败时退回词法 base——
        // 词法 base 与 canonical 路径比较只会误拒（退化为不读），不会误放。
        let canonical_base = fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf());
        if !path_is_within(&canonical_base, &canonical) {
            log::warn!(
                "Codex model_catalog_json 经符号链接解析到配置目录外: {} -> {}（允许目录: {}）",
                resolved.display(),
                canonical.display(),
                canonical_base.display()
            );
            return None;
        }
        return Some(canonical);
    }

    Some(resolved)
}

/// Pure reverse-parsing core: convert Codex catalog JSON text back into the
/// frontend's simplified model-mapping shape. Returns `None` when the catalog
/// is unparseable, has no `models` array, or yields zero valid entries.
fn build_simplified_catalog_from_texts(config_text: &str, catalog_text: &str) -> Option<Value> {
    let catalog: Value = serde_json::from_str(catalog_text).ok()?;
    let models = catalog.get("models").and_then(|m| m.as_array())?;

    let default_context_window =
        extract_codex_top_level_u64(config_text, "model_context_window").unwrap_or(128_000);

    let mut entries = Vec::with_capacity(models.len());
    for entry in models {
        let Some(model) = entry
            .get("slug")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        let mut obj = serde_json::Map::new();
        obj.insert("model".to_string(), json!(model));

        if let Some(display_name) = entry
            .get("display_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != model)
        {
            obj.insert("displayName".to_string(), json!(display_name));
        }

        if let Some(context_window) = entry
            .get("context_window")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0 && *v != default_context_window)
        {
            obj.insert("contextWindow".to_string(), json!(context_window));
        }

        // Preserve native-profile per-row overrides so a DB-SSOT-missing
        // fallback round-trip doesn't silently drop them.
        if let Some(parallel) = entry
            .get("supports_parallel_tool_calls")
            .and_then(|v| v.as_bool())
        {
            obj.insert("supportsParallelToolCalls".to_string(), json!(parallel));
        }
        if let Some(modalities) = entry.get("input_modalities").and_then(|v| v.as_array()) {
            let mods: Vec<String> = modalities
                .iter()
                .filter_map(|m| m.as_str())
                .map(str::to_string)
                .collect();
            let inferred = codex_catalog_input_modalities(model, None);
            if !mods.is_empty() && mods != inferred {
                obj.insert("inputModalities".to_string(), json!(mods));
            }
        }

        entries.push(Value::Object(obj));
    }

    if entries.is_empty() {
        return None;
    }

    Some(json!({ "models": entries }))
}

/// Decide the `config.toml` text to write during a takeover-off restore,
/// projecting the model catalog **only when `settings` carries an inline
/// `modelCatalog`**.
///
/// Restore feeds back a stored backup, and Codex backups come in two shapes that
/// need opposite handling:
///
/// - **Snapshot backup** (`read_codex_live_settings`): `{ auth, config }` with no
///   inline `modelCatalog`. Its `config.toml` text already carries whatever
///   `model_catalog_json` pointer existed at backup time, and the generated
///   catalog file on disk is untouched. Here we must keep the config **raw** —
///   running catalog projection would see "no specs" and strip the live pointer.
/// - **Provider-rebuilt backup** (`update_live_backup_from_provider`): the DB
///   provider's settings, i.e. `{ auth, config (no pointer), modelCatalog
///   (inline DB SSOT) }`. Here the pointer/catalog file must be (re)generated
///   from the inline `modelCatalog`, or the mapping is lost on restore.
///
/// Gating on the presence of the inline `modelCatalog` key routes each shape
/// correctly; an empty inline catalog still projects (and so correctly drops a
/// now-stale pointer), while an absent key leaves the text untouched. This is
/// **orthogonal to auth** — a provider-rebuilt backup can pair an inline
/// `modelCatalog` with empty `auth.json` (the API key living in the config's
/// `experimental_bearer_token`), so the caller must decide config projection
/// independently of whether it writes or deletes `auth.json`.
pub fn prepare_codex_live_config_text_with_optional_catalog(
    settings: &Value,
    config_text: &str,
    profile: CodexCatalogToolProfile,
) -> Result<String, AppError> {
    if settings.get("modelCatalog").is_some() {
        prepare_codex_config_text_with_model_catalog(settings, config_text, profile)
    } else {
        Ok(config_text.to_string())
    }
}

pub fn write_codex_provider_live_with_catalog(
    settings: &Value,
    category: Option<&str>,
    auth: &Value,
    config_text: Option<&str>,
    profile: CodexCatalogToolProfile,
) -> Result<(), AppError> {
    let prepared_config = config_text
        .map(|text| prepare_codex_config_text_with_model_catalog(settings, text, profile))
        .transpose()?;

    write_codex_live_for_provider(category, auth, prepared_config.as_deref())
}

/// Extract a provider-scoped `experimental_bearer_token` from Codex `config.toml`.
///
/// Mobile compat: third-party providers may store the API key inside
/// `[model_providers.<id>].experimental_bearer_token` while keeping the
/// user's ChatGPT login cache intact in `auth.json`. Falls back to the
/// top-level `experimental_bearer_token` when no active model provider is set.
pub fn extract_codex_experimental_bearer_token(config_text: &str) -> Option<String> {
    if !config_text.contains("experimental_bearer_token") {
        return None;
    }
    let doc = config_text.parse::<DocumentMut>().ok()?;
    let provider_id = active_codex_model_provider_id(&doc);

    let top_level_token = || {
        doc.get("experimental_bearer_token")
            .and_then(|item| item.as_str())
    };
    let token = match provider_id.as_deref() {
        // `as_table_like` (not `as_table`): user configs may use inline tables
        // (`model_providers = { foo = {...} }`), which `as_table` rejects.
        Some(id) if is_custom_codex_model_provider_id(id) => doc
            .get("model_providers")
            .and_then(|item| item.as_table_like())
            .and_then(|table| table.get(id))
            .and_then(|item| item.as_table_like())
            .and_then(|table| table.get("experimental_bearer_token"))
            .and_then(|item| item.as_str())
            .or_else(top_level_token),
        Some(_) => top_level_token(),
        None => top_level_token(),
    };

    token
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Whether a provider's `http_headers` / `env_http_headers` table carries an
/// `Authorization` entry. Header names are case-insensitive on the wire, so
/// match TOML keys case-insensitively too.
fn table_declares_authorization_header(item: Option<&toml_edit::Item>) -> bool {
    item.and_then(|item| item.as_table_like())
        .is_some_and(|table| {
            table
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("authorization"))
        })
}

/// Whether this provider table resolves its auth from `auth.json` on Codex
/// 0.149. `resolve_provider_auth` short-circuits on `env_key` /
/// `experimental_bearer_token`; with neither,
/// `requires_openai_auth = false` resolves to the unauthenticated provider —
/// it never reads `auth.json`, no matter what the table carries (x-api-key
/// headers, query params, or nothing at all for local servers). Only
/// `requires_openai_auth = true` without a short-circuit falls through to
/// the official login.
///
/// `auth` / `aws` are deliberately NOT short-circuits here: 0.149 validates
/// both as mutually exclusive with `requires_openai_auth` (and `aws` is
/// Bedrock-only anyway), so a `requires_openai_auth = true` table carrying
/// them is a dead config the whole file fails to load with. Treating them
/// as "own credentials" would wave that dead config through the safety
/// gate; flagging it keeps it from being written.
fn codex_provider_table_falls_back_to_official_auth(table: &dyn toml_edit::TableLike) -> bool {
    table
        .get("requires_openai_auth")
        .and_then(|item| item.as_bool())
        .unwrap_or(false)
        && table.get("env_key").is_none()
        && table.get("experimental_bearer_token").is_none()
}

/// Codex 0.149 guard: a provider table that already declares its own
/// credential source must not receive an injected bearer token. `auth` /
/// `aws` sub-tables hard-conflict with `experimental_bearer_token` at
/// deserialization — the whole config.toml fails to parse and Codex refuses
/// to start. `env_key` outranks the token at runtime, so injection buys
/// nothing and only leaks the key into config.toml. An explicit
/// `Authorization` in `http_headers` / `env_http_headers` is how header-auth
/// providers survive on 0.149 — auth is applied after provider headers and
/// would overwrite it.
///
/// `requires_openai_auth` is deliberately NOT part of this guard, and it
/// even disables the header check: without an injected token,
/// `requires_openai_auth = true` routes auth to the preserved `auth.json`
/// OAuth login, which is applied after provider headers and would send the
/// official credentials to the third-party endpoint. The injected token
/// short-circuits that (the preservation-mode bridge contract); a
/// contradictory Authorization header loses either way on 0.149.
fn codex_provider_table_declares_auth(table: &dyn toml_edit::TableLike) -> bool {
    let requires_openai_auth = table
        .get("requires_openai_auth")
        .and_then(|item| item.as_bool())
        .unwrap_or(false);
    table.get("auth").is_some()
        || table.get("aws").is_some()
        || table.get("env_key").is_some()
        || (!requires_openai_auth
            && (table_declares_authorization_header(table.get("http_headers"))
                || table_declares_authorization_header(table.get("env_http_headers"))))
}

/// Whether a config routes requests away from the official provider while
/// offering no custom provider table to carry a bearer token: a custom
/// `model_provider` whose table is missing, or a built-in/unset provider
/// rerouted by a top-level `openai_base_url`. In both shapes the token can
/// only land at the top level, which Codex 0.149 ignores — on a config-only
/// switch the preserved `auth.json` credentials would be sent to the
/// third-party endpoint. Configs without any routing directive are fine:
/// they leave Codex on the official provider, and the top-level token is
/// cc-switch's own record (extract/backfill), never read by Codex.
fn codex_config_routes_third_party_without_token_slot(config_text: &str) -> bool {
    let Ok(doc) = config_text.parse::<DocumentMut>() else {
        // Syntactically invalid TOML is rejected later by the write validators.
        return false;
    };
    match active_codex_model_provider_id(&doc) {
        Some(id) if is_custom_codex_model_provider_id(&id) => doc
            .get("model_providers")
            .and_then(|item| item.as_table_like())
            .and_then(|table| table.get(&id))
            .and_then(|item| item.as_table_like())
            .is_none(),
        _ => doc
            .get("openai_base_url")
            .and_then(|item| item.as_str())
            .map(str::trim)
            .is_some_and(|url| !url.is_empty()),
    }
}

/// Whether a config with NO injectable API key still routes third-party
/// traffic through the `auth.json` fallback. On 0.149 a custom provider
/// with `requires_openai_auth = true` and no `env_key` /
/// `experimental_bearer_token` short-circuit resolves to whatever `auth.json`
/// holds — under login preservation that is the official OAuth login,
/// applied after provider headers, so even an explicit
/// `http_headers.Authorization` is overwritten and the ChatGPT access
/// token + account id go to the third-party endpoint. A top-level
/// `openai_base_url` reroutes the built-in `openai` provider the same way
/// (other built-ins never read the OAuth login). With a token present the
/// injected bearer short-circuits the fallback instead (bridge contract),
/// so this predicate only matters on the no-token path.
fn codex_config_falls_back_to_official_auth_for_third_party(config_text: &str) -> bool {
    let Ok(doc) = config_text.parse::<DocumentMut>() else {
        // Syntactically invalid TOML is rejected later by the write validators.
        return false;
    };
    let openai_base_url_reroutes = || {
        doc.get("openai_base_url")
            .and_then(|item| item.as_str())
            .map(str::trim)
            .is_some_and(|url| !url.is_empty())
    };
    match active_codex_model_provider_id(&doc) {
        Some(id) if is_custom_codex_model_provider_id(&id) => doc
            .get("model_providers")
            .and_then(|item| item.as_table_like())
            .and_then(|table| table.get(&id))
            .and_then(|item| item.as_table_like())
            .is_some_and(codex_provider_table_falls_back_to_official_auth),
        Some(id) if id == "openai" => openai_base_url_reroutes(),
        None => openai_base_url_reroutes(),
        // Other reserved built-ins (ollama, lmstudio, bedrock…) have their
        // own auth paths and never fall back to the OAuth login.
        Some(_) => false,
    }
}

/// cc-switch-owned provider id used by the legacy-shape normalization below.
/// Not a Codex reserved id, so an injected token lands inside the table.
const CODEX_MIGRATED_PROVIDER_ID: &str = "cc-switch";

/// Pick the first free cc-switch-owned provider id (`cc-switch`,
/// `cc-switch-2`, …) so migrations never overwrite a user-authored table.
fn first_free_cc_switch_provider_id(model_providers: Option<&dyn toml_edit::TableLike>) -> String {
    let mut candidate = CODEX_MIGRATED_PROVIDER_ID.to_string();
    let mut suffix = 2usize;
    while model_providers.is_some_and(|table| table.get(&candidate).is_some()) {
        candidate = format!("{CODEX_MIGRATED_PROVIDER_ID}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// The reserved built-in ids whose `[model_providers.<id>]` tables make
/// Codex reject the WHOLE config at load (`validate_reserved_model_provider_ids`,
/// present since 0.148, case-sensitive; the bedrock ids are exempt).
const CODEX_STALE_RESERVED_TABLE_IDS: &[&str] = &["openai", "ollama", "lmstudio"];

/// Migrate stale reserved provider tables (`[model_providers.openai]`,
/// `.ollama`, `.lmstudio`). Codex rejects the WHOLE config at load when one
/// of these reserved built-in ids is overridden, so any surviving table
/// means "switch reports success, Codex refuses to start" — older cc-switch
/// takeover projections created exactly these shapes.
///
/// The reserved-id match is EXACT, mirroring upstream: `OpenAI` and other
/// case variants are legitimate custom ids and must not be touched. Each
/// table is renamed losslessly to the first free cc-switch id (nothing
/// proves which of its keys the user cares about), with
/// `wire_api = "responses"` defaulted in — all three built-ins speak
/// Responses on 0.149.
///
/// Route policy: when the renamed table was the active route, a third-party
/// write follows to the migrated id unless the table would resolve its auth
/// from auth.json (`codex_provider_table_falls_back_to_official_auth`) with
/// no injectable token to short-circuit it. Tables that never fall back —
/// own credentials (env_key / experimental_bearer_token),
/// header or query-param auth, or unauthenticated local servers — keep
/// their legitimate route; only a credential-less
/// `requires_openai_auth = true` table without a token snaps back to the
/// built-in provider, because following it would send the preserved OAuth
/// login to a stale address. The renamed table is also normalized into a
/// shape 0.149 will load: `wire_api` forced to "responses" (the chat wire
/// API was removed; any other value fails deserialization of the whole
/// config) and an empty/missing `name` backfilled (rejected at load
/// otherwise, active or not). The shape never loaded since 0.148, so there
/// is no prior behavior to preserve. Official writes never follow — an
/// official card's route belongs to the built-in provider. Returns None
/// when there is nothing to migrate.
fn migrate_stale_reserved_provider_tables(
    config_text: &str,
    official: bool,
    has_token: bool,
) -> Result<Option<String>, AppError> {
    if !config_text.contains("model_providers") {
        return Ok(None);
    }
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    let stale_ids: Vec<&str> = CODEX_STALE_RESERVED_TABLE_IDS
        .iter()
        .copied()
        .filter(|id| {
            doc.get("model_providers")
                .and_then(|item| item.as_table_like())
                .and_then(|table| table.get(id))
                .and_then(|item| item.as_table_like())
                .is_some()
        })
        .collect();
    if stale_ids.is_empty() {
        return Ok(None);
    }

    for stale_id in stale_ids {
        let migrated_id = first_free_cc_switch_provider_id(
            doc.get("model_providers")
                .and_then(|item| item.as_table_like()),
        );
        // `model_provider` unset defaults to the built-in openai provider.
        let table_is_active_route = match active_codex_model_provider_id(&doc) {
            None => stale_id == "openai",
            Some(active) => active == stale_id,
        };

        let Some(model_providers) = doc
            .get_mut("model_providers")
            .and_then(|item| item.as_table_like_mut())
        else {
            return Ok(None);
        };
        let Some(mut stale_item) = model_providers.remove(stale_id) else {
            continue;
        };
        let mut falls_back_to_official = false;
        if let Some(table) = stale_item.as_table_like_mut() {
            // 0.149 removed the chat wire API entirely: `wire_api = "chat"`
            // (or any other non-"responses" value) fails deserialization for
            // the WHOLE config, so normalize unconditionally. These tables
            // never loaded since 0.148 — there is no prior behavior to keep.
            if table.get("wire_api").and_then(|item| item.as_str()) != Some("responses") {
                table.insert("wire_api", toml_edit::value("responses"));
            }
            // Non-bedrock tables with an empty/missing `name` are rejected at
            // load ("provider name must not be empty"), active or not — the
            // legacy update path created name-less tables.
            if table
                .get("name")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .is_none()
            {
                table.insert("name", toml_edit::value("Custom"));
            }
            falls_back_to_official = codex_provider_table_falls_back_to_official_auth(&*table);
        }
        model_providers.insert(&migrated_id, stale_item);

        // Follow the rename whenever the table cannot leak the official
        // login: an injected token short-circuits the auth.json fallback,
        // and a table that never falls back (own credentials, header/query
        // auth, or unauthenticated local servers) keeps its legitimate
        // third-party route. Only a credential-less
        // `requires_openai_auth = true` table without a token snaps back to
        // the built-in provider — following it would send the preserved
        // OAuth login to the stale base_url.
        if table_is_active_route && !official && (has_token || !falls_back_to_official) {
            doc["model_provider"] = toml_edit::value(migrated_id.as_str());
        }
    }

    Ok(Some(doc.to_string()))
}

/// Codex 0.149 rejects the WHOLE config at deserialization when any
/// non-Bedrock provider table has an empty/missing `name` — active or not
/// ("provider name must not be empty"). Historic cc-switch updates and
/// hand-written configs created tables carrying only `base_url`, so every
/// live write normalizes custom tables into a loadable shape; the name is
/// cosmetic, so the table id is as good a value as any. Bedrock tables are
/// the opposite: 0.149 only lets them override
/// base_url/auth/http_headers/aws.*, and any other non-default field —
/// `name` included — fails the built-in merge for the whole config, so the
/// reserved ids are skipped entirely.
fn backfill_codex_custom_provider_names(config_text: &str) -> Result<Option<String>, AppError> {
    if !config_text.contains("model_providers") {
        return Ok(None);
    }
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;
    let Some(model_providers) = doc
        .get_mut("model_providers")
        .and_then(|item| item.as_table_like_mut())
    else {
        return Ok(None);
    };

    let ids: Vec<String> = model_providers
        .iter()
        .filter(|(id, item)| {
            is_custom_codex_model_provider_id(id) && item.as_table_like().is_some()
        })
        .map(|(id, _)| id.to_string())
        .collect();
    let mut changed = false;
    for id in ids {
        let Some(table) = model_providers
            .get_mut(&id)
            .and_then(toml_edit::Item::as_table_like_mut)
        else {
            continue;
        };
        if table
            .get("name")
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .is_none()
        {
            table.insert("name", toml_edit::value(id.as_str()));
            changed = true;
        }
    }
    Ok(changed.then(|| doc.to_string()))
}

/// Codex 0.149 validates EVERY provider table at deserialization — active
/// or not — and rejects the whole config over field combinations it
/// forbids: `aws` outside the two Bedrock built-ins, and a command-backed
/// `auth` combined with `requires_openai_auth` / `env_key` /
/// `experimental_bearer_token` (ModelProviderInfo::validate). None of these
/// can be normalized away (dropping user-authored fields is not ours to
/// do), so the switch path refuses up front with an actionable error
/// instead of writing a config Codex refuses to start on. Deliberately
/// called only from plan_codex_live_write: the gate-less paths (proxy
/// backup/restore) must not fail closed on the user's own backup.
fn preflight_codex_provider_table_conflicts(config_text: &str) -> Result<(), AppError> {
    if !config_text.contains("model_providers") {
        return Ok(());
    }
    let Ok(doc) = config_text.parse::<DocumentMut>() else {
        // Syntactically invalid TOML is rejected later by the write validators.
        return Ok(());
    };
    let Some(model_providers) = doc
        .get("model_providers")
        .and_then(|item| item.as_table_like())
    else {
        return Ok(());
    };
    for (id, item) in model_providers.iter() {
        let Some(table) = item.as_table_like() else {
            continue;
        };
        let is_bedrock = matches!(id, "amazon-bedrock" | "amazon-bedrock-runtime");
        if !is_bedrock && table.get("aws").is_some() {
            return Err(AppError::localized(
                "provider.codex.config.invalid_provider_table",
                format!(
                    "Codex 0.149 拒绝加载该配置：`aws` 字段仅允许用于内置的 amazon-bedrock / amazon-bedrock-runtime，[model_providers.{id}] 不能携带它。请移除该字段或改用 Bedrock 内置 id"
                ),
                format!(
                    "Codex 0.149 refuses to load this config: `aws` is only supported on the built-in amazon-bedrock / amazon-bedrock-runtime providers, so [model_providers.{id}] must not carry it. Remove the field or use a Bedrock built-in id"
                ),
            ));
        }
        if table.get("auth").is_some() {
            let requires_openai_auth = table
                .get("requires_openai_auth")
                .and_then(|item| item.as_bool())
                .unwrap_or(false);
            let conflict = if requires_openai_auth {
                Some("requires_openai_auth")
            } else if table.get("env_key").is_some() {
                Some("env_key")
            } else if table.get("experimental_bearer_token").is_some() {
                Some("experimental_bearer_token")
            } else {
                None
            };
            if let Some(conflict) = conflict {
                return Err(AppError::localized(
                    "provider.codex.config.invalid_provider_table",
                    format!(
                        "Codex 0.149 拒绝加载该配置：[model_providers.{id}] 的 `auth` 不能与 `{conflict}` 同时存在。请移除其中之一"
                    ),
                    format!(
                        "Codex 0.149 refuses to load this config: `auth` on [model_providers.{id}] cannot be combined with `{conflict}`. Remove one of them"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Rewrite the legacy "reroute the built-in openai provider" shape —
/// `model_provider` unset/"openai" plus a top-level `openai_base_url` — into
/// a custom provider table named `cc-switch`. Before Codex 0.149 this shape
/// worked because the built-in provider read the third-party key from
/// auth.json (ambient auth); auth.json no longer carries third-party keys,
/// so the key needs a provider-scoped slot. The built-in `openai` provider
/// speaks the Responses wire protocol, so the table pins
/// `wire_api = "responses"` and traffic semantics stay unchanged.
fn normalize_codex_legacy_openai_reroute(config_text: &str) -> Result<Option<String>, AppError> {
    if !config_text.contains("openai_base_url") {
        return Ok(None);
    }
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    // Exact match, mirroring upstream: `openai_base_url` reroutes only the
    // built-in provider, and the built-in lookup is case-sensitive — a
    // config routing to `OpenAI` targets a custom table, not the knob.
    let targets_built_in_openai = match active_codex_model_provider_id(&doc) {
        None => true,
        Some(id) => id == "openai",
    };
    if !targets_built_in_openai {
        return Ok(None);
    }
    let Some(base_url) = doc
        .get("openai_base_url")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
    else {
        return Ok(None);
    };
    // `model_providers` present but not any table shape (scalar garbage):
    // leave it to the safety gates instead of guessing. Inline tables ARE
    // handled — proxy backup/restore call prepare without the gates, so
    // skipping them would leave the key in a dead top-level field next to
    // live auth.json credentials.
    if let Some(item) = doc.get("model_providers") {
        if item.as_table_like().is_none() {
            return Ok(None);
        }
    }

    // A user-authored table may already claim our id: nothing proves it is
    // ours to overwrite (their headers/query params would be lost and later
    // backfilled into the DB for good), so pick the first free suffixed id
    // instead. Idempotency is unaffected: a normalized config routes to the
    // migrated id, so this function early-returns before reaching here.
    let migrated_id = first_free_cc_switch_provider_id(
        doc.get("model_providers")
            .and_then(|item| item.as_table_like()),
    );

    doc.as_table_mut().remove("openai_base_url");
    doc["model_provider"] = toml_edit::value(migrated_id.as_str());

    // Match the container's own style: a standard table gets a sub-table, an
    // inline `model_providers = { … }` gets an inline member.
    let container_is_inline = doc
        .get("model_providers")
        .is_some_and(|item| item.as_table().is_none());
    if doc.get("model_providers").is_none() {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc.insert("model_providers", toml_edit::Item::Table(table));
    }
    let Some(model_providers) = doc
        .get_mut("model_providers")
        .and_then(|item| item.as_table_like_mut())
    else {
        return Ok(None);
    };
    if container_is_inline {
        let mut provider_table = toml_edit::InlineTable::new();
        provider_table.insert("name", "Custom".into());
        provider_table.insert("base_url", base_url.into());
        provider_table.insert("wire_api", "responses".into());
        model_providers.insert(
            &migrated_id,
            toml_edit::Item::Value(toml_edit::Value::InlineTable(provider_table)),
        );
    } else {
        let mut provider_table = toml_edit::Table::new();
        provider_table.insert("name", toml_edit::value("Custom"));
        provider_table.insert("base_url", toml_edit::value(base_url));
        provider_table.insert("wire_api", toml_edit::value("responses"));
        model_providers.insert(&migrated_id, toml_edit::Item::Table(provider_table));
    }

    Ok(Some(doc.to_string()))
}

/// Flip a proxy-managed OAuth card's `requires_openai_auth = true` to
/// `false` on the active custom provider table.
///
/// Such cards (xai_oauth, github_copilot, …) are keyless by design — the
/// local proxy injects the real token per request, and the stored config is
/// only a snapshot of the upstream shape — yet their presets inherited the
/// pre-0.149 template's `requires_openai_auth = true`. Left in place, the
/// keyless safety gate rightly refuses the switch
/// (`provider.codex.config.official_auth_fallback`), and on disk the flag
/// would either send a preserved official login to the third-party endpoint
/// or trap Codex on the login screen. Forcing `false` makes the snapshot
/// honest about its keyless state: 0.149 resolves the provider as
/// unauthenticated and never reads auth.json, so the gate passes on its own
/// merits instead of being exempted. Callers gate on
/// `Provider::uses_proxy_injected_oauth` — `codex_oauth` cards must never
/// come through here, the official login IS their credential.
///
/// Returns `Some(updated)` only when the flag was an explicit `true`;
/// absent/false flags, non-custom routing, and unparsable TOML pass through
/// unchanged (`None`) so downstream validators keep ownership of errors.
pub fn neutralize_codex_official_auth_fallback_for_proxy_oauth(
    config_text: &str,
) -> Option<String> {
    let mut doc = config_text.parse::<DocumentMut>().ok()?;
    let provider_id = active_codex_model_provider_id(&doc)?;
    if !is_custom_codex_model_provider_id(&provider_id) {
        return None;
    }
    let provider_table = doc
        .get_mut("model_providers")
        .and_then(|item| item.as_table_like_mut())
        .and_then(|table| table.get_mut(provider_id.as_str()))
        .and_then(|item| item.as_table_like_mut())?;
    if provider_table
        .get("requires_openai_auth")
        .and_then(|item| item.as_bool())
        != Some(true)
    {
        return None;
    }
    provider_table.insert("requires_openai_auth", toml_edit::value(false));
    Some(doc.to_string())
}

/// Align the active custom provider table's `requires_openai_auth` with the
/// login-preservation setting on a third-party switch.
///
/// On Codex 0.149 the flag never decides request auth for these tables —
/// `resolve_provider_auth` short-circuits on `env_key` /
/// `experimental_bearer_token` before consulting it — but it does drive the
/// login UX: `true` with no login in `auth.json` traps the TUI in the
/// login/onboarding screen (preservation off deletes the file on every
/// third-party switch), while `false` next to a preserved ChatGPT login
/// makes Codex treat the session as logged out (account state hidden, the
/// preserved tokens never refreshed). Stored third-party configs cannot be
/// trusted here: presets and the custom template carried
/// `requires_openai_auth = true` from the pre-0.149 era when auth.json held
/// the third-party key, so the stamp overrides whatever the card says.
///
/// Only tables that short-circuit request auth (`env_key` or an
/// injected/stored `experimental_bearer_token`) are touched. Stamping
/// `true` on a table without a short-circuit would route request auth to
/// the preserved official OAuth login — the exact leak the safety gates
/// refuse — and keyless header-auth or local-server tables must keep their
/// user-authored shape (0.149 keeps them unauthenticated either way).
///
/// `preserve_official_login` is the post-write login state of `auth.json`.
/// The direct-switch plan derives it from the preservation setting (which
/// decides whether the file survives the switch); the takeover writer
/// derives it from the live file itself — takeover never touches
/// `auth.json`, but it no longer owns the file's presence (a
/// preservation-off direct switch deletes it before takeover is enabled),
/// so the stored card's flag cannot be trusted there either.
pub(crate) fn align_codex_requires_openai_auth_with_login_preservation(
    config_text: &str,
    preserve_official_login: bool,
) -> Result<String, AppError> {
    if !config_text.contains("model_providers") {
        return Ok(config_text.to_string());
    }
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;
    let Some(provider_id) = active_codex_model_provider_id(&doc) else {
        return Ok(config_text.to_string());
    };
    if !is_custom_codex_model_provider_id(&provider_id) {
        return Ok(config_text.to_string());
    }
    let Some(provider_table) = doc
        .get_mut("model_providers")
        .and_then(|item| item.as_table_like_mut())
        .and_then(|table| table.get_mut(provider_id.as_str()))
        .and_then(|item| item.as_table_like_mut())
    else {
        return Ok(config_text.to_string());
    };
    let short_circuits_request_auth = provider_table.get("experimental_bearer_token").is_some()
        || provider_table.get("env_key").is_some();
    if !short_circuits_request_auth {
        return Ok(config_text.to_string());
    }
    if provider_table
        .get("requires_openai_auth")
        .and_then(|item| item.as_bool())
        == Some(preserve_official_login)
    {
        return Ok(config_text.to_string());
    }
    provider_table.insert(
        "requires_openai_auth",
        toml_edit::value(preserve_official_login),
    );
    Ok(doc.to_string())
}

fn set_codex_experimental_bearer_token(config_text: &str, token: &str) -> Result<String, AppError> {
    if config_text.trim().is_empty() {
        return Err(AppError::localized(
            "provider.codex.config.missing",
            "Codex 第三方供应商缺少 config.toml 配置，无法写入 bearer token",
            "Codex third-party provider is missing config.toml, cannot write bearer token",
        ));
    }

    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    let Some(provider_id) = active_codex_model_provider_id(&doc) else {
        doc["experimental_bearer_token"] = toml_edit::value(token);
        return Ok(doc.to_string());
    };

    if !is_custom_codex_model_provider_id(&provider_id) {
        // Reserved Codex provider IDs are owned by the CLI. Keep third-party
        // bearer tokens at the top level so we do not shadow built-in tables.
        doc["experimental_bearer_token"] = toml_edit::value(token);
        return Ok(doc.to_string());
    }

    // `as_table_like_mut` (not `as_table_mut`): inline tables would return
    // None and silently divert the token to the top level, where Codex 0.149
    // has no such field and ignores it (401 persists). Same pitfall as
    // `update_codex_toml_field`.
    if let Some(provider_table) = doc
        .get_mut("model_providers")
        .and_then(|item| item.as_table_like_mut())
        .and_then(|table| table.get_mut(provider_id.as_str()))
        .and_then(|item| item.as_table_like_mut())
    {
        if codex_provider_table_declares_auth(&*provider_table) {
            return Ok(config_text.to_string());
        }
        provider_table.insert("experimental_bearer_token", toml_edit::value(token));
        return Ok(doc.to_string());
    }

    doc["experimental_bearer_token"] = toml_edit::value(token);
    Ok(doc.to_string())
}

pub fn remove_codex_experimental_bearer_token_if(
    config_text: &str,
    predicate: impl Fn(&str) -> bool,
) -> Result<String, AppError> {
    if config_text.trim().is_empty() || !config_text.contains("experimental_bearer_token") {
        return Ok(config_text.to_string());
    }

    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    if let Some(provider_id) = active_codex_model_provider_id(&doc) {
        if let Some(provider_table) = doc
            .get_mut("model_providers")
            .and_then(|item| item.as_table_like_mut())
            .and_then(|table| table.get_mut(provider_id.as_str()))
            .and_then(|item| item.as_table_like_mut())
        {
            let should_remove = provider_table
                .get("experimental_bearer_token")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .is_some_and(&predicate);
            if should_remove {
                provider_table.remove("experimental_bearer_token");
            }
        }
    }

    let should_remove_top_level = doc
        .get("experimental_bearer_token")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .is_some_and(&predicate);
    if should_remove_top_level {
        doc.as_table_mut().remove("experimental_bearer_token");
    }
    Ok(doc.to_string())
}

fn remove_codex_experimental_bearer_token(config_text: &str) -> Result<String, AppError> {
    remove_codex_experimental_bearer_token_if(config_text, |_| true)
}

/// Read the current Codex live settings as a `{ auth, config }` object.
///
/// Missing `auth.json` collapses to `{}` so a config-only third-party install
/// is still importable; both files missing is treated as "no live install".
/// A `config.toml` that exists but is empty is a valid state — e.g. the
/// official seed after stale-auth cleanup — and must stay readable.
pub fn read_codex_live_settings() -> Result<Value, AppError> {
    let auth_path = get_codex_auth_path();
    let auth_present = auth_path.exists();
    let auth: Value = if auth_present {
        read_json_file(&auth_path)?
    } else {
        json!({})
    };
    let cfg_text = read_and_validate_codex_config_text()?;
    if !auth_present && !get_codex_config_path().exists() {
        return Err(AppError::localized(
            "codex.live.missing",
            "Codex 配置文件不存在",
            "Codex configuration is missing",
        ));
    }
    Ok(json!({ "auth": auth, "config": cfg_text }))
}

/// `[model_providers.custom]` entry that makes an official (ChatGPT OAuth)
/// provider behave like Codex's built-in `openai` entry while running under
/// the shared custom id: `requires_openai_auth` routes auth to the ChatGPT
/// login in `auth.json` (base_url then defaults to the official Codex
/// backend), `name = "OpenAI"` keeps Codex's `is_openai()` feature gates
/// (web search, remote compaction), and `supports_websockets` restores the
/// built-in default that custom entries otherwise lose.
fn codex_official_provider_table(
    base_url: Option<&str>,
    supports_websockets: bool,
) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table["name"] = toml_edit::value("OpenAI");
    table["requires_openai_auth"] = toml_edit::value(true);
    table["supports_websockets"] = toml_edit::value(supports_websockets);
    table["wire_api"] = toml_edit::value("responses");
    if let Some(base_url) = base_url {
        table["base_url"] = toml_edit::value(base_url.trim_end_matches('/'));
    }
    table
}

fn codex_unified_official_provider_table() -> toml_edit::Table {
    codex_official_provider_table(None, true)
}

fn remove_codex_proxy_placeholders_from_providers(providers: &mut toml_edit::Table) {
    for (_, item) in providers.iter_mut() {
        if let Some(table) = item.as_table_mut() {
            let should_remove = table
                .get("experimental_bearer_token")
                .and_then(|item| item.as_str())
                == Some(CODEX_PROXY_AUTH_PLACEHOLDER);
            if should_remove {
                table.remove("experimental_bearer_token");
            }
        } else if let Some(table) = item.as_inline_table_mut() {
            let should_remove = table
                .get("experimental_bearer_token")
                .and_then(|value| value.as_str())
                == Some(CODEX_PROXY_AUTH_PLACEHOLDER);
            if should_remove {
                table.remove("experimental_bearer_token");
            }
        }
    }
}

/// Project a Codex official account card through the local proxy while keeping
/// authentication owned by Codex itself.
///
/// The resulting custom provider explicitly opts into OpenAI authentication,
/// so Codex forwards its existing ChatGPT login to the local `/responses`
/// endpoint.  No API key or bearer placeholder is written to `auth.json`.
pub fn apply_codex_official_proxy_route(
    config_text: &str,
    proxy_base_url: &str,
) -> Result<String, AppError> {
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    // A third-party takeover may have left the proxy placeholder in config.toml.
    // The official route must use Codex's native OpenAI login instead.
    doc.as_table_mut().remove("experimental_bearer_token");
    doc["model_provider"] = toml_edit::value(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID);

    let mut providers = match doc.as_table_mut().remove("model_providers") {
        Some(item) => item.into_table().map_err(|_| {
            AppError::Message(
                "Invalid Codex config.toml: model_providers must be a table".to_string(),
            )
        })?,
        None => {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            table
        }
    };

    // Clean only CC Switch's placeholder from every stale provider table. Real
    // user bearer tokens are preserved, as are all unrelated provider fields.
    remove_codex_proxy_placeholders_from_providers(&mut providers);

    // The local proxy currently exposes HTTP/SSE, not Codex websocket routes.
    let table = codex_official_provider_table(Some(proxy_base_url), false);

    providers.insert(
        CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID,
        toml_edit::Item::Table(table),
    );
    doc["model_providers"] = toml_edit::Item::Table(providers);
    Ok(doc.to_string())
}

/// Whether a live Codex config is the official route projected by CC Switch.
pub fn codex_config_has_official_proxy_route(config_text: &str) -> bool {
    if !config_text.contains(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID) {
        return false;
    }
    config_text
        .parse::<DocumentMut>()
        .ok()
        .and_then(|doc| {
            doc.get("model_provider")
                .and_then(|item| item.as_str())
                .map(str::to_string)
        })
        .as_deref()
        == Some(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID)
}

/// Remove only the official takeover route owned by CC Switch. This is a
/// last-resort crash cleanup when no live backup or provider SSOT is usable.
pub fn remove_codex_official_proxy_route(config_text: &str) -> Result<String, AppError> {
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;
    if doc.get("model_provider").and_then(|item| item.as_str())
        != Some(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID)
    {
        return Ok(config_text.to_string());
    }

    doc.as_table_mut().remove("model_provider");
    if let Some(item) = doc.as_table_mut().remove("model_providers") {
        let mut providers = item.into_table().map_err(|_| {
            AppError::Message(
                "Invalid Codex config.toml: model_providers must be a table".to_string(),
            )
        })?;
        providers.remove(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID);
        remove_codex_proxy_placeholders_from_providers(&mut providers);
        if !providers.is_empty() {
            doc["model_providers"] = toml_edit::Item::Table(providers);
        }
    }
    Ok(doc.to_string())
}

fn table_matches_codex_unified_official_provider(table: &toml_edit::Table) -> bool {
    table.len() == 4
        && table.get("name").and_then(|item| item.as_str()) == Some("OpenAI")
        && table
            .get("requires_openai_auth")
            .and_then(|item| item.as_bool())
            == Some(true)
        && table
            .get("supports_websockets")
            .and_then(|item| item.as_bool())
            == Some(true)
        && table.get("wire_api").and_then(|item| item.as_str()) == Some("responses")
}

/// 统一 Codex 会话历史：把官方供应商的 live 配置改写为以共享的
/// `custom` model_provider 标识运行（认证仍走 `auth.json` 的 ChatGPT 登录），
/// 使开关开启后创建的官方会话与第三方会话共用同一个 resume 历史桶。
///
/// 两种情况拒绝注入、原样返回：
/// - 配置已有显式 `model_provider`：用户手工指定的路由不被覆盖；
/// - 配置已有形态不同的 `[model_providers.custom]` 表：设置 `model_provider`
///   会激活这张我们不认识的表（可能带第三方 base_url/token，会把 ChatGPT
///   OAuth 流量路由到错误后端），宁可让开关对该配置不生效。
pub fn inject_codex_unified_session_bucket(config_text: &str) -> Result<String, AppError> {
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    if doc.get("model_provider").is_some() {
        return Ok(config_text.to_string());
    }

    let existing_custom_conflicts = doc
        .get("model_providers")
        .and_then(|item| item.as_table())
        .and_then(|providers| providers.get(CC_SWITCH_CODEX_MODEL_PROVIDER_ID))
        .and_then(|item| item.as_table())
        .is_some_and(|table| !table_matches_codex_unified_official_provider(table));
    if existing_custom_conflicts {
        log::warn!(
            "官方 Codex 配置已存在自定义 [model_providers.custom]，跳过统一会话路由注入以避免激活未知路由"
        );
        return Ok(config_text.to_string());
    }

    doc["model_provider"] = toml_edit::value(CC_SWITCH_CODEX_MODEL_PROVIDER_ID);

    if doc.get("model_providers").is_none() {
        let mut parent = toml_edit::Table::new();
        parent.set_implicit(true);
        doc["model_providers"] = toml_edit::Item::Table(parent);
    }
    if let Some(providers) = doc["model_providers"].as_table_mut() {
        if !providers.contains_key(CC_SWITCH_CODEX_MODEL_PROVIDER_ID) {
            providers.insert(
                CC_SWITCH_CODEX_MODEL_PROVIDER_ID,
                toml_edit::Item::Table(codex_unified_official_provider_table()),
            );
        }
    }
    Ok(doc.to_string())
}

/// `inject_codex_unified_session_bucket` 的反向操作：从配置文本里剥掉注入的
/// 统一会话路由，保证切换回填不会把它带进数据库的存储配置（关闭开关后
/// 切换即可完全还原）。仅当形态与注入产物完全一致时才剥离；第三方模板和
/// 用户自定义的 `custom` 条目（带 base_url 等差异字段）原样保留。
pub fn strip_codex_unified_session_bucket(config_text: &str) -> Result<String, AppError> {
    if !config_text.contains("model_provider") {
        return Ok(config_text.to_string());
    }
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    if doc.get("model_provider").and_then(|item| item.as_str())
        != Some(CC_SWITCH_CODEX_MODEL_PROVIDER_ID)
    {
        return Ok(config_text.to_string());
    }
    let matches_injected = doc
        .get("model_providers")
        .and_then(|item| item.as_table())
        .and_then(|providers| providers.get(CC_SWITCH_CODEX_MODEL_PROVIDER_ID))
        .and_then(|item| item.as_table())
        .is_some_and(table_matches_codex_unified_official_provider);
    if !matches_injected {
        return Ok(config_text.to_string());
    }

    doc.as_table_mut().remove("model_provider");
    let providers_empty = doc["model_providers"]
        .as_table_mut()
        .map(|providers| {
            providers.remove(CC_SWITCH_CODEX_MODEL_PROVIDER_ID);
            providers.is_empty()
        })
        .unwrap_or(false);
    if providers_empty {
        doc.as_table_mut().remove("model_providers");
    }
    Ok(doc.to_string())
}

/// 统一会话开关开启时，把官方供应商 `{ auth, config }` 设置对象中的
/// config 文本注入共享 custom 路由；开关关闭或非官方供应商时不做改动。
///
/// 普通 live 写入（`write_codex_live_for_provider`）与代理接管备份
/// （`update_live_backup_from_provider`）两条落盘路径共用：接管期间
/// live 归代理所有，注入必须进备份，接管释放恢复的 live 才带统一路由。
pub fn apply_codex_unified_session_bucket_to_settings(
    category: Option<&str>,
    settings: &mut Value,
) -> Result<(), AppError> {
    if category != Some("official") || !crate::settings::unify_codex_session_history() {
        return Ok(());
    }
    let config_text = settings
        .get("config")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let injected = inject_codex_unified_session_bucket(&config_text)?;
    if injected != config_text {
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("config".to_string(), Value::String(injected));
        }
    }
    Ok(())
}

/// Backfill helper: strip the unified-session injection from a live
/// `{ auth, config }` settings object before it is stored back to the DB.
pub fn strip_codex_unified_session_bucket_from_settings(
    settings: &mut Value,
) -> Result<(), AppError> {
    let Some(config_text) = settings
        .get("config")
        .and_then(|value| value.as_str())
        .map(str::to_string)
    else {
        return Ok(());
    };
    let stripped = strip_codex_unified_session_bucket(&config_text)?;
    if stripped != config_text {
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("config".to_string(), Value::String(stripped));
        }
    }
    Ok(())
}

/// Backfill helper: strip `[mcp_servers]` from a live `{ auth, config }`
/// settings object before it is stored back to the DB.
///
/// MCP 服务器的 SSOT 是 DB 的 mcp_servers 表，live `config.toml` 里的
/// `[mcp_servers]` 只是每次写 live 之后由 MCP 同步重新投影的产物。若回填时
/// 烙进供应商存储配置，已在应用里删除的服务器会随下次激活该供应商被写回
/// live，而逐条 reconcile 只认识 DB 现存条目、永远清不掉这种孤儿。
pub fn strip_codex_mcp_servers_from_settings(settings: &mut Value) -> Result<(), AppError> {
    let Some(config_text) = settings
        .get("config")
        .and_then(|value| value.as_str())
        .map(str::to_string)
    else {
        return Ok(());
    };
    if !config_text.contains("mcp") {
        return Ok(());
    }
    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;
    let mut changed = doc.as_table_mut().remove("mcp_servers").is_some();
    // 历史错误格式 [mcp.servers] 一并清理（live 侧 MCP 同步也做同样迁移）
    if let Some(mcp_tbl) = doc.get_mut("mcp").and_then(|item| item.as_table_like_mut()) {
        if mcp_tbl.remove("servers").is_some() {
            changed = true;
        }
        if mcp_tbl.is_empty() {
            doc.as_table_mut().remove("mcp");
        }
    }
    if changed {
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("config".to_string(), Value::String(doc.to_string()));
        }
    }
    Ok(())
}

/// Route a Codex live write between full auth+config or config-only.
///
/// Official providers with usable login material own `auth.json`. Third-party
/// providers only touch `config.toml` when the compatibility setting is enabled
/// so the user's ChatGPT login cache survives provider switches.
///
/// 统一会话开关开启时，官方配置在落盘前注入共享的 `custom` 路由
/// （见 `inject_codex_unified_session_bucket`）。
/// A computed Codex live write. All validation (legacy-shape normalization,
/// safety gates, token injection, TOML parsing) happens while building the
/// plan, so callers can preflight a switch — build and discard — before
/// committing any state, then execute the same computation for the real
/// write. Keeping validation and execution in one builder makes it
/// impossible for the two to drift apart.
struct CodexLiveWritePlan {
    write_full_auth: bool,
    config_text: Option<String>,
    remove_auth_file: bool,
}

fn plan_codex_live_write(
    category: Option<&str>,
    auth: &Value,
    config_text: Option<&str>,
    preserve_official_login: bool,
) -> Result<CodexLiveWritePlan, AppError> {
    // Semantic preflight over EVERY provider table (official and
    // third-party alike, idle tables included): field combinations 0.149
    // rejects at load can't be normalized away, so refuse the switch with
    // an actionable error instead of writing a config Codex won't start on.
    // Independent of the two auth-safety gates below — those only judge the
    // active route and are skipped when a key is carried.
    if let Some(text) = config_text {
        preflight_codex_provider_table_conflicts(text)?;
    }
    if category == Some("official") {
        // Official configs seeded by older cc-switch versions can carry
        // stale reserved tables too — Codex refuses those at load, so
        // migrate on every write path, not only third-party. Official
        // context: the route never follows the renamed table.
        let migrated = match config_text {
            Some(text) => migrate_stale_reserved_provider_tables(text, true, false)?,
            None => None,
        };
        let config_text = migrated.as_deref().or(config_text);
        // Official writes never go through prepare_codex_provider_live_config,
        // so normalize name-less custom tables here too — 0.149 validates
        // EVERY provider table at load, and an official config can carry
        // idle leftovers from older cc-switch versions.
        let named = match config_text {
            Some(text) => backfill_codex_custom_provider_names(text)?,
            None => None,
        };
        let config_text = named.as_deref().or(config_text);
        let unified_official_config = if crate::settings::unify_codex_session_history() {
            Some(inject_codex_unified_session_bucket(
                config_text.unwrap_or(""),
            )?)
        } else {
            None
        };
        let config_text = unified_official_config.as_deref().or(config_text);
        // Official cards own auth.json: a material-carrying login is written
        // in full, a material-less card follows the live login and only
        // writes config. Official auth never travels through config.toml.
        return Ok(CodexLiveWritePlan {
            write_full_auth: codex_auth_has_login_material(auth),
            config_text: config_text.map(str::to_string),
            remove_auth_file: false,
        });
    }

    // Third-party switches are config-only. Since Codex 0.149
    // (openai/codex#39214) custom providers no longer inherit ambient auth
    // from auth.json, so the API key travels as a provider-scoped
    // `experimental_bearer_token` in config.toml (honored since Codex 0.48).
    // auth.json is reserved for the official ChatGPT login: kept when the
    // preservation setting is on, deleted otherwise. It never carries
    // third-party keys, so a `requires_openai_auth = true` fallback has no
    // third-party credential to mis-send and pre-0.48 auth.json-only Codex
    // releases are the only casualty.
    // The key may live in auth.OPENAI_API_KEY or already sit in the config
    // text (e.g. `auth = {}` raw-edited providers) — mirror
    // prepare_codex_provider_live_config's token sources.
    let carried_key = extract_codex_api_key(Some(auth), config_text);

    // Stale reserved tables are migrated BEFORE the safety gates so the
    // gates judge the same text prepare will write (a mixed stale-table +
    // openai_base_url shape would otherwise be mis-refused). prepare
    // migrates again internally (idempotent) for the gate-less proxy paths.
    let migrated = match config_text {
        Some(text) => migrate_stale_reserved_provider_tables(text, false, carried_key.is_some())?,
        None => None,
    };
    let config_text = migrated.as_deref().or(config_text);

    // The legacy reroute shape (built-in `openai` provider + top-level
    // `openai_base_url`) has no provider table to carry the key — rewrite it
    // into a cc-switch-owned custom table before the safety gates run.
    // prepare_codex_provider_live_config normalizes again internally
    // (idempotent); the gates need the normalized text here.
    let normalized = match config_text {
        Some(text) if carried_key.is_some() => normalize_codex_legacy_openai_reroute(text)?,
        _ => None,
    };
    let config_text = normalized.as_deref().or(config_text);

    // The preservation setting decides whether the official login in
    // auth.json survives a third-party switch. Off means the file is
    // deleted — a lingering login next to a third-party route is the leak
    // shape the gates exist to prevent, and `{}` is not logout, the file
    // must go (see clear_stale_codex_live_auth_after_official_switch). The
    // active table's `requires_openai_auth` is stamped to match below, so
    // Codex's login UX agrees with the file state either way.
    let remove_auth_file = !preserve_official_login;

    let live_config = match config_text {
        Some(text) if !text.trim().is_empty() => {
            // Both safety gates protect the same invariant: the auth Codex
            // resolves for a third-party route must never come from
            // auth.json (official OAuth under preservation, nothing at all
            // otherwise — either way the switch would be broken or unsafe).
            if carried_key.is_some() && codex_config_routes_third_party_without_token_slot(text) {
                return Err(AppError::localized(
                    "provider.codex.config.no_custom_provider",
                    "Codex 第三方配置必须包含自定义 model_providers 条目以承载 API 密钥（Codex 不识别顶层 experimental_bearer_token）",
                    "A Codex third-party config must define a custom model_providers entry to carry the API key (Codex ignores a top-level experimental_bearer_token)",
                ));
            }
            if carried_key.is_none()
                && codex_config_falls_back_to_official_auth_for_third_party(text)
            {
                return Err(AppError::localized(
                    "provider.codex.config.official_auth_fallback",
                    "该 Codex 配置没有可用的 API 密钥，而 requires_openai_auth = true（或顶层 openai_base_url）会让 Codex 回退使用 auth.json 里的登录凭据访问第三方地址。请为供应商填写 API 密钥，或移除该回退指令",
                    "This Codex config has no usable API key, and requires_openai_auth = true (or a top-level openai_base_url) would make Codex fall back to whatever login auth.json holds for a third-party route. Add an API key to the provider or remove the fallback directive",
                ));
            }
            prepare_codex_provider_live_config(auth, text)?
        }
        // Empty config: with a key to carry this errs inside
        // set_codex_experimental_bearer_token (no table to attach it to);
        // without a key the empty config is passed through as-is.
        other => prepare_codex_provider_live_config(auth, other.unwrap_or(""))?,
    };
    // After injection, so the stamp sees the final credential shape. Only
    // this direct-switch plan stamps: the takeover subsystem preserves the
    // login unconditionally and keeps its existing config shapes.
    let live_config = align_codex_requires_openai_auth_with_login_preservation(
        &live_config,
        preserve_official_login,
    )?;

    Ok(CodexLiveWritePlan {
        write_full_auth: false,
        config_text: Some(live_config),
        remove_auth_file,
    })
}

/// Validate a Codex live write without touching the filesystem. Callers use
/// this to fail a provider switch BEFORE committing `current`: a write-layer
/// refusal after `current` moved would let the next switch backfill the old
/// live config into the new provider's DB row.
pub fn preflight_codex_live_write(
    category: Option<&str>,
    auth: &Value,
    config_text: Option<&str>,
) -> Result<(), AppError> {
    plan_codex_live_write(
        category,
        auth,
        config_text,
        crate::settings::preserve_codex_official_auth_on_switch(),
    )
    .map(|_| ())
}

pub fn write_codex_live_for_provider(
    category: Option<&str>,
    auth: &Value,
    config_text: Option<&str>,
) -> Result<(), AppError> {
    let plan = plan_codex_live_write(
        category,
        auth,
        config_text,
        crate::settings::preserve_codex_official_auth_on_switch(),
    )?;
    // 切换写入的是 cc-switch 生成的稀疏模板：写盘前与盘上现有 config.toml
    // 做外科手术式合并，保留用户的注释/键序/自定义段落（verbatim 恢复与
    // 接管清理路径写入完整文本、键缺席即删除意图，不经此合并）。
    let merged_config = plan.config_text.as_deref().map(|text| {
        if text.trim().is_empty() {
            text.to_string()
        } else {
            merge_codex_live_config_with_disk(text)
        }
    });
    if plan.write_full_auth {
        return write_codex_live_atomic(auth, merged_config.as_deref());
    }
    write_codex_live_config_atomic(merged_config.as_deref())?;
    // Config is already committed at this point, so a cleanup failure
    // degrades to a warning instead of reporting an unswitched state.
    if plan.remove_auth_file {
        remove_codex_live_auth_after_third_party_switch();
    }
    Ok(())
}

fn remove_codex_live_auth_after_third_party_switch() {
    let auth_path = get_codex_auth_path();
    if !auth_path.exists() {
        return;
    }
    if let Err(e) = delete_file(&auth_path) {
        log::warn!("Failed to remove auth.json after a third-party Codex switch: {e}");
    }
}

/// Build the live Codex config for provider switching.
///
/// The stored provider keeps its API key in `auth.OPENAI_API_KEY`. Live Codex
/// requests can use a provider-scoped `experimental_bearer_token`, so switching
/// providers only needs to update `config.toml`; `auth.json` stays as the user's
/// long-lived ChatGPT login cache.
///
/// This is the single normalize→inject entry point: every caller — provider
/// switches, takeover backup rebuilds (`preserve_codex_auth_in_backup`), and
/// restore (`preserve_codex_oauth_login_on_restore`) — gets the legacy
/// reroute migration, so a pre-0.149 `openai_base_url` shape can never leave
/// its key in a top-level field Codex ignores while auth.json credentials
/// stay live. Idempotent on already-normalized text.
pub fn prepare_codex_provider_live_config(
    auth: &Value,
    config_text: &str,
) -> Result<String, AppError> {
    let token = extract_codex_auth_api_key(auth)
        .or_else(|| extract_codex_experimental_bearer_token(config_text));

    // Unconditional: a stale reserved table makes Codex refuse the whole
    // config (0.148+), token or not. Third-party context — the route may
    // follow the renamed table when it can authenticate (see the migrator).
    let migrated = migrate_stale_reserved_provider_tables(config_text, false, token.is_some())?;
    let config_text = migrated.as_deref().unwrap_or(config_text);

    // Also unconditional (covers the keyless third-party path; the official
    // branch of plan_codex_live_write calls it separately): 0.149 rejects
    // the whole config over any name-less custom table, active or not.
    let named = backfill_codex_custom_provider_names(config_text)?;
    let config_text = named.as_deref().unwrap_or(config_text);

    let Some(token) = token else {
        return Ok(config_text.to_string());
    };
    let normalized = normalize_codex_legacy_openai_reroute(config_text)?;
    let config_text = normalized.as_deref().unwrap_or(config_text);
    set_codex_experimental_bearer_token(config_text, &token)
}

/// During DB backfill, lift a live `experimental_bearer_token` back into
/// `auth.OPENAI_API_KEY` so the stored provider keeps its canonical shape
/// and generated live tokens don't leak into stored provider TOML.
///
/// Only intervenes when the live config actually carries a bearer token —
/// otherwise the function is a no-op so the caller's normal backfill path
/// (which keeps live `auth` as the authoritative source) is unaffected.
pub fn restore_codex_provider_token_for_backfill(
    settings: &mut Value,
    template_settings: &Value,
) -> Result<(), AppError> {
    let Some(config_text) = settings
        .get("config")
        .and_then(|value| value.as_str())
        .map(str::to_string)
    else {
        return Ok(());
    };

    let Some(token) = extract_codex_experimental_bearer_token(&config_text) else {
        return Ok(());
    };

    let cleaned_config = remove_codex_experimental_bearer_token(&config_text)?;

    if let Some(obj) = settings.as_object_mut() {
        obj.insert("config".to_string(), Value::String(cleaned_config));

        let mut auth = template_settings
            .get("auth")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if let Some(auth_obj) = auth.as_object_mut() {
            auth_obj.insert("OPENAI_API_KEY".to_string(), Value::String(token));
        }
        obj.insert("auth".to_string(), auth);
    }

    Ok(())
}

pub fn restore_codex_settings_for_backfill(
    settings: &mut Value,
    template_settings: &Value,
    restore_provider_token: bool,
) -> Result<(), AppError> {
    if restore_provider_token {
        restore_codex_provider_token_for_backfill(settings, template_settings)?;
    }
    Ok(())
}

/// Update a field in Codex config.toml using toml_edit (syntax-preserving).
///
/// Supported fields:
/// - `"base_url"`: writes to `[model_providers.<current>].base_url` if `model_provider` exists,
///   otherwise uses `openai_base_url` for Codex's default built-in provider.
/// - `"wire_api"`: writes to `[model_providers.<current>].wire_api` if `model_provider` exists,
///   otherwise leaves the built-in provider's Responses protocol unchanged.
/// - `"model"` / `"model_catalog_json"`: writes to top-level field.
///
/// Empty value removes the field.
pub fn update_codex_toml_field(toml_str: &str, field: &str, value: &str) -> Result<String, String> {
    let mut doc = toml_str
        .parse::<DocumentMut>()
        .map_err(|e| format!("TOML parse error: {e}"))?;

    let trimmed = value.trim();

    match field {
        "base_url" | "wire_api" => {
            let model_provider = doc
                .get("model_provider")
                .and_then(|item| item.as_str())
                .map(str::to_string)
                // Codex defaults to openai when the selector is absent.
                .or_else(|| (!doc.contains_key("model_provider")).then(|| "openai".to_string()));

            if let Some(provider_key) = model_provider {
                // validate_reserved_model_provider_ids（0.148 起）对配置里出现
                // `[model_providers.openai]` 等保留 id 表整份报错（"Built-in
                // providers cannot be overridden"），Codex 直接起不来。上游的
                // 保留判定是**大小写精确**的——`OpenAI` 等变体是合法自定义
                // id，照常走建表分支；bedrock 两个 id 被上游豁免，覆盖表合法。
                if provider_key == "openai" {
                    // 内置 openai 的改址走它的正统机制——顶层
                    // `openai_base_url`；wire_api 由 CLI 内置固定，无需写。
                    if field == "base_url" {
                        if trimmed.is_empty() {
                            doc.as_table_mut().remove("openai_base_url");
                        } else {
                            doc["openai_base_url"] = toml_edit::value(trimmed);
                        }
                    }
                    return Ok(doc.to_string());
                }
                if provider_key == "ollama" || provider_key == "lmstudio" {
                    // 这两个保留 id 没有等价的顶层旋钮：建表=生成 Codex 拒绝
                    // 加载的配置（接管期间整个 CLI 起不来），明确报错优于
                    // 静默写出致命配置。
                    return Err(format!(
                        "Codex 禁止覆盖内置 provider `{provider_key}`（0.148 起会拒绝加载整份配置），无法改写其 {field}；请改用自定义 provider id"
                    ));
                }

                // Ensure [model_providers] table exists
                //
                // 用 as_table_like_mut 而非 as_table_mut：用户把配置写成 inline table
                // （`model_providers = { foo = {...} }`，TOML 合法）时 as_table_mut
                // 返回 None，会一路掉进下面的顶层 fallback——用户改的 base_url 被写到
                // 了错误层级且毫无提示。
                if doc
                    .get("model_providers")
                    .is_none_or(|item| item.as_table_like().is_none())
                {
                    // 键存在但不是表（`model_providers = 42`）时，下面这行会把用户
                    // 手写的值替换掉。旧代码在这种形状下会掉进顶层 fallback 而不动
                    // 它，所以归一化必须留痕——与 mcp/codex.rs 的同款处理保持一致。
                    if doc
                        .get("model_providers")
                        .is_some_and(|item| !item.is_none())
                    {
                        log::warn!("config.toml 的 model_providers 不是表，已重置为空表");
                    }
                    doc["model_providers"] = toml_edit::table();
                }

                if let Some(model_providers) = doc
                    .get_mut("model_providers")
                    .and_then(toml_edit::Item::as_table_like_mut)
                {
                    // Ensure [model_providers.<provider_key>] table exists
                    if !model_providers.contains_key(&provider_key) {
                        model_providers.insert(&provider_key, toml_edit::table());
                    }

                    if let Some(provider_table) = model_providers
                        .get_mut(&provider_key)
                        .and_then(toml_edit::Item::as_table_like_mut)
                    {
                        // 0.149 在反序列化时就拒绝 name 为空/缺失的非 bedrock
                        // 表（"provider name must not be empty"，整份配置拒
                        // 载）——本函数正是历史上无 name 表的制造源头，建表
                        // /改表时必须保证 name 非空。反向豁免 bedrock 两个保留
                        // id（此分支唯一能到达的保留 id）：0.149 只允许它们覆盖
                        // base_url/auth/http_headers/aws.*，写入 name 会让内置
                        // 合并校验拒绝整份配置——代理接管改 base_url 正走此路。
                        if is_custom_codex_model_provider_id(&provider_key)
                            && provider_table
                                .get("name")
                                .and_then(|item| item.as_str())
                                .map(str::trim)
                                .filter(|name| !name.is_empty())
                                .is_none()
                        {
                            provider_table.insert("name", toml_edit::value(provider_key.as_str()));
                        }
                        if trimmed.is_empty() {
                            provider_table.remove(field);
                        } else {
                            provider_table.insert(field, toml_edit::value(trimmed));
                        }
                        return Ok(doc.to_string());
                    }
                }

                log::warn!(
                    "config.toml 的 [model_providers.{provider_key}] 结构异常，{field} 改写为顶层字段"
                );
            }

            // Fallback: no model_provider or structure mismatch → top-level field
            if trimmed.is_empty() {
                doc.as_table_mut().remove(field);
            } else {
                doc[field] = toml_edit::value(trimmed);
            }
        }
        "model" | "model_catalog_json" => {
            if trimmed.is_empty() {
                doc.as_table_mut().remove(field);
            } else {
                doc[field] = toml_edit::value(trimmed);
            }
        }
        _ => return Err(format!("unsupported field: {field}")),
    }

    Ok(doc.to_string())
}

/// Remove `base_url` from the active model_provider section only if it matches `predicate`.
/// Also removes top-level `base_url` if it matches.
/// Used by proxy cleanup to strip local proxy URLs without touching user-configured URLs.
pub fn remove_codex_toml_base_url_if(toml_str: &str, predicate: impl Fn(&str) -> bool) -> String {
    let mut doc = match toml_str.parse::<DocumentMut>() {
        Ok(doc) => doc,
        Err(_) => return toml_str.to_string(),
    };

    let model_provider = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::to_string);

    if let Some(provider_key) = model_provider {
        if let Some(model_providers) = doc
            .get_mut("model_providers")
            .and_then(|v| v.as_table_mut())
        {
            if let Some(provider_table) = model_providers
                .get_mut(provider_key.as_str())
                .and_then(|v| v.as_table_mut())
            {
                let should_remove = provider_table
                    .get("base_url")
                    .and_then(|item| item.as_str())
                    .map(&predicate)
                    .unwrap_or(false);
                if should_remove {
                    provider_table.remove("base_url");
                }
            }
        }
    }

    // Fallback: also clean up top-level base_url if it matches
    let should_remove_root = doc
        .get("base_url")
        .and_then(|item| item.as_str())
        .map(&predicate)
        .unwrap_or(false);
    if should_remove_root {
        doc.as_table_mut().remove("base_url");
    }

    doc.to_string()
}

#[cfg(test)]
mod tests;
