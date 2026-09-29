//! 应用配置快照编排服务
//!
//! 快照记录三个应用（claude/codex/pi）各自的「当前供应商 id + 代理接管
//! 状态」，可一键整体恢复（参考 magpie 的 save/use 工作流）。与「项目
//! Profile」（services/profile.rs，按分组快照供应商/MCP/Skills/Prompt）
//! 是两个独立功能：本服务面向全局整体切换工作环境。
//!
//! apply 与 UI 手动切换共用同一代码路径（`ProviderService::switch`，内建
//! 代理接管热切换与接管下禁切官方）；接管状态恢复调用 ProxyService 的
//! 现成 `set_takeover_for_app` 接口，不自写文件操作。
//!
//! apply 为 best-effort：单个应用的失败记入 skipped 继续，不整体回滚。
//! 若某个应用在多步恢复的中途失败（如切换成功但接管开启失败），已完成
//! 的步骤保持生效；由于 apply 幂等，最经济的补救是重新执行一次「应用
//! 快照」，剩余未完成的步骤会被重新计算并补齐（失败时会以 warn 日志提示）。
//!
//! apply/save 与数据库导入恢复（import/export、WebDAV/S3 同步）同为
//! 「配置变更序列」，共用 sync_protocol::sync_mutex 串行化，避免两个
//! apply 并发交错（计划基于过期状态），或 apply 与数据库整体恢复竞态。

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::app_config::AppType;
use crate::error::AppError;
use crate::services::ProviderService;
use crate::store::AppState;

/// 单个应用在快照中的槽位
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotAppEntry {
    /// 快照时的当前供应商 id（None = 快照时无有效供应商，应用时跳过切换）
    pub provider_id: Option<String>,
    /// 快照时的代理接管状态
    pub takeover: bool,
}

/// 快照 JSON 结构（存入 app_snapshots.data，与规格书定义一致）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSnapshotData {
    pub name: String,
    pub created_at: i64,
    /// 按 app 分槽；key 与 AppType::as_str 一致（claude/codex/pi）
    pub apps: BTreeMap<String, SnapshotAppEntry>,
}

/// 单个应用的恢复步骤（纯决策，便于单元测试）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyStep {
    /// 先关闭接管（当前接管开、快照要求关：先恢复 live 再普通切换）
    DisableTakeover,
    /// 切换供应商到快照记录的目标
    SwitchProvider,
    /// 后开启接管（快照要求开、当前关：切换完成后再接管）
    EnableTakeover,
}

/// 计算单个应用从当前状态到快照目标的恢复步骤
///
/// - 目标供应商与当前相同 → 不切换（幂等）
/// - 需要关接管时先关（否则 live 文件归代理所有，普通切换写不进去）
/// - 需要开接管时后开（先以真实供应商落盘，再备份并改写为代理占位）
pub fn plan_app_restore(
    current_provider_id: Option<&str>,
    current_takeover: bool,
    entry: &SnapshotAppEntry,
) -> Vec<ApplyStep> {
    let mut steps = Vec::new();
    if current_takeover && !entry.takeover {
        steps.push(ApplyStep::DisableTakeover);
    }
    if entry.provider_id.is_some() && current_provider_id != entry.provider_id.as_deref() {
        steps.push(ApplyStep::SwitchProvider);
    }
    if entry.takeover && !current_takeover {
        steps.push(ApplyStep::EnableTakeover);
    }
    steps
}

/// 列表展示用的单应用槽位（provider_id 查不到对应供应商时 provider_name 为 None，
/// 前端显示为「(已删除)」）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotAppMeta {
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub takeover: bool,
}

/// 列表展示用的快照元信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMeta {
    pub name: String,
    pub created_at: i64,
    pub apps: BTreeMap<String, SnapshotAppMeta>,
}

/// 应用快照时被跳过的应用
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedApp {
    pub app: String,
    /// 稳定原因码（可带 ": detail" 后缀），前端按码做 i18n
    pub reason: String,
}

