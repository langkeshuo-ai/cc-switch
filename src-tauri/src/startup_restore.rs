//! 启动阶段的数据库/配置状态恢复（自 lib.rs 拆出，M2）。
//!
//! 在 setup 闭包中调用：先抽取公共配置片段（必须先于代理恢复，
//! 否则会把代理占位符配置当成用户真实 Live 配置读走），再恢复代理接管状态。

use crate::store;

// ============================================================
// 启动时恢复代理状态
// ============================================================

/// 启动时需要恢复代理接管状态的应用列表。
///
/// 只含 claude / codex：这两个应用走"本地代理接管 Live 配置"路径，代理状态
/// 持久化在 `proxy_config` 表，重启后需要恢复。Pi 走独立的原生配置写入路径
/// （`services/provider/pi.rs`），不经过本地代理；Gemini 已在 v22 trim 中
/// 移除（见 commit 5d40d825）。若未来 Pi 也接入代理接管，需要在此处补充并
/// 同步 `enabled_proxy_apps_on_startup` 的读取逻辑。
const PROXY_STARTUP_APP_TYPES: [&str; 2] = ["claude", "codex"];

pub(crate) async fn enabled_proxy_apps_on_startup(
    db: &crate::database::Database,
) -> Vec<&'static str> {
    let mut apps = Vec::new();
    for app_type in PROXY_STARTUP_APP_TYPES {
        if db
            .get_proxy_config_for_app(app_type)
            .await
            .is_ok_and(|config| config.enabled)
        {
            apps.push(app_type);
        }
    }
    apps
}

/// 启动时根据 proxy_config 表中的代理状态自动恢复代理服务
///
/// 检查 `proxy_config.enabled` 字段，如果有任一应用的状态为 `true`，
/// 则自动启动代理服务并接管对应应用的 Live 配置。
pub(crate) async fn restore_proxy_state_on_startup(state: &store::AppState) {
    // 收集需要恢复接管的应用列表（从 proxy_config.enabled 读取）
    let apps_to_restore = enabled_proxy_apps_on_startup(&state.db).await;

    if apps_to_restore.is_empty() {
        log::debug!("启动时无需恢复代理状态");
        return;
    }

    log::info!("检测到上次代理状态需要恢复，应用列表: {apps_to_restore:?}");

    // 逐个恢复接管状态
    for app_type in apps_to_restore {
        match state
            .proxy_service
            .set_takeover_for_app(app_type, true)
            .await
        {
            Ok(()) => {
                log::info!("✓ 已恢复 {app_type} 的代理接管状态");
            }
            Err(e) => {
                log::error!("✗ 恢复 {app_type} 的代理接管状态失败: {e}");
                // 失败时清除该应用的状态，避免下次启动再次尝试
                if let Err(clear_err) = state
                    .proxy_service
                    .set_takeover_for_app(app_type, false)
                    .await
                {
                    log::error!("清除 {app_type} 代理状态失败: {clear_err}");
                }
            }
        }
    }
}

pub(crate) fn initialize_common_config_snippets(state: &store::AppState) {
    // Auto-extract common config snippets from clean live files when snippet is missing.
    // This must run before proxy takeover is restored on startup, otherwise we'd read
    // proxy-placeholder configs instead of the user's actual live settings.
    for app_type in crate::app_config::AppType::all() {
        if !state
            .db
            .should_auto_extract_config_snippet(app_type.as_str())
            .unwrap_or(false)
        {
            continue;
        }

        let settings = match crate::services::provider::ProviderService::read_live_settings(
            app_type.clone(),
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };

        match crate::services::provider::ProviderService::extract_common_config_snippet_from_settings(
            app_type.clone(),
            &settings,
        ) {
            Ok(snippet) if !snippet.is_empty() && snippet != "{}" => {
                match state.db.set_config_snippet(app_type.as_str(), Some(snippet)) {
                    Ok(()) => {
                        if let Err(e) =
                            state.db.set_config_snippet_cleared(app_type.as_str(), false)
                        {
                            log::warn!(
                                "已保存通用配置片段，但清除 snippet_cleared 标记失败 {}: {e}",
                                app_type.as_str()
                            );
                        }
                        log::info!(
                            "✓ Auto-extracted common config snippet for {}",
                            app_type.as_str()
                        );
                    }
                    Err(e) => log::warn!(
                        "✗ Failed to save config snippet for {}: {e}",
                        app_type.as_str()
                    ),
                }
            }
            Ok(_) => log::debug!(
                "○ Live config for {} has no extractable common fields",
                app_type.as_str()
            ),
            Err(e) => log::warn!(
                "✗ Failed to extract config snippet for {}: {e}",
                app_type.as_str()
            ),
        }
    }

    let should_run_legacy_migration = state
        .db
        .is_legacy_common_config_migrated()
        .map(|done| !done)
        .unwrap_or(true);

    if should_run_legacy_migration {
        for app_type in [
            crate::app_config::AppType::Claude,
            crate::app_config::AppType::Codex,
        ] {
            if let Err(e) = crate::services::provider::ProviderService::migrate_legacy_common_config_usage_if_needed(
                state,
                app_type.clone(),
            ) {
                log::warn!(
                    "✗ Failed to migrate legacy common-config usage for {}: {e}",
                    app_type.as_str()
                );
            }
        }

        if let Err(e) = state.db.set_legacy_common_config_migrated(true) {
            log::warn!("✗ Failed to persist legacy common-config migration flag: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::enabled_proxy_apps_on_startup;
    use crate::database::Database;

    #[tokio::test]
    async fn startup_restore_includes_enabled_codex_route() {
        let db = Database::memory().expect("initialize database");
        let mut config = db
            .get_proxy_config_for_app("codex")
            .await
            .expect("read Codex proxy config");
        config.enabled = true;
        db.update_proxy_config_for_app(config)
            .await
            .expect("enable Codex proxy config");

        let apps = enabled_proxy_apps_on_startup(&db).await;

        assert_eq!(apps, vec!["codex"]);
    }
}
