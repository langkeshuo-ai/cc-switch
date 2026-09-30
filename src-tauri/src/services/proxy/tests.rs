//! ProxyService 单元测试。
//! （自 proxy.rs 的内联 `mod tests` 机械外移；内容零改动。）

use super::*;
use crate::provider::{AuthBinding, AuthBindingSource, ProviderMeta};
use serial_test::serial;
use std::env;
use tempfile::TempDir;

struct TempHome {
    #[allow(dead_code)]
    dir: TempDir,
    original_home: Option<String>,
    original_userprofile: Option<String>,
    original_test_home: Option<String>,
}

impl TempHome {
    fn new() -> Self {
        let dir = TempDir::new().expect("failed to create temp home");
        let original_home = env::var("HOME").ok();
        let original_userprofile = env::var("USERPROFILE").ok();
        let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();

        env::set_var("HOME", dir.path());
        env::set_var("USERPROFILE", dir.path());
        env::set_var("CC_SWITCH_TEST_HOME", dir.path());

        Self {
            dir,
            original_home,
            original_userprofile,
            original_test_home,
        }
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        match &self.original_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }

        match &self.original_userprofile {
            Some(value) => env::set_var("USERPROFILE", value),
            None => env::remove_var("USERPROFILE"),
        }

        match &self.original_test_home {
            Some(value) => env::set_var("CC_SWITCH_TEST_HOME", value),
            None => env::remove_var("CC_SWITCH_TEST_HOME"),
        }
    }
}

fn assert_env_str(env: &Map<String, Value>, key: &str, expected: Option<&str>) {
    assert_eq!(env.get(key).and_then(|value| value.as_str()), expected);
}

async fn use_ephemeral_proxy_port(db: &Arc<Database>) {
    let mut proxy_config = db.get_proxy_config().await.expect("get test proxy config");
    proxy_config.listen_port = 0;
    db.update_proxy_config(proxy_config)
        .await
        .expect("set test proxy config to an ephemeral port");
}

async fn seed_distinct_app_proxy_configs(db: &Database) -> Vec<Value> {
    let mut configs = Vec::new();
    // 应用域已收敛到 claude/codex/pi（v22 起其余 app 的 proxy_config 行被清退，
    // get_proxy_config_for_app 对已删应用会硬报错），用 pi 顶替原 gemini/grokbuild。
    for (app, retries) in [("claude", 6), ("codex", 0), ("pi", 3)] {
        let mut config = db.get_proxy_config_for_app(app).await.unwrap();
        config.enabled = retries % 2 == 0;
        config.auto_failover_enabled = retries % 2 != 0;
        config.max_retries = retries;
        config.streaming_first_byte_timeout = 30 + retries;
        config.streaming_idle_timeout = 90 + retries;
        config.non_streaming_timeout = 300 + retries;
        config.circuit_failure_threshold = 5 + retries;
        configs.push(serde_json::to_value(&config).unwrap());
        db.update_proxy_config_for_app(config).await.unwrap();
    }
    configs
}

async fn assert_app_proxy_configs_unchanged(db: &Database, configs: &[Value]) {
    for expected in configs {
        let app = expected["appType"].as_str().unwrap();
        let actual = db.get_proxy_config_for_app(app).await.unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), *expected, "{app}");
    }
}

#[tokio::test]
#[serial]
async fn shutdown_preserves_app_proxy_configs() {
    let _home = TempHome::new();
    crate::settings::reload_settings().unwrap();
    let db = Arc::new(Database::memory().unwrap());
    let configs = seed_distinct_app_proxy_configs(&db).await;
    let service = ProxyService::new(db.clone());

    let backup = json!({"env": {"ANTHROPIC_BASE_URL": "https://example.com"}});
    db.save_live_backup("claude", &backup.to_string())
        .await
        .unwrap();

    service.stop_with_restore_keep_state().await.unwrap();

    assert_app_proxy_configs_unchanged(&db, &configs).await;
    assert_eq!(
        read_json_file::<Value>(&get_claude_settings_path()).unwrap(),
        backup
    );
    assert!(db.get_live_backup("claude").await.unwrap().is_none());
}

#[tokio::test]
async fn ephemeral_port_preserves_app_proxy_configs() {
    let db = Arc::new(Database::memory().unwrap());
    let configs = seed_distinct_app_proxy_configs(&db).await;
    let service = ProxyService::new(db.clone());
    let mut config = db.get_proxy_config().await.unwrap();
    config.listen_port = 0;
    let mut expected_global = db.get_global_proxy_config().await.unwrap();
    expected_global.listen_port = 23456;

    service
        .persist_ephemeral_listen_port_if_needed(&config, 23456)
        .await
        .unwrap();

    assert_app_proxy_configs_unchanged(&db, &configs).await;
    assert_eq!(
        serde_json::to_value(db.get_global_proxy_config().await.unwrap()).unwrap(),
        serde_json::to_value(expected_global).unwrap()
    );

    config.listen_port = 23456;
    service
        .persist_ephemeral_listen_port_if_needed(&config, 34567)
        .await
        .unwrap();
    assert_eq!(
        db.get_global_proxy_config().await.unwrap().listen_port,
        23456
    );
    assert_app_proxy_configs_unchanged(&db, &configs).await;
}

#[tokio::test]
async fn unsupported_apps_are_rejected_before_proxy_side_effects() {
    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);

    assert!(service.set_takeover_for_app("gemini", true).await.is_err());
    assert!(!service.is_running().await);
    assert!(service
        .switch_proxy_target("gemini", "missing")
        .await
        .is_err());
}

async fn running_codex_base_url(service: &ProxyService) -> String {
    let status = service.get_status().await.expect("get proxy status");
    format!("http://127.0.0.1:{}/v1", status.port)
}

fn seed_codex_model_template() {
    let codex_dir = crate::codex_config::get_codex_config_dir();
    std::fs::create_dir_all(&codex_dir).expect("create codex dir");
    std::fs::write(
        codex_dir.join("models_cache.json"),
        serde_json::to_string(&serde_json::json!({
            "models": [{
                "slug": "gpt-5.5",
                "display_name": "GPT-5.5",
                "model_messages": { "instructions_template": "t" },
                "additional_speed_tiers": [],
                "context_window": 128000
            }]
        }))
        .expect("serialize models_cache"),
    )
    .expect("write models_cache.json");
}

#[test]
fn is_local_proxy_url_accepts_exact_local_hosts() {
    // 精确命中：默认网关写法 + 各等价本机 host
    for url in [
        "http://127.0.0.1:15721/v1",
        "http://127.0.0.1/v1",
        "http://localhost:15721/pi/anthropic",
        "http://localhost/v1",
        "http://0.0.0.0:15721",
        "http://[::1]:15721/v1",
        "http://[::]:15721",
        "  http://127.0.0.1:15721  ", // 前后空白应被 trim
    ] {
        assert!(
            ProxyService::is_local_proxy_url(url),
            "应为本地网关 URL: {url}"
        );
    }
}

#[test]
fn is_local_proxy_url_rejects_evil_hosts_and_non_local() {
    // 恶意 host：不得因前缀相似而误判（L1 核心）
    for url in [
        "http://127.0.0.1.evil.com:15721",
        "http://localhost.attacker.io/v1",
        "http://0.0.0.0.evil.com",
        "http://127.0.0.2.notlocal.test",
        "https://127.0.0.1:15721/v1", // 非 http scheme
        "https://localhost",
        "http://example.com/v1",
        "ftp://127.0.0.1/v1",
        "not a url",
        "",
    ] {
        assert!(
            !ProxyService::is_local_proxy_url(url),
            "不应视为本地网关 URL: {url}"
        );
    }
}

#[test]
fn is_local_proxy_url_keeps_any_port_semantics() {
    // 端口不参与判定（既有语义）：端口不匹配当前网关配置也判为本地，
    // 以便网关端口变更后仍能识别/清理历史接管占位符
    assert!(ProxyService::is_local_proxy_url("http://127.0.0.1:9999"));
    assert!(ProxyService::is_local_proxy_url("http://localhost:1"));
}

#[test]
fn managed_account_claude_takeover_uses_auth_token_placeholder() {
    let mut provider = Provider::with_id(
        "copilot".to_string(),
        "GitHub Copilot".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com",
                "ANTHROPIC_MODEL": "claude-haiku-4.5"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("github_copilot".to_string()),
        ..Default::default()
    });

    let mut live_config = provider.settings_config.clone();
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    assert_eq!(
        env.get("ANTHROPIC_AUTH_TOKEN")
            .and_then(|value| value.as_str()),
        Some(PROXY_TOKEN_PLACEHOLDER)
    );
    assert!(
            env.get("ANTHROPIC_API_KEY").is_none(),
            "API_KEY placeholders trigger Claude Code's custom-key approval prompt (defaults to No), landing users in Not logged in"
        );
}

#[test]
fn managed_account_claude_takeover_sources_copilot_models_from_provider() {
    let mut provider = Provider::with_id(
        "copilot".to_string(),
        "GitHub Copilot".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com",
                "ANTHROPIC_MODEL": "claude-sonnet-4.6",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4.5",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4.6",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-sonnet-4.6",
                "CLAUDE_CODE_SUBAGENT_MODEL": "claude-sonnet-4.6[1M]"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("github_copilot".to_string()),
        ..Default::default()
    });

    let mut live_config = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://stale.example.com",
            "ANTHROPIC_API_KEY": "stale-key",
            "ANTHROPIC_MODEL": "stale-model",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "stale-haiku",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "Stale Haiku",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "stale-sonnet",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Stale Sonnet",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "stale-opus",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "Stale Opus",
            "CLAUDE_CODE_SUBAGENT_MODEL": "stale-subagent"
        }
    });
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    assert_env_str(env, "ANTHROPIC_MODEL", None);
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        Some("claude-haiku-4-5"),
    );
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        Some("claude-haiku-4.5"),
    );
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        Some("claude-sonnet-5"),
    );
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
        Some("claude-sonnet-4.6"),
    );
    assert_env_str(env, "ANTHROPIC_DEFAULT_OPUS_MODEL", Some("claude-opus-5"));
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
        Some("claude-sonnet-4.6"),
    );
    assert_env_str(
        env,
        "CLAUDE_CODE_SUBAGENT_MODEL",
        Some("claude-sonnet-4.6[1M]"),
    );
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
    assert_env_str(env, "ANTHROPIC_API_KEY", None);
}

#[test]
fn managed_account_claude_takeover_removes_stale_subagent_model_when_provider_omits_it() {
    let mut provider = Provider::with_id(
        "codex".to_string(),
        "Codex".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "provider-sonnet"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("codex_oauth".to_string()),
        ..Default::default()
    });

    let mut live_config = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://stale.example.com",
            "ANTHROPIC_API_KEY": "stale-key",
            "CLAUDE_CODE_SUBAGENT_MODEL": "stale-subagent"
        }
    });
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    assert_env_str(env, "CLAUDE_CODE_SUBAGENT_MODEL", None);
}

#[test]
fn managed_account_claude_takeover_sources_codex_models_from_provider() {
    let mut provider = Provider::with_id(
        "codex".to_string(),
        "Codex".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex",
                "ANTHROPIC_MODEL": "gpt-5.4",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "gpt-5.4-mini",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gpt-5.4",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "gpt-5.4"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("codex_oauth".to_string()),
        ..Default::default()
    });

    let mut live_config = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://stale.example.com",
            "ANTHROPIC_AUTH_TOKEN": "stale-token",
            "ANTHROPIC_MODEL": "stale-model",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "stale-haiku",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "Stale Haiku",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "stale-sonnet",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Stale Sonnet",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "stale-opus",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "Stale Opus"
        }
    });
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    assert_env_str(env, "ANTHROPIC_MODEL", None);
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        Some("claude-haiku-4-5"),
    );
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        Some("gpt-5.4-mini"),
    );
    assert_env_str(
        env,
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        Some("claude-sonnet-5"),
    );
    assert_env_str(env, "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME", Some("gpt-5.4"));
    assert_env_str(env, "ANTHROPIC_DEFAULT_OPUS_MODEL", Some("claude-opus-5"));
    assert_env_str(env, "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME", Some("gpt-5.4"));
    // Codex 系只保留 AUTH_TOKEN；双键会触发 Claude Code 告警（#4919）
    assert_env_str(env, "ANTHROPIC_API_KEY", None);
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
}

#[test]
fn managed_account_claude_takeover_codex_injects_auth_token_without_preexisting_key() {
    let mut provider = Provider::with_id(
        "codex".to_string(),
        "Codex".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("codex_oauth".to_string()),
        ..Default::default()
    });

    // 全新安装/热切换形态：传入的 env 没有任何 token 键。
    let mut live_config = provider.settings_config.clone();
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    assert_env_str(env, "ANTHROPIC_API_KEY", None);
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
}

#[test]
fn managed_account_claude_takeover_xai_keeps_one_auth_key() {
    let mut provider = Provider::with_id(
        "xai".to_string(),
        "xAI".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.x.ai/v1"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("xai_oauth".to_string()),
        ..Default::default()
    });

    let mut live_config = json!({
        "env": {
            "ANTHROPIC_AUTH_TOKEN": "old-token",
            "ANTHROPIC_API_KEY": "old-key",
            "OPENAI_API_KEY": "old-openai-key"
        }
    });
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(Value::as_object)
        .expect("env should exist");
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
    assert_env_str(env, "ANTHROPIC_API_KEY", None);
    assert_env_str(env, "OPENAI_API_KEY", None);
}

#[test]
fn managed_account_claude_takeover_codex_by_base_url_keeps_auth_token() {
    // 无 provider_type meta、仅凭 base_url 识别为受管 codex 的供应商，
    // 也必须保留 AUTH_TOKEN 占位符（与策略选择共用同一判定族）。
    let provider = Provider::with_id(
        "codex-url-only".to_string(),
        "Codex (URL only)".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex"
            }
        }),
        None,
    );
    assert!(provider.uses_managed_account_auth());
    assert!(!provider.is_codex_oauth());

    let mut live_config = provider.settings_config.clone();
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    assert_env_str(env, "ANTHROPIC_API_KEY", None);
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
}

// #4919 复现场景：从第三方 Claude 供应商（live 已有 AUTH_TOKEN）切换到
// Codex 受管供应商时，只应保留 AUTH_TOKEN 占位符，不得同时写入 API_KEY。
#[test]
fn managed_account_claude_takeover_codex_from_third_party_keeps_single_auth_key() {
    let mut provider = Provider::with_id(
        "codex".to_string(),
        "Codex".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("codex_oauth".to_string()),
        ..Default::default()
    });

    let mut live_config = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "sk-third-party"
        }
    });
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
    assert_env_str(env, "ANTHROPIC_API_KEY", None);
}

#[test]
fn managed_account_claude_takeover_copilot_defaults_to_auth_token() {
    let mut provider = Provider::with_id(
        "copilot".to_string(),
        "GitHub Copilot".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("github_copilot".to_string()),
        ..Default::default()
    });

    let mut live_config = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://stale.example.com",
            "ANTHROPIC_AUTH_TOKEN": "stale-token",
            "ANTHROPIC_API_KEY": "stale-key"
        }
    });
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    // Default Copilot takeover injects AUTH_TOKEN: the API_KEY placeholder
    // triggers Claude Code's custom-key approval prompt (defaults to
    // "No (recommended)"), which lands users in "Not logged in".
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
    assert_env_str(env, "ANTHROPIC_API_KEY", None);
}

#[test]
fn managed_account_claude_takeover_copilot_honors_api_key_field_choice() {
    let mut provider = Provider::with_id(
        "copilot".to_string(),
        "GitHub Copilot".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        provider_type: Some("github_copilot".to_string()),
        api_key_field: Some("ANTHROPIC_API_KEY".to_string()),
        ..Default::default()
    });

    let mut live_config = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://stale.example.com",
            "ANTHROPIC_AUTH_TOKEN": "stale-token"
        }
    });
    ProxyService::apply_claude_takeover_fields_for_provider(
        &mut live_config,
        "http://127.0.0.1:15721",
        &provider,
    );

    let env = live_config
        .get("env")
        .and_then(|value| value.as_object())
        .expect("env should exist");
    // Explicit API-key-field choice keeps the API_KEY placeholder to avoid
    // conflicting with the /login-managed key (#1049).
    assert_env_str(env, "ANTHROPIC_API_KEY", Some(PROXY_TOKEN_PLACEHOLDER));
    assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", None);
}

#[test]
fn normal_claude_takeover_without_token_keeps_auth_token_fallback() {
    let mut live_config = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://api.example.com",
            "ANTHROPIC_MODEL": "claude-haiku-4.5"
        }
    });

    ProxyService::apply_claude_takeover_fields(
        &mut live_config,
        "http://127.0.0.1:15721",
        None,
        None,
    );

    assert_eq!(
        live_config
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN"))
            .and_then(|value| value.as_str()),
        Some(PROXY_TOKEN_PLACEHOLDER)
    );
    assert!(
        live_config
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_API_KEY"))
            .is_none(),
        "non-managed providers should retain the legacy fallback behavior"
    );
}

#[tokio::test]
#[serial]
async fn start_with_takeover_ephemeral_port_writes_actual_live_url() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());

    let provider = Provider::with_id(
        "p1".to_string(),
        "P1".to_string(),
        json!({
            "env": {
                "ANTHROPIC_API_KEY": "provider-key",
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
            }
        }),
        None,
    );
    db.save_provider("claude", &provider)
        .expect("save provider");
    db.set_current_provider("claude", "p1")
        .expect("set db current provider");
    crate::settings::set_current_provider(&AppType::Claude, Some("p1"))
        .expect("set local current provider");
    service
        .write_claude_live(&json!({
            "env": {
                "ANTHROPIC_API_KEY": "live-key",
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
            }
        }))
        .expect("seed claude live config");

    let info = service
        .start_with_takeover()
        .await
        .expect("start proxy with takeover");
    assert_ne!(info.port, 0, "OS should assign a concrete port");

    let stored_config = db.get_proxy_config().await.expect("read proxy config");
    assert_eq!(
        stored_config.listen_port, info.port,
        "resolved dynamic port should be persisted for DB-only proxy URL paths"
    );

    let live = service.read_claude_live().expect("read taken-over live");
    let base_url = live
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
        .and_then(|value| value.as_str())
        .expect("taken-over base url");
    assert_eq!(base_url, format!("http://127.0.0.1:{}", info.port));
    assert!(
        !base_url.contains(":0"),
        "takeover must never write an unresolved :0 port"
    );

    service
        .stop_with_restore()
        .await
        .expect("stop proxy and restore live config");
}

