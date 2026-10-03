//! codex_config 单元测试。
//! （自 codex_config.rs 的内联 `mod tests` 机械外移；内容零改动。）

use super::*;
use serde_json::json;
use std::ffi::OsString;

#[test]
fn codex_id_token_user_identity_requires_a_nonempty_subject() {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let subject_payload = URL_SAFE_NO_PAD.encode(json!({ "sub": "stable-user" }).to_string());
    assert_eq!(
        extract_codex_id_token_user_identity(&test_codex_id_token("stable-user")),
        Some("sub:stable-user".to_string())
    );
    assert_eq!(extract_codex_id_token_user_identity("not-a-jwt"), None);
    assert_eq!(
        extract_codex_id_token_user_identity(&format!("{header}.{subject_payload}")),
        None
    );
    assert_eq!(
        extract_codex_id_token_user_identity(&format!("{header}.{subject_payload}..extra")),
        None
    );
    assert_eq!(
        extract_codex_id_token_user_identity(&format!("invalid.{subject_payload}.signature")),
        None
    );
    assert_eq!(
        extract_codex_id_token_user_identity(&test_codex_id_token("   ")),
        None
    );

    let payload = URL_SAFE_NO_PAD.encode(json!({ "email": "user@example.test" }).to_string());
    assert_eq!(
        extract_codex_id_token_user_identity(&format!("{header}.{payload}.")),
        None
    );
}

struct CodexLiveTestHome {
    _dir: tempfile::TempDir,
    original_test_home: Option<OsString>,
}

impl CodexLiveTestHome {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create isolated Codex live test home");
        let original_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
        crate::settings::reload_settings().expect("reload settings for isolated test home");

        Self {
            _dir: dir,
            original_test_home,
        }
    }
}

impl Drop for CodexLiveTestHome {
    fn drop(&mut self) {
        match &self.original_test_home {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        let _ = crate::settings::reload_settings();
    }
}

#[derive(Debug, PartialEq)]
struct CodexLiveTestState {
    auth_bytes: Vec<u8>,
    auth_value: Value,
    config_bytes: Vec<u8>,
    config_value: toml::Value,
    catalog_bytes: Vec<u8>,
    catalog_value: Value,
    marker_bytes: Vec<u8>,
    marker_value: Value,
}

fn capture_codex_live_test_state() -> CodexLiveTestState {
    let auth_bytes = fs::read(get_codex_auth_path()).expect("read live auth bytes");
    let config_bytes = fs::read(get_codex_config_path()).expect("read live config bytes");
    let catalog_bytes = fs::read(get_codex_model_catalog_path()).expect("read live catalog bytes");
    let marker_bytes = fs::read(get_codex_managed_oauth_live_auth_marker_path())
        .expect("read managed auth marker bytes");

    CodexLiveTestState {
        auth_value: serde_json::from_slice(&auth_bytes).expect("parse live auth"),
        config_value: toml::from_str(
            std::str::from_utf8(&config_bytes).expect("live config must be UTF-8"),
        )
        .expect("parse live config"),
        catalog_value: serde_json::from_slice(&catalog_bytes).expect("parse live catalog"),
        marker_value: serde_json::from_slice(&marker_bytes).expect("parse managed auth marker"),
        auth_bytes,
        config_bytes,
        catalog_bytes,
        marker_bytes,
    }
}

fn seed_rotated_managed_codex_live_state() -> CodexLiveTestState {
    let id_token = test_codex_id_token("user-a");
    let auth = codex_managed_oauth_auth_value(
        "account-a",
        "access-r1",
        Some(&id_token),
        "refresh-r1",
        "2026-08-06T00:00:01Z",
    );
    crate::config::write_json_file(&get_codex_auth_path(), &auth).expect("seed live auth R1");
    crate::config::write_text_file(
            &get_codex_config_path(),
            "# cas-guard-sentinel\nmodel = \"gpt-5.5\"\nmodel_catalog_json = \"cc-switch-model-catalog.json\"\n",
        )
        .expect("seed live config");
    crate::config::write_json_file(
        &get_codex_model_catalog_path(),
        &json!({ "models": [{ "slug": "cas-guard-sentinel" }] }),
    )
    .expect("seed live catalog");
    record_codex_managed_oauth_live_auth(&auth, "account-a").expect("seed managed auth marker");

    capture_codex_live_test_state()
}

#[test]
#[serial_test::serial(global_env)]
fn ensure_live_auth_guard_rejects_rotated_refresh_without_mutating_live_bundle() {
    let _home = CodexLiveTestHome::new();
    let before = seed_rotated_managed_codex_live_state();

    let result = ensure_codex_live_auth_unchanged_for_managed_account("account-a", "refresh-r0");

    assert!(result.is_err(), "R1 live auth must reject an expected R0");
    assert_eq!(capture_codex_live_test_state(), before);
}

#[test]
#[serial_test::serial(global_env)]
fn clear_live_auth_guard_rejects_rotated_refresh_without_mutating_live_bundle() {
    let _home = CodexLiveTestHome::new();
    let before = seed_rotated_managed_codex_live_state();

    let result =
        clear_codex_live_auth_for_managed_account_if_unchanged("account-a", Some("refresh-r0"));

    assert!(result.is_err(), "R1 live auth must reject an expected R0");
    assert_eq!(capture_codex_live_test_state(), before);
}

#[test]
fn catalog_tool_profile_from_api_format() {
    assert_eq!(
        CodexCatalogToolProfile::from_api_format(Some("anthropic")),
        CodexCatalogToolProfile::Anthropic
    );
    assert_eq!(
        CodexCatalogToolProfile::from_api_format(Some("openai_responses")),
        CodexCatalogToolProfile::NativeResponses
    );
    assert_eq!(
        CodexCatalogToolProfile::from_api_format(Some("openai_chat")),
        CodexCatalogToolProfile::ProxyChat
    );
    assert_eq!(
        CodexCatalogToolProfile::from_api_format(None),
        CodexCatalogToolProfile::ProxyChat
    );
}

#[test]
fn unified_session_bucket_injects_for_empty_official_config() {
    let injected = inject_codex_unified_session_bucket("").expect("inject");
    let doc: toml::Table = toml::from_str(&injected).expect("parse injected config");

    assert_eq!(
        doc.get("model_provider").and_then(|v| v.as_str()),
        Some(CC_SWITCH_CODEX_MODEL_PROVIDER_ID)
    );
    let custom = doc["model_providers"][CC_SWITCH_CODEX_MODEL_PROVIDER_ID]
        .as_table()
        .expect("custom provider table");
    assert_eq!(custom.get("name").and_then(|v| v.as_str()), Some("OpenAI"));
    assert_eq!(
        custom.get("requires_openai_auth").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        custom.get("supports_websockets").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        custom.get("wire_api").and_then(|v| v.as_str()),
        Some("responses")
    );
}

#[test]
fn official_proxy_route_uses_native_auth_and_local_responses_provider() {
    let input = r#"model = "gpt-5.4"
experimental_bearer_token = "PROXY_MANAGED"

[mcp_servers.example]
command = "example"
"#;
    let output = apply_codex_official_proxy_route(input, "http://127.0.0.1:15721/v1")
        .expect("apply official proxy route");
    let doc: toml::Value = toml::from_str(&output).expect("parse output");

    assert_eq!(
        doc.get("model_provider").and_then(toml::Value::as_str),
        Some(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID)
    );
    assert!(doc.get("experimental_bearer_token").is_none());
    assert!(
        doc.get("mcp_servers").is_some(),
        "unrelated config survives"
    );

    let provider = &doc["model_providers"][CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID];
    assert_eq!(
        provider.get("base_url").and_then(toml::Value::as_str),
        Some("http://127.0.0.1:15721/v1")
    );
    assert_eq!(
        provider
            .get("requires_openai_auth")
            .and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        provider
            .get("supports_websockets")
            .and_then(toml::Value::as_bool),
        Some(false)
    );
    assert!(codex_config_has_official_proxy_route(&output));
}

#[test]
fn official_proxy_route_cleanup_only_removes_owned_provider() {
    let projected =
        apply_codex_official_proxy_route("model = \"gpt-5.4\"\n", "http://127.0.0.1:15721/v1")
            .expect("project");
    let cleaned = remove_codex_official_proxy_route(&projected).expect("clean");
    let doc: toml::Value = toml::from_str(&cleaned).expect("parse cleaned");
    assert!(doc.get("model_provider").is_none());
    assert!(doc.get("model_providers").is_none());
    assert_eq!(
        doc.get("model").and_then(toml::Value::as_str),
        Some("gpt-5.4")
    );
}

#[test]
fn official_proxy_route_rejects_non_table_model_providers_without_panicking() {
    for input in [
        "model_providers = 3\n",
        "[[model_providers]]\nname = \"broken\"\n",
    ] {
        let result = apply_codex_official_proxy_route(input, "http://127.0.0.1:15721/v1");
        assert!(result.is_err());
    }
}

#[test]
fn official_proxy_route_normalizes_inline_tables_and_cleans_stale_placeholder() {
    let input = r#"model_provider = "rightcode"
model_providers = { rightcode = { name = "RightCode", experimental_bearer_token = "PROXY_MANAGED" } }
"#;
    let projected = apply_codex_official_proxy_route(input, "http://127.0.0.1:15721/v1")
        .expect("project inline provider table");
    let projected_doc: toml::Value = toml::from_str(&projected).expect("parse projected");
    assert!(projected_doc["model_providers"]["rightcode"]
        .get("experimental_bearer_token")
        .is_none());
    assert!(projected_doc["model_providers"]
        .get(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID)
        .is_some());

    let cleaned = remove_codex_official_proxy_route(&projected).expect("clean projected");
    let cleaned_doc: toml::Value = toml::from_str(&cleaned).expect("parse cleaned");
    assert!(cleaned_doc.get("model_provider").is_none());
    assert!(cleaned_doc["model_providers"].get("rightcode").is_some());
    assert!(cleaned_doc["model_providers"]
        .get(CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID)
        .is_none());
}

#[test]
fn unified_session_bucket_preserves_other_keys_and_explicit_routing() {
    let with_catalog = "model_catalog_json = \"cc-switch-model-catalog.json\"\n";
    let injected = inject_codex_unified_session_bucket(with_catalog).expect("inject");
    assert!(injected.contains("model_catalog_json"));
    assert!(injected.contains("model_provider = \"custom\""));

    // 用户显式指定过 model_provider 的官方配置不被覆盖
    let explicit = "model_provider = \"openai_https\"\n";
    let unchanged = inject_codex_unified_session_bucket(explicit).expect("inject");
    assert_eq!(unchanged, explicit);
}

#[test]
fn unified_session_bucket_skips_conflicting_custom_table() {
    // 残留的非注入形态 custom 表：设置 model_provider 会把官方流量
    // 路由到表里的第三方端点，必须整体拒绝注入。
    let stale = r#"[model_providers.custom]
name = "Relay"
base_url = "https://relay.example/v1"
"#;
    let unchanged = inject_codex_unified_session_bucket(stale).expect("inject");
    assert_eq!(unchanged, stale);

    // 已是注入形态的 custom 表（如重复注入）则照常补上 model_provider
    let injected_once = inject_codex_unified_session_bucket("").expect("inject");
    let reinjected = inject_codex_unified_session_bucket(&injected_once).expect("re-inject");
    assert_eq!(reinjected, injected_once);
}

#[test]
fn unified_session_bucket_strip_round_trips_injection() {
    let injected = inject_codex_unified_session_bucket("").expect("inject");
    let stripped = strip_codex_unified_session_bucket(&injected).expect("strip");
    assert_eq!(stripped.trim(), "");

    let with_catalog = "model_catalog_json = \"cc-switch-model-catalog.json\"\n";
    let injected = inject_codex_unified_session_bucket(with_catalog).expect("inject");
    let stripped = strip_codex_unified_session_bucket(&injected).expect("strip");
    assert_eq!(stripped, with_catalog);
}

#[test]
fn unified_session_bucket_strip_keeps_third_party_custom_entry() {
    // 第三方模板同样用 custom 路由，但条目带 base_url 等差异字段，
    // 形态不等于注入产物，必须原样保留。
    let third_party = r#"model_provider = "custom"

[model_providers.custom]
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
    let untouched = strip_codex_unified_session_bucket(third_party).expect("strip");
    assert_eq!(untouched, third_party);
}

#[test]
fn unified_session_bucket_strip_from_settings_only_touches_config() {
    let injected = inject_codex_unified_session_bucket("").expect("inject");
    let mut settings = json!({
        "auth": { "tokens": { "access_token": "secret" } },
        "config": injected,
    });
    strip_codex_unified_session_bucket_from_settings(&mut settings).expect("strip settings");
    assert_eq!(
        settings
            .get("config")
            .and_then(|v| v.as_str())
            .map(str::trim),
        Some("")
    );
    assert!(settings.pointer("/auth/tokens/access_token").is_some());
}

#[test]
fn strip_mcp_servers_from_settings_removes_table_and_legacy_form() {
    let mut settings = json!({
        "auth": { "OPENAI_API_KEY": "sk-test" },
        "config": "# user comment\nmodel = \"gpt-5.5\"\n\n[mcp_servers.echo]\ntype = \"stdio\"\ncommand = \"echo\"\n\n[mcp.servers.legacy]\ncommand = \"noop\"\n",
    });
    strip_codex_mcp_servers_from_settings(&mut settings).expect("strip mcp");
    let config = settings
        .get("config")
        .and_then(|v| v.as_str())
        .expect("config text");
    assert!(!config.contains("mcp_servers"), "got: {config}");
    assert!(
        !config.contains("[mcp"),
        "legacy [mcp.servers] gone: {config}"
    );
    assert!(config.contains("# user comment"), "comments preserved");
    assert!(config.contains("model = \"gpt-5.5\""));
}

#[test]
fn strip_mcp_servers_from_settings_is_noop_without_mcp() {
    let original = "# comment\nmodel = \"gpt-5.5\"\n";
    let mut settings = json!({
        "auth": {},
        "config": original,
    });
    strip_codex_mcp_servers_from_settings(&mut settings).expect("strip mcp");
    assert_eq!(
        settings.get("config").and_then(|v| v.as_str()),
        Some(original),
        "config text must be byte-identical when nothing is stripped"
    );
}

#[test]
fn extract_base_url_prefers_active_provider_section() {
    let input = r#"model_provider = "azure"

[model_providers.azure]
base_url = "https://azure.example.com/v1"

[model_providers.other]
base_url = "https://other.example.com/v1"
"#;

    assert_eq!(
        extract_codex_base_url(input).as_deref(),
        Some("https://azure.example.com/v1")
    );
}

#[test]
fn extract_base_url_falls_back_to_top_level_only() {
    let top_level = r#"base_url = "https://top-level.example.com/v1""#;
    assert_eq!(
        extract_codex_base_url(top_level).as_deref(),
        Some("https://top-level.example.com/v1")
    );
}

// Mirrors the frontend extractCodexBaseUrl: a non-active provider section
// is never a credential source, whether the active provider points
// elsewhere (e.g. the built-in "openai") or none is selected at all.
#[test]
fn extract_base_url_ignores_non_active_provider_sections() {
    let mismatched = r#"model_provider = "openai"

[model_providers.custom]
base_url = "https://leftover.example.com/v1"
"#;
    assert_eq!(extract_codex_base_url(mismatched), None);

    let no_active = r#"[model_providers.any]
base_url = "https://single.example.com/v1"
"#;
    assert_eq!(extract_codex_base_url(no_active), None);
}

#[test]
fn prepare_provider_live_config_rejects_key_without_config() {
    let err = prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), "")
        .expect_err("empty config with API key should not truncate live config");

    assert!(
        err.to_string().contains("config.toml"),
        "error should explain missing config.toml, got: {err}"
    );
}

#[test]
#[serial_test::serial(global_env)]
fn managed_chatgpt_login_matches_local_marker_and_workspace() {
    let _home = CodexLiveTestHome::new();
    let shared_chatgpt_user_token = |subject: &str| {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            json!({
                "sub": subject,
                "https://api.openai.com/auth": {
                    "chatgpt_user_id": "shared-team-user-id"
                }
            })
            .to_string(),
        );
        format!("{header}.{payload}.")
    };
    // 原生 auth 保留 workspace ID；marker 用本地 ID 区分同 workspace 登录。
    let full_bundle = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": shared_chatgpt_user_token("user-a"),
            "access_token": "access",
            "refresh_token": "refresh-secret",
            "account_id": "workspace-shared"
        },
        "last_refresh": "2026-01-02T03:04:05.000000000Z"
    });
    record_codex_managed_oauth_live_auth(&full_bundle, "local-account-a")
        .expect("record managed auth marker");
    crate::config::write_json_file(&get_codex_auth_path(), &full_bundle)
        .expect("write managed live auth");
    assert!(
        codex_live_auth_matches_managed_request("local-account-a", "access").unwrap(),
        "the selected account's exact live bearer must match"
    );
    assert!(
        !codex_live_auth_matches_managed_request("local-account-a", "other-access").unwrap(),
        "another user's bearer in the same workspace must not match"
    );
    let managed_id_token = full_bundle
        .pointer("/tokens/id_token")
        .and_then(Value::as_str)
        .expect("managed id token");
    assert!(
        codex_live_auth_is_managed_chatgpt_login(&full_bundle, "local-account-a"),
        "a full refreshable bundle for the managed account must be recognized"
    );
    assert!(
        !codex_live_auth_is_managed_chatgpt_login(&full_bundle, "local-account-b"),
        "another local login in the same workspace must not match"
    );
    let mut other_user = full_bundle.clone();
    other_user["tokens"]["id_token"] = json!(shared_chatgpt_user_token("user-b"));
    assert!(
        !codex_live_auth_is_managed_chatgpt_login(&other_user, "local-account-a"),
        "a native login for another user in the same workspace must not match"
    );
    crate::config::write_json_file(&get_codex_auth_path(), &other_user)
        .expect("write other user's native login");
    assert!(
        read_codex_live_auth_refresh_for_account("local-account-a").is_none(),
        "another user's refresh token must not be adopted"
    );
    clear_codex_live_auth_for_managed_account("local-account-a")
        .expect("clear stale local ownership marker");
    assert!(
        get_codex_auth_path().exists(),
        "removing local account A must not delete native account B"
    );

    crate::config::write_json_file(
        &get_codex_managed_oauth_live_auth_marker_path(),
        &json!({
            "version": 2,
            "account_id": "workspace-shared"
        }),
    )
    .expect("write legacy managed auth marker");
    assert!(
        read_codex_live_auth_refresh_for_managed_account(
            "workspace-shared",
            Some(managed_id_token),
        )
        .is_err(),
        "a legacy marker must not migrate across users in one workspace"
    );
    assert!(
        !codex_live_auth_is_managed_chatgpt_login(&other_user, "workspace-shared"),
        "a legacy marker without user identity must not establish ownership"
    );
    assert!(
        read_codex_live_auth_refresh_for_account("workspace-shared").is_none(),
        "a legacy marker must not authorize refresh-token adoption"
    );
    clear_codex_live_auth_for_managed_account("workspace-shared")
        .expect("clear ambiguous legacy marker");
    assert!(
        get_codex_auth_path().exists(),
        "clearing an ambiguous legacy marker must preserve native auth"
    );

    // 非 chatgpt 模式（API key）不应命中。
    let api_key_auth = json!({ "OPENAI_API_KEY": "sk-live" });
    assert!(!codex_live_auth_is_managed_chatgpt_login(
        &api_key_auth,
        "local-account-a"
    ));
}

