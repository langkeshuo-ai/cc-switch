use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use cc_switch_lib::{update_settings, AppSettings, AppState, Database, MultiAppConfig};

/// 为测试设置隔离的 HOME 目录，避免污染真实用户数据。
pub fn ensure_test_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let base = std::env::temp_dir().join("cc-switch-test-home");
        if base.exists() {
            let _ = std::fs::remove_dir_all(&base);
        }
        std::fs::create_dir_all(&base).expect("create test home");
        // Windows 上 `dirs::home_dir()` 不受 HOME/USERPROFILE 影响（走 Known Folder API），
        // 用 CC_SWITCH_TEST_HOME 显式覆盖，以确保测试不会污染真实用户目录。
        std::env::set_var("CC_SWITCH_TEST_HOME", &base);
        std::env::set_var("HOME", &base);
        #[cfg(windows)]
        std::env::set_var("USERPROFILE", &base);
        // Claude Desktop 的配置目录在 Windows 上只读 LOCALAPPDATA（见 claude_desktop_config.rs
        // 的 windows_local_app_data_dir），既不认 CC_SWITCH_TEST_HOME 也不认 HOME。不覆盖它，
        // 涉及 Claude Desktop 供应商切换的测试会写进开发者真实的桌面版配置。
        #[cfg(windows)]
        std::env::set_var("LOCALAPPDATA", base.join("AppData").join("Local"));
        base
    })
    .as_path()
}

/// 清理测试目录中生成的配置文件与缓存。
pub fn reset_test_fs() {
    let home = ensure_test_home();
    for sub in [
        ".claude",
        ".codex",
        ".cc-switch",
        ".gemini",
        ".grok",
        ".config",
        ".openclaw",
        "profiles",
    ] {
        let path = home.join(sub);
        if path.exists() {
            if let Err(err) = std::fs::remove_dir_all(&path) {
                eprintln!("failed to clean {}: {}", path.display(), err);
            }
        }
    }
    let claude_json = home.join(".claude.json");
    if claude_json.exists() {
        let _ = std::fs::remove_file(&claude_json);
    }

    // 重置内存中的设置缓存，确保测试环境不受上一次调用影响
    let _ = update_settings(AppSettings::default());
}

#[allow(dead_code)]
pub fn enable_codex_official_auth_preservation() {
    update_settings(AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");
}

/// 取测试互斥锁，避免多测试并发写入相同的 HOME 目录。
///
/// 直接返回守卫，调用点不必再写 `.lock().expect(...)`。
///
/// 刻意吞掉 poison：`std::sync::Mutex` 在持锁用例 panic 后会把锁标记为中毒，
/// 于是**一个真实失败会放大成后续所有用例的 `PoisonError` 失败**（WSL2 Nightly
/// 上就出现过 1 个 `database is locked` + 1 个连坐的假失败）。测试锁只需要
/// 串行化，不需要失败传播。
pub fn test_mutex() -> MutexGuard<'static, ()> {
    static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 创建测试用的 AppState，包含一个空的数据库。
///
/// 使用内存库而非 `Database::init()` 的共享文件库：
/// 12 个集成测试二进制此前共享同一个 `~/.cc-switch/cc-switch.db`，
/// 进程内 `test_mutex` 只能在单个二进制内串行，跨进程/在锁语义较弱的
/// 文件系统（WSL2 9P）上仍会互相争锁，WSL2 Nightly 出现过 9 个用例
/// `database is locked` 失败。内存库天然按连接隔离，从根因消除争锁。
/// "数据写入 DB（而非 config.json）"的语义由各用例对 `state.db` 的
/// 查询断言覆盖；文件库 `Database::init()` 路径由 lib 层 backup/settings
/// 单测覆盖（见 `database/backup.rs`、`settings.rs`）。
#[allow(dead_code)]
pub fn create_test_state() -> Result<AppState, Box<dyn std::error::Error>> {
    let db = Arc::new(Database::memory()?);
    Ok(AppState::new(db))
}

/// 创建测试用的 AppState，并从 MultiAppConfig 迁移数据。
/// 与 [`create_test_state`] 同理使用内存库，避免跨测试争用文件锁。
#[allow(dead_code)]
pub fn create_test_state_with_config(
    config: &MultiAppConfig,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let db = Arc::new(Database::memory()?);
    db.migrate_from_json(config)?;
    Ok(AppState::new(db))
}