#[tokio::test]
#[serial]
async fn update_config_reprojection_waits_for_codex_switch_lock_before_rebuilding_live_auth() {
    use tokio::time::{sleep, timeout, Duration};

    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let state = crate::store::AppState::new(db.clone());
    state
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
        .await
        .expect("seed managed account");

    let mut provider = Provider::with_id(
        "managed-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    provider.category = Some("official".to_string());
    provider.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed".to_string()),
        }),
        ..Default::default()
    });
    db.save_provider(AppType::Codex.as_str(), &provider)
        .expect("save managed official provider");
    db.set_current_provider(AppType::Codex.as_str(), &provider.id)
        .expect("set DB current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
        .expect("set local current provider");

    let initial_info = state
        .proxy_service
        .start()
        .await
        .expect("start proxy server");
    db.save_live_backup(
        AppType::Codex.as_str(),
        &serde_json::to_string(&provider.settings_config).expect("serialize backup"),
    )
    .await
    .expect("seed takeover backup");
    let mut codex_proxy_config = db
        .get_proxy_config_for_app(AppType::Codex.as_str())
        .await
        .expect("get Codex proxy config");
    codex_proxy_config.enabled = true;
    db.update_proxy_config_for_app(codex_proxy_config)
        .await
        .expect("enable Codex takeover state");
    state
        .proxy_service
        .sync_codex_live_from_provider_while_proxy_active(&provider)
        .await
        .expect("seed managed Codex takeover Live config");
    assert!(crate::codex_config::get_codex_auth_path().exists());
    assert!(crate::codex_config::codex_managed_oauth_live_auth_marker_exists());

    let port_reservation =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve replacement port");
    let replacement_port = port_reservation
        .local_addr()
        .expect("read replacement port")
        .port();
    assert_ne!(replacement_port, initial_info.port);
    drop(port_reservation);

    let switch_guard = state
        .proxy_service
        .lock_switch_for_app(AppType::Codex.as_str())
        .await;
    let mut new_config = state
        .proxy_service
        .get_config()
        .await
        .expect("get current proxy config");
    new_config.listen_port = replacement_port;
    let service_for_update = state.proxy_service.clone();
    let mut update_task =
        tokio::spawn(async move { service_for_update.update_config(&new_config).await });

    timeout(Duration::from_secs(5), async {
        loop {
            let status = state
                .proxy_service
                .get_status()
                .await
                .expect("get restarted proxy status");
            if status.port == replacement_port {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("proxy should restart on the replacement port");
    sleep(Duration::from_millis(50)).await;
    assert!(
        !update_task.is_finished(),
        "update_config must wait for the Codex switch lock before re-projecting Live auth"
    );

    // Perform the credential-removal critical section while owning the same
    // lock. Once released, update_config must rebuild from current manager
    // state instead of a credential bundle prepared before removal.
    state
        .codex_oauth_manager
        .remove_account("acct-managed")
        .await
        .expect("remove managed account while switch lock is held");
    assert!(!crate::codex_config::get_codex_auth_path().exists());
    assert!(!crate::codex_config::codex_managed_oauth_live_auth_marker_exists());
    drop(switch_guard);

    let _update_result = timeout(Duration::from_secs(5), &mut update_task)
        .await
        .expect("update_config should finish after releasing the Codex switch lock")
        .expect("join update_config task");
    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "post-removal restart reprojection must not recreate auth.json"
    );
    assert!(
        !crate::codex_config::codex_managed_oauth_live_auth_marker_exists(),
        "post-removal restart reprojection must not recreate the ownership marker"
    );

    state.proxy_service.stop().await.expect("stop proxy server");
}

#[test]
#[serial]
fn codex_custom_provider_live_write_preserves_oauth_auth_json() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    crate::codex_config::write_codex_live_atomic(
        &oauth_auth,
        Some(
            r#"model_provider = "openai"
model = "gpt-5-codex"
"#,
        ),
    )
    .expect("seed live OAuth auth");

    let mut provider = Provider::with_id(
        "rightcode".to_string(),
        "RightCode".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "rightcode-key"
            },
            "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("custom".to_string());
    let takeover_settings = json!({
        "auth": {
            "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
        },
        "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
"#
    });

    service
        .write_codex_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write provider-driven Codex live config");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "third-party Codex proxy writes must not overwrite ChatGPT OAuth login state"
    );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains("experimental_bearer_token"),
        "proxy placeholder should move into config.toml instead of auth.json"
    );
    assert!(
        live_config.contains(PROXY_TOKEN_PLACEHOLDER),
        "live config should carry the proxy placeholder token"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[tokio::test]
#[serial]
async fn codex_takeover_preserves_oauth_auth_json_when_preserve_enabled() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
        .expect("seed live OAuth auth with DeepSeek config");

    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("cn_official".to_string());
    db.save_provider("codex", &provider)
        .expect("save DeepSeek provider");
    db.set_current_provider("codex", "deepseek")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
        .expect("set local current provider");

    service
        .takeover_live_config_strict(&AppType::Codex)
        .await
        .expect("take over Codex live config");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "Codex takeover should not overwrite ChatGPT OAuth auth when preservation is enabled"
    );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains(PROXY_TOKEN_PLACEHOLDER),
        "takeover placeholder should move into config.toml"
    );
    assert!(
        service.detect_takeover_in_live_config_for_app(&AppType::Codex),
        "Codex takeover detection should recognize config.toml placeholders"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[tokio::test]
#[serial]
async fn codex_takeover_preserves_oauth_auth_json_even_when_provider_category_is_official() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
        .expect("seed live OAuth auth with DeepSeek config");

    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("official".to_string());
    db.save_provider("codex", &provider)
        .expect("save misclassified DeepSeek provider");
    db.set_current_provider("codex", "deepseek")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
        .expect("set local current provider");

    service
        .takeover_live_config_strict(&AppType::Codex)
        .await
        .expect("take over Codex live config");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
            live_auth, oauth_auth,
            "Codex takeover must not rewrite auth.json when preservation is enabled, even if provider category is stale or misclassified"
        );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains(PROXY_TOKEN_PLACEHOLDER),
        "takeover placeholder should move into config.toml"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[tokio::test]
#[serial]
async fn codex_takeover_hot_switches_between_builtin_official_and_third_party() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    // Exercise the default setting: takeover itself must now preserve native
    // auth regardless of the legacy compatibility toggle.
    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some("model = \"gpt-5.4\"\n"))
        .expect("seed official live config");

    let mut official = Provider::with_id(
        "codex-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    official.category = Some("official".to_string());
    db.save_provider("codex", &official)
        .expect("save official provider");

    let mut third_party = Provider::with_id(
        "rightcode".to_string(),
        "RightCode".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "rightcode-key" },
            "config": r#"model_provider = "rightcode"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    third_party.category = Some("custom".to_string());
    db.save_provider("codex", &third_party)
        .expect("save third-party provider");
    db.set_current_provider("codex", "codex-official")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("codex-official"))
        .expect("set local current provider");

    service
        .set_takeover_for_app("codex", true)
        .await
        .expect("enable official takeover");

    let read_auth = || -> Value {
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth")
    };
    assert_eq!(read_auth(), oauth_auth);
    let official_live = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read official takeover config");
    assert!(crate::codex_config::codex_config_has_official_proxy_route(
        &official_live
    ));
    assert!(official_live.contains("requires_openai_auth = true"));
    assert!(!official_live.contains(PROXY_TOKEN_PLACEHOLDER));

    service
        .hot_switch_provider("codex", "rightcode")
        .await
        .expect("switch to third-party provider");
    assert_eq!(
        read_auth(),
        oauth_auth,
        "third-party route must preserve OAuth"
    );
    let third_party_live = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read third-party takeover config");
    assert!(third_party_live.contains(PROXY_TOKEN_PLACEHOLDER));
    assert!(!crate::codex_config::codex_config_has_official_proxy_route(
        &third_party_live
    ));

    service
        .hot_switch_provider("codex", "codex-official")
        .await
        .expect("switch back to official provider");
    assert_eq!(
        read_auth(),
        oauth_auth,
        "official switch must reuse native OAuth"
    );
    let official_live = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read restored official takeover config");
    assert!(crate::codex_config::codex_config_has_official_proxy_route(
        &official_live
    ));
    assert!(!official_live.contains(PROXY_TOKEN_PLACEHOLDER));

    service
        .set_takeover_for_app("codex", false)
        .await
        .expect("disable takeover");
    assert_eq!(read_auth(), oauth_auth);
}

#[tokio::test]
#[serial]
async fn codex_takeover_hot_switch_adopts_and_clears_outgoing_managed_auth() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
        .await
        .expect("seed managed account");

    let mut managed = Provider::with_id(
        "managed-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    managed.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed".to_string()),
        }),
        ..Default::default()
    });
    let mut third_party = Provider::with_id(
        "rightcode".to_string(),
        "RightCode".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "rightcode-key" },
            "config": r#"model_provider = "rightcode"
[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    third_party.category = Some("custom".to_string());
    db.save_provider("codex", &managed)
        .expect("save managed official");
    db.save_provider("codex", &third_party)
        .expect("save third party");
    db.set_current_provider("codex", &managed.id)
        .expect("set managed current");
    crate::settings::set_current_provider(&AppType::Codex, Some(&managed.id))
        .expect("set local current");
    crate::services::provider::write_live_with_common_config_for_codex_oauth_manager(
        db.as_ref(),
        &AppType::Codex,
        &managed,
        &service.codex_oauth_manager,
    )
    .expect("seed managed normal live config");

    service
        .set_takeover_for_app("codex", true)
        .await
        .expect("enable managed takeover");
    let id_token = crate::codex_config::test_codex_id_token("managed-user");
    crate::config::write_json_file(
        &crate::codex_config::get_codex_auth_path(),
        &crate::codex_config::codex_managed_oauth_auth_value(
            "acct-managed",
            "cli-access-r1",
            Some(&id_token),
            "cli-refresh-r1",
            "2099-02-01T00:00:00Z",
        ),
    )
    .expect("simulate CLI refresh during takeover");

    service
        .hot_switch_provider("codex", &third_party.id)
        .await
        .expect("hot switch managed official to third party");

    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "managed auth must not remain live after takeover hot-switch"
    );
    assert!(!crate::codex_config::codex_managed_oauth_live_auth_marker_exists());
    assert_eq!(
        service
            .codex_oauth_manager
            .test_refresh_token_for_account("acct-managed")
            .await
            .as_deref(),
        Some("cli-refresh-r1")
    );
    let backup = db
        .get_live_backup("codex")
        .await
        .expect("read backup")
        .expect("backup exists");
    let backup: Value = serde_json::from_str(&backup.original_config).expect("parse backup");
    assert_eq!(
        backup
            .pointer("/auth/OPENAI_API_KEY")
            .and_then(Value::as_str),
        Some("rightcode-key"),
        "stripping outgoing managed auth must preserve the target provider key"
    );
}

#[tokio::test]
#[serial]
async fn codex_active_takeover_hot_switches_between_managed_accounts() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed-a", "managed-access-a", "user-a")
        .await
        .expect("seed managed account A");
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed-b", "managed-access-b", "user-b")
        .await
        .expect("seed managed account B");

    let mut managed_a = Provider::with_id(
        "managed-a".to_string(),
        "OpenAI Official A".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    managed_a.category = Some("official".to_string());
    managed_a.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed-a".to_string()),
        }),
        ..Default::default()
    });
    let mut managed_b = Provider::with_id(
        "managed-official-b".to_string(),
        "OpenAI Official B".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    managed_b.category = Some("official".to_string());
    managed_b.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed-b".to_string()),
        }),
        ..Default::default()
    });
    db.save_provider("codex", &managed_a)
        .expect("save managed provider A");
    db.save_provider("codex", &managed_b)
        .expect("save managed provider B");
    db.set_current_provider("codex", &managed_a.id)
        .expect("set managed A current");
    crate::settings::set_current_provider(&AppType::Codex, Some(&managed_a.id))
        .expect("set local managed A current");
    crate::services::provider::write_live_with_common_config_for_codex_oauth_manager(
        db.as_ref(),
        &AppType::Codex,
        &managed_a,
        &service.codex_oauth_manager,
    )
    .expect("seed managed A normal live config");

    service
        .set_takeover_for_app("codex", true)
        .await
        .expect("enable managed A takeover");
    let id_token_a = crate::codex_config::test_codex_id_token("user-a");
    crate::config::write_json_file(
        &crate::codex_config::get_codex_auth_path(),
        &crate::codex_config::codex_managed_oauth_auth_value(
            "acct-managed-a",
            "cli-access-a1",
            Some(&id_token_a),
            "cli-refresh-a1",
            "2099-02-01T00:00:00Z",
        ),
    )
    .expect("simulate account A CLI refresh during takeover");

    service
        .hot_switch_provider("codex", &managed_b.id)
        .await
        .expect("hot switch managed A to managed B");

    assert_eq!(
        service
            .codex_oauth_manager
            .test_refresh_token_for_account("acct-managed-a")
            .await
            .as_deref(),
        Some("cli-refresh-a1"),
        "A's CLI-rotated refresh token must be adopted before B overwrites live auth"
    );
    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read managed B live auth");
    assert_eq!(
        live_auth
            .pointer("/tokens/account_id")
            .and_then(Value::as_str),
        Some("acct-managed-b")
    );
    assert_eq!(
        live_auth
            .pointer("/tokens/access_token")
            .and_then(Value::as_str),
        Some("managed-access-b")
    );
    assert!(
        crate::codex_config::codex_auth_matches_recorded_managed_oauth(
            &live_auth,
            "acct-managed-b"
        )
        .expect("check managed B marker"),
        "the live ownership marker must move to account B"
    );

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("read managed B backup")
        .expect("managed B backup exists");
    let backup_value: Value =
        serde_json::from_str(&backup.original_config).expect("parse managed B backup");
    assert!(
        backup_value.get("auth").is_none(),
        "managed official auth must remain live-only instead of freezing account A in backup"
    );
    assert!(
        !backup.original_config.contains("cli-access-a1")
            && !backup.original_config.contains("cli-refresh-a1")
            && !backup.original_config.contains("cli-id-a1"),
        "the restore backup must not contain account A's rotated auth generation"
    );
    assert_eq!(
        db.get_current_provider("codex")
            .expect("read DB current provider")
            .as_deref(),
        Some(managed_b.id.as_str())
    );
}

#[tokio::test]
#[serial]
async fn codex_half_takeover_hot_switch_to_managed_records_marker() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
        .await
        .expect("seed managed account");

    let previous = Provider::with_id(
        "previous".to_string(),
        "Previous".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "previous-key" },
            "config": "model_provider = \"previous\"\n"
        }),
        None,
    );
    let mut managed = Provider::with_id(
        "managed-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    managed.category = Some("official".to_string());
    managed.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed".to_string()),
        }),
        ..Default::default()
    });
    db.save_provider("codex", &previous)
        .expect("save previous provider");
    db.save_provider("codex", &managed)
        .expect("save managed provider");
    db.set_current_provider("codex", &previous.id)
        .expect("set previous current");
    crate::settings::set_current_provider(&AppType::Codex, Some(&previous.id))
        .expect("set local current");
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&previous.settings_config).expect("serialize backup"),
    )
    .await
    .expect("seed backup without live takeover marker");

    service
        .hot_switch_provider("codex", &managed.id)
        .await
        .expect("hot switch half-takeover to managed official");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read managed auth");
    assert!(
        crate::codex_config::codex_auth_matches_recorded_managed_oauth(&live_auth, "acct-managed")
            .expect("check managed marker"),
        "semi-takeover managed write must record the ownership marker"
    );
}

#[test]
fn codex_takeover_backup_preserves_api_key_login_material() {
    let mut target = json!({ "auth": {}, "config": "" });
    let existing = json!({
        "auth": { "OPENAI_API_KEY": "sk-real" },
        "config": "model = \"gpt-5.4\"\n"
    });

    ProxyService::preserve_codex_auth_in_backup(&mut target, &existing, true)
        .expect("preserve API-key auth");
    assert_eq!(target["auth"]["OPENAI_API_KEY"], "sk-real");
}

#[tokio::test]
#[serial]
async fn codex_official_backup_rebuild_keeps_live_auth_out_of_backup() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    crate::codex_config::write_codex_live_atomic(
        &json!({ "OPENAI_API_KEY": "sk-real" }),
        Some("model = \"gpt-5.4\"\n"),
    )
    .expect("seed live auth");
    let mut official = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "" }),
        None,
    );
    official.category = Some("official".to_string());

    service
        .update_live_backup_from_provider_inner("codex", &official, None)
        .await
        .expect("rebuild backup");
    let backup = db
        .get_live_backup("codex")
        .await
        .expect("read backup")
        .expect("backup exists");
    let value: Value = serde_json::from_str(&backup.original_config).expect("parse backup");
    assert!(
        value.get("auth").is_none(),
        "official login material must remain live-only"
    );
}

#[test]
#[serial]
fn codex_empty_restore_snapshot_does_not_delete_existing_auth_json() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let auth = json!({ "OPENAI_API_KEY": "sk-real" });
    crate::codex_config::write_codex_live_atomic(&auth, Some("model = \"gpt-5.4\"\n"))
        .expect("seed auth");

    service
        .write_codex_live_verbatim(&json!({
            "auth": {},
            "config": "model = \"gpt-5.4-mini\"\n"
        }))
        .expect("restore empty snapshot");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read auth");
    assert_eq!(live_auth, auth);
}