#[test]
#[serial_test::serial(global_env)]
fn legacy_managed_marker_migrates_by_user_without_breaking_refresh_rollback() {
    let _home = CodexLiveTestHome::new();
    let id_token = test_codex_id_token("legacy-user");
    let auth_r0 = codex_managed_oauth_auth_value(
        "legacy-workspace",
        "access-r0",
        Some(&id_token),
        "refresh-r0",
        "2026-01-01T00:00:00Z",
    );
    crate::config::write_json_file(&get_codex_auth_path(), &auth_r0)
        .expect("write legacy live auth");
    crate::config::write_json_file(
        &get_codex_managed_oauth_live_auth_marker_path(),
        &json!({
            "version": 2,
            "account_id": "legacy-workspace"
        }),
    )
    .expect("write legacy marker");
    let snapshot = CodexLiveStateSnapshot::capture().expect("capture legacy generation");

    let migrated =
        read_codex_live_auth_refresh_for_managed_account("legacy-workspace", Some(&id_token))
            .expect("migrate matching legacy marker")
            .expect("read matching live refresh");
    assert_eq!(migrated.refresh_token, "refresh-r0");
    assert!(codex_live_auth_is_managed_chatgpt_login(
        &auth_r0,
        "legacy-workspace"
    ));

    let auth_r1 = codex_managed_oauth_auth_value(
        "legacy-workspace",
        "access-r1",
        Some(&id_token),
        "refresh-r1",
        "2026-01-02T00:00:00Z",
    );
    crate::config::write_json_file(&get_codex_auth_path(), &auth_r1)
        .expect("write rotated live auth");
    snapshot
        .restore_preserving_newer_same_account_auth()
        .expect("rollback after marker migration");

    let restored: Value =
        crate::config::read_json_file(&get_codex_auth_path()).expect("read preserved rotated auth");
    assert_eq!(restored, auth_r1);
    assert!(codex_live_auth_is_managed_chatgpt_login(
        &restored,
        "legacy-workspace"
    ));
}

#[test]
#[serial_test::serial(global_env)]
fn legacy_managed_marker_removal_requires_manager_identity() {
    let _home = CodexLiveTestHome::new();
    let id_token = test_codex_id_token("legacy-user");
    let auth = codex_managed_oauth_auth_value(
        "legacy-workspace",
        "access",
        Some(&id_token),
        "refresh",
        "2026-01-01T00:00:00Z",
    );
    crate::config::write_json_file(&get_codex_auth_path(), &auth).expect("write legacy live auth");
    crate::config::write_json_file(
        &get_codex_managed_oauth_live_auth_marker_path(),
        &json!({
            "version": 2,
            "account_id": "legacy-workspace"
        }),
    )
    .expect("write legacy marker");

    let other_user = test_codex_id_token("other-user");
    assert!(prepare_codex_live_auth_for_managed_account_removal(
        "legacy-workspace",
        Some(&other_user),
    )
    .is_err());
    assert!(get_codex_auth_path().exists());
    assert!(get_codex_managed_oauth_live_auth_marker_path().exists());

    prepare_codex_live_auth_for_managed_account_removal("legacy-workspace", Some(&id_token))
        .expect("prove and migrate legacy ownership");
    clear_codex_live_auth_for_managed_account("legacy-workspace")
        .expect("remove proven managed live auth");
    assert!(!get_codex_auth_path().exists());
    assert!(!get_codex_managed_oauth_live_auth_marker_path().exists());
}

#[test]
fn prepare_provider_live_config_uses_top_level_token_for_reserved_provider() {
    let input = r#"model_provider = "openai"
model = "gpt-5"
"#;

    let output = prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), input)
        .expect("prepare live config");
    let parsed: toml::Value = toml::from_str(&output).expect("parse output");

    assert_eq!(
        parsed
            .get("experimental_bearer_token")
            .and_then(|v| v.as_str()),
        Some("sk-test")
    );
    assert!(
        parsed.get("model_providers").is_none(),
        "reserved provider tables should not be synthesized"
    );
}

#[test]
fn bearer_token_round_trips_through_inline_provider_tables() {
    // Inline tables (`model_providers = { ... }`) are valid TOML that
    // `as_table` rejects; the token must still land inside the provider
    // table — a top-level fallback is ignored by Codex 0.149 (401 persists).
    let input = r#"model_provider = "aihubmix"
model_providers = { aihubmix = { name = "AiHubMix", base_url = "https://aihubmix.example/v1" } }
"#;

    let output = prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-inline"}), input)
        .expect("prepare live config");
    let parsed: toml::Value = toml::from_str(&output).expect("parse output");
    assert_eq!(
        parsed
            .get("model_providers")
            .and_then(|v| v.get("aihubmix"))
            .and_then(|v| v.get("experimental_bearer_token"))
            .and_then(|v| v.as_str()),
        Some("sk-inline"),
        "token must land inside the inline provider table; got:\n{output}"
    );
    assert!(
        parsed.get("experimental_bearer_token").is_none(),
        "token must not leak to the top level for a custom provider"
    );

    assert_eq!(
        extract_codex_experimental_bearer_token(&output).as_deref(),
        Some("sk-inline"),
        "extraction must read the token back out of an inline provider table"
    );

    let cleaned = remove_codex_experimental_bearer_token(&output).expect("remove token");
    assert!(
        !cleaned.contains("experimental_bearer_token"),
        "removal must strip the token from an inline provider table; got:\n{cleaned}"
    );
}

#[test]
fn prepare_provider_live_config_skips_tables_with_explicit_auth() {
    // Codex 0.149 rejects `experimental_bearer_token` alongside `auth` /
    // `aws` at deserialization (the whole config fails to parse), and
    // `env_key` outranks the token at runtime, so injection buys nothing.
    // All three shapes must be left untouched.
    for provider_table in [
        "env_key = \"AZURE_OPENAI_API_KEY\"",
        "auth = { command = \"my-auth-helper\" }",
        "aws = { region = \"us-east-1\" }",
        // Header-based auth survives 0.149 only if we leave it alone: the
        // injected bearer would be applied after provider headers and
        // overwrite the explicit Authorization. Header names are
        // case-insensitive.
        "http_headers = { Authorization = \"Bearer header-token\" }",
        "http_headers = { authorization = \"Bearer header-token\" }",
        "env_http_headers = { AUTHORIZATION = \"MY_AUTH_ENV_VAR\" }",
    ] {
        let input = format!(
            r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "https://example.com/v1"
{provider_table}
"#
        );

        let output =
            prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), &input)
                .expect("prepare live config");
        assert_eq!(
            output, input,
            "provider table declaring `{provider_table}` must not receive an injected token"
        );
    }

    // `requires_openai_auth = true` must NOT suppress injection: the token
    // outranks it at runtime, which is exactly what keeps a preserved
    // official OAuth login from being sent to a third-party endpoint.
    let bridge_input = r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "https://example.com/v1"
requires_openai_auth = true
"#;
    let output =
        prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), bridge_input)
            .expect("prepare live config");
    let parsed: toml::Value = toml::from_str(&output).expect("parse output");
    assert_eq!(
        parsed
            .get("model_providers")
            .and_then(|v| v.get("custom"))
            .and_then(|v| v.get("experimental_bearer_token"))
            .and_then(|v| v.as_str()),
        Some("sk-test"),
        "requires_openai_auth tables must still receive the token (bridge contract)"
    );

    // requires_openai_auth = true disables the header guard too: without
    // the token, Codex would fall back to the preserved official OAuth
    // (applied after provider headers) and send it to the third-party
    // endpoint. The bridge contract outranks a contradictory header.
    let contradictory_input = r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "https://example.com/v1"
requires_openai_auth = true
http_headers = { Authorization = "Bearer header-token" }
"#;
    let output = prepare_codex_provider_live_config(
        &json!({"OPENAI_API_KEY": "sk-test"}),
        contradictory_input,
    )
    .expect("prepare live config");
    assert!(
            output.contains("experimental_bearer_token = \"sk-test\""),
            "requires_openai_auth must re-enable injection despite an Authorization header; got:\n{output}"
        );

    // Non-Authorization headers are not credentials — injection proceeds.
    let plain_headers_input = r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "https://example.com/v1"
http_headers = { x-api-version = "2026-01-01" }
"#;
    let output = prepare_codex_provider_live_config(
        &json!({"OPENAI_API_KEY": "sk-test"}),
        plain_headers_input,
    )
    .expect("prepare live config");
    assert!(
        output.contains("experimental_bearer_token = \"sk-test\""),
        "plain http_headers without Authorization must not suppress injection; got:\n{output}"
    );
}

#[test]
fn third_party_route_without_token_slot_detection() {
    // Dangerous shapes: routing points away from the official provider
    // but the token has no provider table to land in.
    for dangerous in [
        // custom id but its table is missing
        "model_provider = \"aihubmix\"\n",
        // built-in provider rerouted to a third party
        "openai_base_url = \"https://relay.example/v1\"\n",
        "model_provider = \"openai\"\nopenai_base_url = \"https://relay.example/v1\"\n",
    ] {
        assert!(
            codex_config_routes_third_party_without_token_slot(dangerous),
            "shape must be flagged (third-party route, no token slot):\n{dangerous}"
        );
    }

    // Safe shapes: either the token has a landing spot, or nothing
    // reroutes requests away from the official provider (top-level token
    // stays a cc-switch-only record).
    let custom_with_table = r#"model_provider = "aihubmix"

[model_providers.aihubmix]
base_url = "https://aihubmix.example/v1"
"#;
    let custom_inline_table = r#"model_provider = "aihubmix"
model_providers = { aihubmix = { base_url = "https://aihubmix.example/v1" } }
"#;
    for safe in [
        custom_with_table,
        custom_inline_table,
        // no routing directive at all (e.g. an MCP-only config)
        "model = \"gpt-5\"\n",
        "[mcp_servers.echo]\ncommand = \"echo\"\n",
        // explicit built-in provider without a reroute
        "model_provider = \"openai\"\n",
    ] {
        assert!(
            !codex_config_routes_third_party_without_token_slot(safe),
            "shape must not be flagged:\n{safe}"
        );
    }
}

