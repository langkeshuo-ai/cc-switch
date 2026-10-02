//! 跨模块共享的测试辅助设施。
//!
//! 目前只放 [`TestHomeGuard`]：把 `CC_SWITCH_TEST_HOME` 指向临时目录，隔离
//! `config::get_app_config_dir()`，让 `Database::init()` 之类的"读全局配置目录"
//! API 在测试里可用而不污染用户真实数据。
//!
//! 它原先内联在 `database::backup` 的 tests mod 里，只能被该模块使用；
//! `settings` 的测试同样需要隔离 DB，于是提到 crate 级共享而非复制第二份。

#![cfg(test)]

/// 把 `CC_SWITCH_TEST_HOME` 指向临时目录的守卫。
///
/// Drop 时恢复原环境变量。
pub struct TestHomeGuard {
    previous_test_home: Option<std::ffi::OsString>,
    temp_dir: tempfile::TempDir,
}

impl TestHomeGuard {
    /// 建立隔离的测试 HOME，并断言配置目录确实落在临时目录内。
    ///
    /// 断言是刻意的：`get_app_config_dir()` 在 Windows 上有 legacy-HOME 回退逻辑，
    /// 若隔离失效，测试会写到用户真实目录。宁可在这里 panic，也不要静默污染。
    pub fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("create isolated test home");
        let previous_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", temp_dir.path());
        // Prevent the Windows legacy-HOME fallback without mutating HOME:
        // an existing default DB keeps get_app_config_dir() anchored under
        // CC_SWITCH_TEST_HOME and makes import exercise its safety backup.
        let config_dir = temp_dir.path().join(".cc-switch");
        std::fs::create_dir_all(&config_dir).expect("create isolated config directory");
        std::fs::File::create(config_dir.join("cc-switch.db"))
            .expect("create isolated database sentinel");
        let guard = Self {
            previous_test_home,
            temp_dir,
        };
        let resolved = crate::config::get_app_config_dir();
        assert!(
            resolved.starts_with(guard.temp_dir.path()),
            "isolated test home resolved outside its temp directory: {}",
            resolved.display()
        );
        guard
    }

    /// 临时 HOME 根目录。
    pub fn path(&self) -> &std::path::Path {
        self.temp_dir.path()
    }
}

impl Drop for TestHomeGuard {
    fn drop(&mut self) {
        match self.previous_test_home.as_ref() {
            Some(previous) => std::env::set_var("CC_SWITCH_TEST_HOME", previous),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
    }
}