#[tokio::test]
#[serial]
async fn codex_sync_live_does_not_store_credentials_in_builtin_official_row() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let mut official = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "" }),
        None,
    );
    official.category = Some("official".to_string());
    db.save_provider("codex", &official).expect("save official");
    db.set_current_provider("codex", crate::database::CODEX_OFFICIAL_PROVIDER_ID)
        .expect("set current");
    crate::settings::set_current_provider(
        &AppType::Codex,
        Some(crate::database::CODEX_OFFICIAL_PROVIDER_ID),
    )
    .expect("set local current");
    crate::codex_config::write_codex_live_atomic(
        &json!({ "OPENAI_API_KEY": "sk-real" }),
        Some("model = \"gpt-5.4\"\n"),
    )
    .expect("seed live auth");

    service
        .sync_live_to_provider(&AppType::Codex)
        .await
        .expect("sync live");

    let stored = db
        .get_provider_by_id(crate::database::CODEX_OFFICIAL_PROVIDER_ID, "codex")
        .expect("read official")
        .expect("official exists");
    assert_eq!(stored.settings_config["auth"], json!({}));
}

#[tokio::test]
#[serial]
async fn codex_set_takeover_for_app_preserves_oauth_auth_json_when_preserve_enabled() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
        .expect("seed live OAuth auth with DeepSeek config");

    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("official".to_string());
    db.save_provider("codex", &provider)
        .expect("save misclassified DeepSeek provider");
    db.set_current_provider("codex", "deepseek")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
        .expect("set local current provider");

    service
        .set_takeover_for_app("codex", true)
        .await
        .expect("enable Codex takeover");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "the public takeover command path must not rewrite auth.json when preservation is enabled"
    );

    service
        .set_takeover_for_app("codex", false)
        .await
        .expect("disable Codex takeover");
    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[tokio::test]
#[serial]
async fn codex_takeover_enabled_commit_failure_restores_native_live_bundle() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
        .await
        .expect("seed managed account");

    let native_auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "native-id",
            "access_token": "native-access",
            "refresh_token": "native-refresh",
            "account_id": "acct-native"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    let native_config = "model = \"gpt-5.4\"\n";
    crate::codex_config::write_codex_live_atomic(&native_auth, Some(native_config))
        .expect("seed native live bundle");

    let mut provider = Provider::with_id(
        "managed-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": native_config }),
        None,
    );
    provider.category = Some("official".to_string());
    provider.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed".to_string()),
        }),
        ..Default::default()
    });
    db.save_provider("codex", &provider)
        .expect("save managed official");
    db.set_current_provider("codex", &provider.id)
        .expect("set DB current");
    crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
        .expect("set local current");

    {
        let conn = db.conn.lock().expect("lock database");
        conn.execute_batch(
            "CREATE TRIGGER reject_codex_takeover_enabled_commit
                 BEFORE UPDATE OF enabled ON proxy_config
                 WHEN NEW.app_type = 'codex' AND NEW.enabled = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'forced Codex takeover enabled failure');
                 END;",
        )
        .expect("install enabled failure trigger");
    }

    let error = service
        .set_takeover_for_app("codex", true)
        .await
        .expect_err("enabled DB failure must abort takeover");
    service.stop().await.expect("stop test proxy");

    assert!(
        error.contains("forced Codex takeover enabled failure"),
        "surface DB failure: {error}"
    );
    let restored = service
        .read_codex_live()
        .expect("read restored live bundle");
    assert_eq!(restored.get("auth"), Some(&native_auth));
    assert_eq!(
        restored.get("config").and_then(Value::as_str),
        Some(native_config)
    );
    assert!(
        db.get_live_backup("codex")
            .await
            .expect("read backup")
            .is_none(),
        "successful in-memory rollback should clean the takeover backup"
    );
    assert!(
        !db.get_proxy_config_for_app("codex")
            .await
            .expect("read Codex proxy config")
            .enabled
    );
}

#[tokio::test]
#[serial]
async fn codex_sync_current_to_live_during_takeover_preserves_oauth_auth_json() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let state = crate::store::AppState::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
        .expect("seed live OAuth auth with DeepSeek config");

    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("official".to_string());
    db.save_provider("codex", &provider)
        .expect("save misclassified DeepSeek provider");
    db.set_current_provider("codex", "deepseek")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
        .expect("set local current provider");

    state
        .proxy_service
        .set_takeover_for_app("codex", true)
        .await
        .expect("enable Codex takeover");

    crate::services::provider::ProviderService::sync_current_to_live(&state)
        .expect("sync current providers while Codex is taken over");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "post-change provider sync must not rewrite Codex auth.json during takeover"
    );

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let backup_value: Value = serde_json::from_str(&backup.original_config).expect("parse backup");
    assert_eq!(
        backup_value.get("auth"),
        Some(&oauth_auth),
        "provider-derived takeover backup should preserve official OAuth auth"
    );
    assert!(
        backup_value
            .get("config")
            .and_then(|value| value.as_str())
            .is_some_and(|config| config.contains("deepseek-key")),
        "provider token should be carried by config.toml in the restore backup"
    );

    state
        .proxy_service
        .set_takeover_for_app("codex", false)
        .await
        .expect("disable Codex takeover");
    let restored_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read restored auth");
    assert_eq!(
        restored_auth, oauth_auth,
        "turning takeover off should restore the preserved official OAuth auth"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[tokio::test]
#[serial]
async fn codex_sync_current_to_live_during_takeover_activation_keeps_proxy_live_config() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let state = crate::store::AppState::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
        .expect("seed live OAuth auth with DeepSeek config");

    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("official".to_string());
    db.save_provider("codex", &provider)
        .expect("save misclassified DeepSeek provider");
    db.set_current_provider("codex", "deepseek")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
        .expect("set local current provider");

    state
        .proxy_service
        .backup_live_config_strict(&AppType::Codex)
        .await
        .expect("backup Codex live config");
    state
        .proxy_service
        .takeover_live_config_strict(&AppType::Codex)
        .await
        .expect("take over Codex live config");
    assert!(
        !db.get_proxy_config_for_app("codex")
            .await
            .expect("get Codex proxy config")
            .enabled,
        "this reproduces the activation window before set_takeover_for_app marks enabled=true"
    );

    crate::services::provider::ProviderService::sync_current_to_live(&state)
        .expect("sync current providers during takeover activation");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "activation-time provider sync must not rewrite Codex OAuth auth.json"
    );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains(PROXY_TOKEN_PLACEHOLDER),
        "activation-time provider sync must keep the proxy bearer placeholder"
    );
    assert!(
        live_config.contains("http://127.0.0.1"),
        "activation-time provider sync must keep the local proxy base_url"
    );
    assert!(
        state
            .proxy_service
            .detect_takeover_in_live_config_for_app(&AppType::Codex),
        "Codex live config should still be detected as taken over"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[tokio::test]
#[serial]
async fn codex_set_takeover_rebuilds_stale_enabled_state_without_overwriting_backup() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    let original_deepseek_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
    let stale_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "PROXY_MANAGED"
"#;
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(stale_live_config))
        .expect("seed stale Codex live config");

    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("official".to_string());
    db.save_provider("codex", &provider)
        .expect("save misclassified DeepSeek provider");
    db.set_current_provider("codex", "deepseek")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
        .expect("set local current provider");
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": oauth_auth,
            "config": original_deepseek_config
        }))
        .expect("serialize original backup"),
    )
    .await
    .expect("seed original live backup");
    let mut proxy_config = db
        .get_proxy_config_for_app("codex")
        .await
        .expect("get Codex proxy config");
    proxy_config.enabled = true;
    db.update_proxy_config_for_app(proxy_config)
        .await
        .expect("mark Codex takeover enabled");

    service
        .set_takeover_for_app("codex", true)
        .await
        .expect("rebuild Codex takeover");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "repairing stale takeover must restore the preserved OAuth auth from backup"
    );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    let expected_base_url = running_codex_base_url(&service).await;
    assert!(
        live_config.contains(&expected_base_url),
        "stale enabled takeover must be rebuilt to the current proxy base_url"
    );
    assert!(
        live_config.contains(PROXY_TOKEN_PLACEHOLDER),
        "rebuilt takeover should keep the proxy bearer placeholder"
    );
    assert!(
        service
            .live_takeover_matches_current_proxy(&AppType::Codex)
            .await
            .expect("detect rebuilt Codex takeover"),
        "rebuilt Codex live config should match the active proxy address"
    );

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get Codex live backup")
        .expect("backup exists");
    let backup_value: Value = serde_json::from_str(&backup.original_config).expect("parse backup");
    assert_eq!(
        backup_value.get("auth"),
        Some(&oauth_auth),
        "rebuilding stale takeover must not overwrite the original OAuth backup"
    );
    assert!(
        backup_value
            .get("config")
            .and_then(|value| value.as_str())
            .is_some_and(
                |config| config.contains("deepseek-key") && !config.contains("http://127.0.0.1")
            ),
        "backup should remain the restorable DeepSeek config, not the proxy config"
    );

    service
        .set_takeover_for_app("codex", false)
        .await
        .expect("disable Codex takeover");
    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[tokio::test]
#[serial]
async fn codex_takeover_preserves_native_auth_even_when_legacy_toggle_is_disabled() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: false,
        ..Default::default()
    })
    .expect("disable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#;
    crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
        .expect("seed live OAuth auth with DeepSeek config");

    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("cn_official".to_string());
    db.save_provider("codex", &provider)
        .expect("save DeepSeek provider");
    db.set_current_provider("codex", "deepseek")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
        .expect("set local current provider");

    service
        .takeover_live_config_strict(&AppType::Codex)
        .await
        .expect("take over Codex live config");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "takeover must preserve native OAuth independently of the legacy toggle"
    );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains(PROXY_TOKEN_PLACEHOLDER),
        "third-party takeover should carry its local placeholder in config.toml"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[test]
#[serial]
fn codex_takeover_cleanup_removes_config_placeholder_without_touching_oauth_auth() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    crate::codex_config::write_codex_live_atomic(
        &oauth_auth,
        Some(
            r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
experimental_bearer_token = "PROXY_MANAGED"
"#,
        ),
    )
    .expect("seed taken-over Codex live config");

    assert!(
        service.detect_takeover_in_live_config_for_app(&AppType::Codex),
        "config.toml placeholder should be detected before cleanup"
    );

    service
        .cleanup_codex_takeover_placeholders_in_live()
        .expect("cleanup Codex takeover placeholders");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read live auth");
    assert_eq!(
        live_auth, oauth_auth,
        "cleanup should preserve ChatGPT OAuth auth"
    );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        !live_config.contains(PROXY_TOKEN_PLACEHOLDER),
        "cleanup should remove config.toml proxy bearer placeholder"
    );
    assert!(
        !live_config.contains("http://127.0.0.1:15721"),
        "cleanup should remove local proxy base_url"
    );
}

#[test]
#[serial]
fn codex_takeover_stamps_requires_openai_auth_false_when_auth_json_absent() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    // Preservation off: the direct switch that preceded takeover already
    // deleted auth.json (see plan_codex_live_write).
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: false,
        ..Default::default()
    })
    .expect("disable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    assert!(!crate::codex_config::get_codex_auth_path().exists());

    // Stored cards carry `requires_openai_auth = true` from the pre-0.149
    // presets; the takeover projection must not replay it onto a login-less
    // auth.json or Codex traps the TUI in the login screen.
    let mut provider = Provider::with_id(
        "kimi".to_string(),
        "Kimi".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "kimi-key" },
            "config": r#"model_provider = "kimi"
model = "kimi-k2"

[model_providers.kimi]
name = "Kimi"
base_url = "https://api.moonshot.cn/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }),
        None,
    );
    provider.category = Some("custom".to_string());

    let mut takeover_settings = provider.settings_config.clone();
    ProxyService::apply_codex_takeover_fields_for_provider(
        &mut takeover_settings,
        "http://127.0.0.1:15721/v1",
        &provider,
    )
    .expect("apply takeover fields");
    service
        .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write takeover live config");

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains("requires_openai_auth = false"),
        "no login on disk: the stored `true` must be stamped false; got:\n{live_config}"
    );
    assert!(
        live_config.contains(&format!(
            "experimental_bearer_token = \"{PROXY_TOKEN_PLACEHOLDER}\""
        )),
        "placeholder must still ride as the provider token; got:\n{live_config}"
    );
    assert!(live_config.contains("base_url = \"http://127.0.0.1:15721/v1\""));
    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "takeover must not create auth.json"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[test]
#[serial]
fn codex_takeover_stamps_requires_openai_auth_true_when_login_present() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access",
            "refresh_token": "oauth-refresh"
        }
    });
    crate::codex_config::write_codex_live_atomic(
        &oauth_auth,
        Some(
            r#"model_provider = "openai"
model = "gpt-5-codex"
"#,
        ),
    )
    .expect("seed live OAuth auth");

    // The card omits the flag: with a login on disk it is stamped true so
    // Codex keeps showing the account and refreshing the preserved tokens
    // (the placeholder bearer token still short-circuits request auth).
    let mut provider = Provider::with_id(
        "kimi".to_string(),
        "Kimi".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "kimi-key" },
            "config": r#"model_provider = "kimi"
model = "kimi-k2"

[model_providers.kimi]
name = "Kimi"
base_url = "https://api.moonshot.cn/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("custom".to_string());

    let mut takeover_settings = provider.settings_config.clone();
    ProxyService::apply_codex_takeover_fields_for_provider(
        &mut takeover_settings,
        "http://127.0.0.1:15721/v1",
        &provider,
    )
    .expect("apply takeover fields");
    service
        .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write takeover live config");

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains("requires_openai_auth = true"),
        "login on disk: the flag must be stamped true; got:\n{live_config}"
    );
    assert!(live_config.contains(&format!(
        "experimental_bearer_token = \"{PROXY_TOKEN_PLACEHOLDER}\""
    )));

    let auth_after: Value = serde_json::from_str(
        &std::fs::read_to_string(crate::codex_config::get_codex_auth_path())
            .expect("read auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(
        auth_after, oauth_auth,
        "takeover must leave the OAuth login untouched"
    );
}

#[test]
#[serial]
fn codex_takeover_does_not_stamp_true_over_bedrock_only_auth_json() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    // Bedrock credentials are a login for the Bedrock provider only: on
    // any requires_openai_auth provider Codex's account probe returns
    // UnsupportedBedrockApiKeyAuth and the TUI fails to start.
    let bedrock_auth = json!({ "bedrock_api_key": "bedrock-key" });
    crate::codex_config::write_codex_live_atomic(
        &bedrock_auth,
        Some("model_provider = \"amazon-bedrock\"\nmodel = \"claude\"\n"),
    )
    .expect("seed bedrock auth");

    let mut provider = Provider::with_id(
        "kimi".to_string(),
        "Kimi".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "kimi-key" },
            "config": r#"model_provider = "kimi"
model = "kimi-k2"

[model_providers.kimi]
name = "Kimi"
base_url = "https://api.moonshot.cn/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("custom".to_string());

    let mut takeover_settings = provider.settings_config.clone();
    ProxyService::apply_codex_takeover_fields_for_provider(
        &mut takeover_settings,
        "http://127.0.0.1:15721/v1",
        &provider,
    )
    .expect("apply takeover fields");
    service
        .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write takeover live config");

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        !live_config.contains("requires_openai_auth = true"),
        "bedrock-only auth.json must never be promoted to an OpenAI login; got:\n{live_config}"
    );
    assert!(live_config.contains("requires_openai_auth = false"));
    let auth_after: Value = serde_json::from_str(
        &std::fs::read_to_string(crate::codex_config::get_codex_auth_path())
            .expect("read auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(
        auth_after, bedrock_auth,
        "takeover must leave auth.json untouched"
    );
}

#[test]
#[serial]
fn codex_takeover_keeps_stored_flag_when_auth_store_is_keyring_backed() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    assert!(!crate::codex_config::get_codex_auth_path().exists());

    // Codex deletes auth.json after saving to the keyring, so an absent
    // file says nothing about login state under these store modes: the
    // stored `true` must survive instead of being rewritten to false
    // (which would hide a valid keyring login).
    for mode in ["keyring", "auto"] {
        let mut provider = Provider::with_id(
            "kimi".to_string(),
            "Kimi".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "kimi-key" },
                "config": format!(
                    r#"model_provider = "kimi"
model = "kimi-k2"
cli_auth_credentials_store = "{mode}"

[model_providers.kimi]
name = "Kimi"
base_url = "https://api.moonshot.cn/v1"
wire_api = "responses"
requires_openai_auth = true
"#
                )
            }),
            None,
        );
        provider.category = Some("custom".to_string());

        let mut takeover_settings = provider.settings_config.clone();
        ProxyService::apply_codex_takeover_fields_for_provider(
            &mut takeover_settings,
            "http://127.0.0.1:15721/v1",
            &provider,
        )
        .expect("apply takeover fields");
        service
            .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
            .expect("write takeover live config");

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains("requires_openai_auth = true"),
            "store mode {mode}: stored flag must be left alone; got:\n{live_config}"
        );
        assert!(
            live_config.contains(&format!("cli_auth_credentials_store = \"{mode}\"")),
            "store mode key must survive projection; got:\n{live_config}"
        );
        assert!(!crate::codex_config::get_codex_auth_path().exists());
    }
}

#[test]
#[serial]
fn codex_takeover_stamps_false_when_bedrock_outranks_stale_openai_key() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    // AuthDotJson::resolved_mode ranks bedrock_api_key above
    // OPENAI_API_KEY, so Codex loads this as Bedrock auth; a stamped
    // `true` would make account_state() fail with
    // UnsupportedBedrockApiKeyAuth.
    let mixed_auth = json!({
        "OPENAI_API_KEY": "sk-stale",
        "bedrock_api_key": "bedrock-key"
    });
    crate::codex_config::write_codex_live_atomic(
        &mixed_auth,
        Some("model_provider = \"amazon-bedrock\"\nmodel = \"claude\"\n"),
    )
    .expect("seed mixed auth");

    let mut provider = Provider::with_id(
        "kimi".to_string(),
        "Kimi".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "kimi-key" },
            "config": r#"model_provider = "kimi"
model = "kimi-k2"