#[test]
fn official_auth_fallback_for_third_party_detection() {
    // Dangerous shapes: with no injectable key, auth resolution falls
    // back to `auth.json` while requests go to a third-party endpoint.
    let header_auth_with_fallback = r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "https://relay.example/v1"
requires_openai_auth = true
http_headers = { Authorization = "Bearer explicit-header-token" }
"#;
    for dangerous in [
            header_auth_with_fallback,
            // bare fallback flag, no credentials anywhere
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nrequires_openai_auth = true\n",
            // built-in openai rerouted to a third party
            "openai_base_url = \"https://relay.example/v1\"\n",
            "model_provider = \"openai\"\nopenai_base_url = \"https://relay.example/v1\"\n",
            // auth/aws are NOT own-credential short-circuits: 0.149 rejects
            // both as mutually exclusive with requires_openai_auth (aws is
            // Bedrock-only on top), so these are dead configs the whole file
            // fails to load with — flag them instead of writing them out
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nrequires_openai_auth = true\nauth = { command = \"my-auth\" }\n",
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nrequires_openai_auth = true\naws = { region = \"us-east-1\" }\n",
        ] {
            assert!(
                codex_config_falls_back_to_official_auth_for_third_party(dangerous),
                "shape must be flagged (auth.json fallback on a third-party route):\n{dangerous}"
            );
        }

    for safe in [
            // no fallback flag: 0.149 resolves this as unauthenticated and
            // the provider's own headers survive (header-auth contract)
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nhttp_headers = { Authorization = \"Bearer k\" }\n",
            // provider-own credentials outrank / replace the fallback
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nrequires_openai_auth = true\nenv_key = \"MY_KEY\"\n",
            // a scoped token is second in the 0.149 short-circuit chain
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nrequires_openai_auth = true\nexperimental_bearer_token = \"tok\"\n",
            // auth/aws without the fallback flag are loadable own-credential
            // shapes (command-backed auth; aws on its Bedrock-only ids never
            // reaches this custom-table arm) — requires_openai_auth unset
            // means no auth.json fallback either way
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nauth = { command = \"my-auth\" }\n",
            // no routing directive at all: stays on the official provider
            "model = \"gpt-5\"\n",
            "[mcp_servers.echo]\ncommand = \"echo\"\n",
            "model_provider = \"openai\"\n",
            // custom id with a missing table: Codex refuses to start, no leak
            "model_provider = \"custom\"\n",
            // openai_base_url is inert for non-openai built-ins
            "model_provider = \"ollama\"\nopenai_base_url = \"https://relay.example/v1\"\n",
        ] {
            assert!(
                !codex_config_falls_back_to_official_auth_for_third_party(safe),
                "shape must not be flagged:\n{safe}"
            );
        }
}

#[test]
fn neutralize_proxy_oauth_fallback_flips_only_active_custom_true() {
    // The managed-OAuth preset snapshot (keyless card carrying the legacy
    // template flag): flagged by the gate as-is, clean once neutralized.
    let poisoned = "model_provider = \"custom\"\nmodel = \"grok-4.5\"\n\n[model_providers.custom]\nname = \"xai\"\nbase_url = \"https://api.x.ai/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n";
    let neutralized = neutralize_codex_official_auth_fallback_for_proxy_oauth(poisoned)
        .expect("explicit true on the active custom table must be flipped");
    assert!(neutralized.contains("requires_openai_auth = false"));
    assert!(codex_config_falls_back_to_official_auth_for_third_party(
        poisoned
    ));
    assert!(!codex_config_falls_back_to_official_auth_for_third_party(
        &neutralized
    ));
    // Idempotent: the neutralized snapshot passes through unchanged.
    assert!(neutralize_codex_official_auth_fallback_for_proxy_oauth(&neutralized).is_none());

    // Inline-table containers must be reachable too (as_table_like, not
    // as_table — the recurring 0.149 inline-table lesson).
    let inline = "model_provider = \"custom\"\nmodel_providers = { custom = { base_url = \"https://api.x.ai/v1\", requires_openai_auth = true } }\n";
    let inline_neutralized = neutralize_codex_official_auth_fallback_for_proxy_oauth(inline)
        .expect("inline provider table must be neutralized");
    assert!(inline_neutralized.contains("requires_openai_auth = false"));

    for untouched in [
            // absent flag — already the safe keyless shape
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://api.x.ai/v1\"\n",
            // built-in routing / top-level reroute: the gate keeps ownership
            // of those shapes, this function only mends the active custom table
            "model_provider = \"openai\"\nopenai_base_url = \"https://relay.example/v1\"\n",
            "openai_base_url = \"https://relay.example/v1\"\n",
            // missing table / unparsable TOML: downstream validators report
            "model_provider = \"custom\"\n",
            "model_provider = [",
        ] {
            assert!(
                neutralize_codex_official_auth_fallback_for_proxy_oauth(untouched).is_none(),
                "shape must pass through unchanged:\n{untouched}"
            );
        }
}

#[test]
fn legacy_openai_reroute_is_normalized_into_a_custom_table() {
    let legacy = r#"# keep me
model = "gpt-5.4"
model_provider = "openai"
openai_base_url = "https://relay.example/v1"
"#;
    let normalized = normalize_codex_legacy_openai_reroute(legacy)
        .expect("normalize")
        .expect("legacy shape must be rewritten");

    assert!(
        !normalized.contains("openai_base_url"),
        "the top-level reroute must be removed; got:\n{normalized}"
    );
    assert!(
        normalized.contains("model_provider = \"cc-switch\""),
        "routing must move to the cc-switch table; got:\n{normalized}"
    );
    assert!(
        normalized.contains("[model_providers.cc-switch]"),
        "a custom provider table must be created; got:\n{normalized}"
    );
    assert!(
        normalized.contains("base_url = \"https://relay.example/v1\""),
        "the reroute URL must land in the table; got:\n{normalized}"
    );
    assert!(
        normalized.contains("wire_api = \"responses\""),
        "the built-in openai provider speaks Responses; got:\n{normalized}"
    );
    assert!(
        normalized.contains("# keep me") && normalized.contains("model = \"gpt-5.4\""),
        "unrelated content must survive; got:\n{normalized}"
    );

    // Idempotent: the normalized shape no longer matches.
    assert!(
        normalize_codex_legacy_openai_reroute(&normalized)
            .expect("normalize")
            .is_none(),
        "re-running normalization must be a no-op"
    );

    // The rewritten shape gives the key a provider-scoped slot.
    let injected =
        prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), &normalized)
            .expect("prepare live config");
    assert!(
        injected.contains("experimental_bearer_token = \"sk-test\""),
        "token must land inside the cc-switch table; got:\n{injected}"
    );
    assert_eq!(
        extract_codex_experimental_bearer_token(&injected).as_deref(),
        Some("sk-test"),
    );
}

#[test]
fn legacy_reroute_normalization_covers_exact_built_in_openai_only() {
    // Unset model_provider defaults to the built-in openai provider.
    assert!(
        normalize_codex_legacy_openai_reroute("openai_base_url = \"https://relay.example/v1\"\n")
            .expect("normalize")
            .is_some(),
        "unset model_provider defaults to the built-in openai provider"
    );

    for untouched in [
            // upstream built-in lookup is case-sensitive: `OpenAI` targets a
            // custom table, the reroute knob is inert for it
            "model_provider = \"OpenAI\"\nopenai_base_url = \"https://relay.example/v1\"\n",
            // custom provider: openai_base_url is not what routes it
            "model_provider = \"custom\"\nopenai_base_url = \"https://relay.example/v1\"\n\n[model_providers.custom]\nbase_url = \"https://aihubmix.example/v1\"\n",
            // openai_base_url is inert for non-openai built-ins
            "model_provider = \"ollama\"\nopenai_base_url = \"https://relay.example/v1\"\n",
            // nothing to rewrite
            "model_provider = \"openai\"\n",
            "openai_base_url = \"\"\n",
        ] {
            assert!(
                normalize_codex_legacy_openai_reroute(untouched)
                    .expect("normalize")
                    .is_none(),
                "shape must be left alone:\n{untouched}"
            );
        }
}

#[test]
fn legacy_reroute_normalization_never_overwrites_a_user_cc_switch_table() {
    // A user-authored [model_providers.cc-switch] proves nothing about
    // ownership — overwriting it would drop their headers/query params
    // and backfill the loss into the DB. Migration continues under the
    // first free suffixed id instead: refusing outright would let proxy
    // backup/restore (which call prepare without the safety gates) write
    // an unmigrated reroute with live auth.json credentials.
    let conflicted = r#"model_provider = "openai"
openai_base_url = "https://relay.example/v1"

[model_providers.cc-switch]
name = "Mine"
base_url = "https://mine.example/v1"
http_headers = { x-team = "42" }
"#;
    let normalized = normalize_codex_legacy_openai_reroute(conflicted)
        .expect("normalize")
        .expect("conflicted shape must still migrate");
    assert!(
        normalized.contains("model_provider = \"cc-switch-2\"")
            && normalized.contains("[model_providers.cc-switch-2]"),
        "migration must pick the first free suffixed id; got:\n{normalized}"
    );
    assert!(
        normalized.contains("name = \"Mine\"")
            && normalized.contains("base_url = \"https://mine.example/v1\"")
            && normalized.contains("x-team"),
        "the user's own table must survive untouched; got:\n{normalized}"
    );
    assert!(
        !normalized.contains("openai_base_url"),
        "the reroute must still be rewritten away; got:\n{normalized}"
    );
}

#[test]
fn stale_reserved_tables_are_renamed_with_fallback_aware_routing() {
    // Older cc-switch takeover projections created reserved
    // [model_providers.openai]/[.ollama]/[.lmstudio] tables; Codex 0.148+
    // rejects the whole config at load. Tables are renamed and made
    // loadable; the route follows unless the table would resolve
    // auth.json with no injected token to short-circuit it.
    let stale = r#"model_provider = "openai"
model = "gpt-5.4"

[model_providers.openai]
name = "OpenAI"
base_url = "https://relay.example/v1"
http_headers = { x-team = "42" }
"#;

    // With an injectable key: follow the renamed table and inject into it
    // — snapping back to the built-in provider would silently bill the
    // preserved official account (or 401 with preservation off).
    let prepared = prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), stale)
        .expect("prepare live config");
    assert!(
        !prepared.contains("[model_providers.openai]")
            && prepared.contains("[model_providers.cc-switch]")
            && prepared.contains("x-team")
            && prepared.contains("wire_api = \"responses\""),
        "the table must be renamed losslessly (wire_api defaulted); got:\n{prepared}"
    );
    assert!(
        prepared.contains("model_provider = \"cc-switch\""),
        "with a key the route must follow the renamed table; got:\n{prepared}"
    );
    assert_eq!(
        extract_codex_experimental_bearer_token(&prepared).as_deref(),
        Some("sk-test"),
        "the key must land in the followed table"
    );

    // Keyless but the table carries its own credentials (plain
    // Authorization header): follow — 0.149 resolves it unauthenticated
    // and the provider headers survive. The name-less table is also
    // backfilled so the renamed table loads at all.
    let header_auth_stale = r#"model_provider = "openai"

[model_providers.openai]
base_url = "https://relay.example/v1"
http_headers = { Authorization = "Bearer own-key" }
"#;
    let keyless = prepare_codex_provider_live_config(&json!({}), header_auth_stale)
        .expect("prepare live config without token");
    assert!(
        keyless.contains("model_provider = \"cc-switch\"") && keyless.contains("own-key"),
        "self-authenticating tables must keep their route; got:\n{keyless}"
    );
    assert!(
            keyless.contains("name = \"Custom\""),
            "a missing name must be backfilled — 0.149 rejects the whole config otherwise; got:\n{keyless}"
        );

    // Keyless with no credentials at all (requires_openai_auth defaults
    // to false): follow — 0.149 resolves such a table unauthenticated
    // and never reads auth.json, so the local/relay route is kept. A
    // stale `wire_api = "chat"` is normalized: 0.149 removed the chat
    // wire API and rejects the whole config on any non-"responses"
    // value.
    let unauthenticated_stale = r#"model_provider = "openai"

[model_providers.openai]
name = "Local Ollama"
base_url = "http://127.0.0.1:11434/v1"
wire_api = "chat"
"#;
    let local = prepare_codex_provider_live_config(&json!({}), unauthenticated_stale)
        .expect("prepare live config without token");
    assert!(
        local.contains("model_provider = \"cc-switch\"")
            && local.contains("wire_api = \"responses\"")
            && !local.contains("wire_api = \"chat\""),
        "unauthenticated tables keep their route and chat wire_api is normalized; got:\n{local}"
    );

    // Keyless but the table carries its own scoped token: follow —
    // `experimental_bearer_token` is second in the 0.149 short-circuit
    // chain, the table authenticates itself.
    let scoped_token_stale = r#"model_provider = "openai"

[model_providers.openai]
name = "Relay"
base_url = "https://relay.example/v1"
experimental_bearer_token = "own-scoped-token"
"#;
    let scoped = prepare_codex_provider_live_config(&json!({}), scoped_token_stale)
        .expect("prepare live config without token");
    assert!(
        scoped.contains("model_provider = \"cc-switch\"") && scoped.contains("own-scoped-token"),
        "tables with a scoped token must keep their route; got:\n{scoped}"
    );

    // Keyless with no usable credentials (requires_openai_auth only):
    // never follow — the route snaps back to the built-in provider so a
    // requires_openai_auth fallback cannot resolve auth.json against a
    // stale third-party address.
    let fallback_stale = r#"model_provider = "openai"

[model_providers.openai]
base_url = "https://relay.example/v1"
requires_openai_auth = true
"#;
    let snapped = prepare_codex_provider_live_config(&json!({}), fallback_stale)
        .expect("prepare live config without token");
    assert!(
        snapped.contains("model_provider = \"openai\"")
            && !snapped.contains("[model_providers.openai]")
            && snapped.contains("[model_providers.cc-switch]"),
        "credential-less tables are renamed but the route snaps back; got:\n{snapped}"
    );

    // Official context never follows, even with credentials in the table.
    let official = migrate_stale_reserved_provider_tables(header_auth_stale, true, true)
        .expect("migrate")
        .expect("stale table must still be renamed");
    assert!(
        official.contains("model_provider = \"openai\"")
            && official.contains("[model_providers.cc-switch]"),
        "official routes never follow a renamed table; got:\n{official}"
    );

    // All three reserved ids are migrated; inactive ones never retarget
    // the route.
    let multi_stale = r#"model_provider = "third"

[model_providers.third]
base_url = "https://third.example/v1"

[model_providers.ollama]
base_url = "http://127.0.0.1:11434/v1"

[model_providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
"#;
    let cleaned = migrate_stale_reserved_provider_tables(multi_stale, false, true)
        .expect("migrate")
        .expect("stale tables must be renamed");
    assert!(
        !cleaned.contains("[model_providers.ollama]")
            && !cleaned.contains("[model_providers.lmstudio]")
            && cleaned.contains("[model_providers.cc-switch]")
            && cleaned.contains("[model_providers.cc-switch-2]")
            && cleaned.contains("model_provider = \"third\""),
        "every reserved table is renamed, the active route stays; got:\n{cleaned}"
    );

    // Upstream reserved-id validation is case-sensitive: `OpenAI` is a
    // legitimate custom id and must not be touched.
    let custom_case_variant = r#"model_provider = "OpenAI"

[model_providers.OpenAI]
base_url = "https://mine.example/v1"
"#;
    assert!(
        migrate_stale_reserved_provider_tables(custom_case_variant, false, true)
            .expect("migrate")
            .is_none(),
        "case-variant custom ids are not stale residue"
    );

    // Nothing to migrate -> no rewrite.
    assert!(
        migrate_stale_reserved_provider_tables("model = \"gpt-5\"\n", false, true)
            .expect("migrate")
            .is_none()
    );
}

#[test]
fn case_variant_reserved_ids_are_custom_providers() {
    // Upstream's built-in lookup and reserved-id validation are both
    // case-sensitive, so [model_providers.OpenAI] is a legitimate custom
    // provider — the token must land inside its table, not in a dead
    // top-level field.
    let case_variant = r#"model_provider = "OpenAI"
model = "gpt-5.4"

[model_providers.OpenAI]
name = "Mine"
base_url = "https://mine.example/v1"
wire_api = "responses"
"#;
    assert!(is_custom_codex_model_provider_id("OpenAI"));
    assert!(is_custom_codex_model_provider_id("Ollama"));
    assert!(!is_custom_codex_model_provider_id("openai"));
    // `oss` / `ollama-chat` are NOT reserved on 0.148/0.149 — both load
    // as ordinary custom tables, so the token must reach them too.
    assert!(is_custom_codex_model_provider_id("oss"));
    assert!(is_custom_codex_model_provider_id("ollama-chat"));

    let legacy_alias = r#"model_provider = "oss"

[model_providers.oss]
name = "My OSS Relay"
base_url = "https://oss.example/v1"
"#;
    let alias_prepared =
        prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-oss"}), legacy_alias)
            .expect("prepare live config");
    let alias_parsed: toml::Value = toml::from_str(&alias_prepared).expect("parse output");
    assert!(
        alias_parsed
            .get("model_providers")
            .and_then(|mp| mp.get("oss"))
            .and_then(|t| t.get("experimental_bearer_token"))
            .is_some(),
        "the token must land inside the oss custom table; got:\n{alias_prepared}"
    );
    assert!(
        alias_parsed.get("experimental_bearer_token").is_none(),
        "no dead top-level token for legacy-alias ids; got:\n{alias_prepared}"
    );

    let prepared =
        prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), case_variant)
            .expect("prepare live config");
    assert!(
        prepared.contains("[model_providers.OpenAI]"),
        "the custom table must survive; got:\n{prepared}"
    );
    assert_eq!(
        extract_codex_experimental_bearer_token(&prepared).as_deref(),
        Some("sk-test"),
    );
    let parsed: toml::Value = toml::from_str(&prepared).expect("parse output");
    assert!(
        parsed
            .get("model_providers")
            .and_then(|mp| mp.get("OpenAI"))
            .and_then(|t| t.get("experimental_bearer_token"))
            .is_some(),
        "the token must land inside the case-variant custom table; got:\n{prepared}"
    );
    assert!(
        parsed.get("experimental_bearer_token").is_none(),
        "no dead top-level token; got:\n{prepared}"
    );
}

#[test]
fn legacy_reroute_normalization_handles_inline_model_providers() {
    // Proxy backup/restore call prepare without the safety gates, so an
    // inline `model_providers = { … }` next to a legacy reroute must be
    // migrated too — skipping it would leave the key in a dead top-level
    // field beside live auth.json credentials.
    let inline_shape = r#"model_provider = "openai"
openai_base_url = "https://relay.example/v1"
model_providers = { mine = { name = "Mine", base_url = "https://mine.example/v1" } }
"#;
    let prepared =
        prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), inline_shape)
            .expect("prepare live config");
    assert!(
        !prepared.contains("openai_base_url"),
        "the reroute must be rewritten away; got:\n{prepared}"
    );
    assert!(
        prepared.contains("model_provider = \"cc-switch\"") && prepared.contains("cc-switch = {"),
        "migration must add an inline member matching the container style; got:\n{prepared}"
    );
    assert!(
        prepared.contains("mine = {") && prepared.contains("https://mine.example/v1"),
        "the user's inline table must survive untouched; got:\n{prepared}"
    );
    assert_eq!(
        extract_codex_experimental_bearer_token(&prepared).as_deref(),
        Some("sk-test"),
        "the key must resolve for the migrated inline provider"
    );
}

#[test]
fn update_toml_field_refuses_other_reserved_built_in_ids() {
    // ollama/lmstudio have no top-level reroute knob and Codex 0.148+
    // rejects any [model_providers.ollama]/[model_providers.lmstudio]
    // table at load — refusing beats writing a config Codex cannot start
    // with.
    for reserved in ["ollama", "lmstudio"] {
        let input = format!("model_provider = \"{reserved}\"\n");
        let err = update_codex_toml_field(&input, "base_url", "http://127.0.0.1:5000/v1")
            .expect_err("reserved id must be refused");
        assert!(
            err.contains(reserved),
            "the error must name the offending id; got: {err}"
        );
    }

    // Case variants are legitimate custom ids upstream — they keep the
    // normal custom-table path.
    let output = update_codex_toml_field(
        "model_provider = \"Ollama\"\n",
        "base_url",
        "http://127.0.0.1:5000/v1",
    )
    .expect("case-variant custom id must stay writable");
    assert!(
        output.contains("[model_providers.Ollama]"),
        "case variants take the custom table path; got:\n{output}"
    );
}

#[test]
fn update_toml_field_backfills_provider_name() {
    // 0.149 rejects the whole config when any non-bedrock provider table
    // has an empty/missing `name` — and this function historically
    // created exactly such tables. Creating or touching a table must
    // leave it loadable.
    let created = update_codex_toml_field(
        "model_provider = \"myrelay\"\n",
        "base_url",
        "https://relay.example/v1",
    )
    .expect("update");
    assert!(
        created.contains("name = \"myrelay\""),
        "a newly created table must get a non-empty name; got:\n{created}"
    );

    let existing_nameless = r#"model_provider = "myrelay"

[model_providers.myrelay]
base_url = "https://old.example/v1"
"#;
    let touched = update_codex_toml_field(existing_nameless, "base_url", "https://new.example/v1")
        .expect("update");
    assert!(
        touched.contains("name = \"myrelay\""),
        "touching a name-less table must backfill the name; got:\n{touched}"
    );

    let existing_named = r#"model_provider = "myrelay"

[model_providers.myrelay]
name = "My Relay"
base_url = "https://old.example/v1"
"#;
    let kept = update_codex_toml_field(existing_named, "base_url", "https://new.example/v1")
        .expect("update");
    assert!(
        kept.contains("name = \"My Relay\"") && !kept.contains("name = \"myrelay\""),
        "an existing name must never be overwritten; got:\n{kept}"
    );
}

#[test]
fn update_toml_field_leaves_bedrock_tables_nameless() {
    // 0.149 lets the Bedrock built-ins override only
    // base_url/auth/http_headers/aws.*; any other non-default field —
    // `name` included — fails the built-in merge for the whole config.
    // The proxy takeover rewrites base_url + wire_api through this
    // function, so the name backfill must skip both reserved ids.
    // (wire_api survives because "responses" is the only value 0.149
    // deserializes, which equals the default.)
    for id in ["amazon-bedrock", "amazon-bedrock-runtime"] {
        let input = format!(
                "model_provider = \"{id}\"\n\n[model_providers.{id}]\nbase_url = \"https://bedrock.example/v1\"\n"
            );
        let rerouted = update_codex_toml_field(&input, "base_url", "http://127.0.0.1:5000/v1")
            .expect("update base_url");
        let rerouted =
            update_codex_toml_field(&rerouted, "wire_api", "responses").expect("update wire_api");
        assert!(
            rerouted.contains("base_url = \"http://127.0.0.1:5000/v1\"")
                && rerouted.contains("wire_api = \"responses\""),
            "the takeover overrides must land in the table; got:\n{rerouted}"
        );
        assert!(
            !rerouted.contains("name ="),
            "bedrock tables must never receive a name; got:\n{rerouted}"
        );
    }
}

#[test]
fn prepare_normalizes_legacy_reroute_for_every_caller() {
    // prepare_codex_provider_live_config is the single normalize→inject
    // entry point — takeover backup rebuilds and restore call it directly,
    // so the legacy shape must be migrated here, not only in the switch
    // path's plan.
    let legacy = r#"model_provider = "openai"
model = "gpt-5.4"
openai_base_url = "https://relay.example/v1"
"#;
    let prepared =
        prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), legacy)
            .expect("prepare live config");
    assert!(
        !prepared.contains("openai_base_url") && prepared.contains("[model_providers.cc-switch]"),
        "prepare must rewrite the legacy reroute shape; got:\n{prepared}"
    );
    assert_eq!(
        extract_codex_experimental_bearer_token(&prepared).as_deref(),
        Some("sk-test"),
        "the key must land in the rewritten provider table"
    );
    // No token → nothing to protect, the shape passes through untouched.
    let untouched = prepare_codex_provider_live_config(&json!({}), legacy)
        .expect("prepare live config without token");
    assert_eq!(untouched, legacy);
}

