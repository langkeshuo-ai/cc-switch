//! 应用配置快照命令
//!
//! 与「项目 Profile」命令（commands/profile.rs）是两个独立功能：本模块
//! 面向三应用（claude/codex/pi）整体工作环境的保存与一键恢复。

use tauri::Emitter;

use crate::services::snapshots::{SnapshotApplyResult, SnapshotMeta, SnapshotService};
use crate::store::AppState;

/// 列出所有快照（附每个应用将恢复到的供应商名称）
#[tauri::command]
pub fn list_app_snapshots(state: tauri::State<'_, AppState>) -> Result<Vec<SnapshotMeta>, String> {
    SnapshotService::list_meta(&state).map_err(|e| e.to_string())
}

/// 保存当前状态为命名快照（同名覆盖，前端对覆盖做二次确认）
#[tauri::command]
pub fn save_app_snapshot(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    SnapshotService::save(&state, &name).map_err(|e| e.to_string())
}

/// 快照应用完成后的统一收尾：发事件 + 重建托盘菜单
fn emit_snapshot_apply_events(app: &tauri::AppHandle, state: &AppState) {
    for app_type in crate::app_config::AppType::all() {
        let app_str = app_type.as_str();
        let (proxy_enabled, auto_failover_enabled) = state.db.get_proxy_flags_sync(app_str);
        let provider_id = crate::settings::get_effective_current_provider_with(
            &state.db,
            &app_type,
            crate::pi_config::pi_proxy_current_provider_key,
        )
        .ok()
        .flatten()
        .unwrap_or_default();
        let event_data = serde_json::json!({
            "appType": app_str,
            "proxyEnabled": proxy_enabled,
            "autoFailoverEnabled": auto_failover_enabled,
            "providerId": provider_id,
        });
        if let Err(e) = app.emit("provider-switched", event_data) {
            log::error!("发射 provider-switched 事件失败: {e}");
        }
    }
    crate::tray::refresh_tray_menu(app);
}

/// 应用命名快照（逐应用恢复供应商与接管状态，best-effort）
///
/// 注意：必须保持同步命令（跑在 Tauri 线程池）——`ProviderService::switch`
/// 内部使用 block_on 获取切换锁，放进 async 命令会在运行时线程上 panic。
#[tauri::command]
pub fn apply_app_snapshot(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    name: String,
) -> Result<SnapshotApplyResult, String> {
    let result = SnapshotService::apply(&state, &name).map_err(|e| e.to_string())?;
    emit_snapshot_apply_events(&app, &state);
    Ok(result)
}

/// 删除快照
#[tauri::command]
pub fn delete_app_snapshot(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    SnapshotService::delete(&state, &name).map_err(|e| e.to_string())
}