[model_providers.kimi]
name = "Kimi"
base_url = "https://api.moonshot.cn/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }),
        None,
    );
    provider.category = Some("custom".to_string());

    let mut takeover_settings = provider.settings_config.clone();
    ProxyService::apply_codex_takeover_fields_for_provider(
        &mut takeover_settings,
        "http://127.0.0.1:15721/v1",
        &provider,
    )
    .expect("apply takeover fields");
    service
        .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write takeover live config");

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
            live_config.contains("requires_openai_auth = false"),
            "bedrock outranks the stale key: the stored `true` must be stamped false; got:\n{live_config}"
        );
    let auth_after: Value = serde_json::from_str(
        &std::fs::read_to_string(crate::codex_config::get_codex_auth_path())
            .expect("read auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(
        auth_after, mixed_auth,
        "takeover must leave auth.json untouched"
    );
}

#[test]
#[serial]
fn codex_takeover_keeps_stored_flag_under_auto_even_when_file_holds_login() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    // Under `auto` the keyring is consulted first and the file is only a
    // fallback: a keyring entry (say Bedrock) would shadow this login, so
    // the file alone cannot justify forcing the flag to true.
    let file_login = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access",
            "refresh_token": "oauth-refresh"
        }
    });
    crate::codex_config::write_codex_live_atomic(
        &file_login,
        Some("model_provider = \"openai\"\nmodel = \"gpt-5-codex\"\n"),
    )
    .expect("seed file login");

    let mut provider = Provider::with_id(
        "kimi".to_string(),
        "Kimi".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "kimi-key" },
            "config": r#"model_provider = "kimi"
model = "kimi-k2"
cli_auth_credentials_store = "auto"

[model_providers.kimi]
name = "Kimi"
base_url = "https://api.moonshot.cn/v1"
wire_api = "responses"
requires_openai_auth = false
"#
        }),
        None,
    );
    provider.category = Some("custom".to_string());

    let mut takeover_settings = provider.settings_config.clone();
    ProxyService::apply_codex_takeover_fields_for_provider(
        &mut takeover_settings,
        "http://127.0.0.1:15721/v1",
        &provider,
    )
    .expect("apply takeover fields");
    service
        .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write takeover live config");

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
            live_config.contains("requires_openai_auth = false"),
            "auto store: the stored flag must be left alone, not forced true by the fallback file; got:\n{live_config}"
        );
    assert!(!live_config.contains("requires_openai_auth = true"));
    let auth_after: Value = serde_json::from_str(
        &std::fs::read_to_string(crate::codex_config::get_codex_auth_path())
            .expect("read auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(
        auth_after, file_login,
        "takeover must leave auth.json untouched"
    );
}

#[test]
#[serial]
fn codex_takeover_tolerates_corrupt_auth_json_for_every_store_mode() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let auth_path = crate::codex_config::get_codex_auth_path();
    std::fs::create_dir_all(auth_path.parent().expect("codex dir")).expect("mkdir");
    // A leftover file Codex itself cannot parse. Keyring/ephemeral stores
    // never open it, and the file store treats the failed load as "no
    // stored auth"; in no case may it fail the takeover write.
    std::fs::write(&auth_path, "{").expect("seed corrupt auth.json");

    // (store line, expected flag after projection)
    let cases = [
        (
            "cli_auth_credentials_store = \"keyring\"\n",
            "requires_openai_auth = true",
        ),
        (
            "cli_auth_credentials_store = \"auto\"\n",
            "requires_openai_auth = true",
        ),
        (
            "cli_auth_credentials_store = \"ephemeral\"\n",
            "requires_openai_auth = false",
        ),
        ("", "requires_openai_auth = false"),
    ];
    for (store_line, expected) in cases {
        let mut provider = Provider::with_id(
            "kimi".to_string(),
            "Kimi".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "kimi-key" },
                "config": format!(
                    r#"model_provider = "kimi"
model = "kimi-k2"
{store_line}
[model_providers.kimi]
name = "Kimi"
base_url = "https://api.moonshot.cn/v1"
wire_api = "responses"
requires_openai_auth = true
"#
                )
            }),
            None,
        );
        provider.category = Some("custom".to_string());

        let mut takeover_settings = provider.settings_config.clone();
        ProxyService::apply_codex_takeover_fields_for_provider(
            &mut takeover_settings,
            "http://127.0.0.1:15721/v1",
            &provider,
        )
        .expect("apply takeover fields");
        service
            .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
            .unwrap_or_else(|error| {
                panic!("store {store_line:?}: corrupt auth.json must not fail the write: {error}")
            });

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains(expected),
            "store {store_line:?}: expected `{expected}`; got:\n{live_config}"
        );
        assert_eq!(
            std::fs::read_to_string(&auth_path).expect("read auth.json"),
            "{",
            "takeover must leave the corrupt file untouched"
        );
    }
}

#[test]
#[serial]
fn codex_takeover_never_raises_flag_on_proxy_injected_oauth_card_with_login_on_disk() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    // A preserved ChatGPT login is on disk (file store), which would
    // stamp an ordinary third-party card to `true`.
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access",
            "refresh_token": "oauth-refresh"
        }
    });
    crate::codex_config::write_codex_live_atomic(
        &oauth_auth,
        Some("model_provider = \"openai\"\nmodel = \"gpt-5-codex\"\n"),
    )
    .expect("seed live OAuth auth");

    // Preset snapshot of a proxy-injected OAuth card still carrying the
    // pre-0.149 `true`; the effective builder neutralizes it to `false`
    // before the takeover writer ever sees it.
    let stored = r#"model_provider = "xai"
model = "grok-4"

[model_providers.xai]
name = "xAI"
base_url = "https://api.x.ai/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
    let neutralized =
        crate::codex_config::neutralize_codex_official_auth_fallback_for_proxy_oauth(stored)
            .expect("legacy true is neutralized");
    let mut provider = Provider::with_id(
        "xai".to_string(),
        "xAI".to_string(),
        json!({ "auth": {}, "config": neutralized }),
        None,
    );
    provider.category = Some("custom".to_string());
    provider.meta = Some(ProviderMeta {
        provider_type: Some("xai_oauth".to_string()),
        ..Default::default()
    });
    assert!(provider.uses_proxy_injected_oauth());

    let mut takeover_settings = provider.settings_config.clone();
    ProxyService::apply_codex_takeover_fields_for_provider(
        &mut takeover_settings,
        "http://127.0.0.1:15721/v1",
        &provider,
    )
    .expect("apply takeover fields");
    service
        .write_codex_takeover_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write takeover live config");

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains("requires_openai_auth = false"),
        "proxy-injected OAuth card must keep its neutralized flag; got:\n{live_config}"
    );
    assert!(
        !live_config.contains("requires_openai_auth = true"),
        "a login on disk must not raise the flag on a keyless OAuth card; got:\n{live_config}"
    );
    assert!(
        live_config.contains(&format!(
            "experimental_bearer_token = \"{PROXY_TOKEN_PLACEHOLDER}\""
        )),
        "placeholder must still ride as the provider token; got:\n{live_config}"
    );
    let auth_after: Value = serde_json::from_str(
        &std::fs::read_to_string(crate::codex_config::get_codex_auth_path())
            .expect("read auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(
        auth_after, oauth_auth,
        "takeover must leave the login untouched"
    );
}

#[test]
#[serial]
fn codex_custom_provider_live_write_removes_auth_when_preserve_disabled() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: false,
        ..Default::default()
    })
    .expect("disable Codex official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let oauth_auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "oauth-id",
            "access_token": "oauth-access"
        }
    });
    crate::codex_config::write_codex_live_atomic(
        &oauth_auth,
        Some(
            r#"model_provider = "openai"
model = "gpt-5-codex"
"#,
        ),
    )
    .expect("seed live OAuth auth");

    let mut provider = Provider::with_id(
        "rightcode".to_string(),
        "RightCode".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "rightcode-key"
            },
            "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.category = Some("custom".to_string());
    let takeover_auth = json!({
        "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
    });
    let takeover_settings = json!({
        "auth": takeover_auth,
        "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
"#
    });

    service
        .write_codex_live_for_provider(&takeover_settings, Some(&provider))
        .expect("write provider-driven Codex live config");

    // Disabled preservation historically overwrote the OAuth login with
    // the placeholder; config-only switching removes auth.json instead —
    // the login is equally gone, and the placeholder now travels as the
    // provider-scoped bearer token that Codex >= 0.149 actually sends.
    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "disabled preservation removes auth.json on a third-party takeover write"
    );

    let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read live config");
    assert!(
        live_config.contains(&format!(
            "experimental_bearer_token = \"{PROXY_TOKEN_PLACEHOLDER}\""
        )),
        "the placeholder must ride in config.toml as the provider token; got:\n{live_config}"
    );

    crate::settings::update_settings(crate::settings::AppSettings::default())
        .expect("reset settings");
}

#[test]
fn update_toml_base_url_updates_active_model_provider_base_url() {
    let input = r#"
model_provider = "any"
model = "gpt-5.1-codex"
disable_response_storage = true

[model_providers.any]
name = "any"
base_url = "https://anyrouter.top/v1"
wire_api = "responses"
requires_openai_auth = true
"#;

    let new_url = "http://127.0.0.1:5000/v1";
    let output = crate::codex_config::update_codex_toml_field(input, "base_url", new_url)
        .expect("update base_url");

    let parsed: toml::Value = toml::from_str(&output).expect("updated config should be valid TOML");

    let base_url = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .and_then(|v| v.get("base_url"))
        .and_then(|v| v.as_str())
        .expect("model_providers.any.base_url should exist");

    assert_eq!(base_url, new_url);
    assert!(
        parsed.get("base_url").is_none(),
        "should not write top-level base_url"
    );

    let wire_api = parsed
        .get("model_providers")
        .and_then(|v| v.get("any"))
        .and_then(|v| v.get("wire_api"))
        .and_then(|v| v.as_str())
        .expect("model_providers.any.wire_api should exist");
    assert_eq!(wire_api, "responses");
}

#[test]
fn codex_takeover_without_provider_selects_a_local_authenticated_route() {
    for input in [
            "",
            "model = \"gpt-5\"\nbase_url = \"https://old.example/v1\"\n",
            "model_providers = { cc-switch = { name = \"Existing\", base_url = \"https://keep.example/v1\" } }\n",
        ] {
            let url = "http://127.0.0.1:15721/v1";
            let projected = ProxyService::apply_codex_proxy_toml_config_for_provider(input, url, None).unwrap();
            let auth = json!({"OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER});
            let live = crate::codex_config::prepare_codex_provider_live_config(&auth, &projected).unwrap();
            println!("takeover_fixture={}", serde_json::to_string(&live).unwrap());
            let doc: toml::Value = toml::from_str(&live).unwrap();
            let id = doc["model_provider"].as_str().expect("explicit provider");
            assert_ne!(id, "openai");
            let table = &doc["model_providers"][id];
            assert_eq!(table["base_url"].as_str(), Some(url));
            assert_eq!(table["wire_api"].as_str(), Some("responses"));
            assert_eq!(table["experimental_bearer_token"].as_str(), Some(PROXY_TOKEN_PLACEHOLDER));
            if input.contains("Existing") {
                assert_eq!(doc["model_providers"]["cc-switch"]["base_url"].as_str(), Some("https://keep.example/v1"));
            }
            let repeated = ProxyService::apply_codex_proxy_toml_config_for_provider(&live, url, None).unwrap();
            let repeated = crate::codex_config::prepare_codex_provider_live_config(&auth, &repeated).unwrap();
            assert_eq!(toml::from_str::<toml::Value>(&repeated).unwrap(), doc);
        }
}

#[test]
fn apply_codex_proxy_toml_config_forces_local_responses_wire_api() {
    let input = r#"
model_provider = "chat_only"
model = "gpt-5.1-codex"

[model_providers.chat_only]
name = "Chat Only"
base_url = "https://chat-only.example/v1"
wire_api = "chat"
"#;

    let proxy_url = "http://127.0.0.1:5000/v1";
    let output = ProxyService::apply_codex_proxy_toml_config_for_provider(input, proxy_url, None)
        .expect("apply proxy config");
    let parsed: toml::Value = toml::from_str(&output).expect("updated config should be valid TOML");

    let provider = parsed
        .get("model_providers")
        .and_then(|v| v.get("chat_only"))
        .expect("model_providers.chat_only should exist");

    assert_eq!(
        provider.get("base_url").and_then(|v| v.as_str()),
        Some(proxy_url)
    );
    assert_eq!(
        provider.get("wire_api").and_then(|v| v.as_str()),
        Some("responses")
    );
}

#[test]
fn apply_codex_proxy_toml_config_routes_builtin_official_with_native_auth() {
    let mut provider = Provider::with_id(
        "codex-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "" }),
        None,
    );
    provider.category = Some("official".to_string());
    let proxy_url = "http://127.0.0.1:5000/v1";

    let output = ProxyService::apply_codex_proxy_toml_config_for_provider(
        "experimental_bearer_token = \"PROXY_MANAGED\"\n",
        proxy_url,
        Some(&provider),
    )
    .expect("apply official proxy config");
    let parsed: toml::Value = toml::from_str(&output).expect("valid official route");
    let route_id = crate::codex_config::CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID;
    let route = &parsed["model_providers"][route_id];

    assert_eq!(parsed["model_provider"].as_str(), Some(route_id));
    assert_eq!(route["base_url"].as_str(), Some(proxy_url));
    assert_eq!(route["requires_openai_auth"].as_bool(), Some(true));
    assert!(parsed.get("experimental_bearer_token").is_none());
}

#[test]
fn apply_codex_proxy_toml_config_fails_closed_for_invalid_official_config() {
    let mut provider = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "" }),
        None,
    );
    provider.category = Some("official".to_string());

    let result = ProxyService::apply_codex_proxy_toml_config_for_provider(
        "model_providers = 3\n",
        "http://127.0.0.1:5000/v1",
        Some(&provider),
    );
    assert!(result.is_err());
}

#[test]
fn apply_codex_proxy_toml_config_keeps_upstream_model_for_chat_provider() {
    let input = r#"
model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#;
    let mut provider = Provider::with_id(
        "deepseek".to_string(),
        "DeepSeek".to_string(),
        json!({
            "config": input
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        api_format: Some("openai_chat".to_string()),
        ..Default::default()
    });

    let proxy_url = "http://127.0.0.1:5000/v1";
    let output =
        ProxyService::apply_codex_proxy_toml_config_for_provider(input, proxy_url, Some(&provider))
            .expect("apply chat proxy config");
    let parsed: toml::Value = toml::from_str(&output).expect("updated config should be valid TOML");

    assert_eq!(
        parsed.get("model").and_then(|v| v.as_str()),
        Some("deepseek-v4-flash")
    );
    assert_eq!(
        parsed
            .get("model_providers")
            .and_then(|v| v.get("deepseek"))
            .and_then(|v| v.get("base_url"))
            .and_then(|v| v.as_str()),
        Some(proxy_url)
    );
}

#[test]
fn apply_codex_proxy_toml_config_preserves_model_for_responses_provider() {
    let input = r#"
model_provider = "responses"
model = "upstream-responses-model"

[model_providers.responses]
name = "Responses"
base_url = "https://responses.example/v1"
wire_api = "responses"
"#;
    let mut provider = Provider::with_id(
        "responses".to_string(),
        "Responses".to_string(),
        json!({
            "config": input
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        api_format: Some("openai_responses".to_string()),
        ..Default::default()
    });

    let output = ProxyService::apply_codex_proxy_toml_config_for_provider(
        input,
        "http://127.0.0.1:5000/v1",
        Some(&provider),
    )
    .expect("apply responses proxy config");
    let parsed: toml::Value = toml::from_str(&output).expect("updated config should be valid TOML");

    assert_eq!(
        parsed.get("model").and_then(|v| v.as_str()),
        Some("upstream-responses-model")
    );
}

#[test]
fn apply_codex_proxy_toml_config_restores_upstream_model_for_responses_provider() {
    let input = r#"
model_provider = "responses"
model = "gpt-5.4"

[model_providers.responses]
name = "Responses"
base_url = "http://127.0.0.1:5000/v1"
wire_api = "responses"
"#;
    let mut provider = Provider::with_id(
        "responses".to_string(),
        "Responses".to_string(),
        json!({
            "config": r#"model_provider = "responses"
model = "upstream-responses-model"

[model_providers.responses]
name = "Responses"
base_url = "https://responses.example/v1"
wire_api = "responses"
"#
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        api_format: Some("openai_responses".to_string()),
        ..Default::default()
    });

    let output = ProxyService::apply_codex_proxy_toml_config_for_provider(
        input,
        "http://127.0.0.1:5000/v1",
        Some(&provider),
    )
    .expect("restore responses model");
    let parsed: toml::Value = toml::from_str(&output).expect("updated config should be valid TOML");

    assert_eq!(
        parsed.get("model").and_then(|v| v.as_str()),
        Some("upstream-responses-model")
    );
}

#[test]
fn update_toml_base_url_uses_implicit_openai_override() {
    let input = r#"
model = "gpt-5.1-codex"
"#;

    let new_url = "http://127.0.0.1:5000/v1";
    let output = crate::codex_config::update_codex_toml_field(input, "base_url", new_url)
        .expect("update implicit openai base_url");

    let parsed: toml::Value = toml::from_str(&output).expect("updated config should be valid TOML");

    let base_url = parsed
        .get("openai_base_url")
        .and_then(|v| v.as_str())
        .expect("openai_base_url should exist");

    assert_eq!(base_url, new_url);
}

#[tokio::test]
#[serial]
async fn sync_claude_token_does_not_add_anthropic_api_key() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider = Provider::with_id(
        "p1".to_string(),
        "P1".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                "ANTHROPIC_AUTH_TOKEN": "stale"
            }
        }),
        None,
    );
    db.save_provider("claude", &provider)
        .expect("save provider");
    db.set_current_provider("claude", "p1")
        .expect("set current provider");

    let live_config = json!({
        "env": {
            "ANTHROPIC_AUTH_TOKEN": "fresh"
        }
    });

    service
        .sync_live_config_to_provider(&AppType::Claude, &live_config)
        .await
        .expect("sync");

    let updated = db
        .get_provider_by_id("p1", "claude")
        .expect("get provider")
        .expect("provider exists");
    let env = updated
        .settings_config
        .get("env")
        .and_then(|v| v.as_object())
        .expect("env object");

    assert_eq!(
        env.get("ANTHROPIC_AUTH_TOKEN").and_then(|v| v.as_str()),
        Some("fresh")
    );
    assert!(
        !env.contains_key("ANTHROPIC_API_KEY"),
        "should not add ANTHROPIC_API_KEY when absent"
    );
}