#[test]
fn prepare_backfills_names_on_plain_custom_tables() {
    // 0.149 rejects the whole config over any name-less custom table,
    // active or not — plain config-only switches never go through the
    // update path, so prepare itself must normalize. Bedrock tables are
    // the mirror image: adding `name` there fails the built-in merge,
    // so the reserved ids must stay untouched.
    let config = r#"model_provider = "myrelay"

[model_providers.myrelay]
base_url = "https://relay.example/v1"

[model_providers.idle]
base_url = "https://idle.example/v1"

[model_providers.amazon-bedrock]
base_url = "https://bedrock.example/v1"
"#;
    let prepared =
        prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), config)
            .expect("prepare live config");
    assert!(
        prepared.contains("name = \"myrelay\"") && prepared.contains("name = \"idle\""),
        "custom tables (active or not) must get a non-empty name; got:\n{prepared}"
    );
    assert!(
        !prepared.contains("name = \"amazon-bedrock\""),
        "bedrock tables must never receive a name; got:\n{prepared}"
    );

    // The keyless path (official cards, key-less providers) writes the
    // same file and must normalize too.
    let keyless = prepare_codex_provider_live_config(&json!({}), config)
        .expect("prepare live config without token");
    assert!(
        keyless.contains("name = \"myrelay\"") && keyless.contains("name = \"idle\""),
        "the keyless path must backfill names too; got:\n{keyless}"
    );
}

#[test]
fn official_plan_backfills_custom_table_names() {
    // The official branch of plan_codex_live_write never goes through
    // prepare_codex_provider_live_config, so it must normalize name-less
    // custom tables itself — 0.149 validates every table at load, and an
    // official config can carry idle leftovers from older versions.
    let config = r#"model = "gpt-5.4"

[model_providers.idle]
base_url = "https://idle.example/v1"

[model_providers.amazon-bedrock]
base_url = "https://bedrock.example/v1"
"#;
    let plan = plan_codex_live_write(Some("official"), &json!({}), Some(config), false)
        .expect("official plan");
    let written = plan.config_text.expect("official plan carries config");
    assert!(
        written.contains("name = \"idle\""),
        "the official write must backfill idle custom-table names; got:\n{written}"
    );
    assert!(
        !written.contains("name = \"amazon-bedrock\""),
        "bedrock tables must never receive a name; got:\n{written}"
    );
}

#[test]
fn third_party_plan_stamps_requires_openai_auth_to_match_preservation() {
    // Presets and the custom template shipped `requires_openai_auth =
    // true` from the pre-0.149 era (auth.json carried the third-party
    // key back then). On 0.149 the injected bearer decides request auth
    // either way, but the flag drives the login UX: true with auth.json
    // deleted (preservation off) traps the TUI in the login screen,
    // false next to a preserved login hides the official account and
    // lets its tokens go stale. The plan overrides the stored value
    // with the preservation setting.
    let auth = json!({"OPENAI_API_KEY": "sk-test"});
    let stale_true = "model_provider = \"relay\"\n\n[model_providers.relay]\nname = \"Relay\"\nbase_url = \"https://relay.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n";

    let off = plan_codex_live_write(None, &auth, Some(stale_true), false)
        .expect("third-party plan with preservation off");
    let off_text = off.config_text.expect("plan carries config");
    assert!(
        off_text.contains("requires_openai_auth = false")
            && !off_text.contains("requires_openai_auth = true"),
        "preservation off must stamp the stale flag to false; got:\n{off_text}"
    );
    assert!(
        off_text.contains("experimental_bearer_token = \"sk-test\""),
        "the bearer injection must be unaffected; got:\n{off_text}"
    );
    assert!(off.remove_auth_file, "preservation off deletes auth.json");

    let on = plan_codex_live_write(None, &auth, Some(stale_true), true)
        .expect("third-party plan with preservation on");
    let on_text = on.config_text.expect("plan carries config");
    assert!(
        on_text.contains("requires_openai_auth = true"),
        "preservation on must keep/stamp the flag true; got:\n{on_text}"
    );
    assert!(!on.remove_auth_file, "preservation on keeps auth.json");

    // A card that never carried the flag gets it stamped too — the
    // preserved login stays visible to Codex (account state + token
    // refresh) only through requires_openai_auth = true.
    let flagless = "model_provider = \"relay\"\n\n[model_providers.relay]\nname = \"Relay\"\nbase_url = \"https://relay.example/v1\"\nwire_api = \"responses\"\n";
    let on_flagless = plan_codex_live_write(None, &auth, Some(flagless), true)
        .expect("third-party plan for a flagless card");
    let on_flagless_text = on_flagless.config_text.expect("plan carries config");
    assert!(
        on_flagless_text.contains("requires_openai_auth = true"),
        "preservation on must stamp flagless cards; got:\n{on_flagless_text}"
    );
}

#[test]
fn requires_openai_auth_stamp_only_touches_tables_with_a_request_auth_short_circuit() {
    // Keyless header-auth card: no env_key / bearer short-circuit, so
    // stamping true would route request auth to the preserved OAuth
    // login (applied after provider headers — the leak the gates
    // refuse). It must keep its user-authored shape under both
    // settings; 0.149 resolves it as unauthenticated and the static
    // header survives.
    let header_auth = "model_provider = \"hdr\"\n\n[model_providers.hdr]\nname = \"Header\"\nbase_url = \"https://hdr.example/v1\"\nwire_api = \"responses\"\nhttp_headers = { Authorization = \"Bearer sk-static\" }\n";
    for preserve in [false, true] {
        let plan = plan_codex_live_write(None, &json!({}), Some(header_auth), preserve)
            .expect("keyless header-auth plan");
        let text = plan.config_text.expect("plan carries config");
        assert!(
            !text.contains("requires_openai_auth"),
            "header-auth cards must not be stamped (preserve={preserve}); got:\n{text}"
        );
    }

    // env_key short-circuits request auth on 0.149 just like the
    // bearer, so the stamp applies: a stale true would otherwise trap
    // the TUI in the login screen once auth.json is deleted.
    let env_key = "model_provider = \"envd\"\n\n[model_providers.envd]\nname = \"EnvKey\"\nbase_url = \"https://envd.example/v1\"\nwire_api = \"responses\"\nenv_key = \"MY_KEY\"\nrequires_openai_auth = true\n";
    let plan = plan_codex_live_write(None, &json!({}), Some(env_key), false)
        .expect("env_key plan with preservation off");
    let text = plan.config_text.expect("plan carries config");
    assert!(
        text.contains("requires_openai_auth = false"),
        "env_key cards must be stamped like bearer cards; got:\n{text}"
    );
}

#[test]
fn openai_account_material_mirrors_codex_account_probe() {
    assert!(codex_auth_has_openai_account_material(&json!({
        "OPENAI_API_KEY": "sk-test"
    })));
    assert!(codex_auth_has_openai_account_material(&json!({
        "auth_mode": "chatgpt",
        "tokens": { "access_token": "acc" }
    })));
    assert!(codex_auth_has_openai_account_material(&json!({
        "personal_access_token": "pat"
    })));
    // Bedrock credentials make account_state() fail on a
    // requires_openai_auth provider, so they must not count as a login.
    assert!(!codex_auth_has_openai_account_material(&json!({
        "bedrock_api_key": "bedrock"
    })));
    assert!(!codex_auth_has_openai_account_material(&json!({
        "last_refresh": "2026-09-01T00:00:00Z",
        "tokens": { "account_id": "acct" }
    })));
    assert!(!codex_auth_has_openai_account_material(&json!({
        "OPENAI_API_KEY": "   "
    })));

    // Precedence mirrors AuthDotJson::resolved_mode: an implicit Bedrock
    // credential outranks a leftover OPENAI_API_KEY, so the pair is still
    // Bedrock and must not be promoted to an OpenAI login.
    assert!(!codex_auth_has_openai_account_material(&json!({
        "OPENAI_API_KEY": "sk-stale",
        "bedrock_api_key": "bedrock"
    })));
    assert!(!codex_auth_has_openai_account_material(&json!({
        "OPENAI_API_KEY": "sk-stale",
        "bedrock_access_keys": { "access_key_id": "a", "secret_access_key": "s" }
    })));
    // An explicit auth_mode wins outright, in both directions.
    assert!(!codex_auth_has_openai_account_material(&json!({
        "auth_mode": "bedrockApiKey",
        "OPENAI_API_KEY": "sk-stale",
        "bedrock_api_key": "bedrock"
    })));
    assert!(codex_auth_has_openai_account_material(&json!({
        "auth_mode": "apikey",
        "OPENAI_API_KEY": "sk-live",
        "bedrock_api_key": "bedrock"
    })));
    // personal_access_token outranks the API key even when blank: Codex
    // then attempts PAT auth with nothing and ends up signed out.
    assert!(!codex_auth_has_openai_account_material(&json!({
        "personal_access_token": "",
        "OPENAI_API_KEY": "sk-live"
    })));
    // agent_identity only counts under an explicit mode; implicitly the
    // payload resolves to ChatGPT, which has no tokens here.
    assert!(!codex_auth_has_openai_account_material(&json!({
        "agent_identity": "jwt"
    })));
    assert!(codex_auth_has_openai_account_material(&json!({
        "auth_mode": "agentIdentity",
        "agent_identity": "jwt"
    })));
    // `auth_mode: null` is absent to serde; a string it rejects fails the
    // whole load (exact-match, so casing matters); headers auth cannot be
    // loaded from storage at all.
    assert!(codex_auth_has_openai_account_material(&json!({
        "auth_mode": null,
        "OPENAI_API_KEY": "sk-live"
    })));
    assert!(!codex_auth_has_openai_account_material(&json!({
        "auth_mode": "ApiKey",
        "OPENAI_API_KEY": "sk-live"
    })));
    assert!(!codex_auth_has_openai_account_material(&json!({
        "auth_mode": "headers",
        "OPENAI_API_KEY": "sk-live"
    })));
}

#[test]
fn auth_store_mode_reads_top_level_cli_auth_credentials_store() {
    assert_eq!(
        codex_config_auth_store_mode("model = \"gpt-5\"\n"),
        CodexAuthStoreMode::File
    );
    assert_eq!(
        codex_config_auth_store_mode("cli_auth_credentials_store = \"file\"\n"),
        CodexAuthStoreMode::File
    );
    assert_eq!(
        codex_config_auth_store_mode("cli_auth_credentials_store = \"keyring\"\n"),
        CodexAuthStoreMode::Keyring
    );
    assert_eq!(
        codex_config_auth_store_mode("cli_auth_credentials_store = \"auto\"\n"),
        CodexAuthStoreMode::Auto
    );
    assert_eq!(
        codex_config_auth_store_mode("cli_auth_credentials_store = \"ephemeral\"\n"),
        CodexAuthStoreMode::Ephemeral
    );
    // Codex's serde is lowercase-only; anything else fails its load.
    assert_eq!(
        codex_config_auth_store_mode("cli_auth_credentials_store = \"Keyring\"\n"),
        CodexAuthStoreMode::Unknown
    );
    // Only the top-level key counts.
    assert_eq!(
        codex_config_auth_store_mode(
            "[model_providers.x]\ncli_auth_credentials_store = \"keyring\"\n"
        ),
        CodexAuthStoreMode::File
    );
}

#[test]
fn requires_openai_auth_stamp_is_a_noop_when_already_aligned() {
    let aligned = "model_provider = \"relay\"\n\n[model_providers.relay]\nname = \"Relay\"\nbase_url = \"https://relay.example/v1\"\nexperimental_bearer_token = \"sk-test\"\nrequires_openai_auth = false\n";
    let output =
        align_codex_requires_openai_auth_with_login_preservation(aligned, false).expect("align");
    assert_eq!(
        output, aligned,
        "an aligned config must pass through untouched"
    );

    // No custom-table route → nothing to stamp.
    let no_route = "model = \"gpt-5.6\"\n";
    let output =
        align_codex_requires_openai_auth_with_login_preservation(no_route, true).expect("align");
    assert_eq!(output, no_route);
}

#[test]
fn preflight_rejects_provider_table_conflicts_codex_refuses_to_load() {
    // 0.149 validates EVERY provider table (idle ones included) and
    // rejects: aws outside the Bedrock built-ins, and auth combined with
    // requires_openai_auth / env_key / experimental_bearer_token. These
    // can't be normalized away, so the switch must refuse up front —
    // with or without a carried key, official or third-party.
    let with_key = json!({"OPENAI_API_KEY": "sk-test"});
    let rejected = [
            // bare aws on a custom table, no requires_openai_auth anywhere
            "model_provider = \"custom\"\n\n[model_providers.custom]\nname = \"Custom\"\nbase_url = \"https://relay.example/v1\"\naws = { region = \"us-east-1\" }\n",
            // auth × requires_openai_auth — carried key skips the fallback
            // gate, so the preflight must catch it independently
            "model_provider = \"custom\"\n\n[model_providers.custom]\nname = \"Custom\"\nbase_url = \"https://relay.example/v1\"\nrequires_openai_auth = true\nauth = { command = \"my-auth\" }\n",
            // auth × env_key / experimental_bearer_token
            "model_provider = \"custom\"\n\n[model_providers.custom]\nname = \"Custom\"\nbase_url = \"https://relay.example/v1\"\nenv_key = \"MY_KEY\"\nauth = { command = \"my-auth\" }\n",
            "model_provider = \"custom\"\n\n[model_providers.custom]\nname = \"Custom\"\nbase_url = \"https://relay.example/v1\"\nexperimental_bearer_token = \"tok\"\nauth = { command = \"my-auth\" }\n",
            // an IDLE conflicting table poisons the whole config too
            "model_provider = \"active\"\n\n[model_providers.active]\nname = \"Active\"\nbase_url = \"https://relay.example/v1\"\n\n[model_providers.idle]\nname = \"Idle\"\nbase_url = \"https://idle.example/v1\"\naws = { region = \"us-east-1\" }\n",
        ] ;
    for config in rejected {
        assert!(
            preflight_codex_live_write(None, &with_key, Some(config)).is_err(),
            "third-party preflight must refuse:\n{config}"
        );
        assert!(
            preflight_codex_live_write(Some("official"), &json!({}), Some(config)).is_err(),
            "official preflight must refuse the same shapes:\n{config}"
        );
    }

    // Loadable shapes stay accepted: command-backed auth alone, and aws
    // on the Bedrock built-ins.
    let accepted = [
            "model_provider = \"custom\"\n\n[model_providers.custom]\nname = \"Custom\"\nbase_url = \"https://relay.example/v1\"\nauth = { command = \"my-auth\" }\n",
            "model_provider = \"amazon-bedrock\"\n\n[model_providers.amazon-bedrock]\nbase_url = \"https://bedrock.example/v1\"\naws = { region = \"us-east-1\" }\n",
        ];
    for config in accepted {
        assert!(
            preflight_codex_live_write(None, &with_key, Some(config)).is_ok(),
            "loadable shape must pass the preflight:\n{config}"
        );
    }
}

#[test]
fn update_toml_field_reroutes_built_in_openai_via_top_level_knob() {
    // Codex 0.149 refuses any [model_providers.openai] table outright
    // (validate_reserved_model_provider_ids), so rewriting base_url for
    // the built-in provider must use the top-level openai_base_url knob.
    let input = "model_provider = \"openai\"\nmodel = \"gpt-5.4\"\n";
    let output = update_codex_toml_field(input, "base_url", "http://127.0.0.1:5000/v1")
        .expect("update base_url");
    assert!(
        !output.contains("[model_providers.openai]") && !output.contains("model_providers"),
        "no reserved provider table may be created; got:\n{output}"
    );
    assert!(
        output.contains("openai_base_url = \"http://127.0.0.1:5000/v1\""),
        "the reroute must use the top-level knob; got:\n{output}"
    );

    // Clearing the value removes the knob again.
    let cleared = update_codex_toml_field(&output, "base_url", "").expect("clear base_url");
    assert!(!cleared.contains("openai_base_url"));

    // wire_api is fixed by the CLI for built-ins — a no-op, not a table.
    let wire = update_codex_toml_field(input, "wire_api", "responses").expect("set wire_api");
    assert!(!wire.contains("model_providers"));
}

#[test]
fn bedrock_runtime_is_a_reserved_provider_id() {
    // `amazon-bedrock-runtime` is reserved by Codex 0.149; treating it as
    // custom would inject a token into a table whose `aws` config
    // hard-conflicts with it. Reserved IDs keep the top-level fallback.
    assert!(!is_custom_codex_model_provider_id("amazon-bedrock-runtime"));

    let input = r#"model_provider = "amazon-bedrock-runtime"
"#;
    let output = prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), input)
        .expect("prepare live config");
    let parsed: toml::Value = toml::from_str(&output).expect("parse output");
    assert!(
        parsed.get("model_providers").is_none(),
        "reserved provider tables should not be synthesized"
    );
}

#[test]
fn extract_bearer_uses_top_level_token_for_reserved_provider() {
    let input = r#"model_provider = "openai"
experimental_bearer_token = "top-level-key"

[model_providers.openai]
experimental_bearer_token = "stale-table-key"
"#;

    assert_eq!(
        extract_codex_experimental_bearer_token(input).as_deref(),
        Some("top-level-key")
    );
}

#[test]
fn should_not_restore_provider_token_for_oauth_only_template() {
    let oauth_template = json!({
        "auth": {
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "oauth-access"
            }
        }
    });
    let api_key_template = json!({
        "auth": {
            "OPENAI_API_KEY": "sk-test"
        }
    });

    assert!(
        !should_restore_codex_provider_token_for_backfill(Some("custom"), &oauth_template),
        "OAuth-only templates should not backfill bearer tokens into OPENAI_API_KEY"
    );
    assert!(
        should_restore_codex_provider_token_for_backfill(Some("custom"), &api_key_template),
        "custom API-key providers should still restore provider bearer tokens"
    );
    assert!(
        !should_restore_codex_provider_token_for_backfill(Some("official"), &api_key_template),
        "official providers should never restore third-party bearer tokens"
    );
}

#[test]
fn credential_login_material_only_counts_real_credentials() {
    assert!(codex_auth_has_credential_login_material(&json!({
        "tokens": { "access_token": "t" }
    })));
    assert!(codex_auth_has_credential_login_material(&json!({
        "tokens": { "refresh_token": "r" }
    })));
    assert!(codex_auth_has_credential_login_material(&json!({
        "personal_access_token": "pat"
    })));

    // API key and pure metadata are not credentials in this predicate's
    // sense — they must not shield a stale key from cleanup.
    assert!(!codex_auth_has_credential_login_material(&json!({
        "OPENAI_API_KEY": "sk-x"
    })));
    assert!(!codex_auth_has_credential_login_material(&json!({
        "OPENAI_API_KEY": "sk-x",
        "last_refresh": "2026-01-01T00:00:00Z",
        "tokens": { "account_id": "acct-meta-only" }
    })));
    assert!(!codex_auth_has_credential_login_material(&json!({})));
}