/// 应用快照的结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotApplyResult {
    pub applied: Vec<String>,
    pub skipped: Vec<SkippedApp>,
}

pub struct SnapshotService;

/// 串行化配置变更序列：复用同步层的全局 `sync_mutex`（数据库导入/恢复、
/// WebDAV/S3 同步共用同一把锁），使快照 apply/save 与这些序列互斥。
///
/// 调用方为同步 Tauri 命令（跑在 Tauri 线程池而非 tokio worker 线程），
/// 因此这里的 `block_on` 是安全的，与 `apply_one` 内部对
/// `set_takeover_for_app` 的既有用法一致；异步任务持有该锁时不会走到本函数。
fn lock_config_sequence() -> tokio::sync::MutexGuard<'static, ()> {
    tauri::async_runtime::block_on(crate::services::sync_protocol::sync_mutex().lock())
}

impl SnapshotService {
    /// 读取应用当前的"有效供应商 id"。
    ///
    /// Pi 的唯一权威来源是 Pi settings.json 的 `defaultProvider`（Pi CLI 只认
    /// 它，与 CC Switch 数据库的 current provider 可能分叉），因此对 Pi 必须
    /// 读取实际文件；仅当 settings.json 缺失或不可读时才退回数据库 current
    /// provider。其他应用维持原逻辑。
    fn capture_provider_id(state: &AppState, app: &AppType) -> Result<Option<String>, AppError> {
        if app == &AppType::Pi {
            let settings_exists = crate::pi_config::get_pi_settings_path()
                .map(|path| path.exists())
                .unwrap_or(false);
            if settings_exists {
                match crate::pi_config::read_pi_native_defaults() {
                    Ok(defaults) => {
                        return Ok(defaults
                            .default_provider
                            .filter(|key| !key.trim().is_empty()));
                    }
                    Err(error) => {
                        log::warn!(
                            "读取 Pi settings.json 失败，快照退回数据库 current provider: {error}"
                        );
                    }
                }
            }
        }
        crate::settings::get_effective_current_provider(&state.db, app)
    }

    /// 抓取三个应用的当前状态
    fn capture_apps(state: &AppState) -> Result<BTreeMap<String, SnapshotAppEntry>, AppError> {
        let mut apps = BTreeMap::new();
        for app in AppType::all() {
            let provider_id = Self::capture_provider_id(state, &app)?;
            // 接管状态与 UI 展示同源：proxy_config.enabled（get_proxy_flags_sync 的第一个值）
            let takeover = state.db.get_proxy_flags_sync(app.as_str()).0;
            apps.insert(
                app.as_str().to_string(),
                SnapshotAppEntry {
                    provider_id,
                    takeover,
                },
            );
        }
        Ok(apps)
    }