#[tokio::test]
#[serial]
async fn sync_claude_token_respects_existing_api_key_field() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider = Provider::with_id(
        "p1".to_string(),
        "P1".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                "ANTHROPIC_API_KEY": "stale"
            }
        }),
        None,
    );
    db.save_provider("claude", &provider)
        .expect("save provider");
    db.set_current_provider("claude", "p1")
        .expect("set current provider");

    let live_config = json!({
        "env": {
            "ANTHROPIC_AUTH_TOKEN": "fresh"
        }
    });

    service
        .sync_live_config_to_provider(&AppType::Claude, &live_config)
        .await
        .expect("sync");

    let updated = db
        .get_provider_by_id("p1", "claude")
        .expect("get provider")
        .expect("provider exists");
    let env = updated
        .settings_config
        .get("env")
        .and_then(|v| v.as_object())
        .expect("env object");

    assert_eq!(
        env.get("ANTHROPIC_API_KEY").and_then(|v| v.as_str()),
        Some("fresh")
    );
    assert!(
        !env.contains_key("ANTHROPIC_AUTH_TOKEN"),
        "should not add ANTHROPIC_AUTH_TOKEN when absent"
    );
}

#[tokio::test]
#[serial]
async fn switch_proxy_target_updates_live_backup_when_taken_over() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider_a = Provider::with_id(
        "a".to_string(),
        "A".to_string(),
        json!({
            "env": {
                "ANTHROPIC_API_KEY": "a-key"
            }
        }),
        None,
    );
    let provider_b = Provider::with_id(
        "b".to_string(),
        "B".to_string(),
        json!({
            "env": {
                "ANTHROPIC_API_KEY": "b-key"
            }
        }),
        None,
    );
    db.save_provider("claude", &provider_a)
        .expect("save provider a");
    db.save_provider("claude", &provider_b)
        .expect("save provider b");
    db.set_current_provider("claude", "a")
        .expect("set current provider");

    // 模拟"已接管"状态：存在 Live 备份（内容不重要，会被热切换更新）
    db.save_live_backup("claude", "{\"env\":{}}")
        .await
        .expect("seed live backup");

    service
        .switch_proxy_target("claude", "b")
        .await
        .expect("switch proxy target");

    // 断言：本地 settings 的 current provider 已同步
    assert_eq!(
        crate::settings::get_current_provider(&AppType::Claude).as_deref(),
        Some("b")
    );

    // 断言：Live 备份已更新为目标供应商配置（用于 stop_with_restore 恢复）
    let backup = db
        .get_live_backup("claude")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let expected = serde_json::to_string(&provider_b.settings_config).expect("serialize");
    assert_eq!(backup.original_config, expected);
}

#[tokio::test]
#[serial]
async fn hot_switch_provider_updates_claude_live_while_preserving_takeover_fields() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider_a = Provider::with_id(
        "a".to_string(),
        "A".to_string(),
        json!({
            "env": {
                "ANTHROPIC_API_KEY": "a-key",
                "ANTHROPIC_BASE_URL": "https://api.a.example",
                "ANTHROPIC_MODEL": "claude-old"
            },
            "permissions": { "allow": ["Bash"] }
        }),
        None,
    );
    let provider_b = Provider::with_id(
        "b".to_string(),
        "B".to_string(),
        json!({
            "env": {
                "ANTHROPIC_API_KEY": "b-key",
                "ANTHROPIC_BASE_URL": "https://api.b.example",
                "ANTHROPIC_MODEL": "claude-new",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "deepseek-v4-flash",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "DeepSeek V4 Flash",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "deepseek-v4-pro[1M]",
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "DeepSeek V4 Pro",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "deepseek-v4-ultra [1m]",
                "CLAUDE_CODE_SUBAGENT_MODEL": "deepseek-v4-pro[1M]"
            },
            "permissions": { "allow": ["Read"] }
        }),
        None,
    );

    db.save_provider("claude", &provider_a)
        .expect("save provider a");
    db.save_provider("claude", &provider_b)
        .expect("save provider b");
    db.set_current_provider("claude", "a")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Claude, Some("a"))
        .expect("set local current provider");
    db.save_live_backup(
        "claude",
        &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
    )
    .await
    .expect("seed live backup");
    service
        .write_claude_live(&json!({
            "env": {
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721",
                "ANTHROPIC_API_KEY": PROXY_TOKEN_PLACEHOLDER,
                "ANTHROPIC_MODEL": "stale-model",
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Stale Sonnet",
                "CLAUDE_CODE_SUBAGENT_MODEL": "stale-subagent"
            },
            "permissions": { "allow": ["Bash"] }
        }))
        .expect("seed taken-over live file");

    service
        .hot_switch_provider("claude", "b")
        .await
        .expect("hot switch provider");

    let live = service.read_claude_live().expect("read live config");
    assert_eq!(
        live.get("permissions"),
        provider_b.settings_config.get("permissions"),
        "provider-derived live settings should be refreshed"
    );
    assert_eq!(
        live.get("env")
            .and_then(|env| env.get("ANTHROPIC_API_KEY"))
            .and_then(|v| v.as_str()),
        Some(PROXY_TOKEN_PLACEHOLDER),
        "takeover token placeholder should be preserved"
    );
    assert_eq!(
        live.get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(|v| v.as_str()),
        Some("http://127.0.0.1:15721"),
        "takeover proxy URL should remain active"
    );
    assert!(
        live.get("env")
            .and_then(|env| env.get("ANTHROPIC_MODEL"))
            .is_none(),
        "fallback model override should be removed in takeover mode"
    );
    let live_env = live
        .get("env")
        .and_then(|env| env.as_object())
        .expect("live env");
    assert_eq!(
        live_env
            .get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
            .and_then(|v| v.as_str()),
        Some("claude-haiku-4-5"),
        "takeover mode should expose a stable Haiku role model"
    );
    assert_eq!(
        live_env
            .get("ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME")
            .and_then(|v| v.as_str()),
        Some("DeepSeek V4 Flash"),
        "model menu should show the current provider Haiku display name"
    );
    assert_eq!(
        live_env
            .get("ANTHROPIC_DEFAULT_SONNET_MODEL")
            .and_then(|v| v.as_str()),
        Some("claude-sonnet-5[1M]"),
        "Sonnet role should carry the local 1M declaration for Claude Code"
    );
    assert_eq!(
        live_env
            .get("ANTHROPIC_DEFAULT_SONNET_MODEL_NAME")
            .and_then(|v| v.as_str()),
        Some("DeepSeek V4 Pro"),
        "stale model display names should be replaced during hot switch"
    );
    assert_eq!(
        live_env
            .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
            .and_then(|v| v.as_str()),
        Some("claude-opus-5[1M]"),
        "Opus role should preserve the current provider 1M capability marker"
    );
    assert_eq!(
        live_env
            .get("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME")
            .and_then(|v| v.as_str()),
        Some("deepseek-v4-ultra"),
        "implicit display names should strip the local 1M marker"
    );
    assert_eq!(
        live_env
            .get("CLAUDE_CODE_SUBAGENT_MODEL")
            .and_then(|v| v.as_str()),
        Some("deepseek-v4-pro[1M]"),
        "subagent model should follow the target provider during hot switch"
    );

    let backup = db
        .get_live_backup("claude")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let expected = serde_json::to_string(&provider_b.settings_config).expect("serialize");
    assert_eq!(backup.original_config, expected);
}

#[tokio::test]
#[serial]
async fn hot_switch_provider_serializes_same_app_switches() {
    use tokio::time::{sleep, Duration};

    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider_a = Provider::with_id(
        "a".to_string(),
        "A".to_string(),
        json!({ "env": { "ANTHROPIC_API_KEY": "a-key" } }),
        None,
    );
    let provider_b = Provider::with_id(
        "b".to_string(),
        "B".to_string(),
        json!({ "env": { "ANTHROPIC_API_KEY": "b-key" } }),
        None,
    );
    let provider_c = Provider::with_id(
        "c".to_string(),
        "C".to_string(),
        json!({ "env": { "ANTHROPIC_API_KEY": "c-key" } }),
        None,
    );

    db.save_provider("claude", &provider_a)
        .expect("save provider a");
    db.save_provider("claude", &provider_b)
        .expect("save provider b");
    db.save_provider("claude", &provider_c)
        .expect("save provider c");
    db.set_current_provider("claude", "a")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Claude, Some("a"))
        .expect("set local current provider");
    db.save_live_backup("claude", "{\"env\":{}}")
        .await
        .expect("seed live backup");

    let guard = service.lock_switch_for_test("claude").await;
    let service_for_b = service.clone();
    let service_for_c = service.clone();

    let switch_b = tokio::spawn(async move {
        service_for_b
            .hot_switch_provider("claude", "b")
            .await
            .expect("switch to b")
    });
    sleep(Duration::from_millis(20)).await;
    let switch_c = tokio::spawn(async move {
        service_for_c
            .hot_switch_provider("claude", "c")
            .await
            .expect("switch to c")
    });

    sleep(Duration::from_millis(20)).await;
    drop(guard);

    let outcome_b = switch_b.await.expect("join switch b");
    let outcome_c = switch_c.await.expect("join switch c");
    assert!(outcome_b.logical_target_changed);
    assert!(outcome_c.logical_target_changed);

    assert_eq!(
        crate::settings::get_effective_current_provider(&db, &AppType::Claude)
            .expect("effective current"),
        Some("c".to_string())
    );
    assert_eq!(
        crate::settings::get_current_provider(&AppType::Claude).as_deref(),
        Some("c")
    );
    assert_eq!(
        db.get_current_provider("claude").expect("db current"),
        Some("c".to_string())
    );

    let backup = db
        .get_live_backup("claude")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let expected = serde_json::to_string(&provider_c.settings_config).expect("serialize");
    assert_eq!(backup.original_config, expected);
}

#[tokio::test]
#[serial]
async fn restore_waits_for_hot_switch_and_restores_latest_backup() {
    use tokio::time::{sleep, Duration};

    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider_a = Provider::with_id(
        "a".to_string(),
        "A".to_string(),
        json!({ "env": { "ANTHROPIC_API_KEY": "a-key" } }),
        None,
    );
    let provider_b = Provider::with_id(
        "b".to_string(),
        "B".to_string(),
        json!({ "env": { "ANTHROPIC_API_KEY": "b-key" } }),
        None,
    );

    db.save_provider("claude", &provider_a)
        .expect("save provider a");
    db.save_provider("claude", &provider_b)
        .expect("save provider b");
    db.set_current_provider("claude", "a")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Claude, Some("a"))
        .expect("set local current provider");
    db.save_live_backup(
        "claude",
        &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
    )
    .await
    .expect("seed live backup");
    service
        .write_claude_live(&json!({ "env": { "ANTHROPIC_API_KEY": "stale" } }))
        .expect("seed live file");

    let guard = service.lock_switch_for_test("claude").await;
    let service_for_switch = service.clone();
    let service_for_restore = service.clone();

    let switch_to_b = tokio::spawn(async move {
        service_for_switch
            .hot_switch_provider("claude", "b")
            .await
            .expect("switch to b")
    });
    sleep(Duration::from_millis(20)).await;
    let restore = tokio::spawn(async move {
        service_for_restore
            .restore_live_config_for_app_with_fallback(&AppType::Claude)
            .await
            .expect("restore claude live")
    });

    sleep(Duration::from_millis(20)).await;
    drop(guard);

    let outcome = switch_to_b.await.expect("join switch");
    restore.await.expect("join restore");
    assert!(outcome.logical_target_changed);

    assert_eq!(
        crate::settings::get_effective_current_provider(&db, &AppType::Claude)
            .expect("effective current"),
        Some("b".to_string())
    );

    let backup = db
        .get_live_backup("claude")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let expected = serde_json::to_string(&provider_b.settings_config).expect("serialize");
    assert_eq!(backup.original_config, expected);
    assert_eq!(
        service.read_claude_live().expect("read live"),
        provider_b.settings_config
    );
}

#[tokio::test]
#[serial]
async fn update_live_backup_from_provider_applies_claude_common_config() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    db.set_config_snippet(
        "claude",
        Some(
            serde_json::json!({
                "includeCoAuthoredBy": false
            })
            .to_string(),
        ),
    )
    .expect("set common config snippet");

    let service = ProxyService::new(db.clone());

    let mut provider = Provider::with_id(
        "p1".to_string(),
        "P1".to_string(),
        json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "token",
                "ANTHROPIC_BASE_URL": "https://claude.example"
            }
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        common_config_enabled: Some(true),
        ..Default::default()
    });

    service
        .update_live_backup_from_provider("claude", &provider)
        .await
        .expect("update live backup");

    let backup = db
        .get_live_backup("claude")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let stored: Value = serde_json::from_str(&backup.original_config).expect("parse backup json");

    assert_eq!(
        stored.get("includeCoAuthoredBy").and_then(|v| v.as_bool()),
        Some(false),
        "common config should be applied into Claude restore backup"
    );
}

#[tokio::test]
#[serial]
async fn update_live_backup_from_provider_applies_codex_common_config() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    db.set_config_snippet(
        "codex",
        Some("disable_response_storage = true\n".to_string()),
    )
    .expect("set common config snippet");

    let service = ProxyService::new(db.clone());

    let mut provider = Provider::with_id(
        "p1".to_string(),
        "P1".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "token"
            },
            "config": r#"model_provider = "any"
model = "gpt-5"

[model_providers.any]
base_url = "https://codex.example/v1"
"#
        }),
        None,
    );
    provider.meta = Some(ProviderMeta {
        common_config_enabled: Some(true),
        ..Default::default()
    });

    service
        .update_live_backup_from_provider("codex", &provider)
        .await
        .expect("update live backup");

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let stored: Value = serde_json::from_str(&backup.original_config).expect("parse backup json");
    let config = stored
        .get("config")
        .and_then(|v| v.as_str())
        .expect("config string");

    assert!(
        config.contains("disable_response_storage = true"),
        "common config should be applied into Codex restore backup"
    );
}

#[tokio::test]
#[serial]
async fn update_live_backup_from_managed_official_does_not_freeze_oauth_tokens() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed", "managed-token", "managed-user")
        .await
        .expect("seed managed Codex OAuth account");

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "access_token": "old-native-token",
                    "account_id": "acct-native"
                }
            },
            "config": ""
        }))
        .expect("serialize seed backup"),
    )
    .await
    .expect("seed live backup");

    let mut provider = Provider::with_id(
        "managed-official".to_string(),
        "OpenAI Official".to_string(),
        json!({
            "auth": {},
            "config": ""
        }),
        None,
    );
    provider.category = Some("official".to_string());
    provider.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed".to_string()),
        }),
        ..Default::default()
    });

    service
        .update_live_backup_from_provider("codex", &provider)
        .await
        .expect("update live backup");

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let stored: Value = serde_json::from_str(&backup.original_config).expect("parse backup json");

    assert!(
        stored.get("auth").is_none(),
        "official OAuth generations must remain live-only so restore cannot roll them back"
    );
}

#[tokio::test]
#[serial]
async fn codex_takeover_switch_to_managed_official_replaces_native_account() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    db.update_proxy_config(ProxyConfig {
        listen_port: 15_721,
        ..Default::default()
    })
    .await
    .expect("set proxy port");
    let service = ProxyService::new(db);
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
        .await
        .expect("seed managed account");

    let native_auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "native-id",
            "access_token": "native-access",
            "refresh_token": "native-refresh",
            "account_id": "acct-native"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(&native_auth, Some("model = \"gpt-5.4\"\n"))
        .expect("seed native auth");

    let mut provider = Provider::with_id(
        "managed-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    provider.category = Some("official".to_string());
    provider.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed".to_string()),
        }),
        ..Default::default()
    });

    service
        .sync_codex_live_from_provider_while_proxy_active(&provider)
        .await
        .expect("sync managed official takeover");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read managed live auth");
    assert_eq!(
        live_auth
            .pointer("/tokens/account_id")
            .and_then(Value::as_str),
        Some("acct-managed")
    );
    assert_eq!(
        live_auth
            .pointer("/tokens/access_token")
            .and_then(Value::as_str),
        Some("managed-access")
    );
    assert!(
        crate::codex_config::codex_auth_matches_recorded_managed_oauth(&live_auth, "acct-managed")
            .expect("read managed marker"),
        "takeover write must record ownership of the managed auth"
    );
}

#[tokio::test]
#[serial]
async fn codex_takeover_unbound_official_preserves_native_account() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    db.update_proxy_config(ProxyConfig {
        listen_port: 15_721,
        ..Default::default()
    })
    .await
    .expect("set proxy port");
    let service = ProxyService::new(db);
    let native_auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "native-id",
            "access_token": "native-access",
            "refresh_token": "native-refresh",
            "account_id": "acct-native"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(&native_auth, Some("model = \"gpt-5.4\"\n"))
        .expect("seed native auth");

    let mut provider = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    provider.category = Some("official".to_string());

    service
        .sync_codex_live_from_provider_while_proxy_active(&provider)
        .await
        .expect("sync unbound official takeover");

    let live_auth: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read native live auth");
    assert_eq!(live_auth, native_auth);
}