#[test]
fn stale_third_party_residue_detection() {
    // Shapes a preserve-off third-party switch leaves behind: cleared.
    assert!(codex_live_auth_is_stale_third_party_residue(&json!({
        "OPENAI_API_KEY": "sk-third-party"
    })));
    assert!(codex_live_auth_is_stale_third_party_residue(&json!({
        "auth_mode": "apikey",
        "OPENAI_API_KEY": "sk-third-party"
    })));
    assert!(codex_live_auth_is_stale_third_party_residue(&json!({
        "OPENAI_API_KEY": "sk-third-party",
        "last_refresh": "2026-01-01T00:00:00Z",
        "tokens": { "account_id": "acct-meta-only" }
    })));

    // Anything carrying a real credential must survive untouched.
    assert!(!codex_live_auth_is_stale_third_party_residue(&json!({
        "OPENAI_API_KEY": "sk-x",
        "tokens": { "access_token": "t" }
    })));
    assert!(!codex_live_auth_is_stale_third_party_residue(&json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": { "access_token": "official-oauth-token" }
    })));

    // Nothing to clear.
    assert!(!codex_live_auth_is_stale_third_party_residue(&json!({})));
    assert!(!codex_live_auth_is_stale_third_party_residue(&json!({
        "OPENAI_API_KEY": ""
    })));
}

#[test]
fn prepare_provider_live_config_does_not_create_incomplete_provider_table() {
    let input = r#"model_provider = "vendor_x"
model = "gpt-5"
"#;

    let output = prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), input)
        .expect("prepare live config");
    let parsed: toml::Value = toml::from_str(&output).expect("parse output");

    assert_eq!(
        parsed
            .get("experimental_bearer_token")
            .and_then(|v| v.as_str()),
        Some("sk-test")
    );
    assert!(
        parsed.get("model_providers").is_none(),
        "missing provider tables should not be synthesized without endpoint fields"
    );
}

#[test]
fn prepare_provider_live_config_preserves_custom_provider_id() {
    let input = r#"model_provider = "vendor_alpha"
model = "gpt-5.4"
profile = "work"

[model_providers.vendor_alpha]
name = "Vendor Alpha"
base_url = "https://alpha.example/v1"
wire_api = "responses"

[profiles.work]
model_provider = "vendor_alpha"
model = "gpt-5.4"
"#;

    let result = prepare_codex_provider_live_config(&json!({"OPENAI_API_KEY": "sk-test"}), input)
        .expect("prepare live config");
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    assert_eq!(
        parsed.get("model_provider").and_then(|v| v.as_str()),
        Some("vendor_alpha")
    );
    assert!(
        parsed
            .get("model_providers")
            .and_then(|v| v.get("custom"))
            .is_none(),
        "provider writes should not force custom provider ids"
    );
    assert_eq!(
        parsed
            .get("model_providers")
            .and_then(|v| v.get("vendor_alpha"))
            .and_then(|v| v.get("experimental_bearer_token"))
            .and_then(|v| v.as_str()),
        Some("sk-test")
    );
    assert_eq!(
        parsed
            .get("profiles")
            .and_then(|v| v.get("work"))
            .and_then(|v| v.get("model_provider"))
            .and_then(|v| v.as_str()),
        Some("vendor_alpha"),
        "profile provider references should be preserved"
    );
}

#[test]
fn backfill_preserves_live_model_provider_id() {
    let mut live_settings = json!({
        "auth": {},
        "config": r#"model_provider = "vendor_beta"

[model_providers.vendor_beta]
name = "Vendor Beta"
base_url = "https://beta.example/v1"
wire_api = "responses"
"#,
    });
    let template_settings = json!({
        "auth": {},
        "config": r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "https://custom.example/v1"
wire_api = "responses"
"#,
    });

    restore_codex_settings_for_backfill(&mut live_settings, &template_settings, false).unwrap();
    let config = live_settings.get("config").and_then(Value::as_str).unwrap();
    let parsed: toml::Value = toml::from_str(config).unwrap();

    assert_eq!(
        parsed.get("model_provider").and_then(|v| v.as_str()),
        Some("vendor_beta")
    );
    assert!(
        parsed
            .get("model_providers")
            .and_then(|v| v.get("vendor_beta"))
            .is_some(),
        "backfill should not rewrite user-selected provider tables"
    );
}

#[test]
fn base_url_writes_into_correct_model_provider_section() {
    let input = r#"model_provider = "any"
model = "gpt-5.1-codex"

[model_providers.any]
name = "any"
wire_api = "responses"
"#;

    let result = update_codex_toml_field(input, "base_url", "https://example.com/v1").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    let base_url = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .and_then(|v| v.get("base_url"))
        .and_then(|v| v.as_str())
        .expect("base_url should be in model_providers.any");
    assert_eq!(base_url, "https://example.com/v1");

    // Should NOT have top-level base_url
    assert!(parsed.get("base_url").is_none());

    // wire_api preserved
    let wire_api = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .and_then(|v| v.get("wire_api"))
        .and_then(|v| v.as_str());
    assert_eq!(wire_api, Some("responses"));
}

#[test]
fn wire_api_writes_into_correct_model_provider_section() {
    let input = r#"model_provider = "chat_only"
model = "gpt-5.1-codex"

[model_providers.chat_only]
name = "Chat Only"
base_url = "https://example.com/v1"
wire_api = "chat"
"#;

    let result = update_codex_toml_field(input, "wire_api", "responses").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    let provider = parsed
        .get("model_providers")
        .and_then(|v| v.get("chat_only"))
        .expect("model_providers.chat_only should exist");

    assert_eq!(
        provider.get("wire_api").and_then(|v| v.as_str()),
        Some("responses")
    );
    assert_eq!(
        provider.get("base_url").and_then(|v| v.as_str()),
        Some("https://example.com/v1")
    );
    assert!(parsed.get("wire_api").is_none());
}

#[test]
fn base_url_creates_section_when_missing() {
    let input = r#"model_provider = "custom"
model = "gpt-4"
"#;

    let result = update_codex_toml_field(input, "base_url", "https://custom.api/v1").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    let base_url = parsed
        .get("model_providers")
        .and_then(|v| v.get("custom"))
        .and_then(|v| v.get("base_url"))
        .and_then(|v| v.as_str())
        .expect("should create section and set base_url");
    assert_eq!(base_url, "https://custom.api/v1");
}

#[test]
fn base_url_uses_openai_override_without_model_provider() {
    let input = r#"model = "gpt-4"
"#;

    let result = update_codex_toml_field(input, "base_url", "https://fallback.api/v1").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    let base_url = parsed
        .get("openai_base_url")
        .and_then(|v| v.as_str())
        .expect("should set the built-in provider's URL override");
    assert_eq!(base_url, "https://fallback.api/v1");
    assert!(parsed.get("base_url").is_none());
    let responses = update_codex_toml_field(&result, "wire_api", "responses").unwrap();
    assert_eq!(responses, result);
    let cleared = update_codex_toml_field(&result, "base_url", "").unwrap();
    assert_eq!(
        toml::from_str::<toml::Value>(&cleared).unwrap(),
        toml::from_str::<toml::Value>(input).unwrap()
    );
}

#[test]
fn base_url_writes_into_inline_table_provider_section() {
    // inline table 是合法 TOML，但 as_table_mut() 对它返回 None。旧代码会因此
    // 掉进「写顶层字段」的 fallback：用户改的 base_url 落在错误层级，
    // Codex 读不到，且界面毫无提示。
    let input = r#"model_provider = "any"
model_providers = { any = { name = "any", base_url = "https://old.api/v1", wire_api = "responses" } }
"#;

    let result = update_codex_toml_field(input, "base_url", "https://new.api/v1").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    assert_eq!(
        parsed["model_providers"]["any"]["base_url"].as_str(),
        Some("https://new.api/v1"),
        "must update the provider section, not a top-level field"
    );
    assert!(
        parsed.get("base_url").is_none(),
        "must not leak a top-level base_url fallback"
    );
    assert_eq!(
        parsed["model_providers"]["any"]["wire_api"].as_str(),
        Some("responses"),
        "sibling fields must survive"
    );
}

#[test]
fn clearing_base_url_removes_only_from_correct_section() {
    let input = r#"model_provider = "any"

[model_providers.any]
name = "any"
base_url = "https://old.api/v1"
wire_api = "responses"

[mcp_servers.context7]
command = "npx"
"#;

    let result = update_codex_toml_field(input, "base_url", "").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    // base_url removed from model_providers.any
    let any_section = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .expect("model_providers.any should exist");
    assert!(any_section.get("base_url").is_none());

    // wire_api preserved
    assert_eq!(
        any_section.get("wire_api").and_then(|v| v.as_str()),
        Some("responses")
    );

    // mcp_servers untouched
    assert!(parsed.get("mcp_servers").is_some());
}

#[test]
fn model_field_operates_on_top_level() {
    let input = r#"model_provider = "any"
model = "gpt-4"

[model_providers.any]
name = "any"
"#;

    let result = update_codex_toml_field(input, "model", "gpt-5").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(parsed.get("model").and_then(|v| v.as_str()), Some("gpt-5"));

    // Clear model
    let result2 = update_codex_toml_field(&result, "model", "").unwrap();
    let parsed2: toml::Value = toml::from_str(&result2).unwrap();
    assert!(parsed2.get("model").is_none());
}

#[test]
fn preserves_comments_and_whitespace() {
    let input = r#"# My Codex config
model_provider = "any"
model = "gpt-4"

# Provider section
[model_providers.any]
name = "any"
base_url = "https://old.api/v1"
"#;

    let result = update_codex_toml_field(input, "base_url", "https://new.api/v1").unwrap();

    // Comments should be preserved
    assert!(result.contains("# My Codex config"));
    assert!(result.contains("# Provider section"));
}

#[test]
fn does_not_misplace_when_profiles_section_follows() {
    let input = r#"model_provider = "any"

[model_providers.any]
name = "any"
base_url = "https://old.api/v1"

[profiles.default]
model = "gpt-4"
"#;

    let result = update_codex_toml_field(input, "base_url", "https://new.api/v1").unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    // base_url in correct section
    let base_url = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .and_then(|v| v.get("base_url"))
        .and_then(|v| v.as_str());
    assert_eq!(base_url, Some("https://new.api/v1"));

    // profiles section untouched
    let profile_model = parsed
        .get("profiles")
        .and_then(|v| v.get("default"))
        .and_then(|v| v.get("model"))
        .and_then(|v| v.as_str());
    assert_eq!(profile_model, Some("gpt-4"));
}

#[test]
fn remove_base_url_if_predicate() {
    let input = r#"model_provider = "any"

[model_providers.any]
name = "any"
base_url = "http://127.0.0.1:5000/v1"
wire_api = "responses"
"#;

    let result = remove_codex_toml_base_url_if(input, |url| url.starts_with("http://127.0.0.1"));
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    let any_section = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .unwrap();
    assert!(any_section.get("base_url").is_none());
    assert_eq!(
        any_section.get("wire_api").and_then(|v| v.as_str()),
        Some("responses")
    );
}

#[test]
fn remove_base_url_if_keeps_non_matching() {
    let input = r#"model_provider = "any"

[model_providers.any]
base_url = "https://production.api/v1"
"#;

    let result = remove_codex_toml_base_url_if(input, |url| url.starts_with("http://127.0.0.1"));
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    let base_url = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .and_then(|v| v.get("base_url"))
        .and_then(|v| v.as_str());
    assert_eq!(base_url, Some("https://production.api/v1"));
}

