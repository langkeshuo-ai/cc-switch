#![allow(non_snake_case)]

use indexmap::IndexMap;

use tauri::State;

use crate::claude_mcp;
use crate::services::mcp::McpAppSyncOutcome;
use crate::services::McpService;
use crate::store::AppState;

/// 获取 Claude MCP 状态
#[tauri::command]
pub async fn get_claude_mcp_status() -> Result<claude_mcp::McpStatus, String> {
    claude_mcp::get_mcp_status().map_err(|e| e.to_string())
}

/// 读取 mcp.json 文本内容
#[tauri::command]
pub async fn read_claude_mcp_config() -> Result<Option<String>, String> {
    claude_mcp::read_mcp_json().map_err(|e| e.to_string())
}

/// 新增或更新一个 MCP 服务器条目
#[tauri::command]
pub async fn upsert_claude_mcp_server(id: String, spec: serde_json::Value) -> Result<bool, String> {
    claude_mcp::upsert_mcp_server(&id, spec).map_err(|e| e.to_string())
}

/// 删除一个 MCP 服务器条目
#[tauri::command]
pub async fn delete_claude_mcp_server(id: String) -> Result<bool, String> {
    claude_mcp::delete_mcp_server(&id).map_err(|e| e.to_string())
}

/// 校验命令是否在 PATH 中可用（不执行）
#[tauri::command]
pub async fn validate_mcp_command(cmd: String) -> Result<bool, String> {
    claude_mcp::validate_command_in_path(&cmd).map_err(|e| e.to_string())
}

// ============================================================================
// v3.7.0 新增：统一 MCP 管理命令
// ============================================================================

use crate::app_config::McpServer;

/// 获取所有 MCP 服务器（统一结构）
#[tauri::command]
pub async fn get_mcp_servers(
    state: State<'_, AppState>,
) -> Result<IndexMap<String, McpServer>, String> {
    McpService::get_all_servers(&state).map_err(|e| e.to_string())
}

/// 添加或更新 MCP 服务器
#[tauri::command]
pub async fn upsert_mcp_server(
    state: State<'_, AppState>,
    server: McpServer,
) -> Result<(), String> {
    McpService::upsert_server(&state, server).map_err(|e| e.to_string())
}

/// 删除 MCP 服务器
#[tauri::command]
pub async fn delete_mcp_server(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    McpService::delete_server(&state, &id).map_err(|e| e.to_string())
}

/// 切换 MCP 服务器在指定应用的启用状态
#[tauri::command]
pub async fn toggle_mcp_app(
    state: State<'_, AppState>,
    server_id: String,
    app: String,
    enabled: bool,
) -> Result<(), String> {
    let app_ty = super::parse_app(&app)?;
    McpService::toggle_app(&state, &server_id, app_ty, enabled).map_err(|e| e.to_string())
}

/// 从所有应用导入 MCP 服务器（复用已有的导入逻辑）
#[tauri::command]
pub async fn import_mcp_from_apps(state: State<'_, AppState>) -> Result<usize, String> {
    McpService::import_from_all_apps(&state).map_err(|e| e.to_string())
}

/// 重新同步 MCP 到各应用。
///
/// `apps` 缺省或为空时同步全部受管应用。每个应用先拿它的切换锁再写，
/// 和切换供应商互斥；单个应用失败（如配置文件解析失败）不影响其余应用。
#[tauri::command]
pub async fn resync_mcp_to_apps(
    state: State<'_, AppState>,
    apps: Option<Vec<String>>,
) -> Result<Vec<McpAppSyncOutcome>, String> {
    let targets = McpService::resync_targets(apps.as_deref()).map_err(|e| e.to_string())?;
    let mut outcomes = Vec::with_capacity(targets.len());
    for app in targets {
        let _guard = state.proxy_service.lock_switch_for_app(app.as_str()).await;
        outcomes.push(McpService::resync_app(&state, &app));
    }
    Ok(outcomes)
}