#[tokio::test]
#[serial]
async fn restoring_official_codex_takeover_preserves_rotated_live_auth() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let mut provider = Provider::with_id(
        "managed-official".to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "" }),
        None,
    );
    provider.category = Some("official".to_string());
    provider.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed".to_string()),
        }),
        ..Default::default()
    });
    db.save_provider("codex", &provider)
        .expect("save official provider");
    db.set_current_provider("codex", &provider.id)
        .expect("set DB current");
    crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
        .expect("set local current");

    let stale_backup_auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "id-r0",
            "access_token": "access-r0",
            "refresh_token": "refresh-r0",
            "account_id": "acct-managed"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": stale_backup_auth,
            "config": "model = \"gpt-5.4\"\n"
        }))
        .expect("serialize stale backup"),
    )
    .await
    .expect("save stale backup");

    let rotated_live_auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "id-r1",
            "access_token": "access-r1",
            "refresh_token": "refresh-r1",
            "account_id": "acct-managed"
        },
        "last_refresh": "2026-01-02T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(
        &rotated_live_auth,
        Some(
            r#"model_provider = "cc-switch"
[model_providers.cc-switch]
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
"#,
        ),
    )
    .expect("seed rotated takeover live");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore official Codex config");

    let restored = service.read_codex_live().expect("read restored Codex live");
    assert_eq!(restored.get("auth"), Some(&rotated_live_auth));
    assert_eq!(
        restored.get("config").and_then(Value::as_str),
        Some("model = \"gpt-5.4\"\n")
    );
}

#[test]
#[serial]
fn codex_snapshot_rollback_preserves_newer_native_login_without_marker() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    let id_token = crate::codex_config::test_codex_id_token("native-user");

    let auth_r0 = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token,
            "access_token": "access-r0",
            "refresh_token": "refresh-r0",
            "account_id": "acct-a"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(&auth_r0, Some("model = \"before\"\n"))
        .expect("seed R0 live");
    let snapshot = crate::codex_config::CodexLiveStateSnapshot::capture()
        .expect("capture transaction snapshot");

    let auth_r1 = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": crate::codex_config::test_codex_id_token("native-user"),
            "access_token": "access-r1",
            "refresh_token": "refresh-r1",
            "account_id": "acct-a"
        },
        "last_refresh": "2026-01-02T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(&auth_r1, Some("model = \"after\"\n"))
        .expect("write concurrent R1 live");

    snapshot
        .restore_preserving_newer_same_account_auth()
        .expect("selective rollback");

    let restored: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read restored auth");
    assert_eq!(restored, auth_r1, "newer same-account auth must survive");
    assert!(!crate::codex_config::codex_managed_oauth_live_auth_marker_exists());
    assert_eq!(
        std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read rolled-back config"),
        "model = \"before\"\n"
    );
}

#[test]
#[serial]
fn codex_snapshot_rollback_restores_previous_local_account_in_same_workspace() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    let id_token_a = crate::codex_config::test_codex_id_token("user-a");

    let auth_a = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token_a,
            "access_token": "access-a",
            "refresh_token": "refresh-a",
            "account_id": "workspace-shared"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(&auth_a, Some("model = \"before\"\n"))
        .expect("seed account A live");
    crate::codex_config::record_codex_managed_oauth_live_auth(&auth_a, "local-a")
        .expect("record A marker");
    let snapshot =
        crate::codex_config::CodexLiveStateSnapshot::capture().expect("capture account A snapshot");

    let auth_b = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": crate::codex_config::test_codex_id_token("user-b"),
            "access_token": "access-b",
            "refresh_token": "refresh-b",
            "account_id": "workspace-shared"
        },
        "last_refresh": "2026-01-02T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(&auth_b, Some("model = \"after\"\n"))
        .expect("write account B live");
    crate::codex_config::record_codex_managed_oauth_live_auth(&auth_b, "local-b")
        .expect("record B marker");

    snapshot
        .restore_preserving_newer_same_account_auth()
        .expect("cross-account rollback");

    let restored: Value =
        crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .expect("read restored auth");
    assert_eq!(restored, auth_a, "failed A to B change must restore A");
    assert!(
        crate::codex_config::codex_auth_matches_recorded_managed_oauth(&restored, "local-a")
            .expect("check restored A marker")
    );
    assert_eq!(
        std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read rolled-back config"),
        "model = \"before\"\n"
    );
}

#[tokio::test]
#[serial]
async fn update_live_backup_clearing_managed_codex_auth_keeps_official_auth_live_only() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    crate::settings::update_settings(crate::settings::AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable official auth preservation");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let native_auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "access_token": "native-token",
            "account_id": "acct-managed"
        }
    });

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": native_auth,
            "config": ""
        }))
        .expect("serialize seed backup"),
    )
    .await
    .expect("seed live backup");

    let mut provider = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
        "OpenAI Official".to_string(),
        json!({
            "auth": {},
            "config": ""
        }),
        None,
    );
    provider.category = Some("official".to_string());

    service
        .update_live_backup_from_provider_inner("codex", &provider, Some("acct-managed"))
        .await
        .expect("update live backup");

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let stored: Value = serde_json::from_str(&backup.original_config).expect("parse backup json");

    assert!(
        stored.get("auth").is_none(),
        "official native auth must not be frozen into the restore backup"
    );
}

#[tokio::test]
#[serial]
async fn update_live_backup_from_provider_preserves_codex_mcp_servers() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {
                "OPENAI_API_KEY": "old-token"
            },
            "config": r#"model_provider = "any"
model = "gpt-4"

[model_providers.any]
base_url = "https://old.example/v1"

[mcp_servers.echo]
command = "npx"
args = ["echo-server"]
"#
        }))
        .expect("serialize seed backup"),
    )
    .await
    .expect("seed live backup");

    let provider = Provider::with_id(
        "p2".to_string(),
        "P2".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "new-token"
            },
            "config": r#"model_provider = "any"
model = "gpt-5"

[model_providers.any]
base_url = "https://new.example/v1"
"#
        }),
        None,
    );

    service
        .update_live_backup_from_provider("codex", &provider)
        .await
        .expect("update live backup");

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let stored: Value = serde_json::from_str(&backup.original_config).expect("parse backup json");
    let config = stored
        .get("config")
        .and_then(|v| v.as_str())
        .expect("config string");

    assert!(
        config.contains("[mcp_servers.echo]"),
        "existing Codex MCP section should survive proxy hot-switch backup update"
    );
    assert!(
        config.contains("https://new.example/v1"),
        "provider-specific base_url should still update to the new provider"
    );
}

#[tokio::test]
#[serial]
async fn hot_switch_codex_provider_preserves_provider_model_provider_in_backup_and_restore() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider_a = Provider::with_id(
        "a".to_string(),
        "RightCode".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "rightcode-key"
            },
            "config": r#"model_provider = "rightcode"
model = "gpt-5.4"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }),
        None,
    );
    let provider_b = Provider::with_id(
        "b".to_string(),
        "AiHubMix".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "aihubmix-key"
            },
            "config": r#"model_provider = "aihubmix"
model = "gpt-5.4"

[model_providers.aihubmix]
name = "AiHubMix"
base_url = "https://aihubmix.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }),
        None,
    );

    db.save_provider("codex", &provider_a)
        .expect("save provider a");
    db.save_provider("codex", &provider_b)
        .expect("save provider b");
    db.set_current_provider("codex", "a")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("a"))
        .expect("set local current provider");
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
    )
    .await
    .expect("seed live backup");
    service
        .write_codex_live_verbatim(&json!({
            "auth": {
                "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
            },
            "config": r#"model_provider = "rightcode"
model = "gpt-5.4"

[model_providers.rightcode]
name = "RightCode"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }))
        .expect("seed taken-over Codex live config");

    service
        .hot_switch_provider("codex", "b")
        .await
        .expect("hot switch Codex provider");

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let stored: Value = serde_json::from_str(&backup.original_config).expect("parse backup json");
    let backup_config = stored
        .get("config")
        .and_then(|v| v.as_str())
        .expect("backup config string");
    let parsed_backup: toml::Value = toml::from_str(backup_config).expect("parse backup config");
    assert_eq!(
        parsed_backup.get("model_provider").and_then(|v| v.as_str()),
        Some("aihubmix"),
        "provider-derived restore backup should preserve the provider's model_provider"
    );
    let backup_model_providers = parsed_backup
        .get("model_providers")
        .and_then(|v| v.as_table())
        .expect("backup model_providers");
    assert!(backup_model_providers.get("custom").is_none());
    assert_eq!(
        backup_model_providers
            .get("aihubmix")
            .and_then(|v| v.get("base_url"))
            .and_then(|v| v.as_str()),
        Some("https://aihubmix.example/v1"),
        "provider id should point at the hot-switched provider endpoint"
    );

    let live = service.read_codex_live().expect("read Codex live config");
    let live_config = live
        .get("config")
        .and_then(|v| v.as_str())
        .expect("live config string");
    let parsed_live: toml::Value = toml::from_str(live_config).expect("parse live config");
    assert_eq!(
        parsed_live.get("model_provider").and_then(|v| v.as_str()),
        Some("aihubmix"),
        "hot-switched Codex live config should expose the selected provider"
    );
    assert_eq!(
        parsed_live
            .get("model_providers")
            .and_then(|v| v.get("aihubmix"))
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str()),
        Some("AiHubMix"),
        "Codex app provider label should follow the selected provider"
    );
    assert_eq!(
        parsed_live
            .get("model_providers")
            .and_then(|v| v.get("aihubmix"))
            .and_then(|v| v.get("base_url"))
            .and_then(|v| v.as_str()),
        Some("http://127.0.0.1:15721/v1"),
        "taken-over live config should stay pointed at the local proxy"
    );

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore Codex live config");

    let live = service.read_codex_live().expect("read Codex live config");
    let live_config = live
        .get("config")
        .and_then(|v| v.as_str())
        .expect("live config string");
    let parsed_live: toml::Value = toml::from_str(live_config).expect("parse live config");
    assert_eq!(
        parsed_live.get("model_provider").and_then(|v| v.as_str()),
        Some("aihubmix"),
        "restored Codex live config should preserve the provider's model_provider"
    );
    assert_eq!(
        live.get("auth")
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(|v| v.as_str()),
        Some("aihubmix-key"),
        "restore should still use the hot-switched provider auth"
    );
}

#[tokio::test]
#[serial]
async fn hot_switch_codex_chat_provider_updates_live_provider_display() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let provider_a = Provider::with_id(
        "a".to_string(),
        "Responses".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "responses-key"
            },
            "config": r#"model_provider = "stable"
model = "responses-model"

[model_providers.stable]
name = "Stable"
base_url = "https://responses.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }),
        None,
    );
    let mut provider_b = Provider::with_id(
        "b".to_string(),
        "DeepSeek".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "deepseek-key"
            },
            "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }),
        None,
    );
    provider_b.meta = Some(ProviderMeta {
        api_format: Some("openai_chat".to_string()),
        ..Default::default()
    });

    db.save_provider("codex", &provider_a)
        .expect("save provider a");
    db.save_provider("codex", &provider_b)
        .expect("save provider b");
    db.set_current_provider("codex", "a")
        .expect("set current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some("a"))
        .expect("set local current provider");
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
    )
    .await
    .expect("seed live backup");
    service
        .write_codex_live_verbatim(&json!({
            "auth": {
                "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
            },
            "config": r#"model_provider = "stable"
model = "responses-model"

[model_providers.stable]
name = "Stable"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        }))
        .expect("seed taken-over Codex live config");

    service
        .hot_switch_provider("codex", "b")
        .await
        .expect("hot switch Codex provider");

    let live = service.read_codex_live().expect("read Codex live config");
    let live_config = live
        .get("config")
        .and_then(|v| v.as_str())
        .expect("live config string");
    let parsed_live: toml::Value = toml::from_str(live_config).expect("parse live config");

    assert_eq!(
        parsed_live.get("model_provider").and_then(|v| v.as_str()),
        Some("deepseek")
    );
    assert_eq!(
        parsed_live
            .get("model_providers")
            .and_then(|v| v.get("deepseek"))
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str()),
        Some("DeepSeek")
    );
    assert_eq!(
        parsed_live
            .get("model_providers")
            .and_then(|v| v.get("deepseek"))
            .and_then(|v| v.get("base_url"))
            .and_then(|v| v.as_str()),
        Some("http://127.0.0.1:15721/v1")
    );
    assert_eq!(
        parsed_live.get("model").and_then(|v| v.as_str()),
        Some("deepseek-v4-flash")
    );
    assert_eq!(
        live.get("auth")
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(|v| v.as_str()),
        Some(PROXY_TOKEN_PLACEHOLDER)
    );
}

#[tokio::test]
#[serial]
async fn update_live_backup_from_provider_keeps_new_codex_mcp_entries_on_conflict() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {
                "OPENAI_API_KEY": "old-token"
            },
            "config": r#"[mcp_servers.shared]
command = "old-command"

[mcp_servers.legacy]
command = "legacy-command"
"#
        }))
        .expect("serialize seed backup"),
    )
    .await
    .expect("seed live backup");

    let provider = Provider::with_id(
        "p2".to_string(),
        "P2".to_string(),
        json!({
            "auth": {
                "OPENAI_API_KEY": "new-token"
            },
            "config": r#"[mcp_servers.shared]
command = "new-command"

[mcp_servers.latest]
command = "latest-command"
"#
        }),
        None,
    );

    service
        .update_live_backup_from_provider("codex", &provider)
        .await
        .expect("update live backup");

    let backup = db
        .get_live_backup("codex")
        .await
        .expect("get live backup")
        .expect("backup exists");
    let stored: Value = serde_json::from_str(&backup.original_config).expect("parse backup json");
    let config = stored
        .get("config")
        .and_then(|v| v.as_str())
        .expect("config string");
    let parsed: toml::Value = toml::from_str(config).expect("parse merged codex config");

    let mcp_servers = parsed
        .get("mcp_servers")
        .expect("mcp_servers should be present");
    assert_eq!(
        mcp_servers
            .get("shared")
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str()),
        Some("new-command"),
        "new provider/common-config MCP definition should win on conflict"
    );
    assert_eq!(
        mcp_servers
            .get("legacy")
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str()),
        Some("legacy-command"),
        "backup-only MCP entries should still be preserved"
    );
    assert_eq!(
        mcp_servers
            .get("latest")
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str()),
        Some("latest-command"),
        "new MCP entries should remain in the restore backup"
    );
}

#[tokio::test]
#[serial]
async fn provider_switch_with_restored_codex_backup_refreshes_catalog_and_common_config() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    seed_codex_model_template();

    let db = Arc::new(Database::memory().expect("init db"));
    let state = crate::store::AppState::new(db.clone());

    db.set_config_snippet(
        "codex",
        Some(
            r#"[mcp_servers.shared]
command = "shared-command"
"#
            .to_string(),
        ),
    )
    .expect("set common config snippet");

    let proxy_config = ProxyConfig {
        listen_port: 0,
        ..Default::default()
    };
    db.update_proxy_config(proxy_config)
        .await
        .expect("set test proxy config");
    state
        .proxy_service
        .start()
        .await
        .expect("start proxy server");

    let config_a = r#"model_provider = "provider-a"
model = "model-a"

[model_providers.provider-a]
name = "ProviderA"
base_url = "https://provider-a.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
    let config_b = r#"model_provider = "provider-b"
model = "model-b"

[model_providers.provider-b]
name = "ProviderB"
base_url = "https://provider-b.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;

    let provider_a = Provider::with_id(
        "a".to_string(),
        "ProviderA".to_string(),
        serde_json::json!({
            "auth": { "OPENAI_API_KEY": "key-a" },
            "config": config_a,
            "modelCatalog": { "models": [{ "model": "model-a" }] }
        }),
        None,
    );
    let mut provider_b = Provider::with_id(
        "b".to_string(),
        "ProviderB".to_string(),
        serde_json::json!({
            "auth": { "OPENAI_API_KEY": "key-b" },
            "config": config_b,
            "modelCatalog": { "models": [{ "model": "model-b" }] }
        }),
        None,
    );
    provider_b.meta = Some(ProviderMeta {
        common_config_enabled: Some(true),
        ..Default::default()
    });

    db.save_provider("codex", &provider_a)
        .expect("save provider a");
    db.save_provider("codex", &provider_b)
        .expect("save provider b");
    db.set_current_provider("codex", "a")
        .expect("set current provider a");
    crate::settings::set_current_provider(&AppType::Codex, Some("a"))
        .expect("set local current provider a");

    state
        .proxy_service
        .write_codex_live_for_provider(&provider_a.settings_config, Some(&provider_a))
        .expect("seed live codex config");
    assert!(
        !state
            .proxy_service
            .detect_takeover_in_live_config_for_app(&AppType::Codex),
        "seeded live config should not be proxy-taken-over"
    );

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&provider_a.settings_config).expect("serialize backup"),
    )
    .await
    .expect("seed restored backup");

    crate::services::provider::ProviderService::switch(&state, AppType::Codex, "b")
        .expect("provider switch to provider b");
    state.proxy_service.stop().await.expect("stop proxy server");

    let catalog_path = crate::codex_config::get_codex_model_catalog_path();
    assert!(
        catalog_path.exists(),
        "cc-switch-model-catalog.json must be created on provider switch"
    );
    let catalog_text = std::fs::read_to_string(&catalog_path).expect("read catalog json");
    let catalog: serde_json::Value =
        serde_json::from_str(&catalog_text).expect("parse catalog json");
    let slugs: Vec<&str> = catalog
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("slug").and_then(|s| s.as_str()))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        slugs.contains(&"model-b"),
        "catalog must contain provider B's model after switch; got: {slugs:?}"
    );
    assert!(
        !slugs.contains(&"model-a"),
        "catalog must not contain stale provider A model after switch; got: {slugs:?}"
    );

    let config_path = crate::codex_config::get_codex_config_path();
    let config_text = std::fs::read_to_string(&config_path).expect("read config.toml");
    assert!(
        config_text.contains("model_catalog_json"),
        "config.toml must reference model_catalog_json after switch"
    );
    assert!(
        config_text.contains("[mcp_servers.shared]"),
        "config.toml must keep common config after switch"
    );
    assert!(
        config_text.contains(r#"command = "shared-command""#),
        "config.toml must include common config content after switch"
    );
}

#[tokio::test]
#[serial]
async fn provider_switch_with_restored_codex_backup_propagates_catalog_write_errors() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    seed_codex_model_template();

    let db = Arc::new(Database::memory().expect("init db"));
    let state = crate::store::AppState::new(db.clone());

    let proxy_config = ProxyConfig {
        listen_port: 0,
        ..Default::default()
    };
    db.update_proxy_config(proxy_config)
        .await
        .expect("set test proxy config");
    state
        .proxy_service
        .start()
        .await
        .expect("start proxy server");

    let config_a = r#"model_provider = "provider-a"