#[test]
fn dynamic_template_backfills_parser_required_fields_from_static() {
    // Simulate a template cloned from a models_cache.json written by a
    // Codex build whose ModelInfo lacks parser-side required fields such
    // as `supports_reasoning_summaries` (codex >= 0.144.5 rejects the
    // whole catalog file without it).
    let mut template = json!({
        "slug": "gpt-5.5",
        "context_window": 272_000,
        "supports_parallel_tool_calls": false
    });
    fill_template_fields_from_static(&mut template);

    assert_eq!(
        template
            .get("supports_reasoning_summaries")
            .and_then(Value::as_bool),
        Some(true)
    );
    // Keys already present in the dynamic template are never overwritten.
    assert_eq!(
        template
            .get("supports_parallel_tool_calls")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        template.get("context_window").and_then(Value::as_u64),
        Some(272_000)
    );
    // Optional capability fields must NOT be backfilled: for the catalog
    // parser "missing" means the parser default, not the static
    // template's value.
    assert!(template.get("supports_search_tool").is_none());
    assert!(template.get("supports_image_detail_original").is_none());
    assert!(template.get("web_search_tool_type").is_none());

    // A cache template missing supports_parallel_tool_calls gets the
    // static gpt-5.5 default backfilled (codex 0.148.0 rejects the
    // catalog without it, #6661).
    let mut stale = json!({ "slug": "gpt-5.5" });
    fill_template_fields_from_static(&mut stale);
    assert_eq!(
        stale
            .get("supports_parallel_tool_calls")
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn proxy_chat_catalog_entries_carry_reasoning_summaries_flag() {
    // End to end: a stale dynamic template, once backfilled, must yield
    // catalog entries codex 0.144.5+ can parse.
    let mut template = json!({ "slug": "gpt-5.5" });
    fill_template_fields_from_static(&mut template);
    let specs = vec![CodexCatalogModelSpec {
        model: "k3".to_string(),
        display_name: Some("Kimi K3".to_string()),
        context_window: Some(262_144),
        supports_parallel_tool_calls: None,
        input_modalities: None,
        base_instructions: None,
        reasoning_levels: None,
        default_reasoning_level: None,
    }];
    let catalog = codex_model_catalog_from_specs(
        &specs,
        &template,
        CodexCatalogToolProfile::ProxyChat,
        128_000,
    );
    assert_eq!(
        catalog["models"][0]
            .get("supports_reasoning_summaries")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        catalog["models"][0]
            .get("supports_parallel_tool_calls")
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn codex_model_catalog_uses_provider_models_and_context() {
    let template = json!({
        "slug": "gpt-5.5",
        "display_name": "GPT-5.5",
        "description": "Frontier model",
        "base_instructions": "gpt-5.5 base instructions",
        "model_messages": {
            "instructions_template": "gpt-5.5 instructions template",
            "instructions_variables": {
                "personality_default": "",
                "personality_friendly": "",
                "personality_pragmatic": ""
            }
        },
        "additional_speed_tiers": ["fast"],
        "service_tiers": [
            {
                "id": "priority",
                "name": "Fast",
                "description": "1.5x speed, increased usage"
            }
        ],
        "availability_nux": {
            "message": "GPT-5.5 is now available."
        },
        "upgrade": {
            "target": "gpt-5.5"
        },
        "context_window": 272000,
        "max_context_window": 272000
    });
    let settings = json!({
        "modelCatalog": {
            "models": [
                {
                    "model": "deepseek-v4-flash",
                    "displayName": "DeepSeek V4 Flash",
                    "contextWindow": "64000"
                },
                {
                    "model": "kimi-k2",
                    "display_name": "Kimi K2"
                }
            ]
        }
    });
    let specs = codex_catalog_model_specs(&settings);
    let catalog = codex_model_catalog_from_specs(
        &specs,
        &template,
        CodexCatalogToolProfile::ProxyChat,
        128_000,
    );
    let models = catalog
        .get("models")
        .and_then(|value| value.as_array())
        .expect("models should be an array");

    assert_eq!(models.len(), 2);
    assert_eq!(
        models[0].get("slug").and_then(|value| value.as_str()),
        Some("deepseek-v4-flash")
    );
    assert_eq!(
        models[0]
            .get("context_window")
            .and_then(|value| value.as_u64()),
        Some(64_000)
    );
    assert_eq!(
        models[1]
            .get("context_window")
            .and_then(|value| value.as_u64()),
        Some(128_000)
    );
    assert!(
        models[0].get("model_messages").is_some(),
        "Codex requires model_messages in custom catalogs"
    );
    assert_eq!(
        models[0]
            .get("base_instructions")
            .and_then(|value| value.as_str()),
        Some("gpt-5.5 base instructions")
    );
    assert_eq!(
        models[0].get("model_messages"),
        template.get("model_messages"),
        "custom catalog entries should keep the gpt-5.5 agent template"
    );
    assert_eq!(
        models[0].get("additional_speed_tiers"),
        Some(&json!([])),
        "generated third-party entries should not inherit OpenAI speed tiers"
    );
    assert!(
        models[0]
            .get("availability_nux")
            .is_some_and(|value| value.is_null()),
        "generated third-party entries should not inherit GPT-5.5 launch messaging"
    );
}

#[test]
fn native_responses_catalog_honors_per_model_reasoning_levels() {
    // The native template only declares none/high. A per-model
    // reasoningLevels override must replace supported_reasoning_levels and
    // pick a sensible default_reasoning_level.
    let settings = json!({
        "modelCatalog": {
            "models": [
                {
                    "model": "deepseek-v4-flash",
                    "reasoningLevels": ["none", "low", "medium", "high", "xhigh", "max"],
                    "defaultReasoningLevel": "xhigh"
                },
                {
                    "model": "no-default-model",
                    "reasoningLevels": ["low", "medium", "high"]
                },
                {
                    "model": "template-default-model",
                    "reasoningLevels": ["none", "high", "xhigh"]
                },
                {
                    "model": "dirty-levels",
                    "reasoningLevels": ["none", "bogus", "high", ""]
                },
                {
                    "model": "unordered-model",
                    "reasoningLevels": ["xhigh", "low", "bogus", "low"],
                    "defaultReasoningLevel": "bogus"
                }
            ]
        }
    });

    let catalog =
        codex_model_catalog_from_settings(&settings, "", CodexCatalogToolProfile::NativeResponses)
            .expect("catalog generation should not error")
            .expect("non-empty modelCatalog must yield a catalog");

    let models = catalog["models"].as_array().expect("models array");
    let efforts = |index: usize| -> Vec<String> {
        models[index]["supported_reasoning_levels"]
            .as_array()
            .expect("supported_reasoning_levels array")
            .iter()
            .filter_map(|level| level.get("effort").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect()
    };

    // Explicit default wins.
    assert_eq!(
        efforts(0),
        vec!["none", "low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(
        models[0]
            .get("default_reasoning_level")
            .and_then(|v| v.as_str()),
        Some("xhigh")
    );

    // No explicit default: falls back to the last (highest) declared level.
    assert_eq!(efforts(1), vec!["low", "medium", "high"]);
    assert_eq!(
        models[1]
            .get("default_reasoning_level")
            .and_then(|v| v.as_str()),
        Some("high")
    );

    // Template default ("high") is kept when it is still in the list.
    assert_eq!(efforts(2), vec!["none", "high", "xhigh"]);
    assert_eq!(
        models[2]
            .get("default_reasoning_level")
            .and_then(|v| v.as_str()),
        Some("high")
    );

    // Unknown / empty efforts are dropped; the default still resolves to
    // a supported level (the template default, "high").
    assert_eq!(efforts(3), vec!["none", "high"]);
    assert_eq!(
        models[3]
            .get("default_reasoning_level")
            .and_then(|v| v.as_str()),
        Some("high")
    );

    // Declaration order is normalized to canonical order, duplicates and
    // an unknown explicit default are dropped, and the fallback picks the
    // highest supported level in canonical order (not the last declared
    // one, and never an unknown effort).
    assert_eq!(efforts(4), vec!["low", "xhigh"]);
    assert_eq!(
        models[4]
            .get("default_reasoning_level")
            .and_then(|v| v.as_str()),
        Some("xhigh")
    );
}

#[test]
fn vendor_catalog_honors_per_model_reasoning_levels() {
    // The DeepSeek official catalog declares low/high/max; a per-model
    // override must win over the official entry.
    let settings = json!({
        "modelCatalog": {
            "models": [
                {
                    "model": "deepseek-v4-flash",
                    "reasoningLevels": ["none", "low", "medium", "high", "xhigh", "max"],
                    "defaultReasoningLevel": "xhigh"
                }
            ]
        }
    });

    let catalog = codex_model_catalog_from_settings(
        &settings,
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses,
    )
    .expect("vendor catalog generation should not error")
    .expect("non-empty modelCatalog must yield a catalog");

    let entry = &catalog["models"][0];
    let efforts: Vec<&str> = entry["supported_reasoning_levels"]
        .as_array()
        .expect("supported_reasoning_levels array")
        .iter()
        .filter_map(|level| level.get("effort").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        efforts,
        vec!["none", "low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(
        entry
            .get("default_reasoning_level")
            .and_then(|v| v.as_str()),
        Some("xhigh")
    );
}

#[test]
fn vendor_catalog_unknown_model_does_not_inherit_flagship_modalities() {
    // A vision variant not in the official DeepSeek catalog must not
    // inherit the flagship entry's text-only modalities; the registry /
    // fail-open logic should resolve it as image-capable instead.
    let settings = json!({
        "modelCatalog": {
            "models": [
                {
                    "model": "deepseek-v4-flash-vision-exp",
                    "displayName": "DeepSeek V4 Flash Vision Exp"
                }
            ]
        }
    });

    let catalog = codex_model_catalog_from_settings(
        &settings,
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses,
    )
    .expect("vendor catalog generation should not error")
    .expect("non-empty modelCatalog must yield a catalog");

    let modalities: Vec<&str> = catalog["models"][0]["input_modalities"]
        .as_array()
        .expect("input_modalities array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        modalities,
        vec!["text", "image"],
        "unknown vision model must not inherit the flagship's text-only modalities"
    );
}

#[test]
fn vendor_catalog_unknown_model_explicit_modalities_override() {
    // An explicit user inputModalities declaration must win over the
    // registry/fail-open resolution even for unmatched models.
    let settings = json!({
        "modelCatalog": {
            "models": [
                {
                    "model": "deepseek-v4-flash-vision-exp",
                    "inputModalities": ["text"]
                }
            ]
        }
    });

    let catalog = codex_model_catalog_from_settings(
        &settings,
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses,
    )
    .expect("vendor catalog generation should not error")
    .expect("non-empty modelCatalog must yield a catalog");

    let modalities: Vec<&str> = catalog["models"][0]["input_modalities"]
        .as_array()
        .expect("input_modalities array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(modalities, vec!["text"]);
}

#[test]
fn vendor_catalog_matched_model_keeps_vendor_modalities() {
    // A model that IS in the official catalog must keep the vendor's
    // declared modalities verbatim (deepseek-v4-pro is text-only there).
    let settings = json!({
        "modelCatalog": {
            "models": [
                {
                    "model": "deepseek-v4-pro",
                    "displayName": "DeepSeek V4 Pro"
                }
            ]
        }
    });

    let catalog = codex_model_catalog_from_settings(
        &settings,
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses,
    )
    .expect("vendor catalog generation should not error")
    .expect("non-empty modelCatalog must yield a catalog");

    let modalities: Vec<&str> = catalog["models"][0]["input_modalities"]
        .as_array()
        .expect("input_modalities array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(modalities, vec!["text"]);
}

#[test]
fn native_responses_profile_suppresses_apply_patch_and_keeps_shell() {
    // Native (direct) /responses providers must NOT emit a freeform
    // apply_patch (type=="custom") tool — gateways like MiMo reject it.
    // The native profile uses the bundled clean template and relies on
    // shell_type="shell_command" for edits, plus per-row overrides.
    let settings = json!({
        "modelCatalog": {
            "models": [
                {
                    "model": "MiniMax-M3",
                    "displayName": "MiniMax-M3",
                    "contextWindow": 1_000_000,
                    "supportsParallelToolCalls": true,
                    "inputModalities": ["text", "image"],
                    "baseInstructions": "You are Codex, a coding agent based on MiniMax-M3."
                }
            ]
        }
    });

    let catalog =
        codex_model_catalog_from_settings(&settings, "", CodexCatalogToolProfile::NativeResponses)
            .expect("native catalog generation should not error")
            .expect("non-empty modelCatalog must yield a catalog");

    let entry = &catalog["models"][0];
    assert_eq!(
        entry.get("slug").and_then(|v| v.as_str()),
        Some("MiniMax-M3")
    );
    assert_eq!(
        entry.get("shell_type").and_then(|v| v.as_str()),
        Some("shell_command"),
        "native entries edit via shell, not the custom apply_patch tool"
    );
    assert!(
        entry.get("apply_patch_tool_type").is_none(),
        "native entries must NOT declare a freeform apply_patch tool"
    );
    // `base_instructions` is REQUIRED by Codex's catalog parser, so it must
    // be present — and the per-row official override must win over the
    // template default.
    assert_eq!(
        entry.get("base_instructions").and_then(|v| v.as_str()),
        Some("You are Codex, a coding agent based on MiniMax-M3."),
        "per-row baseInstructions override must apply (and field must exist)"
    );
    assert!(
        entry.get("model_messages").is_none(),
        "native entries must not carry the gpt-5.5 model_messages persona text"
    );
    assert_eq!(
        entry.get("supports_parallel_tool_calls"),
        Some(&json!(true)),
        "per-row supportsParallelToolCalls override must apply"
    );
    assert_eq!(
        entry.get("input_modalities"),
        Some(&json!(["text", "image"])),
        "per-row inputModalities override must apply"
    );
    assert_eq!(
        entry.get("context_window").and_then(|v| v.as_u64()),
        Some(1_000_000)
    );
}

#[test]
fn catalog_infers_image_input_independently_of_tool_profile() {
    // Start from a deliberately text-only template to prove that every
    // profile overwrites template defaults with shared capability logic.
    let template = json!({
        "input_modalities": ["text"],
        "apply_patch_tool_type": "freeform"
    });
    let specs = vec![
        CodexCatalogModelSpec {
            model: "gpt-5.4".to_string(),
            display_name: Some("GPT 5.4".to_string()),
            context_window: Some(128_000),
            supports_parallel_tool_calls: None,
            input_modalities: None,
            base_instructions: None,
            reasoning_levels: None,
            default_reasoning_level: None,
        },
        CodexCatalogModelSpec {
            model: "qwen/qwen3-coder-plus".to_string(),
            display_name: Some("Qwen3 Coder Plus".to_string()),
            context_window: Some(128_000),
            supports_parallel_tool_calls: None,
            input_modalities: None,
            base_instructions: None,
            reasoning_levels: None,
            default_reasoning_level: None,
        },
        CodexCatalogModelSpec {
            model: "glm-5.2v".to_string(),
            display_name: Some("GLM 5.2V".to_string()),
            context_window: Some(128_000),
            supports_parallel_tool_calls: None,
            input_modalities: None,
            base_instructions: None,
            reasoning_levels: None,
            default_reasoning_level: None,
        },
        CodexCatalogModelSpec {
            model: "deepseek-v4-flash".to_string(),
            display_name: Some("Explicit Visual Override".to_string()),
            context_window: Some(128_000),
            supports_parallel_tool_calls: None,
            input_modalities: Some(vec!["text".to_string(), "image".to_string()]),
            base_instructions: None,
            reasoning_levels: None,
            default_reasoning_level: None,
        },
        CodexCatalogModelSpec {
            model: "custom-text-alias".to_string(),
            display_name: Some("Explicit Text Override".to_string()),
            context_window: Some(128_000),
            supports_parallel_tool_calls: None,
            input_modalities: Some(vec!["text".to_string()]),
            base_instructions: None,
            reasoning_levels: None,
            default_reasoning_level: None,
        },
    ];

    for profile in [
        CodexCatalogToolProfile::ProxyChat,
        CodexCatalogToolProfile::NativeResponses,
        CodexCatalogToolProfile::Anthropic,
    ] {
        let catalog = codex_model_catalog_from_specs(&specs, &template, profile, 128_000);
        let models = catalog["models"].as_array().expect("models array");
        let modalities = |slug: &str| {
            models
                .iter()
                .find(|entry| entry["slug"] == slug)
                .and_then(|entry| entry.get("input_modalities"))
                .cloned()
                .unwrap_or(Value::Null)
        };

        assert_eq!(modalities("gpt-5.4"), json!(["text", "image"]));
        assert_eq!(modalities("qwen/qwen3-coder-plus"), json!(["text"]));
        assert_eq!(modalities("glm-5.2v"), json!(["text", "image"]));
        assert_eq!(
            modalities("deepseek-v4-flash"),
            json!(["text", "image"]),
            "explicit provider metadata must override the text-only registry"
        );
        assert_eq!(modalities("custom-text-alias"), json!(["text"]));
    }
}

#[test]
fn native_responses_catalog_always_carries_base_instructions() {
    // Regression guard for the "missing field `base_instructions`" parse
    // error: Codex refuses to load a model catalog whose entries lack
    // base_instructions. Synthesized presets carry no per-row override, so
    // the entry MUST inherit the template's neutral default rather than
    // dropping the field entirely.
    let settings = json!({
        "modelCatalog": { "models": [{ "model": "qwen3-coder-plus" }] }
    });

    let catalog =
        codex_model_catalog_from_settings(&settings, "", CodexCatalogToolProfile::NativeResponses)
            .expect("native catalog generation should not error")
            .expect("non-empty modelCatalog must yield a catalog");

    let base = catalog["models"][0]
        .get("base_instructions")
        .and_then(|v| v.as_str());
    assert!(
        base.is_some_and(|s| !s.trim().is_empty()),
        "every native entry must carry a non-empty base_instructions (Codex requires it)"
    );
}

const DEEPSEEK_NATIVE_CONFIG: &str = r#"model = "deepseek-v4-flash"
model_provider = "custom"

[model_providers.custom]
name = "deepseek"
base_url = "https://api.deepseek.com"
wire_api = "responses"
"#;

#[test]
fn deepseek_host_native_catalog_mirrors_official_entries() {
    // DeepSeek publishes an official Codex models.json (freeform
    // apply_patch + GPT-5 harness + low/high/max reasoning levels). For a
    // deepseek.com native provider the generated catalog must mirror it
    // verbatim instead of the stripped neutral template — the harness
    // tells the model to use apply_patch, so stripping the tool while
    // keeping the harness would be self-inconsistent.
    let settings = json!({
        "modelCatalog": {
            "models": [
                { "model": "deepseek-flash", "displayName": "DeepSeek Flash" },
                { "model": "deepseek-v4-pro", "contextWindow": 500_000 }
            ]
        }
    });

    let catalog = codex_model_catalog_from_settings(
        &settings,
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses,
    )
    .expect("vendor catalog generation should not error")
    .expect("non-empty modelCatalog must yield a catalog");

    let flash = &catalog["models"][0];
    assert_eq!(
        flash.get("slug").and_then(|v| v.as_str()),
        Some("deepseek-flash")
    );
    assert_eq!(
        flash.get("apply_patch_tool_type").and_then(|v| v.as_str()),
        Some("freeform"),
        "official DeepSeek entries keep the freeform apply_patch grant"
    );
    assert!(
        flash
            .get("base_instructions")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("You are Codex, an agent based on GPT-5")),
        "official GPT-5 harness must survive verbatim"
    );
    let efforts: Vec<&str> = flash["supported_reasoning_levels"]
        .as_array()
        .expect("official reasoning levels array")
        .iter()
        .filter_map(|level| level.get("effort").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(efforts, vec!["low", "high", "max"]);
    // DeepSeek has no tool_search support; `true` makes Codex defer MCP
    // tools behind tool search, so none can ever be called (#6647).
    assert_eq!(flash.get("supports_search_tool"), Some(&json!(false)));
    assert_eq!(
        flash.get("web_search_tool_type").and_then(|v| v.as_str()),
        Some("text")
    );
    assert_eq!(
        flash.get("supports_reasoning_summaries"),
        Some(&json!(true))
    );
    // deepseek-flash accepts image input per the vendor's own catalog and
    // vision guide (api-docs.deepseek.com/guides/vision); the legacy
    // deepseek-v4-flash alias routes to it and must not be gated (#7283).
    assert_eq!(
        flash.get("input_modalities"),
        Some(&json!(["text", "image"]))
    );
    assert!(
        flash.get("model_messages").is_some(),
        "official entries are mirrored verbatim, incl. model_messages"
    );
    // No explicit contextWindow on the row: the official 1m window must
    // survive instead of being clobbered by the 128k default.
    assert_eq!(
        flash.get("context_window").and_then(|v| v.as_u64()),
        Some(1_048_576)
    );
    // Explicit user display name still wins over the official one.
    assert_eq!(
        flash.get("display_name").and_then(|v| v.as_str()),
        Some("DeepSeek Flash")
    );

    let pro = &catalog["models"][1];
    assert_eq!(
        pro.get("slug").and_then(|v| v.as_str()),
        Some("deepseek-v4-pro")
    );
    // Explicit user context window override wins…
    assert_eq!(
        pro.get("context_window").and_then(|v| v.as_u64()),
        Some(500_000)
    );
    assert_eq!(
        pro.get("max_context_window").and_then(|v| v.as_u64()),
        Some(500_000)
    );
    // …while the untouched official display name is kept.
    assert_eq!(
        pro.get("display_name").and_then(|v| v.as_str()),
        Some("DeepSeek-V4-Pro")
    );
}

#[test]
fn deepseek_official_catalog_unknown_model_clones_flagship() {
    // A user-added model id the official file doesn't know keeps the
    // gateway's capability profile (clone of the flagship entry) without
    // impersonating it: own slug/name, demoted priority, and the official
    // context window rather than the 128k synthetic default.
    let settings = json!({
        "modelCatalog": { "models": [{ "model": "deepseek-v4-lite" }] }
    });

    let catalog = codex_model_catalog_from_settings(
        &settings,
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses,
    )
    .expect("vendor catalog generation should not error")
    .expect("non-empty modelCatalog must yield a catalog");

    let entry = &catalog["models"][0];
    assert_eq!(
        entry.get("slug").and_then(|v| v.as_str()),
        Some("deepseek-v4-lite")
    );
    assert_eq!(
        entry.get("display_name").and_then(|v| v.as_str()),
        Some("deepseek-v4-lite")
    );
    assert!(
        entry
            .get("priority")
            .and_then(|v| v.as_u64())
            .is_some_and(|p| p >= 1000),
        "clones must sort after official entries"
    );
    assert_eq!(
        entry.get("apply_patch_tool_type").and_then(|v| v.as_str()),
        Some("freeform")
    );
    assert_eq!(
        entry.get("context_window").and_then(|v| v.as_u64()),
        Some(1_048_576),
        "absent contextWindow keeps the flagship's official window"
    );
    assert!(entry
        .get("base_instructions")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty()));
}

#[test]
fn deepseek_official_catalog_legacy_flash_alias_stays_image_capable() {
    // The vendor's catalog now ships `deepseek-flash` only; the legacy
    // `deepseek-v4-flash` id the preset defaulted to is still accepted by
    // the API and routes to the same vision-capable Flash model, so it must
    // clone the flagship and resolve image-capable instead of being gated
    // text-only (#7283).
    let settings = json!({
        "modelCatalog": { "models": [{ "model": "deepseek-v4-flash" }] }
    });

    let catalog = codex_model_catalog_from_settings(
        &settings,
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses,
    )
    .expect("vendor catalog generation should not error")
    .expect("non-empty modelCatalog must yield a catalog");

    let entry = &catalog["models"][0];
    assert_eq!(
        entry.get("slug").and_then(|v| v.as_str()),
        Some("deepseek-v4-flash")
    );
    assert_eq!(
        entry.get("input_modalities"),
        Some(&json!(["text", "image"])),
        "the legacy alias routes to the vision-capable Flash model and must fail open"
    );
}

#[test]
fn official_vendor_catalog_gated_by_native_profile_and_host() {
    // The official mirror is a capability GRANT, so the gate must be
    // narrow: native `/responses` profile AND the vendor's own host. Chat
    // runs through the proxy converter (gpt-5.5 contract), the Anthropic
    // transform drops custom tools, and aggregators hosting the same
    // model may reject freeform tools — all of them keep their templates.
    assert!(codex_official_vendor_catalog_models(
        DEEPSEEK_NATIVE_CONFIG,
        CodexCatalogToolProfile::NativeResponses
    )
    .is_some_and(|models| !models.is_empty()));

    for profile in [
        CodexCatalogToolProfile::ProxyChat,
        CodexCatalogToolProfile::Anthropic,
    ] {
        assert!(
            codex_official_vendor_catalog_models(DEEPSEEK_NATIVE_CONFIG, profile).is_none(),
            "only the NativeResponses profile may mirror the official catalog"
        );
    }

    let minimax_config = r#"model = "MiniMax-M3"
model_provider = "custom"

[model_providers.custom]
name = "minimax"
base_url = "https://api.minimaxi.com/v1"
wire_api = "responses"
"#;
    assert!(
        codex_official_vendor_catalog_models(
            minimax_config,
            CodexCatalogToolProfile::NativeResponses
        )
        .is_none(),
        "non-DeepSeek native hosts keep the neutral template"
    );
    assert!(
        codex_official_vendor_catalog_models("", CodexCatalogToolProfile::NativeResponses)
            .is_none()
    );
}

#[test]
fn proxy_chat_profile_still_keeps_apply_patch() {
    // Regression guard for Mode A: the proxy-chat profile must keep the
    // freeform apply_patch tool (the proxy rewrites custom<->function).
    let template = load_codex_native_responses_template();
    let specs = vec![CodexCatalogModelSpec {
        model: "x".to_string(),
        display_name: Some("x".to_string()),
        context_window: Some(128_000),
        supports_parallel_tool_calls: None,
        input_modalities: None,
        base_instructions: None,
        reasoning_levels: None,
        default_reasoning_level: None,
    }];
    // Using a gpt-5.5-shaped template under ProxyChat must NOT strip
    // apply_patch_tool_type. (The native template lacks it, so synthesize
    // one with the field present to prove ProxyChat leaves it intact.)
    let mut proxy_template = template.clone();
    proxy_template["apply_patch_tool_type"] = json!("freeform");
    let catalog = codex_model_catalog_from_specs(
        &specs,
        &proxy_template,
        CodexCatalogToolProfile::ProxyChat,
        128_000,
    );
    assert_eq!(
        catalog["models"][0]
            .get("apply_patch_tool_type")
            .and_then(|v| v.as_str()),
        Some("freeform"),
        "ProxyChat must preserve apply_patch_tool_type (no native stripping)"
    );
}

#[test]
fn model_catalog_json_field_writes_relative_filename() {
    let input = r#"model_provider = "any"

[model_providers.any]
name = "any"
"#;
    let catalog_path = Path::new("/tmp/cc-switch-model-catalog.json");

    let result = set_codex_model_catalog_json_field(input, Some(catalog_path)).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(
        parsed
            .get("model_catalog_json")
            .and_then(|value| value.as_str()),
        Some(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)
    );
    assert!(
        parsed
            .get("model_providers")
            .and_then(|value| value.get("any"))
            .and_then(|value| value.get("model_catalog_json"))
            .is_none(),
        "model_catalog_json should stay top-level"
    );
}

#[test]
fn native_web_search_field_disables_at_top_level() {
    // Native `/responses` gateways reject the web_search tool, so the
    // NativeResponses profile must write the top-level disable line even
    // when sections are present (it must NOT land inside a section).
    let input = r#"model_provider = "custom"

[model_providers.custom]
name = "xiaomi_mimo"
"#;
    let result = set_codex_native_web_search_field(input, true).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(
        parsed.get("web_search").and_then(|value| value.as_str()),
        Some("disabled")
    );
    assert!(
        parsed
            .get("model_providers")
            .and_then(|value| value.get("custom"))
            .and_then(|value| value.get("web_search"))
            .is_none(),
        "web_search should stay top-level"
    );
}

#[test]
fn native_web_search_field_removes_own_sentinel_when_not_disabled() {
    // Switching away from a native provider must re-enable web search by
    // removing cc-switch's own "disabled" sentinel.
    let input = r#"model = "gpt-5.5"
web_search = "disabled"
"#;
    let result = set_codex_native_web_search_field(input, false).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert!(
        parsed.get("web_search").is_none(),
        "cc-switch's disabled sentinel should be removed when not native"
    );
}

#[test]
fn native_web_search_field_preserves_user_value() {
    // A user's own web_search value must never be clobbered by cleanup,
    // only cc-switch's "disabled" sentinel is owned/removable.
    let input = r#"web_search = "enabled"
"#;
    let result = set_codex_native_web_search_field(input, false).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(
        parsed.get("web_search").and_then(|value| value.as_str()),
        Some("enabled"),
        "a user-set web_search value must be preserved"
    );
}

#[test]
fn anthropic_profile_disables_web_search_without_catalog() {
    // Regression: even when no model catalog is generated (empty/absent
    // modelCatalog), an Anthropic provider must still disable web_search — the
    // Responses→Anthropic transform drops the hosted tool, so leaving it on
    // exposes a dead tool. The None-catalog branch previously always left it on.
    let config = "model = \"claude-sonnet-4-6\"\n";
    let settings = serde_json::json!({});

    let anthropic = prepare_codex_config_text_with_model_catalog(
        &settings,
        config,
        CodexCatalogToolProfile::Anthropic,
    )
    .unwrap();
    let parsed: toml::Value = toml::from_str(&anthropic).unwrap();
    assert_eq!(
        parsed.get("web_search").and_then(|v| v.as_str()),
        Some("disabled"),
        "Anthropic profile must disable web_search even with no catalog"
    );

    // ProxyChat on the same no-catalog path must NOT add a disable line.
    let proxy = prepare_codex_config_text_with_model_catalog(
        &settings,
        config,
        CodexCatalogToolProfile::ProxyChat,
    )
    .unwrap();
    let parsed: toml::Value = toml::from_str(&proxy).unwrap();
    assert!(
        parsed.get("web_search").is_none(),
        "ProxyChat profile must not disable web_search on the no-catalog path"
    );
}

#[test]
fn web_search_blacklist_disables_only_known_reject_gateways() {
    let cfg = |model: &str, base_url: &str| {
        format!(
                "model_provider = \"custom\"\nmodel = \"{model}\"\n\n[model_providers.custom]\nname = \"x\"\nbase_url = \"{base_url}\"\nwire_api = \"responses\"\n"
            )
    };

    // Blacklisted by host (first-party reject gateways) → disable.
    for (model, host) in [
        ("mimo-v2.5-pro", "https://api.xiaomimimo.com/v1"),
        ("mimo-v2.5", "https://token-plan-cn.xiaomimimo.com/v1"),
        ("LongCat-2.0", "https://api.longcat.chat/openai/v1"),
        ("MiniMax-M3", "https://api.minimax.io/v1"),
        ("MiniMax-M3", "https://api.minimaxi.com/v1"),
        // Use an alias to exercise host detection independently of the
        // MiniMax model-prefix fallback.
        ("custom-model", "https://api.minimax.cn/v1"),
        ("step-4-flash", "https://api.stepfun.com/v1"),
        ("step-4-flash", "https://api.stepfun.ai/v1"),
        ("deepseek-v4-pro", "https://qianfan.baidubce.com/v2"),
        (
            "astron-code-latest",
            "https://maas-coding-api.cn-huabei-1.xf-yun.com/v1",
        ),
        ("glm-5.3", "https://open.bigmodel.cn/api/v1"),
        ("glm-5.3", "https://api.z.ai/api/v1"),
    ] {
        assert!(
            codex_native_gateway_rejects_web_search(&cfg(model, host)),
            "{host} should be blacklisted"
        );
    }

    // Blacklisted by MODEL brand even on an aggregator host (SiliconFlow
    // fronting a reject vendor's model) → disable.
    for (model, host) in [
        ("MiniMax-M3", "https://api.siliconflow.cn/v1"),
        ("MiniMaxAI/MiniMax-M3", "https://api.siliconflow.cn/v1"),
        ("mimo-v2.5-pro", "https://some-aggregator.example/v1"),
        ("zai-org/glm-5.3", "https://some-aggregator.example/v1"),
        (
            "qwen/qwen3-coder-plus",
            "https://some-aggregator.example/v1",
        ),
    ] {
        assert!(
            codex_native_gateway_rejects_web_search(&cfg(model, host)),
            "{model} @ {host} should be blacklisted by model brand"
        );
    }

    // Qwen3-Coder is blacklisted by model, not by DashScope host. This keeps
    // general Qwen models that support built-in web_search on the same host
    // enabled while protecting the native qwen3-coder-plus preset.
    assert!(codex_native_gateway_rejects_web_search(&cfg(
        "qwen3-coder-plus",
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
    )));
    assert!(!codex_native_gateway_rejects_web_search(&cfg(
        "qwen3.7-plus",
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
    )));

    // NOT blacklisted → keep Codex default (relays/GPT, DouBao, general Qwen,
    // and any unknown provider incl. an aggregator serving a non-reject model).
    for (model, host) in [
        ("gpt-5.5", "https://www.packyapi.com/v1"),
        ("gpt-5-codex", "https://aihubmix.com/v1"),
        (
            "doubao-seed-2-1-pro-260628",
            "https://ark.cn-beijing.volces.com/api/v3",
        ),
        ("Pro/moonshotai/Kimi-K2.6", "https://api.siliconflow.cn/v1"),
        // Host-label matching: `z.ai` / `bigmodel.cn` must not swallow
        // unrelated domains that merely contain them as a substring.
        ("gpt-5.5", "https://api.xyz.ai/v1"),
        ("gpt-5.5", "https://viz.ai/v1"),
        ("gpt-5.5", "https://notbigmodel.cn/v1"),
        ("gpt-5.5", "https://z.ai.example.com/v1"),
        ("gpt-5.5", "https://api.stepfun.com.example.com/v1"),
    ] {
        assert!(
            !codex_native_gateway_rejects_web_search(&cfg(model, host)),
            "{model} @ {host} should NOT be blacklisted"
        );
    }
}

#[test]
fn url_host_matcher_uses_label_boundaries() {
    let hosts = &["z.ai", "bigmodel.cn"];
    for url in [
        "https://api.z.ai/api/v1",
        "https://open.bigmodel.cn/api/v1",
        "https://Open.BigModel.cn/api/coding/paas/v4",
        "https://user:pw@api.z.ai:8443/api/v1?x=1#f",
        "z.ai",
        "api.z.ai.",
    ] {
        assert!(codex_url_host_matches_any(url, hosts), "{url}");
    }
    for url in [
        "https://api.xyz.ai/v1",
        "https://viz.ai/v1",
        "https://z.ai.example.com/v1",
        "https://notbigmodel.cn/v1",
        "https://example.com/z.ai/v1",
        "https://example.com/?next=https://api.z.ai",
        "",
    ] {
        assert!(!codex_url_host_matches_any(url, hosts), "{url}");
    }
    assert_eq!(codex_url_host("https://[::1]:8080/v1"), "::1");
    assert_eq!(codex_url_host("HTTP://Example.COM:80"), "example.com");
}

#[test]
fn resolve_catalog_path_returns_none_when_config_missing_field() {
    let base = PathBuf::from("/tmp/.codex");
    assert!(resolve_cc_switch_catalog_path("", &base).is_none());
    assert!(
        resolve_cc_switch_catalog_path("model = \"gpt-5\"", &base).is_none(),
        "no model_catalog_json field should yield None"
    );
}

#[test]
fn resolve_catalog_path_accepts_cc_switch_owned_file() {
    let base = PathBuf::from("/tmp/.codex");
    let config = r#"model_catalog_json = "/tmp/.codex/cc-switch-model-catalog.json"
"#;
    let resolved = resolve_cc_switch_catalog_path(config, &base).expect("path resolves");
    assert_eq!(resolved, base.join(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME));
}

#[test]
fn resolve_catalog_path_rejects_user_owned_external_file() {
    let base = PathBuf::from("/tmp/.codex");
    let config = r#"model_catalog_json = "/Users/me/.codex/my-handwritten-catalog.json"
"#;
    assert!(
        resolve_cc_switch_catalog_path(config, &base).is_none(),
        "external catalog files should be left alone"
    );
}

#[test]
fn build_simplified_catalog_round_trips_user_input() {
    let config = "";
    let catalog = r#"{
            "models": [
                { "slug": "deepseek-v4-pro", "display_name": "deepseek-v4-pro", "context_window": 1000000 },
                { "slug": "deepseek-v4-flash", "display_name": "DeepSeek Flash", "context_window": 1000000 }
            ]
        }"#;
    let result = build_simplified_catalog_from_texts(config, catalog).expect("entries found");
    let models = result
        .get("models")
        .and_then(|m| m.as_array())
        .expect("models array");
    assert_eq!(models.len(), 2);

    // First entry: display_name == slug → displayName squashed; explicit
    // context_window != default 128_000 → preserved.
    assert_eq!(
        models[0].get("model").and_then(|v| v.as_str()),
        Some("deepseek-v4-pro")
    );
    assert!(models[0].get("displayName").is_none());
    assert_eq!(
        models[0].get("contextWindow").and_then(|v| v.as_u64()),
        Some(1_000_000)
    );

    // Second entry: display_name distinct from slug → preserved.
    assert_eq!(
        models[1].get("displayName").and_then(|v| v.as_str()),
        Some("DeepSeek Flash")
    );
}

#[test]
fn build_simplified_catalog_squashes_default_context_window() {
    // Default fallback is 128_000 when config.toml has no model_context_window.
    let catalog = r#"{
            "models": [{ "slug": "kimi", "display_name": "kimi", "context_window": 128000 }]
        }"#;
    let result = build_simplified_catalog_from_texts("", catalog).expect("entry");
    let entry = &result.get("models").unwrap().as_array().unwrap()[0];
    assert!(
            entry.get("contextWindow").is_none(),
            "default 128_000 should be squashed so the form shows blank, matching the user's blank input"
        );
}

#[test]
fn build_simplified_catalog_respects_explicit_model_context_window() {
    // When config.toml sets model_context_window, that becomes the default fallback.
    let config = r#"model_context_window = 200000
"#;
    let catalog = r#"{
            "models": [
                { "slug": "a", "display_name": "a", "context_window": 200000 },
                { "slug": "b", "display_name": "b", "context_window": 500000 }
            ]
        }"#;
    let result = build_simplified_catalog_from_texts(config, catalog).expect("entries");
    let models = result.get("models").unwrap().as_array().unwrap();
    // Matches default → squashed.
    assert!(models[0].get("contextWindow").is_none());
    // Different from default → preserved.
    assert_eq!(
        models[1].get("contextWindow").and_then(|v| v.as_u64()),
        Some(500_000)
    );
}

#[test]
fn build_simplified_catalog_squashes_inferred_modalities_and_keeps_overrides() {
    let catalog = r#"{
            "models": [
                { "slug": "gpt-5.4", "input_modalities": ["text", "image"] },
                { "slug": "qwen3-coder-plus", "input_modalities": ["text"] },
                { "slug": "gpt-text-override", "input_modalities": ["text"] },
                { "slug": "glm-5.2", "input_modalities": ["text", "image"] }
            ]
        }"#;

    let result = build_simplified_catalog_from_texts("", catalog).expect("entries");
    let models = result.get("models").unwrap().as_array().unwrap();

    assert!(
        models[0].get("inputModalities").is_none(),
        "GPT text+image is inferred and must not become a sticky hidden override"
    );
    assert!(
        models[1].get("inputModalities").is_none(),
        "confirmed text-only capability is inferred and must remain registry-driven"
    );
    assert_eq!(
        models[2].get("inputModalities"),
        Some(&json!(["text"])),
        "an unknown model explicitly forced to text-only must round-trip"
    );
    assert_eq!(
        models[3].get("inputModalities"),
        Some(&json!(["text", "image"])),
        "an explicit image override for a registered text-only model must round-trip"
    );
}

#[test]
fn build_simplified_catalog_returns_none_when_unparseable() {
    assert!(build_simplified_catalog_from_texts("", "not json").is_none());
    assert!(build_simplified_catalog_from_texts("", "{}").is_none());
    assert!(
        build_simplified_catalog_from_texts("", r#"{"models": []}"#).is_none(),
        "empty models array should yield None so the field is not inserted at all"
    );
    assert!(
        build_simplified_catalog_from_texts("", r#"{"models": [{"display_name": "no slug"}]}"#,)
            .is_none(),
        "entries lacking slug are skipped; a fully-skipped catalog yields None"
    );
}

#[test]
fn codex_cli_candidates_are_non_empty() {
    let candidates = codex_cli_candidates();
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate == Path::new("codex")),
        "codex CLI candidates must include the PATH entry"
    );
}

#[test]
fn codex_bundled_models_command_uses_expected_program_and_args() {
    let command = codex_bundled_models_command(Path::new("codex"));
    assert_eq!(command.get_program(), "codex");
    assert_eq!(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["debug", "models", "--bundled"]
    );
}

#[test]
fn successful_model_catalog_template_load_is_cached() {
    use std::cell::Cell;

    let cache = OnceCell::new();
    let calls = Cell::new(0);
    let first = get_or_load_codex_model_catalog_template(&cache, || {
        calls.set(calls.get() + 1);
        Ok(json!({ "slug": "first" }))
    })
    .expect("first template load");
    let second = get_or_load_codex_model_catalog_template(&cache, || {
        calls.set(calls.get() + 1);
        Ok(json!({ "slug": "second" }))
    })
    .expect("cached template load");

    assert_eq!(first, json!({ "slug": "first" }));
    assert_eq!(second, first);
    assert_eq!(calls.get(), 1, "successful template should load only once");
}

#[test]
fn failed_model_catalog_template_load_can_retry() {
    use std::cell::Cell;

    let cache = OnceCell::new();
    let calls = Cell::new(0);
    let first = get_or_load_codex_model_catalog_template(&cache, || {
        calls.set(calls.get() + 1);
        Err(AppError::Message("temporary failure".to_string()))
    });
    assert!(first.is_err());

    let second = get_or_load_codex_model_catalog_template(&cache, || {
        calls.set(calls.get() + 1);
        Ok(json!({ "slug": "recovered" }))
    })
    .expect("retry template load");

    assert_eq!(second, json!({ "slug": "recovered" }));
    assert_eq!(calls.get(), 2, "failed loads must not poison the cache");
}

#[test]
fn codex_cli_candidates_include_user_node_manager_bins() {
    let temp_home = tempfile::tempdir().expect("create temp home");
    let home = temp_home.path();
    let expected = [
        home.join(".nvm/versions/node/v22.14.0/bin/codex"),
        home.join(".volta/bin/codex"),
        home.join(".asdf/shims/codex"),
        home.join(".local/share/mise/shims/codex"),
        home.join(".local/share/fnm/node-versions/v22.14.0/installation/bin/codex"),
    ];

    for candidate in &expected {
        std::fs::create_dir_all(candidate.parent().expect("candidate parent"))
            .expect("create candidate parent");
        std::fs::write(candidate, "").expect("create candidate");
    }

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    push_home_codex_cli_candidates(&mut candidates, &mut seen, home);

    for candidate in expected {
        assert!(
            candidates.contains(&candidate),
            "user-level Codex CLI candidate should be discovered: {}",
            candidate.display()
        );
    }
}

#[test]
fn codex_cli_candidates_deduplicate_entries() {
    let temp_home = tempfile::tempdir().expect("create temp home");
    let home = temp_home.path();
    let candidate = home.join(".volta/bin/codex");
    std::fs::create_dir_all(candidate.parent().expect("candidate parent"))
        .expect("create candidate parent");
    std::fs::write(&candidate, "").expect("create candidate");

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    push_existing_codex_cli_candidate(&mut candidates, &mut seen, candidate.clone());
    push_home_codex_cli_candidates(&mut candidates, &mut seen, home);

    assert_eq!(
        candidates.iter().filter(|path| **path == candidate).count(),
        1,
        "duplicate candidates should be removed"
    );
}

#[test]
fn static_template_is_valid_json_with_slug() {
    let template =
        load_codex_model_template_static().expect("static template must parse as valid JSON");
    assert_eq!(
        template.get("slug").and_then(|v| v.as_str()),
        Some("gpt-5.5"),
        "static template slug must be gpt-5.5"
    );
}

#[test]
fn static_template_has_required_keys() {
    let template =
        load_codex_model_template_static().expect("static template must parse as valid JSON");
    for key in &[
        "model_messages",
        "base_instructions",
        "context_window",
        "display_name",
    ] {
        assert!(
            template.get(key).is_some(),
            "static template must contain key '{key}'"
        );
    }
}

#[test]
#[cfg(target_os = "windows")]
fn set_catalog_json_field_writes_filename_ignoring_unc_path() {
    let input = r#"model_provider = "custom"
model = "glm-5"
"#;
    // Simulate a WSL UNC path as cc-switch would see it on Windows;
    // the function now writes just the relative filename.
    let unc_path =
        Path::new(r"\\wsl.localhost\Ubuntu\home\user\.codex\cc-switch-model-catalog.json");

    let result = set_codex_model_catalog_json_field(input, Some(unc_path)).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    let written_path = parsed
        .get("model_catalog_json")
        .and_then(|v| v.as_str())
        .expect("model_catalog_json should be set");
    assert_eq!(
        written_path, CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME,
        "should write only the relative filename, not the UNC path"
    );
}

#[test]
fn set_catalog_json_field_writes_filename_for_any_path() {
    let input = r#"model_provider = "custom"
model = "glm-5"
"#;
    let regular_path = Path::new("/home/user/.codex/cc-switch-model-catalog.json");

    let result = set_codex_model_catalog_json_field(input, Some(regular_path)).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();

    assert_eq!(
        parsed.get("model_catalog_json").and_then(|v| v.as_str()),
        Some(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME),
        "should write only the relative filename, not the full path"
    );
}

#[test]
fn set_catalog_json_none_removes_cc_switch_owned_by_filename() {
    // After the WSL fix, TOML may contain a Linux-style path.
    // The None arm must still remove it (file_name match catches any format).
    let input = r#"model_catalog_json = "/home/user/.codex/cc-switch-model-catalog.json"
"#;
    let result = set_codex_model_catalog_json_field(input, None).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert!(
        parsed.get("model_catalog_json").is_none(),
        "None arm should remove cc-switch-owned field regardless of path format"
    );
}

#[test]
fn set_catalog_json_none_preserves_user_owned_catalog() {
    let input = r#"model_catalog_json = "/Users/me/.codex/my-custom-catalog.json"
"#;
    let result = set_codex_model_catalog_json_field(input, None).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(
        parsed.get("model_catalog_json").and_then(|v| v.as_str()),
        Some("/Users/me/.codex/my-custom-catalog.json"),
        "None arm should NOT remove user-owned catalog"
    );
}

#[test]
fn set_catalog_json_some_preserves_user_owned_catalog() {
    // When CC Switch generates a catalog (Some arm), it must still respect a
    // user-managed external catalog file instead of clobbering it with the
    // cc-switch-owned filename. Only an absent or cc-switch-owned pointer is
    // claimed; this mirrors the None arm's ownership rule.
    let input = r#"model_provider = "custom"
model = "glm-5"
model_catalog_json = "/Users/me/.codex/my-custom-catalog.json"
"#;
    let catalog_path = Path::new("/tmp/cc-switch-model-catalog.json");
    let result = set_codex_model_catalog_json_field(input, Some(catalog_path)).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(
        parsed.get("model_catalog_json").and_then(|v| v.as_str()),
        Some("/Users/me/.codex/my-custom-catalog.json"),
        "Some arm should NOT clobber a user-owned catalog (full path)"
    );
}

#[test]
fn set_catalog_json_some_preserves_user_owned_relative_filename() {
    // A bare custom filename (no directory component) is also user-owned
    // and must be preserved by the Some arm.
    let input = r#"model_provider = "custom"
model = "glm-5"
model_catalog_json = "my-custom-catalog.json"
"#;
    let catalog_path = Path::new("/tmp/cc-switch-model-catalog.json");
    let result = set_codex_model_catalog_json_field(input, Some(catalog_path)).unwrap();
    let parsed: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(
        parsed.get("model_catalog_json").and_then(|v| v.as_str()),
        Some("my-custom-catalog.json"),
        "Some arm should NOT clobber a relative user-owned catalog"
    );
}

#[test]
fn resolve_catalog_finds_relative_filename() {
    let config_text = r#"model_provider = "custom"
model_catalog_json = "cc-switch-model-catalog.json"
"#;
    let base_dir = PathBuf::from("/home/user/.codex");
    let result = resolve_cc_switch_catalog_path(config_text, &base_dir);
    assert_eq!(
        result,
        Some(base_dir.join(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)),
        "relative filename should resolve under base_dir for file I/O"
    );
}

#[test]
fn resolve_catalog_rejects_absolute_path_outside_config_dir() {
    let config_text = r#"model_catalog_json = "/tmp/secret/cc-switch-model-catalog.json"
"#;
    let base_dir = PathBuf::from("/home/user/.codex");
    let result = resolve_cc_switch_catalog_path(config_text, &base_dir);
    assert_eq!(
        result, None,
        "absolute path outside ~/.codex must not be accepted"
    );
}

#[test]
fn resolve_catalog_accepts_absolute_path_inside_config_dir() {
    let config_text = r#"model_catalog_json = "/home/user/.codex/cc-switch-model-catalog.json"
"#;
    let base_dir = PathBuf::from("/home/user/.codex");
    let result = resolve_cc_switch_catalog_path(config_text, &base_dir);
    assert_eq!(
        result,
        Some(base_dir.join(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)),
        "absolute path inside ~/.codex should be accepted"
    );
}

#[test]
fn resolve_catalog_rejects_traversal_to_parent_directory() {
    let config_text = r#"model_catalog_json = "../cc-switch-model-catalog.json"
"#;
    let base_dir = PathBuf::from("/home/user/.codex");
    let result = resolve_cc_switch_catalog_path(config_text, &base_dir);
    assert_eq!(
        result, None,
        "relative traversal outside ~/.codex must not be accepted"
    );
}

#[test]
fn resolve_catalog_rejects_symlink_escaping_config_dir() {
    // 词法包含可被符号链接绕过：~/.codex/link -> 外部目录，
    // "link/cc-switch-model-catalog.json" 词法上在 base 内，真实读取却落到
    // base 外。canonicalize 之后的二次校验必须拒绝。
    let temp = tempfile::tempdir().expect("tempdir");
    let base_dir = temp.path().join("codex");
    let outside_dir = temp.path().join("outside");
    fs::create_dir_all(&base_dir).expect("create base");
    fs::create_dir_all(&outside_dir).expect("create outside");
    let escaped_file = outside_dir.join(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME);
    fs::write(&escaped_file, r#"{"models":[]}"#).expect("write escaped catalog");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside_dir, base_dir.join("link")).expect("symlink");
    #[cfg(windows)]
    {
        // Windows 创建符号链接需要 SeCreateSymbolicLinkPrivilege（管理员或
        // 开发者模式）。在未提权的 CI/本机上该测试无法构造前置条件，应优雅
        // 跳过而非红失败（错误码 1314 = ERROR_PRIVILEGE_NOT_HELD，
        // 1 = ERROR_INVALID_FUNCTION 为未开开发者模式时的常见返回）。
        match std::os::windows::fs::symlink_dir(&outside_dir, base_dir.join("link")) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(1314) || e.raw_os_error() == Some(1) => {
                eprintln!(
                    "skipping symlink-escape test: creating symlinks requires \
                     SeCreateSymbolicLinkPrivilege (run as admin or enable \
                     Developer Mode); os error = {:?}",
                    e.raw_os_error()
                );
                return;
            }
            Err(e) => panic!("symlink: {e}"),
        }
        // 某些环境（安全软件 / FS 过滤驱动）会让 symlink 调用"假成功"——实际
        // 落盘的是一个普通空目录而非 reparse point。此时逃逸场景构造不出来，
        // 断言必然误红，同样优雅跳过。
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            let md = match std::fs::symlink_metadata(base_dir.join("link")) {
                Ok(md) => md,
                Err(e) => panic!("symlink_metadata after Ok(()) creation: {e}"),
            };
            if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                eprintln!(
                    "skipping symlink-escape test: symlink call reported success but \
                     produced a plain directory without a reparse point (likely a \
                     filter driver neutering symlink creation on this host)"
                );
                return;
            }
        }
    }

    let config_text = r#"model_catalog_json = "link/cc-switch-model-catalog.json"
"#;
    let result = resolve_cc_switch_catalog_path(config_text, &base_dir);
    assert_eq!(
        result, None,
        "symlink escaping the config dir must be rejected after canonicalization"
    );
}

#[test]
fn resolve_catalog_accepts_real_file_inside_config_dir() {
    // 存在于 base 内的真实文件：canonical 校验通过后仍应接受
    let temp = tempfile::tempdir().expect("tempdir");
    let base_dir = temp.path().join("codex");
    fs::create_dir_all(&base_dir).expect("create base");
    let catalog_file = base_dir.join(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME);
    fs::write(&catalog_file, r#"{"models":[]}"#).expect("write catalog");

    let config_text = r#"model_catalog_json = "cc-switch-model-catalog.json"
"#;
    let result = resolve_cc_switch_catalog_path(config_text, &base_dir);
    let resolved = result.expect("real file inside config dir should be accepted");
    assert_eq!(
        resolved.file_name().and_then(|n| n.to_str()),
        Some(CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)
    );
}

#[test]
fn read_limited_string_rejects_oversized_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("huge.json");
    let file = std::fs::File::create(&path).expect("create");
    file.set_len(MAX_CODEX_CATALOG_BYTES + 1).expect("set_len");

    let result = read_limited_string(&path, MAX_CODEX_CATALOG_BYTES);
    assert!(
        result.is_err(),
        "file larger than MAX_CODEX_CATALOG_BYTES must be rejected"
    );
}

// ==================== 外科手术式合并（merge_codex_config_surgical） ====================

const MERGE_USER_EXISTING: &str = r#"# my custom comment
model = "gpt-5.1"
model_provider = "custom"

# my other comment
[profiles.work]
model = "o4-mini"

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]
"#;

const MERGE_NEW_CONFIG: &str = r#"model = "gpt-5.2"
model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "https://relay.example.test/v1"
experimental_bearer_token = "sk-test"
"#;

fn merge_assert_toml(text: &str) -> toml::Table {
    toml::from_str(text).expect("merged output must be valid TOML")
}

#[test]
fn merge_preserves_user_comments_and_custom_tables_while_updating_managed_keys() {
    let merged = merge_codex_config_surgical(MERGE_USER_EXISTING, MERGE_NEW_CONFIG);

    // 注释与自定义段逐字保留
    assert!(merged.contains("# my custom comment"));
    assert!(merged.contains("# my other comment"));
    assert!(merged.contains("[profiles.work]"));
    assert!(merged.contains("[mcp_servers.filesystem]"));
    assert!(merged.contains("command = \"npx\""));

    // 管理键已更新为 new_config 值
    let table = merge_assert_toml(&merged);
    assert_eq!(table.get("model"), Some(&toml::Value::from("gpt-5.2")));
    assert_eq!(
        table.get("model_provider"),
        Some(&toml::Value::from("custom"))
    );
    let providers = table.get("model_providers").expect("providers table");
    let custom = providers.get("custom").expect("custom provider");
    assert_eq!(
        custom.get("base_url"),
        Some(&toml::Value::from("https://relay.example.test/v1"))
    );
    assert_eq!(
        custom.get("experimental_bearer_token"),
        Some(&toml::Value::from("sk-test"))
    );
}

#[test]
fn merge_into_blank_text_yields_all_new_keys() {
    for existing in ["", "   \n", "# only a comment\n"] {
        let merged = merge_codex_config_surgical(existing, MERGE_NEW_CONFIG);
        let table = merge_assert_toml(&merged);
        assert_eq!(table.get("model"), Some(&toml::Value::from("gpt-5.2")));
        assert!(
            table.get("model_providers").is_some(),
            "model_providers must be present for existing={existing:?}"
        );
        assert_eq!(
            table.get("model_provider"),
            Some(&toml::Value::from("custom"))
        );
    }
}

#[test]
fn merge_falls_back_to_full_text_when_existing_is_invalid_toml() {
    let invalid = "this is = not [valid toml";
    let merged = merge_codex_config_surgical(invalid, MERGE_NEW_CONFIG);
    // 回退路径输出合法完整的新配置
    let table = merge_assert_toml(&merged);
    assert_eq!(table.get("model"), Some(&toml::Value::from("gpt-5.2")));
    // 回退输出与整份替换行为一致
    assert_eq!(merged, MERGE_NEW_CONFIG);
}

#[test]
fn merge_is_idempotent_across_repeated_switches_and_keeps_comments() {
    let first = merge_codex_config_surgical(MERGE_USER_EXISTING, MERGE_NEW_CONFIG);
    // 第二轮切换：盘上文本已是第一轮输出，新配置语义不变
    let second = merge_codex_config_surgical(&first, MERGE_NEW_CONFIG);

    let first_table = merge_assert_toml(&first);
    let second_table = merge_assert_toml(&second);
    assert_eq!(
        first_table, second_table,
        "repeated merges must not change parsed semantics"
    );
    assert!(second.contains("# my custom comment"));
    assert!(second.contains("[profiles.work]"));
    assert!(second.contains("[mcp_servers.filesystem]"));
    // 标量替换保留旧侧 decor 后，重复合并应逐字稳定
    assert_eq!(first, second);
}

#[test]
fn merge_removes_stale_cc_switch_owned_sentinels_but_keeps_user_values() {
    let existing = r#"model_provider = "cc-switch-official"
model_catalog_json = "cc-switch-model-catalog.json"
web_search = "disabled"
experimental_bearer_token = "PROXY_MANAGED"
user_catalog = "my-own-catalog.json"
web_search_note = "keep-me"
"#;
    let new_text = "model = \"gpt-5.2\"\n";
    let merged = merge_codex_config_surgical(existing, new_text);
    let table = merge_assert_toml(&merged);

    // cc-switch 哨兵键随新配置缺席被移除
    assert!(table.get("model_catalog_json").is_none());
    assert!(table.get("web_search").is_none());
    assert!(table.get("experimental_bearer_token").is_none());
    // 用户自己的键原样保留
    assert_eq!(
        table.get("user_catalog"),
        Some(&toml::Value::from("my-own-catalog.json"))
    );
    assert_eq!(
        table.get("web_search_note"),
        Some(&toml::Value::from("keep-me"))
    );
}

#[test]
fn merge_keeps_existing_scalar_comments_and_harmonizes_newlines() {
    let existing = "# top\r\nmodel = \"gpt-5.1\" # pinned by hand\r\n";
    let new_text = "model = \"gpt-5.2\"\n";
    let merged = merge_codex_config_surgical(existing, new_text);

    assert!(
        merged.contains("# pinned by hand"),
        "user's trailing comment must survive: {merged:?}"
    );
    assert!(
        !merged.contains('\n') || merged.contains("\r\n"),
        "CRLF style must be preserved: {merged:?}"
    );
    assert_eq!(
        merge_assert_toml(&merged).get("model"),
        Some(&toml::Value::from("gpt-5.2"))
    );
}

#[test]
fn merge_does_not_rewrite_newlines_inside_multiline_literals() {
    // 既有文件 CRLF 为主，新模板带一个内部为 LF 的多行字面量：
    // 统一换行不得污染字面量内部内容（语义），允许外部保持混合换行。
    let existing = "# top\r\nmodel = \"gpt-5.1\"\r\n";
    let new_text = "model = \"gpt-5.2\"\nprompt = \"\"\"line one\nline two\n\"\"\"\n";
    let merged = merge_codex_config_surgical(existing, new_text);

    assert!(
        merged.contains("line one\nline two"),
        "multiline literal interior must keep LF: {merged:?}"
    );
    assert_eq!(
        merge_assert_toml(&merged).get("prompt"),
        Some(&toml::Value::from("line one\nline two\n"))
    );
}