    /// 保存当前状态为命名快照（同名覆盖，覆盖时沿用原 created_at）
    pub fn save(state: &AppState, name: &str) -> Result<(), AppError> {
        let _sequence_guard = lock_config_sequence();
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::InvalidInput("快照名称不能为空".to_string()));
        }
        let created_at = match state.db.get_app_snapshot(name)? {
            Some(old) => old.created_at,
            None => chrono::Utc::now().timestamp(),
        };
        let data = AppSnapshotData {
            name: name.to_string(),
            created_at,
            apps: Self::capture_apps(state)?,
        };
        let payload = serde_json::to_string(&data)
            .map_err(|e| AppError::Config(format!("序列化快照失败: {e}")))?;
        state.db.save_app_snapshot(&crate::database::AppSnapshot {
            name: name.to_string(),
            data: payload,
            created_at,
        })?;
        Ok(())
    }

    /// 列出所有快照（附每个应用将恢复到的供应商名称，便于 UI 展示）
    pub fn list_meta(state: &AppState) -> Result<Vec<SnapshotMeta>, AppError> {
        let rows = state.db.get_all_app_snapshots()?;

        let mut provider_maps: HashMap<String, _> = HashMap::new();
        for app in AppType::all() {
            provider_maps.insert(
                app.as_str().to_string(),
                state.db.get_all_providers(app.as_str())?,
            );
        }

        Ok(rows
            .into_iter()
            .map(|row| {
                // 单条 payload 损坏不应拖垮整个列表：降级为默认值并记日志
                let data: AppSnapshotData = serde_json::from_str(&row.data).unwrap_or_else(|e| {
                    log::warn!("解析快照 '{}' 失败，使用默认值: {e}", row.name);
                    AppSnapshotData {
                        name: row.name.clone(),
                        created_at: row.created_at,
                        apps: BTreeMap::new(),
                    }
                });
                let mut apps = BTreeMap::new();
                for app in AppType::all() {
                    let entry = data.apps.get(app.as_str()).cloned().unwrap_or_default();
                    let provider_name = entry
                        .provider_id
                        .as_ref()
                        .and_then(|pid| provider_maps.get(app.as_str())?.get(pid))
                        .map(|p| p.name.clone());
                    apps.insert(
                        app.as_str().to_string(),
                        SnapshotAppMeta {
                            provider_id: entry.provider_id,
                            provider_name,
                            takeover: entry.takeover,
                        },
                    );
                }
                SnapshotMeta {
                    name: row.name,
                    created_at: row.created_at,
                    apps,
                }
            })
            .collect())
    }

    /// 应用快照（best-effort）：逐应用恢复供应商与接管状态
    pub fn apply(state: &AppState, name: &str) -> Result<SnapshotApplyResult, AppError> {
        let _sequence_guard = lock_config_sequence();
        let row = state
            .db
            .get_app_snapshot(name)?
            .ok_or_else(|| AppError::InvalidInput(format!("快照不存在: {name}")))?;
        let data: AppSnapshotData = serde_json::from_str(&row.data)
            .map_err(|e| AppError::Config(format!("解析快照失败: {e}")))?;

        let mut result = SnapshotApplyResult {
            applied: Vec::new(),
            skipped: Vec::new(),
        };
        for app in AppType::all() {
            let entry = match data.apps.get(app.as_str()) {
                Some(entry) => entry.clone(),
                None => {
                    result.skipped.push(SkippedApp {
                        app: app.as_str().to_string(),
                        reason: "snapshot_entry_missing".to_string(),
                    });
                    continue;
                }
            };
            match Self::apply_one(state, &app, &entry) {
                Ok(()) => result.applied.push(app.as_str().to_string()),
                Err(reason) => {
                    // 部分应用语义：该应用已完成的前序步骤保持生效，不做回滚
                    //（apply 幂等，重新执行一次即可补齐剩余步骤）
                    log::warn!(
                        "应用快照 '{name}' 时 {} 恢复失败（{reason}）：已完成步骤保持现状，可重新执行应用快照补齐剩余恢复",
                        app.as_str()
                    );
                    result.skipped.push(SkippedApp {
                        app: app.as_str().to_string(),
                        reason,
                    });
                }
            }
        }
        Ok(result)
    }

    /// 恢复单个应用，失败返回 "原因码: 详情"
    fn apply_one(state: &AppState, app: &AppType, entry: &SnapshotAppEntry) -> Result<(), String> {
        // 供应商已被删除 → 跳过（其余步骤也没有意义：接管恢复依赖切换后的 live）
        if let Some(pid) = entry.provider_id.as_deref() {
            let providers = state
                .db
                .get_all_providers(app.as_str())
                .map_err(|e| format!("provider_list_failed: {e}"))?;
            if !providers.contains_key(pid) {
                return Err(format!("provider_missing: {pid}"));
            }
        }

        let current_provider = Self::capture_provider_id(state, app)
            .map_err(|e| format!("current_provider_failed: {e}"))?;
        let current_takeover = state.db.get_proxy_flags_sync(app.as_str()).0;
        let steps = plan_app_restore(current_provider.as_deref(), current_takeover, entry);

        for step in steps {
            match step {
                ApplyStep::DisableTakeover => tauri::async_runtime::block_on(
                    state
                        .proxy_service
                        .set_takeover_for_app(app.as_str(), false),
                )
                .map_err(|e| format!("takeover_off_failed: {e}"))?,
                ApplyStep::SwitchProvider => {
                    let pid = entry.provider_id.clone().unwrap_or_default();
                    ProviderService::switch(state, app.clone(), &pid)
                        .map_err(|e| format!("switch_failed: {e}"))?;
                    // Pi CLI 只跟随 settings.json 的 defaultProvider；membership
                    // 切换（pi::enable）不会改它，这里补写使 Pi 真正指向快照
                    // 记录的供应商。
                    if app == &AppType::Pi {
                        crate::pi_config::set_pi_default_provider(&pid)
                            .map_err(|e| format!("pi_default_provider_failed: {e}"))?;
                    }
                }
                ApplyStep::EnableTakeover => tauri::async_runtime::block_on(
                    state.proxy_service.set_takeover_for_app(app.as_str(), true),
                )
                .map_err(|e| format!("takeover_on_failed: {e}"))?,
            }
        }
        Ok(())
    }

    /// 删除快照；不存在时报错（由调用方决定如何提示）
    pub fn delete(state: &AppState, name: &str) -> Result<(), AppError> {
        if !state.db.delete_app_snapshot(name)? {
            return Err(AppError::InvalidInput(format!("快照不存在: {name}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::pi_config::test_support::TestAgentDir;
    use crate::provider::Provider;
    use serde_json::json;
    use serial_test::serial;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// 测试期把 HOME/CC_SWITCH_TEST_HOME 指向临时目录，隔离 claude/codex
    /// 的真实用户配置（与 provider 模块测试的 TempHome 同一套路）。
    struct TestHomeGuard {
        _dir: tempfile::TempDir,
        home: Option<std::ffi::OsString>,
        userprofile: Option<std::ffi::OsString>,
        test_home: Option<std::ffi::OsString>,
    }

    impl TestHomeGuard {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().expect("create temp home");
            let home = std::env::var_os("HOME");
            let userprofile = std::env::var_os("USERPROFILE");
            let test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("HOME", dir.path());
            std::env::set_var("USERPROFILE", dir.path());
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");
            Self {
                _dir: dir,
                home,
                userprofile,
                test_home,
            }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            match self.home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.userprofile.take() {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
            match self.test_home.take() {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
            let _ = crate::settings::reload_settings();
        }
    }

    fn entry(provider_id: Option<&str>, takeover: bool) -> SnapshotAppEntry {
        SnapshotAppEntry {
            provider_id: provider_id.map(|s| s.to_string()),
            takeover,
        }
    }

    fn pi_provider(id: &str) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            "Test provider".to_string(),
            json!({
                "name": "Test provider",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "secret",
                "api": "openai-completions",
                "models": [{ "id": "model-a" }]
            }),
            None,
        );
        provider.category = Some("custom".to_string());
        provider
    }

    fn write_pi_settings(content: &str) {
        let settings_path = crate::pi_config::get_pi_settings_path().unwrap();
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(&settings_path, content).unwrap();
    }

    fn claude_provider(id: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            "Claude provider".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": format!("token-{id}"),
                    "ANTHROPIC_BASE_URL": "https://api.example.com"
                }
            }),
            None,
        )
    }

    fn codex_provider(id: &str) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            "Codex provider".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": format!("sk-{id}") },
                "config": format!(
                    "model_provider = \"{id}\"\n\
                     [model_providers.{id}]\n\
                     name = \"{id}\"\n\
                     base_url = \"https://{id}.example/v1\"\n\
                     wire_api = \"chat\"\n"
                )
            }),
            None,
        );
        provider.category = Some("custom".to_string());
        provider
    }

    /// 建立三应用各两个供应商，并让 current 指向各自的 "a" 槽位
    /// （claude/codex = DB + 本地 settings，pi = settings.json defaultProvider）。
    fn seed_three_apps_with_current(state: &AppState) {
        ProviderService::add(state, AppType::Pi, pi_provider("pi-a"), false).expect("add pi-a");
        ProviderService::add(state, AppType::Pi, pi_provider("pi-b"), false).expect("add pi-b");
        state
            .db
            .save_provider("claude", &claude_provider("cl-a"))
            .expect("save cl-a");
        state
            .db
            .save_provider("claude", &claude_provider("cl-b"))
            .expect("save cl-b");
        state
            .db
            .save_provider("codex", &codex_provider("cx-a"))
            .expect("save cx-a");
        state
            .db
            .save_provider("codex", &codex_provider("cx-b"))
            .expect("save cx-b");

        state.db.set_current_provider("claude", "cl-a").unwrap();
        crate::settings::set_current_provider(&AppType::Claude, Some("cl-a")).unwrap();
        state.db.set_current_provider("codex", "cx-a").unwrap();
        crate::settings::set_current_provider(&AppType::Codex, Some("cx-a")).unwrap();
        write_pi_settings(r#"{"defaultProvider":"pi-a"}"#);
    }

    /// 把三个应用的 current 全部漂移到 "b" 槽位
    fn drift_all_apps_to_b(state: &AppState) {
        state.db.set_current_provider("claude", "cl-b").unwrap();
        crate::settings::set_current_provider(&AppType::Claude, Some("cl-b")).unwrap();
        state.db.set_current_provider("codex", "cx-b").unwrap();
        crate::settings::set_current_provider(&AppType::Codex, Some("cx-b")).unwrap();
        write_pi_settings(r#"{"defaultProvider":"pi-b"}"#);
    }

    #[test]
    #[serial]
    fn multi_app_snapshot_apply_restores_all_three_apps() {
        let _home = TestHomeGuard::new();
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        seed_three_apps_with_current(&state);

        SnapshotService::save(&state, "work").expect("save snapshot");
        drift_all_apps_to_b(&state);

        let result = SnapshotService::apply(&state, "work").expect("apply snapshot");
        assert!(
            result.skipped.is_empty(),
            "no app should be skipped: {:?}",
            result.skipped
        );
        for app in ["claude", "codex", "pi"] {
            assert!(
                result.applied.iter().any(|a| a == app),
                "{app} must be applied; got {:?}",
                result.applied
            );
        }

        // 供应商恢复到快照时的 "a" 槽位
        assert_eq!(
            crate::settings::get_effective_current_provider(&state.db, &AppType::Claude)
                .unwrap()
                .as_deref(),
            Some("cl-a"),
            "claude current provider must be restored"
        );
        assert_eq!(
            crate::settings::get_effective_current_provider(&state.db, &AppType::Codex)
                .unwrap()
                .as_deref(),
            Some("cx-a"),
            "codex current provider must be restored"
        );
        assert_eq!(
            crate::pi_config::pi_proxy_current_provider_key().as_deref(),
            Some("pi-a"),
            "Pi defaultProvider must be restored"
        );

        // 接管标志恢复到快照记录的状态（本测试快照记录的均为关闭）
        for app in ["claude", "codex", "pi"] {
            assert!(
                !state.db.get_proxy_flags_sync(app).0,
                "{app} takeover flag must match the snapshot (off)"
            );
        }
    }

    #[test]
    #[serial]
    fn apply_reports_missing_provider_per_app_and_restores_the_rest() {
        let _home = TestHomeGuard::new();
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        seed_three_apps_with_current(&state);

        SnapshotService::save(&state, "work").expect("save snapshot");
        drift_all_apps_to_b(&state);
        // 部分失败场景：claude 的快照目标供应商在 apply 前被删除
        state
            .db
            .delete_provider("claude", "cl-a")
            .expect("delete cl-a");

        let result = SnapshotService::apply(&state, "work").expect("apply snapshot");

        // claude 按应用粒度上报跳过，带稳定原因码
        let claude_skip = result
            .skipped
            .iter()
            .find(|s| s.app == "claude")
            .expect("claude must be reported as skipped");
        assert_eq!(claude_skip.reason, "provider_missing: cl-a");

        // 其余应用不受影响，照常恢复
        assert!(result.applied.iter().any(|a| a == "codex"));
        assert!(result.applied.iter().any(|a| a == "pi"));
        assert_eq!(
            crate::settings::get_effective_current_provider(&state.db, &AppType::Codex)
                .unwrap()
                .as_deref(),
            Some("cx-a")
        );
        assert_eq!(
            crate::pi_config::pi_proxy_current_provider_key().as_deref(),
            Some("pi-a")
        );
    }

    #[test]
    #[serial]
    fn apply_is_serialized_behind_the_config_sequence_lock() {
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));

        // 先持有配置变更序列锁（与 import/export、快照 apply 共用），
        // 并发的 apply 必须等待而不是交错执行
        let lock = crate::services::sync_protocol::sync_mutex().blocking_lock();
        let done = Arc::new(AtomicBool::new(false));
        let handle = {
            let done = done.clone();
            let state = state.clone();
            std::thread::spawn(move || {
                // 快照不存在 → InvalidInput；无论结果如何，必须在拿到锁后才返回
                let _ = SnapshotService::apply(&state, "missing");
                done.store(true, Ordering::Release);
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !done.load(Ordering::Acquire),
            "apply must wait for the config sequence lock instead of running concurrently"
        );

        drop(lock);
        handle.join().expect("apply thread must not panic");
        assert!(done.load(Ordering::Acquire));
    }

    #[test]
    #[serial]
    fn pi_snapshot_apply_result_is_what_provider_router_selects() {
        let _home = TestHomeGuard::new();
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        ProviderService::add(&state, AppType::Pi, pi_provider("pi-a"), false).expect("add pi-a");
        ProviderService::add(&state, AppType::Pi, pi_provider("pi-b"), false).expect("add pi-b");

        write_pi_settings(r#"{"defaultProvider":"pi-a"}"#);
        SnapshotService::save(&state, "route").expect("save snapshot");

        // 漂移：settings.json 与 DB current 都指向 pi-b
        write_pi_settings(r#"{"defaultProvider":"pi-b"}"#);
        state.db.set_current_provider("pi", "pi-b").unwrap();

        SnapshotService::apply(&state, "route").expect("apply snapshot");

        // 端到端切片：DB current 已经不指向快照目标，路由层必须经由
        // settings.json defaultProvider（apply 恢复后的值）选出正确供应商
        let router = crate::proxy::provider_router::ProviderRouter::new(state.db.clone());
        let selected = tauri::async_runtime::block_on(router.select_providers("pi", None))
            .expect("router must select a provider");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "pi-a");
    }

    #[test]
    #[serial]
    fn pi_snapshot_roundtrip_preserves_settings_default_provider() {
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        // DB-only 供应商（不进 models.json）
        ProviderService::add(&state, AppType::Pi, pi_provider("cc-switch-test"), false)
            .expect("save provider");

        // capture 必须读取 settings.json 的 defaultProvider（而非 DB current）
        write_pi_settings(r#"{"defaultProvider":"cc-switch-test"}"#);
        SnapshotService::save(&state, "pi-work").expect("save snapshot");
        let saved = state.db.get_app_snapshot("pi-work").unwrap().unwrap();
        let data: AppSnapshotData = serde_json::from_str(&saved.data).unwrap();
        assert_eq!(
            data.apps
                .get("pi")
                .and_then(|entry| entry.provider_id.clone()),
            Some("cc-switch-test".to_string()),
            "capture must record the actual Pi defaultProvider"
        );

        // 模拟漂移：defaultProvider 指向别处
        write_pi_settings(r#"{"defaultProvider":"somewhere-else"}"#);
        let result = SnapshotService::apply(&state, "pi-work").expect("apply snapshot");
        assert!(result.applied.iter().any(|app| app == "pi"));

        // apply 后 Pi CLI 唯一跟随的 defaultProvider 必须被恢复
        assert_eq!(
            crate::pi_config::pi_proxy_current_provider_key().as_deref(),
            Some("cc-switch-test"),
            "apply must restore Pi defaultProvider"
        );
        // 且该供应商已进入 models.json（membership 经 ProviderService::switch 恢复）
        assert!(
            crate::pi_config::pi_provider_exists("cc-switch-test").unwrap(),
            "apply must restore models.json membership via the normal switch path"
        );
    }

    #[test]
    #[serial]
    fn pi_snapshot_capture_falls_back_to_db_when_settings_unreadable() {
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        ProviderService::add(&state, AppType::Pi, pi_provider("cc-switch-test"), false)
            .expect("save provider");
        // settings.json 存在但不可解析 → 退回 DB current provider
        write_pi_settings("{not-json");
        state
            .db
            .set_current_provider(AppType::Pi.as_str(), "cc-switch-test")
            .unwrap();

        SnapshotService::save(&state, "pi-work").expect("save snapshot");
        let saved = state.db.get_app_snapshot("pi-work").unwrap().unwrap();
        let data: AppSnapshotData = serde_json::from_str(&saved.data).unwrap();
        assert_eq!(
            data.apps
                .get("pi")
                .and_then(|entry| entry.provider_id.clone()),
            Some("cc-switch-test".to_string()),
            "unreadable settings.json must fall back to the DB current provider"
        );
    }

    #[test]
    fn test_snapshot_data_serde_roundtrip() {
        let mut apps = BTreeMap::new();
        apps.insert("claude".to_string(), entry(Some("p1"), false));
        apps.insert("codex".to_string(), entry(Some("p2"), true));
        apps.insert("pi".to_string(), entry(Some("p3"), false));
        let data = AppSnapshotData {
            name: "work".to_string(),
            created_at: 1_730_000_000,
            apps,
        };
        let json = serde_json::to_string(&data).unwrap();
        // key 形式与规格书一致
        assert!(json.contains("\"claude\""));
        assert!(json.contains("\"provider_id\":\"p1\""));
        assert!(json.contains("\"takeover\":true"));
        let back: AppSnapshotData = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn test_snapshot_data_tolerates_missing_fields() {
        // 前向兼容：缺失的 app 槽位在应用时按 snapshot_entry_missing 跳过
        let back: AppSnapshotData = serde_json::from_str(
            r#"{"name":"x","created_at":1,"apps":{"claude":{"provider_id":"p1","takeover":true}}}"#,
        )
        .unwrap();
        assert_eq!(back.apps.get("claude"), Some(&entry(Some("p1"), true)));
        assert_eq!(back.apps.get("codex"), None);

        let empty: AppSnapshotData = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, AppSnapshotData::default());
    }

    #[test]
    fn test_plan_app_restore_switch_and_takeover() {
        // 快照要求接管关、当前开，且供应商不同：先关接管再切换
        let steps = plan_app_restore(Some("cur"), true, &entry(Some("target"), false));
        assert_eq!(
            steps,
            vec![ApplyStep::DisableTakeover, ApplyStep::SwitchProvider]
        );
    }

    #[test]
    fn test_plan_app_restore_enable_takeover_after_switch() {
        // 快照要求接管开、当前关：先切换再开启
        let steps = plan_app_restore(Some("cur"), false, &entry(Some("target"), true));
        assert_eq!(
            steps,
            vec![ApplyStep::SwitchProvider, ApplyStep::EnableTakeover]
        );
    }

    #[test]
    fn test_plan_app_restore_idempotent_and_missing_provider() {
        // 目标与当前完全一致：无需任何操作
        let steps = plan_app_restore(Some("same"), false, &entry(Some("same"), false));
        assert!(steps.is_empty());

        // 快照未捕获供应商（None）：不动供应商；接管状态仍按快照恢复
        let steps = plan_app_restore(Some("cur"), false, &entry(None, true));
        assert_eq!(steps, vec![ApplyStep::EnableTakeover]);

        // 快照接管关、当前也关：仅切换供应商
        let steps = plan_app_restore(Some("cur"), false, &entry(Some("target"), false));
        assert_eq!(steps, vec![ApplyStep::SwitchProvider]);
    }
}