model = "model-a"

[model_providers.provider-a]
name = "ProviderA"
base_url = "https://provider-a.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
    let config_b = r#"model_provider = "provider-b"
model = "model-b"

[model_providers.provider-b]
name = "ProviderB"
base_url = "https://provider-b.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;

    let provider_a = Provider::with_id(
        "a".to_string(),
        "ProviderA".to_string(),
        serde_json::json!({
            "auth": { "OPENAI_API_KEY": "key-a" },
            "config": config_a,
            "modelCatalog": { "models": [{ "model": "model-a" }] }
        }),
        None,
    );
    let provider_b = Provider::with_id(
        "b".to_string(),
        "ProviderB".to_string(),
        serde_json::json!({
            "auth": { "OPENAI_API_KEY": "key-b" },
            "config": config_b,
            "modelCatalog": { "models": [{ "model": "model-b" }] }
        }),
        None,
    );

    db.save_provider("codex", &provider_a)
        .expect("save provider a");
    db.save_provider("codex", &provider_b)
        .expect("save provider b");
    db.set_current_provider("codex", "a")
        .expect("set current provider a");
    crate::settings::set_current_provider(&AppType::Codex, Some("a"))
        .expect("set local current provider a");

    state
        .proxy_service
        .write_codex_live_for_provider(&provider_a.settings_config, Some(&provider_a))
        .expect("seed live codex config");
    assert!(
        !state
            .proxy_service
            .detect_takeover_in_live_config_for_app(&AppType::Codex),
        "seeded live config should not be proxy-taken-over"
    );

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&provider_a.settings_config).expect("serialize backup"),
    )
    .await
    .expect("seed restored backup");

    let catalog_path = crate::codex_config::get_codex_model_catalog_path();
    if catalog_path.exists() {
        std::fs::remove_file(&catalog_path).expect("remove catalog file");
    }
    std::fs::create_dir_all(&catalog_path).expect("turn catalog path into directory");

    let err = crate::services::provider::ProviderService::switch(&state, AppType::Codex, "b")
        .expect_err("provider switch should fail when catalog cannot be written");
    state.proxy_service.stop().await.expect("stop proxy server");

    let message = err.to_string();
    assert!(
        message.contains("写入 Codex 配置失败")
            || message.contains("原子替换失败")
            || (message.contains("捕获 Codex 热切换前状态失败")
                && message.contains("cc-switch-model-catalog.json")),
        "switch should surface catalog write failure, got: {message}"
    );
}

#[tokio::test]
#[serial]
async fn codex_direct_live_write_rolls_back_when_provider_commit_fails() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let state = crate::store::AppState::new(db.clone());
    db.update_proxy_config(ProxyConfig {
        listen_port: 0,
        ..Default::default()
    })
    .await
    .expect("set test proxy config");
    state
        .proxy_service
        .start()
        .await
        .expect("start proxy server");

    let provider_a = Provider::with_id(
        "a".to_string(),
        "Provider A".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "key-a" },
            "config": "model_provider = \"provider-a\"\nmodel = \"model-a\"\n\n[model_providers.provider-a]\nname = \"Provider A\"\nbase_url = \"https://a.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
        }),
        None,
    );
    let provider_b = Provider::with_id(
        "b".to_string(),
        "Provider B".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "key-b" },
            "config": "model_provider = \"provider-b\"\nmodel = \"model-b\"\n\n[model_providers.provider-b]\nname = \"Provider B\"\nbase_url = \"https://b.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
        }),
        None,
    );
    db.save_provider("codex", &provider_a)
        .expect("save provider a");
    db.save_provider("codex", &provider_b)
        .expect("save provider b");
    db.set_current_provider("codex", "a")
        .expect("set current provider a");
    crate::settings::set_current_provider(&AppType::Codex, Some("a"))
        .expect("set local current provider a");
    state
        .proxy_service
        .write_codex_live_for_provider(&provider_a.settings_config, Some(&provider_a))
        .expect("seed direct live config");
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&provider_a.settings_config).expect("serialize backup"),
    )
    .await
    .expect("seed restored backup");
    let original_live = state
        .proxy_service
        .read_codex_live()
        .expect("read original live config");

    {
        let conn = db.conn.lock().expect("lock database");
        conn.execute_batch(
            "CREATE TRIGGER reject_codex_current_update
                 BEFORE UPDATE OF is_current ON providers
                 WHEN NEW.app_type = 'codex'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced current-provider commit failure');
                 END;",
        )
        .expect("install failure trigger");
    }

    let error = crate::services::provider::ProviderService::switch(&state, AppType::Codex, "b")
        .expect_err("database commit should fail");
    state.proxy_service.stop().await.expect("stop proxy server");

    assert!(error
        .to_string()
        .contains("forced current-provider commit failure"));
    assert_eq!(
        state
            .proxy_service
            .read_codex_live()
            .expect("read rolled-back live config"),
        original_live,
        "commit failure must restore the exact direct Live snapshot"
    );
    assert_eq!(
        db.get_current_provider("codex")
            .expect("read current provider")
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        crate::settings::get_current_provider(&AppType::Codex).as_deref(),
        Some("a")
    );
}

#[tokio::test]
#[serial]
async fn codex_active_takeover_hot_switch_failure_restores_native_official_auth() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    use_ephemeral_proxy_port(&db).await;
    let service = ProxyService::new(db.clone());
    service
        .codex_oauth_manager
        .add_test_account_with_user_identity("acct-managed-b", "managed-access-b", "user-b")
        .await
        .expect("seed managed account B");

    let native_auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "native-id",
            "access_token": "native-access",
            "refresh_token": "native-refresh",
            "account_id": "acct-native"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    crate::codex_config::write_codex_live_atomic(&native_auth, Some("model = \"gpt-5.4\"\n"))
        .expect("seed native official live");

    let mut official = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    official.category = Some("official".to_string());
    db.save_provider("codex", &official)
        .expect("save unbound official");
    db.set_current_provider("codex", &official.id)
        .expect("set DB current");
    crate::settings::set_current_provider(&AppType::Codex, Some(&official.id))
        .expect("set local current");

    service
        .set_takeover_for_app("codex", true)
        .await
        .expect("enable unbound official takeover");
    let backup_before = db
        .get_live_backup("codex")
        .await
        .expect("read original backup")
        .expect("backup exists");
    let live_before = crate::codex_config::CodexLiveStateSnapshot::capture()
        .expect("capture native takeover live");

    official.meta = Some(ProviderMeta {
        auth_binding: Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some("codex_oauth".to_string()),
            account_id: Some("acct-managed-b".to_string()),
        }),
        ..Default::default()
    });
    db.save_provider("codex", &official)
        .expect("persist managed target before hot-switch projection");
    {
        let conn = db.conn.lock().expect("lock database");
        conn.execute_batch(
            "CREATE TRIGGER reject_managed_official_current_commit
                 BEFORE UPDATE OF is_current ON providers
                 WHEN NEW.app_type = 'codex'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced managed official current failure');
                 END;",
        )
        .expect("install current failure trigger");
    }

    let error = service
        .hot_switch_provider("codex", &official.id)
        .await
        .expect_err("current commit failure should abort hot-switch");
    service.stop().await.expect("stop test proxy");

    assert!(error.contains("forced managed official current failure"));
    assert_eq!(
        db.get_live_backup("codex")
            .await
            .expect("read rolled-back backup")
            .expect("backup remains")
            .original_config,
        backup_before.original_config
    );
    assert_eq!(
        crate::codex_config::CodexLiveStateSnapshot::capture()
            .expect("capture rolled-back takeover live"),
        live_before,
        "failed projection must restore native auth/config/catalog/marker exactly"
    );
    assert_eq!(
        db.get_current_provider("codex")
            .expect("read DB current")
            .as_deref(),
        Some(official.id.as_str())
    );
}

/// Regression: turning proxy takeover off restores Live from the backup. The
/// backup snapshot is `read_codex_live_settings()` output (`{auth, config}`,
/// never an inline `modelCatalog`). The restore must NOT route the config
/// through catalog projection, which would see no specs and strip the
/// `model_catalog_json` pointer — silently dropping the user's Codex model
/// mapping from Live even though the DB SSOT still holds it.
#[tokio::test]
#[serial]
async fn codex_restore_from_backup_preserves_model_catalog_pointer() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    // Pre-takeover Live state: config.toml points at the cc-switch generated
    // catalog file, and that file exists on disk (takeover never touches it).
    let catalog_path = crate::codex_config::get_codex_model_catalog_path();
    if let Some(parent) = catalog_path.parent() {
        std::fs::create_dir_all(parent).expect("create codex dir");
    }
    std::fs::write(
        &catalog_path,
        r#"{"models":[{"slug":"deepseek-v4-flash"}]}"#,
    )
    .expect("seed generated catalog file");

    let pointer = catalog_path.to_string_lossy().replace('\\', "/");
    let backup_config = format!(
        "model_provider = \"custom\"\n\
             model = \"deepseek-v4-flash\"\n\
             model_catalog_json = \"{pointer}\"\n\n\
             [model_providers.custom]\n\
             name = \"DeepSeek\"\n\
             base_url = \"https://api.deepseek.example/v1\"\n\
             wire_api = \"responses\"\n"
    );
    let backup_json = serde_json::to_string(&json!({
        "auth": { "OPENAI_API_KEY": "deepseek-key" },
        "config": backup_config,
    }))
    .expect("serialize backup");
    db.save_live_backup("codex", &backup_json)
        .await
        .expect("seed live backup");

    // Turning takeover off restores Live from this backup.
    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex live from backup");

    let restored = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read restored config.toml");
    assert!(
        restored.contains("model_catalog_json"),
        "restore must preserve the model_catalog_json pointer, got:\n{restored}"
    );
    assert!(
        restored.contains(pointer.as_str()),
        "restored pointer must still reference the cc-switch generated catalog file"
    );
}

/// Regression: a hot-switch during takeover rebuilds the backup from the DB
/// provider (`update_live_backup_from_provider`), so the backup carries an
/// inline `modelCatalog` (DB SSOT) but a `config.toml` text WITHOUT a
/// `model_catalog_json` pointer. Restoring that backup must project the
/// inline catalog — (re)generating both the catalog file and the pointer —
/// or the Codex model mapping vanishes from Live after takeover-off.
#[tokio::test]
#[serial]
async fn codex_restore_from_backup_projects_inline_model_catalog() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    // Catalog projection needs a model template; seed `models_cache.json`
    // with the template slug so we don't depend on the `codex` CLI.
    let codex_dir = crate::codex_config::get_codex_config_dir();
    std::fs::create_dir_all(&codex_dir).expect("create codex dir");
    std::fs::write(
        codex_dir.join("models_cache.json"),
        r#"{"models":[{"slug":"gpt-5.5"}]}"#,
    )
    .expect("seed models_cache template");

    // Provider-rebuilt backup shape: inline modelCatalog, pointer-less config.
    let backup_json = serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "deepseek-key" },
            "config": "model_provider = \"custom\"\nmodel = \"deepseek-v4-flash\"\n\n[model_providers.custom]\nname = \"DeepSeek\"\nbase_url = \"https://api.deepseek.example/v1\"\nwire_api = \"responses\"\n",
            "modelCatalog": {
                "models": [
                    { "model": "deepseek-v4-flash", "displayName": "DeepSeek V4 Flash", "contextWindow": 1_000_000 }
                ]
            }
        }))
        .expect("serialize backup");
    db.save_live_backup("codex", &backup_json)
        .await
        .expect("seed live backup");
    write_json_file(
        &crate::codex_config::get_codex_auth_path(),
        &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
    )
    .expect("seed takeover placeholder auth");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex live from backup");

    let restored = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read restored config.toml");
    let catalog_path = crate::codex_config::get_codex_model_catalog_path();
    assert!(
            restored.contains("model_catalog_json"),
            "restore must (re)generate the model_catalog_json pointer from inline catalog, got:\n{restored}"
        );
    assert!(
        catalog_path.exists(),
        "restore must generate the cc-switch catalog file on disk"
    );
    let catalog: Value = serde_json::from_str(
        &std::fs::read_to_string(&catalog_path).expect("read generated catalog"),
    )
    .expect("parse generated catalog");
    let slugs: Vec<&str> = catalog
        .get("models")
        .and_then(|m| m.as_array())
        .expect("catalog models")
        .iter()
        .filter_map(|m| m.get("slug").and_then(|s| s.as_str()))
        .collect();
    assert!(
        slugs.contains(&"deepseek-v4-flash"),
        "generated catalog must contain the inline model, got slugs: {slugs:?}"
    );
}

/// Regression: a provider-rebuilt backup can pair an inline `modelCatalog`
/// with EMPTY `auth.json` (`{}`) — the bearer-token / Mobile-compat shape
/// where the API key lives in the config's `experimental_bearer_token`. The
/// empty-auth restore branch deletes `auth.json` and writes config raw; it
/// must still project the inline catalog (decision is orthogonal to auth), or
/// the model mapping vanishes on takeover-off for this provider shape.
#[tokio::test]
#[serial]
async fn codex_restore_empty_auth_backup_still_projects_inline_catalog() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    let codex_dir = crate::codex_config::get_codex_config_dir();
    std::fs::create_dir_all(&codex_dir).expect("create codex dir");
    std::fs::write(
        codex_dir.join("models_cache.json"),
        r#"{"models":[{"slug":"gpt-5.5"}]}"#,
    )
    .expect("seed models_cache template");

    // Empty auth.json + key carried in config.toml's experimental_bearer_token,
    // plus the inline modelCatalog (DB SSOT).
    let backup_json = serde_json::to_string(&json!({
            "auth": {},
            "config": "model_provider = \"custom\"\nmodel = \"deepseek-v4-flash\"\n\n[model_providers.custom]\nname = \"DeepSeek\"\nbase_url = \"https://api.deepseek.example/v1\"\nwire_api = \"responses\"\nexperimental_bearer_token = \"sk-deepseek\"\n",
            "modelCatalog": {
                "models": [ { "model": "deepseek-v4-flash", "displayName": "DeepSeek V4 Flash" } ]
            }
        }))
        .expect("serialize backup");
    db.save_live_backup("codex", &backup_json)
        .await
        .expect("seed live backup");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex live from backup");

    let restored = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
        .expect("read restored config.toml");
    assert!(
        restored.contains("model_catalog_json"),
        "empty-auth restore must still project the inline catalog pointer, got:\n{restored}"
    );
    assert!(
        crate::codex_config::get_codex_model_catalog_path().exists(),
        "empty-auth restore must generate the cc-switch catalog file"
    );
    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "empty-auth restore must delete auth.json rather than write an empty one"
    );
}

/// Regression: when the backup row itself contains the proxy placeholder
/// (a corrupted state where previous start/stop cycles saved the proxy
/// config as the "original Live"), restore must NOT write it back to Live.
/// It should fall through to the SSOT (current provider) path and rebuild
/// Live from the provider DB instead.
#[tokio::test]
#[serial]
async fn restore_falls_through_to_ssot_when_backup_is_proxy_placeholder() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    // Seed DB with a current provider that has a real API key
    let provider = Provider::with_id(
        "p1".to_string(),
        "P1".to_string(),
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.minimaxi.com/anthropic",
                "ANTHROPIC_API_KEY": "real-key-from-db"
            }
        }),
        None,
    );
    db.save_provider("claude", &provider)
        .expect("save provider");
    db.set_current_provider("claude", "p1")
        .expect("set current provider");

    // Seed backup with proxy placeholder (the corrupted state)
    let corrupted_backup = serde_json::to_string(&json!({
        "env": {
            "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
        }
    }))
    .expect("serialize corrupted backup");
    db.save_live_backup("claude", &corrupted_backup)
        .await
        .expect("seed corrupted backup");

    // Seed Live with the same proxy placeholder (matches the corrupted state)
    service
        .write_claude_live(&json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
            }
        }))
        .expect("seed taken-over live file");

    // Restore: must NOT use the corrupted backup
    service
        .restore_live_config_for_app_with_fallback(&AppType::Claude)
        .await
        .expect("restore should succeed via SSOT");

    // The backup should still be the corrupted one (we didn't touch it on this path)
    let backup_after = db
        .get_live_backup("claude")
        .await
        .expect("get backup")
        .expect("backup still exists");
    assert_eq!(
        backup_after.original_config, corrupted_backup,
        "restore must NOT overwrite the corrupted backup"
    );

    // Live should now reflect the SSOT (provider DB), NOT the proxy URL
    let restored_live = service.read_claude_live().expect("read live");
    let restored_url = restored_live
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
        .and_then(|v| v.as_str());
    assert_eq!(
        restored_url,
        Some("https://api.minimaxi.com/anthropic"),
        "Live must be rebuilt from SSOT, not from the corrupted backup"
    );
    let restored_key = restored_live
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_API_KEY"))
        .and_then(|v| v.as_str());
    assert_eq!(
        restored_key,
        Some("real-key-from-db"),
        "Live must carry the real API key from the provider DB"
    );
    assert_ne!(
        restored_live
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN"))
            .and_then(|v| v.as_str()),
        Some(PROXY_TOKEN_PLACEHOLDER),
        "Live must not still carry the proxy placeholder"
    );
}

/// Regression for #6277: the restore backup is a snapshot taken when
/// takeover started. If the user logs into official ChatGPT DURING
/// takeover, live auth.json holds OAuth tokens the backup never saw;
/// restoring the backup verbatim on exit/crash-recovery would wipe the
/// login every restart. Restore must keep the live OAuth login and demote
/// the backup's API key into config.toml instead.
#[tokio::test]
#[serial]
async fn restore_keeps_codex_oauth_login_when_backup_has_api_key_only() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    // Backup snapshot: third-party API-key shape (pre-login state)
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "sk-third-party" },
            "config": r#"model_provider = "any"
model = "gpt-5"

[model_providers.any]
base_url = "https://third.example/v1"
"#
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");

    // Live: user logged into official ChatGPT during takeover
    crate::codex_config::write_codex_live_atomic(
        &json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "idt",
                "access_token": "at",
                "refresh_token": "rt",
                "account_id": "acc"
            },
            "last_refresh": "2026-08-10T00:00:00Z"
        }),
        Some("model_provider = \"any\"\n"),
    )
    .expect("seed live codex files");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex");

    let live = service.read_codex_live().expect("read live");
    let auth = live.get("auth").expect("auth present");
    assert_eq!(
        auth.get("tokens")
            .and_then(|t| t.get("refresh_token"))
            .and_then(|v| v.as_str()),
        Some("rt"),
        "official ChatGPT login must survive restore"
    );
    assert!(
        crate::codex_config::codex_auth_has_oauth_login_material(auth),
        "restored auth.json must keep OAuth login material"
    );

    let config_text = live
        .get("config")
        .and_then(|v| v.as_str())
        .expect("config text");
    assert!(
        config_text.contains("https://third.example/v1"),
        "backup config.toml must still be restored"
    );
    assert!(
        config_text.contains("sk-third-party"),
        "backup API key must be demoted into config.toml as experimental_bearer_token"
    );
}

/// The normal restore path (no login happened during takeover): live auth
/// only carries the takeover placeholder, so the backup must be restored
/// verbatim including its auth.json.
#[tokio::test]
#[serial]
async fn restore_writes_codex_backup_auth_verbatim_when_live_has_no_oauth_login() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "sk-third-party" },
            "config": "model_provider = \"any\"\n"
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");

    crate::codex_config::write_codex_live_atomic(
        &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
        Some("model_provider = \"any\"\n"),
    )
    .expect("seed live codex files");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex");

    let live = service.read_codex_live().expect("read live");
    assert_eq!(
        live.get("auth")
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(|v| v.as_str()),
        Some("sk-third-party"),
        "without a live OAuth login the backup auth must be restored verbatim"
    );
}

#[tokio::test]
#[serial]
async fn restore_preserves_logged_out_official_codex_state() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let mut official = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
        "OpenAI Official".to_string(),
        json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
        None,
    );
    official.category = Some("official".to_string());
    db.save_provider("codex", &official)
        .expect("save official provider");
    db.set_current_provider("codex", &official.id)
        .expect("set DB current provider");
    crate::settings::set_current_provider(&AppType::Codex, Some(&official.id))
        .expect("set local current provider");

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {
                "auth_mode": "chatgpt",
                "tokens": { "refresh_token": "stale-rt" }
            },
            "config": "model_provider = \"any\"\n"
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");
    crate::codex_config::write_codex_live_config_atomic(Some("model_provider = \"any\"\n"))
        .expect("seed logged-out live config");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex");

    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "restore must not replay stale backup credentials after logout"
    );

    let auth_path = crate::codex_config::get_codex_auth_path();
    std::fs::write(&auth_path, b"{").expect("seed malformed live auth");
    let error = service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect_err("malformed live auth must stop restore");
    assert!(error.contains("读取 Codex auth 失败"));
    assert_eq!(
        std::fs::read(&auth_path).expect("read malformed auth"),
        b"{"
    );
}

#[tokio::test]
#[serial]
async fn restore_preserves_missing_codex_auth_when_current_provider_is_unknown() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {
                "auth_mode": "chatgpt",
                "tokens": { "refresh_token": "stale-rt" }
            },
            "config": "model_provider = \"any\"\n"
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");
    crate::codex_config::write_codex_live_config_atomic(Some("model = \"gpt-5.4\"\n"))
        .expect("seed config without auth");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex");

    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "an unclassified backup must not replay credentials over missing live auth"
    );
}

#[test]
#[serial]
fn guarded_codex_restore_rejects_a_changed_auth_file() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let auth_path = crate::codex_config::get_codex_auth_path();
    write_json_file(
        &auth_path,
        &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
    )
    .expect("seed observed auth");
    let observed = CodexAuthFileSnapshot::capture().expect("capture auth");
    write_json_file(
        &auth_path,
        &json!({
            "auth_mode": "chatgpt",
            "tokens": { "refresh_token": "new-rt" }
        }),
    )
    .expect("simulate concurrent login");

    let error = service
        .write_codex_live_verbatim_with_optional_auth_guard(
            &json!({
                "auth": { "OPENAI_API_KEY": "stale-key" },
                "config": "model_provider = \"any\"\n"
            }),
            Some(&observed),
        )
        .expect_err("changed auth must cancel restore");

    assert!(error.contains("发生变化"));
    let current: Value = read_json_file(&auth_path).expect("read current auth");
    assert_eq!(
        current
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str),
        Some("new-rt")
    );
}

#[tokio::test]
#[serial]
async fn restore_empty_auth_without_config_keeps_auth_file_absent() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());
    let provider = Provider::with_id(
        "third-party".to_string(),
        "Third Party".to_string(),
        json!({
            "auth": { "OPENAI_API_KEY": "sk-third-party" },
            "config": "model_provider = \"third-party\"\n"
        }),
        None,
    );
    db.save_provider("codex", &provider)
        .expect("save current provider");
    db.set_current_provider("codex", &provider.id)
        .expect("set current provider");
    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {},
            "config": null
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore empty auth backup");

    assert!(
        !crate::codex_config::get_codex_auth_path().exists(),
        "an empty backup must preserve the missing-file logout state"
    );
}

#[test]
#[serial]
fn changed_auth_after_catalog_prepare_rolls_catalog_back() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    seed_codex_model_template();

    let catalog_path = crate::codex_config::get_codex_model_catalog_path();
    write_json_file(&catalog_path, &json!({ "models": ["old"] })).expect("seed catalog");
    let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture missing auth");

    let auth_path = crate::codex_config::get_codex_auth_path();
    write_json_file(
        &auth_path,
        &json!({
            "auth_mode": "chatgpt",
            "tokens": { "refresh_token": "new-rt" }
        }),
    )
    .expect("simulate concurrent login");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let error = service
        .write_codex_live_verbatim_with_optional_auth_guard(
            &json!({
                "auth": { "OPENAI_API_KEY": "stale-key" },
                "config": "model = \"model-a\"\n",
                "modelCatalog": { "models": [{ "model": "model-a" }] }
            }),
            Some(&auth_snapshot),
        )
        .expect_err("changed auth must cancel the prepared restore");

    assert!(error.contains("发生变化"));
    let catalog: Value = read_json_file(&catalog_path).expect("read rolled-back catalog");
    assert_eq!(catalog, json!({ "models": ["old"] }));
    let auth: Value = read_json_file(&auth_path).expect("read concurrent auth");
    assert_eq!(
        auth.pointer("/tokens/refresh_token")
            .and_then(Value::as_str),
        Some("new-rt")
    );
}

#[test]
#[serial]
fn guarded_restore_delete_preserves_login_created_after_auth_claim() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let auth_path = crate::codex_config::get_codex_auth_path();
    write_json_file(
        &auth_path,
        &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
    )
    .expect("seed takeover auth");
    let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture takeover auth");
    let mut transaction =
        CodexAuthFileTransaction::begin(&auth_snapshot).expect("claim takeover auth");

    write_json_file(
        &auth_path,
        &json!({
            "auth_mode": "chatgpt",
            "tokens": { "refresh_token": "new-rt" }
        }),
    )
    .expect("simulate login after claim");
    transaction.install(None).expect("stage auth deletion");
    transaction.commit().expect("commit auth deletion");

    let current: Value = read_json_file(&auth_path).expect("read newer auth");
    assert_eq!(
        current
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str),
        Some("new-rt")
    );
}

#[test]
#[serial]
fn guarded_restore_write_rejects_login_created_after_auth_claim() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let auth_path = crate::codex_config::get_codex_auth_path();
    write_json_file(
        &auth_path,
        &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
    )
    .expect("seed takeover auth");
    let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture takeover auth");
    let mut transaction =
        CodexAuthFileTransaction::begin(&auth_snapshot).expect("claim takeover auth");

    write_json_file(
        &auth_path,
        &json!({
            "auth_mode": "chatgpt",
            "tokens": { "refresh_token": "new-rt" }
        }),
    )
    .expect("simulate login after claim");
    transaction
        .install(Some(br#"{"OPENAI_API_KEY":"stale-key"}"#.to_vec()))
        .expect_err("newer login must win the no-clobber install");

    let current: Value = read_json_file(&auth_path).expect("read newer auth");
    assert_eq!(
        current
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str),
        Some("new-rt")
    );
}

#[test]
#[serial]
fn guarded_restore_final_config_failure_rolls_back_auth_and_catalog() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");
    seed_codex_model_template();

    let auth_path = crate::codex_config::get_codex_auth_path();
    let original_auth = json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER });
    write_json_file(&auth_path, &original_auth).expect("seed takeover auth");
    let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture takeover auth");

    let catalog_path = crate::codex_config::get_codex_model_catalog_path();
    let original_catalog = json!({ "models": ["old"] });
    write_json_file(&catalog_path, &original_catalog).expect("seed catalog");

    let config_path = crate::codex_config::get_codex_config_path();
    std::fs::create_dir(&config_path).expect("make config target unwritable as a file");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db);
    let error = service
        .write_codex_live_verbatim_with_optional_auth_guard(
            &json!({
                "auth": { "OPENAI_API_KEY": "restored-key" },
                "config": "model = \"model-a\"\n",
                "modelCatalog": { "models": [{ "model": "model-a" }] }
            }),
            Some(&auth_snapshot),
        )
        .expect_err("config write must fail");

    assert!(error.contains("config"));
    let auth: Value = read_json_file(&auth_path).expect("read rolled-back auth");
    assert_eq!(auth, original_auth);
    let catalog: Value = read_json_file(&catalog_path).expect("read rolled-back catalog");
    assert_eq!(catalog, original_catalog);
}

/// Live auth.json can advance through Codex login/token refresh or a managed
/// account switch. That state is newer than the takeover-start snapshot, so
/// even an official-shape backup must not roll live tokens back. The auth
/// check must not depend on config.toml parsing.
#[tokio::test]
#[serial]
async fn restore_prefers_live_codex_oauth_tokens_over_backup_snapshot() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": { "refresh_token": "backup-rt" }
            },
            "config": "model_provider = \"any\"\n"
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");

    crate::codex_config::write_codex_live_atomic(
        &json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": { "refresh_token": "live-rt" }
        }),
        Some("model_provider = \"any\"\n"),
    )
    .expect("seed live codex files");
    crate::config::write_text_file(
        &crate::codex_config::get_codex_config_path(),
        "model_provider = [",
    )
    .expect("corrupt live config after writing valid auth");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex");

    let live = service.read_codex_live().expect("read live");
    assert_eq!(
        live.get("auth")
            .and_then(|auth| auth.get("tokens"))
            .and_then(|t| t.get("refresh_token"))
            .and_then(|v| v.as_str()),
        Some("live-rt"),
        "restore must not roll live tokens back to the takeover-start snapshot"
    );
}

/// Regression for #6277 with the metadata-residue backup shape: an
/// `OPENAI_API_KEY` accompanied by `last_refresh` / `tokens.account_id`
/// is NOT a login and must not be restored over a real live login.
#[tokio::test]
#[serial]
async fn restore_keeps_codex_oauth_login_when_backup_key_carries_metadata_residue() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": {
                "OPENAI_API_KEY": "sk-third-party",
                "last_refresh": "2026-08-01T00:00:00Z",
                "tokens": { "account_id": "acc" }
            },
            "config": "model_provider = \"any\"\n"
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");

    crate::codex_config::write_codex_live_atomic(
        &json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": { "refresh_token": "rt" }
        }),
        Some("model_provider = \"any\"\n"),
    )
    .expect("seed live codex files");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex");

    let live = service.read_codex_live().expect("read live");
    assert_eq!(
        live.get("auth")
            .and_then(|auth| auth.get("tokens"))
            .and_then(|t| t.get("refresh_token"))
            .and_then(|v| v.as_str()),
        Some("rt"),
        "metadata residue must not shield the backup API key from demotion"
    );
    assert!(
        live.get("config")
            .and_then(|v| v.as_str())
            .is_some_and(|cfg| cfg.contains("sk-third-party")),
        "backup API key must still be demoted into config.toml"
    );
}

/// Metadata residue on the LIVE side is not a login either: restore must
/// still write the backup auth verbatim instead of protecting stale
/// `last_refresh` / `tokens.account_id` leftovers.
#[tokio::test]
#[serial]
async fn restore_writes_codex_backup_auth_when_live_has_only_metadata_residue() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "sk-restored" },
            "config": "model_provider = \"any\"\n"
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");

    crate::codex_config::write_codex_live_atomic(
        &json!({
            "OPENAI_API_KEY": "sk-stale",
            "last_refresh": "2026-08-01T00:00:00Z",
            "tokens": { "account_id": "acc" }
        }),
        Some("model_provider = \"any\"\n"),
    )
    .expect("seed live codex files");

    service
        .restore_live_config_for_app_with_fallback(&AppType::Codex)
        .await
        .expect("restore codex");

    let live = service.read_codex_live().expect("read live");
    assert_eq!(
        live.get("auth")
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(|v| v.as_str()),
        Some("sk-restored"),
        "live metadata residue is not a login and must not block the restore"
    );
}

/// The simple restore path (`restore_live_config_for_app_inner`, used by
/// takeover rebuild and takeover-failure rollback) must apply the same
/// OAuth-login protection as the with_fallback path.
#[tokio::test]
#[serial]
async fn simple_restore_path_keeps_codex_oauth_login() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    db.save_live_backup(
        "codex",
        &serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "sk-third-party" },
            "config": "model_provider = \"any\"\n"
        }))
        .expect("serialize backup"),
    )
    .await
    .expect("seed live backup");

    crate::codex_config::write_codex_live_atomic(
        &json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": { "refresh_token": "rt" }
        }),
        Some("model_provider = \"any\"\n"),
    )
    .expect("seed live codex files");

    service
        .restore_live_config_for_app_inner(&AppType::Codex)
        .await
        .expect("restore codex via simple path");

    let live = service.read_codex_live().expect("read live");
    assert_eq!(
        live.get("auth")
            .and_then(|auth| auth.get("tokens"))
            .and_then(|t| t.get("refresh_token"))
            .and_then(|v| v.as_str()),
        Some("rt"),
        "takeover rebuild / rollback restore must not wipe the ChatGPT login"
    );
    assert!(
        live.get("config")
            .and_then(|v| v.as_str())
            .is_some_and(|cfg| cfg.contains("sk-third-party")),
        "backup API key must be demoted into config.toml on the simple path too"
    );
}

/// Regression: when Live is already a proxy placeholder (a corrupted state
/// where previous stop failed to restore), backup must NOT overwrite a
/// previously-good backup with the proxy config. This prevents the bug
/// where stop-then-start cycles permanently corrupt the backup.
#[tokio::test]
#[serial]
async fn backup_skips_when_live_is_already_proxy_placeholder() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    // Seed a GOOD backup (the "real" original Live)
    let good_backup = serde_json::to_string(&json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://api.minimaxi.com/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "real-token"
        }
    }))
    .expect("serialize good backup");
    db.save_live_backup("claude", &good_backup)
        .await
        .expect("seed good backup");

    // Seed Live with proxy placeholder (the corrupted state)
    service
        .write_claude_live(&json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
            }
        }))
        .expect("seed taken-over live file");

    // Call backup_live_config_strict: must skip
    service
        .backup_live_config_strict(&AppType::Claude)
        .await
        .expect("backup should succeed (no-op when live is placeholder)");

    // The good backup must still be intact
    let backup_after = db
        .get_live_backup("claude")
        .await
        .expect("get backup")
        .expect("backup still exists");
    assert_eq!(
        backup_after.original_config, good_backup,
        "must not overwrite a good backup with a proxy placeholder"
    );
}

/// Regression: when ALL apps have Live=proxy-placeholder (worst-case
/// corrupted state), the bulk `backup_live_configs` path used by
/// `start_with_takeover` must skip every save — instead of overwriting
/// good backups with the proxy config.
#[tokio::test]
#[serial]
async fn bulk_backup_skips_all_when_live_is_proxy_placeholder() {
    let _home = TempHome::new();
    crate::settings::reload_settings().expect("reload settings");

    let db = Arc::new(Database::memory().expect("init db"));
    let service = ProxyService::new(db.clone());

    // Seed good backups for all three apps
    let good_backup = serde_json::to_string(&json!({
        "env": {
            "ANTHROPIC_AUTH_TOKEN": "real-token"
        }
    }))
    .expect("serialize good backup");
    db.save_live_backup("claude", &good_backup)
        .await
        .expect("seed claude backup");

    let codex_good_backup = serde_json::to_string(&json!({
        "auth": { "OPENAI_API_KEY": "real-codex-token" }
    }))
    .expect("serialize codex good backup");
    db.save_live_backup("codex", &codex_good_backup)
        .await
        .expect("seed codex backup");

    // Seed all Live files with proxy placeholders
    service
        .write_claude_live(&json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
            }
        }))
        .expect("seed claude live");
    let codex_dir = crate::codex_config::get_codex_config_dir();
    std::fs::create_dir_all(&codex_dir).expect("create codex dir");
    std::fs::write(
        crate::codex_config::get_codex_config_path(),
        r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "chat"
experimental_bearer_token = "PROXY_MANAGED"
"#,
    )
    .expect("seed codex config.toml");
    std::fs::write(
        crate::codex_config::get_codex_auth_path(),
        r#"{"OPENAI_API_KEY":"PROXY_MANAGED"}"#,
    )
    .expect("seed codex auth.json");
    // Call bulk backup: must skip all apps
    service
        .backup_live_configs()
        .await
        .expect("bulk backup should succeed (no-op when all live are placeholders)");

    // All good backups must still be intact
    for (app_type, original) in [
        ("claude", good_backup.as_str()),
        ("codex", codex_good_backup.as_str()),
    ] {
        let backup_after = db
            .get_live_backup(app_type)
            .await
            .expect("get backup")
            .expect("backup still exists");
        assert_eq!(
            backup_after.original_config, original,
            "must not overwrite good backup for {app_type} with proxy placeholder"
        );
    }
}
