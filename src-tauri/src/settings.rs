use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use crate::app_config::AppType;
use crate::error::AppError;
use crate::services::skill::{SkillStorageLocation, SyncMethod};

/// 自定义端点配置（历史兼容，实际存储在 provider.meta.custom_endpoints）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEndpoint {
    pub url: String,
    pub added_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<i64>,
}

fn default_true() -> bool {
    true
}

/// 主页面显示的应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisibleApps {
    #[serde(default = "default_true")]
    pub claude: bool,
    #[serde(default = "default_true")]
    pub codex: bool,
    #[serde(default = "default_true")]
    pub pi: bool,
}

impl Default for VisibleApps {
    fn default() -> Self {
        Self {
            claude: true,
            codex: true,
            pi: true,
        }
    }
}

impl VisibleApps {
    /// Check if the specified app is visible
    pub fn is_visible(&self, app: &AppType) -> bool {
        match app {
            AppType::Claude => self.claude,
            AppType::Codex => self.codex,
            AppType::Pi => self.pi,
        }
    }
}

/// Validate the opt-in end-to-end encryption settings for a sync transport.
///
/// Encryption is only meaningful with a password, so enabling it without one is
/// rejected at the settings boundary rather than failing later at upload time.
fn validate_encryption_settings(
    namespace: &str,
    enabled: bool,
    password: &str,
) -> Result<(), AppError> {
    if enabled && password.is_empty() {
        return Err(AppError::localized(
            "sync.encryption.password_required_setting",
            format!("已启用 {namespace} 端到端加密，但口令为空"),
            format!("{namespace} end-to-end encryption is enabled but the password is empty."),
        ));
    }
    Ok(())
}

/// WebDAV 同步状态（持久化同步进度信息）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebDavSyncStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_local_manifest_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_manifest_hash: Option<String>,
}

fn default_remote_root() -> String {
    "cc-switch-sync".to_string()
}
fn default_profile() -> String {
    "default".to_string()
}

/// WebDAV 同步设置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDavSyncSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_sync: bool,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_remote_root")]
    pub remote_root: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    /// 显式豁免：允许对非回环/内网的 HTTP 端点执行上传/下载（自建内网场景）。
    #[serde(default)]
    pub allow_plaintext_http: bool,
    /// 端到端加密开关（默认关闭，opt-in）。
    #[serde(default)]
    pub encryption_enabled: bool,
    /// 端到端加密口令。仅当 `encryption_enabled` 为真时使用；下发到前端时脱敏。
    #[serde(default)]
    pub encryption_password: String,
    #[serde(default)]
    pub status: WebDavSyncStatus,
}

impl Default for WebDavSyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_sync: false,
            base_url: String::new(),
            username: String::new(),
            password: String::new(),
            remote_root: default_remote_root(),
            profile: default_profile(),
            allow_plaintext_http: false,
            encryption_enabled: false,
            encryption_password: String::new(),
            status: WebDavSyncStatus::default(),
        }
    }
}

impl WebDavSyncSettings {
    pub fn validate(&self) -> Result<(), crate::error::AppError> {
        if self.base_url.trim().is_empty() {
            return Err(crate::error::AppError::localized(
                "webdav.base_url.required",
                "WebDAV 地址不能为空",
                "WebDAV URL is required.",
            ));
        }
        if self.username.trim().is_empty() {
            return Err(crate::error::AppError::localized(
                "webdav.username.required",
                "WebDAV 用户名不能为空",
                "WebDAV username is required.",
            ));
        }
        validate_encryption_settings("webdav", self.encryption_enabled, &self.encryption_password)?;
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.base_url = self.base_url.trim().to_string();
        self.username = self.username.trim().to_string();
        self.remote_root = self.remote_root.trim().to_string();
        self.profile = self.profile.trim().to_string();
        if self.remote_root.is_empty() {
            self.remote_root = default_remote_root();
        }
        if self.profile.is_empty() {
            self.profile = default_profile();
        }
    }

    /// Returns true if all credential fields are blank (no config to persist).
    fn is_empty(&self) -> bool {
        self.base_url.is_empty() && self.username.is_empty() && self.password.is_empty()
    }
}

/// S3 同步设置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S3SyncSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_sync: bool,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_remote_root")]
    pub remote_root: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    /// 显式豁免：允许对非回环/内网的 HTTP 端点执行上传/下载（自建内网场景）。
    #[serde(default)]
    pub allow_plaintext_http: bool,
    /// 端到端加密开关（默认关闭，opt-in）。
    #[serde(default)]
    pub encryption_enabled: bool,
    /// 端到端加密口令。仅当 `encryption_enabled` 为真时使用；下发到前端时脱敏。
    #[serde(default)]
    pub encryption_password: String,
    #[serde(default)]
    pub status: WebDavSyncStatus,
}

impl Default for S3SyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_sync: false,
            region: String::new(),
            bucket: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            endpoint: String::new(),
            remote_root: default_remote_root(),
            profile: default_profile(),
            allow_plaintext_http: false,
            encryption_enabled: false,
            encryption_password: String::new(),
            status: WebDavSyncStatus::default(),
        }
    }
}

impl S3SyncSettings {
    pub fn validate(&self) -> Result<(), crate::error::AppError> {
        if self.bucket.trim().is_empty() {
            return Err(crate::error::AppError::localized(
                "s3.bucket.required",
                "S3 存储桶不能为空",
                "S3 bucket is required.",
            ));
        }
        if self.region.trim().is_empty() {
            return Err(crate::error::AppError::localized(
                "s3.region.required",
                "S3 区域不能为空",
                "S3 region is required.",
            ));
        }
        if self.access_key_id.trim().is_empty() {
            return Err(crate::error::AppError::localized(
                "s3.access_key_id.required",
                "S3 Access Key ID 不能为空",
                "S3 Access Key ID is required.",
            ));
        }
        if self.secret_access_key.trim().is_empty() {
            return Err(crate::error::AppError::localized(
                "s3.secret_access_key.required",
                "S3 Secret Access Key 不能为空",
                "S3 Secret Access Key is required.",
            ));
        }
        validate_encryption_settings("s3", self.encryption_enabled, &self.encryption_password)?;
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.region = self.region.trim().to_string();
        self.bucket = self.bucket.trim().to_string();
        self.access_key_id = self.access_key_id.trim().to_string();
        self.endpoint = self.endpoint.trim().to_string();
        self.remote_root = self.remote_root.trim().to_string();
        self.profile = self.profile.trim().to_string();
        if self.remote_root.is_empty() {
            self.remote_root = default_remote_root();
        }
        if self.profile.is_empty() {
            self.profile = default_profile();
        }
    }

    /// Returns true if all credential fields are blank (no config to persist).
    fn is_empty(&self) -> bool {
        self.bucket.is_empty()
            && self.region.is_empty()
            && self.access_key_id.is_empty()
            && self.secret_access_key.is_empty()
    }
}

/// 本机自动迁移状态。
///
/// 这里记录的是本机启动时执行过的一次性迁移；标记不随数据库同步。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LocalMigrations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_third_party_history_provider_bucket_v1:
        Option<CodexThirdPartyHistoryProviderBucketMigration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_provider_template_v1: Option<CodexProviderTemplateMigration>,
    /// 统一会话开关的官方历史迁移标记。开关关闭时会被清除，
    /// 这样重新开启能把"关闭期间"落入 openai 桶的官方会话补迁进来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_official_history_unify_v1: Option<CodexOfficialHistoryUnifyMigration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexThirdPartyHistoryProviderBucketMigration {
    pub completed_at: String,
    pub target_provider_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_provider_ids: Vec<String>,
    #[serde(default)]
    pub migrated_jsonl_files: usize,
    #[serde(default)]
    pub migrated_state_rows: usize,
    #[serde(default)]
    pub scanned_history_files: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexProviderTemplateMigration {
    pub completed_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrated_provider_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexOfficialHistoryUnifyMigration {
    pub completed_at: String,
    pub target_provider_id: String,
    #[serde(default)]
    pub migrated_jsonl_files: usize,
    #[serde(default)]
    pub migrated_state_rows: usize,
    /// 迁移时的规范化 Codex 目录。标记只对同一目录生效：
    /// 切换 codex_config_dir 后旧标记不会挡住新目录的迁移。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_config_dir: Option<String>,
}

/// 应用设置结构
///
/// 存储设备级别设置，保存在本地 `~/.cc-switch/settings.json`，不随数据库同步。
/// 这确保了云同步场景下多设备可以独立运作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    // ===== 设备级 UI 设置 =====
    #[serde(default = "default_show_in_tray")]
    pub show_in_tray: bool,
    #[serde(default = "default_minimize_to_tray_on_close")]
    pub minimize_to_tray_on_close: bool,
    #[serde(default)]
    pub use_app_window_controls: bool,
    /// 是否启用 Claude 插件联动
    #[serde(default)]
    pub enable_claude_plugin_integration: bool,
    /// 是否跳过 Claude Code 初次安装确认
    #[serde(default)]
    pub skip_claude_onboarding: bool,
    /// 是否开机自启
    #[serde(default)]
    pub launch_on_startup: bool,
    /// 静默启动（程序启动时不显示主窗口，仅托盘运行）
    #[serde(default)]
    pub silent_startup: bool,
    /// 是否在主页面启用本地代理功能（默认关闭）
    #[serde(default)]
    pub enable_local_proxy: bool,
    /// User has confirmed the local proxy first-run notice
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_confirmed: Option<bool>,
    /// User has confirmed the usage query first-run notice
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_dashboard_refresh_interval_ms: Option<u32>,
    /// 会话用量自动扫描开关（默认开启=自动模式）。关闭后停止后台定时扫描
    /// 各客户端会话日志，仅在用户点击"立即同步"时手动扫描；只管扫描时机，
    /// 代理接管记账与启动费用回填（不读会话文件）不受此开关影响。
    #[serde(default = "default_session_auto_sync_enabled")]
    pub session_auto_sync_enabled: bool,
    /// Whether to show the failover toggle independently on the main page
    #[serde(default)]
    pub enable_failover_toggle: bool,
    /// Whether to show the project profile switcher on the main page header
    #[serde(default = "default_show_profile_switcher")]
    pub show_profile_switcher: bool,
    /// Keep Codex ChatGPT login material in auth.json when switching to third-party providers.
    /// Opt-in: defaults to false so third-party switches cleanly overwrite auth.json.
    #[serde(default)]
    pub preserve_codex_official_auth_on_switch: bool,
    /// Run official Codex providers under the shared "custom" model_provider id
    /// so official sessions share one resume-history bucket with third-party
    /// providers. Opt-in: defaults to false.
    #[serde(default)]
    pub unify_codex_session_history: bool,
    /// User opted in (via the enable dialog checkbox) to migrate existing
    /// official sessions ("openai" bucket) into the shared bucket. Persisted so
    /// a failed migration retries at startup; cleared when the toggle turns off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unify_codex_migrate_existing: Option<bool>,
    /// User has confirmed the failover toggle first-run notice
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover_confirmed: Option<bool>,
    /// User has confirmed the first-run welcome notice
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_notice_confirmed: Option<bool>,
    /// User has confirmed the common config first-run notice
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_config_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    // ===== 主页面显示的应用 =====
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_apps: Option<VisibleApps>,

    // ===== 设备级目录覆盖 =====
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_config_dir: Option<String>,

    // ===== 当前供应商 ID（设备级）=====
    /// 当前 Claude 供应商 ID（本地存储，优先于数据库 is_current）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_claude: Option<String>,
    /// 当前 Codex 供应商 ID（本地存储，优先于数据库 is_current）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_codex: Option<String>,

    // ===== Skill 同步设置 =====
    /// Skill 同步方式：auto（默认，优先 symlink）、symlink、copy
    #[serde(default)]
    pub skill_sync_method: SyncMethod,
    /// Skill 存储位置：cc_switch（默认）或 unified（~/.agents/skills/）
    #[serde(default)]
    pub skill_storage_location: SkillStorageLocation,

    // ===== WebDAV 同步设置 =====
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webdav_sync: Option<WebDavSyncSettings>,

    // ===== S3 同步设置 =====
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_sync: Option<S3SyncSettings>,

    // ===== WebDAV 备份设置（旧版，保留向后兼容）=====
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webdav_backup: Option<serde_json::Value>,

    // ===== 备份策略设置 =====
    /// Auto-backup interval in hours (default 24, 0 = disabled)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_interval_hours: Option<u32>,
    /// Maximum number of backup files to retain (default 10)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_retain_count: Option<u32>,

    // ===== 终端设置 =====
    /// 首选终端应用（可选，默认使用系统默认终端）
    /// - macOS: "terminal" | "iterm2" | "warp" | "alacritty" | "kitty" | "ghostty" | "otty" | "wezterm" | "kaku"
    /// - Windows: "cmd" | "powershell" | "wt" (Windows Terminal)
    /// - Linux: "gnome-terminal" | "konsole" | "xfce4-terminal" | "alacritty" | "kitty" | "ghostty"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_terminal: Option<String>,

    // ===== 本机自动迁移状态 =====
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_migrations: Option<LocalMigrations>,
}

fn default_show_in_tray() -> bool {
    true
}

fn default_minimize_to_tray_on_close() -> bool {
    true
}

fn default_show_profile_switcher() -> bool {
    true
}

fn default_session_auto_sync_enabled() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            show_in_tray: true,
            minimize_to_tray_on_close: true,
            use_app_window_controls: false,
            enable_claude_plugin_integration: false,
            skip_claude_onboarding: false,
            launch_on_startup: false,
            silent_startup: false,
            enable_local_proxy: false,
            proxy_confirmed: None,
            usage_confirmed: None,
            usage_dashboard_refresh_interval_ms: None,
            session_auto_sync_enabled: true,
            enable_failover_toggle: false,
            show_profile_switcher: true,
            preserve_codex_official_auth_on_switch: false,
            unify_codex_session_history: false,
            unify_codex_migrate_existing: None,
            failover_confirmed: None,
            first_run_notice_confirmed: None,
            common_config_confirmed: None,
            language: None,
            visible_apps: None,
            claude_config_dir: None,
            codex_config_dir: None,
            pi_config_dir: None,
            current_provider_claude: None,
            current_provider_codex: None,
            skill_sync_method: SyncMethod::default(),
            skill_storage_location: SkillStorageLocation::default(),
            webdav_sync: None,
            s3_sync: None,
            webdav_backup: None,
            backup_interval_hours: None,
            backup_retain_count: None,
            preferred_terminal: None,
            local_migrations: None,
        }
    }
}

impl AppSettings {
    fn settings_path() -> Option<PathBuf> {
        // settings.json 保留用于旧版本迁移和无数据库场景
        Some(
            crate::config::get_home_dir()
                .join(".cc-switch")
                .join("settings.json"),
        )
    }

    fn normalize_paths(&mut self) {
        self.claude_config_dir = self
            .claude_config_dir
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        self.codex_config_dir = self
            .codex_config_dir
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        self.pi_config_dir = self
            .pi_config_dir
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        self.language = self
            .language
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| matches!(*s, "en" | "zh" | "zh-TW" | "ja"))
            .map(|s| s.to_string());

        if let Some(sync) = &mut self.webdav_sync {
            sync.normalize();
            if sync.is_empty() {
                self.webdav_sync = None;
            }
        }

        if let Some(s3) = &mut self.s3_sync {
            s3.normalize();
            if s3.is_empty() {
                self.s3_sync = None;
            }
        }
    }

    fn load_from_file() -> Self {
        let Some(path) = Self::settings_path() else {
            return Self::default();
        };
        if let Ok(content) = fs::read_to_string(&path) {
            match serde_json::from_str::<AppSettings>(&content) {
                Ok(mut settings) => {
                    settings.normalize_paths();
                    // H2: 落盘凭据在 Windows 上是 DPAPI 加密的，读入内存前解密。
                    // 解密失败（用户配置迁移/主密钥变更）时清空对应字段，
                    // 让用户重新输入，而不是把密文当明文用或 panic。
                    decrypt_credentials_in_place(&mut settings);
                    settings
                }
                Err(err) => {
                    log::warn!(
                        "解析设置文件失败，将使用默认设置。路径: {}, 错误: {}",
                        path.display(),
                        err
                    );
                    Self::default()
                }
            }
        } else {
            Self::default()
        }
    }
}

fn save_settings_file(settings: &AppSettings) -> Result<(), AppError> {
    let mut normalized = settings.clone();
    normalized.normalize_paths();
    let Some(path) = AppSettings::settings_path() else {
        return Err(AppError::Config("无法获取用户主目录".to_string()));
    };

    // H2: 序列化到磁盘前加密凭据字段（仅 Windows 生效；Unix 依赖 0o600）。
    // 加密的是 normalized 副本，不影响调用方持有的内存态（始终明文）。
    encrypt_credentials_in_place(&mut normalized);

    let json = serde_json::to_string_pretty(&normalized)
        .map_err(|e| AppError::JsonSerialize { source: e })?;

    // 统一走 atomic_write_private：
    // - 原子写（temp + rename），避免崩溃时半写损坏 settings.json；
    // - Unix 0o600；Windows 收紧 DACL 到当前用户（见 config.rs M8）；
    // - 内部已 create_dir_all 父目录，无需此处重复。
    //
    // 早期实现用 #[cfg(unix)] / #[cfg(not(unix))] 两份手写写入逻辑，Unix 侧
    // 非原子（OpenOptions.truncate）、Windows 侧 fs::write 无权限收紧，且
    // 两处逻辑漂移。合并后单一路径覆盖两个平台。
    crate::config::atomic_write_private(&path, json.as_bytes())
}

// =============================================================================
// H2: 凭据静态加密（Windows DPAPI）
// =============================================================================
//
// 根因：WebDAV password 与 S3 secret_access_key 早期以明文 JSON 落盘。M8 已
// 通过 DACL/0o600 限制"同机其他用户"读取，但对"文件被拷到另一台机器 / 离线
// 磁盘窃取"无防护。DPAPI（CryptProtectData）把数据加密到当前 Windows 用户的
// 主密钥，文件离开本机即无法解密——这是 Chrome/Edge 等存储凭据的标准做法。
//
// 设计取舍：
// - 只加密两个真正敏感的凭据字段，不加密整个文件（避免破坏可读的诊断信息，
//   也避免每次读取都要解密大块数据）。
// - 用 `ccswitch-dpapi-v1:` 前缀标记密文，与历史明文共存：旧明文照常读取，
//   下次保存时自动升级为密文（平滑迁移，无需一次性 migration）。
// - 非 Windows 平台为 no-op：Unix 0o600 已是该威胁模型下的标准答案；
//   macOS Keychain / Linux libsecret 需引入 `keyring` 依赖，属独立增强，
//   不在本轮"做减法"范围内。

/// DPAPI 密文标记前缀。足够独特以避免与用户真实凭据碰撞。
/// `pub(crate)`：OAuth 令牌存储（`services::secure_store`）复用同一标记。
#[cfg(windows)]
pub(crate) const DPAPI_MARKER: &str = "ccswitch-dpapi-v1:";

/// 加密 settings 中的凭据字段（原地）。仅 Windows 生效。
///
/// 注意：DPAPI 只在 Windows 生效。非 Windows 平台（见函数末尾 no-op 分支）
/// 没有 DPAPI，凭据与端到端加密口令以**明文**落盘，仅靠 0o600 文件权限保护；
/// Windows 上若 DPAPI 调用失败，也会回退明文并 warn。E2E 口令是保护远端快照
/// 的密钥（丢失即无法解密，泄露即失去加密保护），其"本地明文存储"这一事实
/// 应被明确知晓，而非被"已加密"的假象掩盖。
fn encrypt_credentials_in_place(settings: &mut AppSettings) {
    #[cfg(windows)]
    {
        if let Some(sync) = settings.webdav_sync.as_mut() {
            if !sync.password.is_empty() && !sync.password.starts_with(DPAPI_MARKER) {
                match dpapi_protect(&sync.password) {
                    Some(encrypted) => sync.password = encrypted,
                    None => log::warn!("WebDAV 密码 DPAPI 加密失败，将以明文保存（DACL 仍受限）"),
                }
            }
            if !sync.encryption_password.is_empty()
                && !sync.encryption_password.starts_with(DPAPI_MARKER)
            {
                // 失败即回退明文：E2E 口令因此可能以明文留在 settings.json，
                // 仅受 DACL 保护。这在丢弃/拷贝磁盘场景下等于泄露远端数据密钥。
                match dpapi_protect(&sync.encryption_password) {
                    Some(encrypted) => sync.encryption_password = encrypted,
                    None => {
                        log::warn!("WebDAV 加密口令 DPAPI 加密失败，将以明文保存（DACL 仍受限）")
                    }
                }
            }
        }
        if let Some(s3) = settings.s3_sync.as_mut() {
            if !s3.secret_access_key.is_empty() && !s3.secret_access_key.starts_with(DPAPI_MARKER) {
                match dpapi_protect(&s3.secret_access_key) {
                    Some(encrypted) => s3.secret_access_key = encrypted,
                    None => log::warn!("S3 secret DPAPI 加密失败，将以明文保存（DACL 仍受限）"),
                }
            }
            if !s3.encryption_password.is_empty()
                && !s3.encryption_password.starts_with(DPAPI_MARKER)
            {
                // 同上：DPAPI 失败时 E2E 口令以明文存储，仅受 DACL 保护。
                match dpapi_protect(&s3.encryption_password) {
                    Some(encrypted) => s3.encryption_password = encrypted,
                    None => {
                        log::warn!("S3 加密口令 DPAPI 加密失败，将以明文保存（DACL 仍受限）")
                    }
                }
            }
        }
    }
    // 非 Windows 无 DPAPI：凭据与 E2E 口令全部以明文写入 settings.json，仅靠
    // 0o600 文件权限（atomic_write_private）限制"同机其他用户"读取。这是该平台
    // 威胁模型下的既定取舍，但意味着本地明文口令是已知暴露面。
    #[cfg(not(windows))]
    let _ = settings;
}

/// 解密 settings 中的凭据字段（原地）。仅 Windows 生效。
///
/// 解密失败时清空字段（而非保留密文或 panic），让用户重新输入。
fn decrypt_credentials_in_place(settings: &mut AppSettings) {
    #[cfg(windows)]
    {
        if let Some(sync) = settings.webdav_sync.as_mut() {
            if sync.password.starts_with(DPAPI_MARKER) {
                match dpapi_unprotect(&sync.password) {
                    Some(plain) => sync.password = plain,
                    None => {
                        log::warn!("WebDAV 密码 DPAPI 解密失败，已清空，需重新输入");
                        sync.password.clear();
                    }
                }
            }
            if sync.encryption_password.starts_with(DPAPI_MARKER) {
                match dpapi_unprotect(&sync.encryption_password) {
                    Some(plain) => sync.encryption_password = plain,
                    None => {
                        log::warn!("WebDAV 加密口令 DPAPI 解密失败，已清空，需重新输入");
                        sync.encryption_password.clear();
                    }
                }
            }
        }
        if let Some(s3) = settings.s3_sync.as_mut() {
            if s3.secret_access_key.starts_with(DPAPI_MARKER) {
                match dpapi_unprotect(&s3.secret_access_key) {
                    Some(plain) => s3.secret_access_key = plain,
                    None => {
                        log::warn!("S3 secret DPAPI 解密失败，已清空，需重新输入");
                        s3.secret_access_key.clear();
                    }
                }
            }
            if s3.encryption_password.starts_with(DPAPI_MARKER) {
                match dpapi_unprotect(&s3.encryption_password) {
                    Some(plain) => s3.encryption_password = plain,
                    None => {
                        log::warn!("S3 加密口令 DPAPI 解密失败，已清空，需重新输入");
                        s3.encryption_password.clear();
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = settings;
}

/// DPAPI 加密：明文字符串 → `ccswitch-dpapi-v1:<base64>`。失败返回 None。
#[cfg(windows)]
pub(crate) fn dpapi_protect(plaintext: &str) -> Option<String> {
    use base64::prelude::*;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };

    let mut plain_bytes = plaintext.as_bytes().to_vec();
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain_bytes.len() as u32,
        pbData: plain_bytes.as_mut_ptr(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY: in_blob 指向本栈上有效的明文字节（plain_bytes 存活至调用结束）；
    // out_blob 是出参，CryptProtectData 成功时会通过 LocalAlloc 填充其 pbData，
    // 由下方 LocalFree 释放。CRYPTPROTECT_UI_FORBIDDEN 禁止弹 UI（后台调用）。
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 || out_blob.pbData.is_null() {
        return None;
    }

    // SAFETY: CryptProtectData 成功后 out_blob.pbData 指向 cbData 字节的有效缓冲。
    let encrypted =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    // SAFETY: out_blob.pbData 由 CryptProtectData 经 LocalAlloc 分配，LocalFree
    // 是文档规定的配对释放；此处非空且本函数拥有唯一所有权。
    unsafe {
        LocalFree(out_blob.pbData as _);
    }

    Some(format!(
        "{DPAPI_MARKER}{}",
        BASE64_STANDARD.encode(&encrypted)
    ))
}

/// DPAPI 解密：`ccswitch-dpapi-v1:<base64>` → 明文字符串。失败返回 None。
#[cfg(windows)]
pub(crate) fn dpapi_unprotect(marked: &str) -> Option<String> {
    use base64::prelude::*;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };

    let b64 = marked.strip_prefix(DPAPI_MARKER)?;
    let mut cipher_bytes = BASE64_STANDARD.decode(b64).ok()?;
    if cipher_bytes.is_empty() {
        return None;
    }

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: cipher_bytes.len() as u32,
        pbData: cipher_bytes.as_mut_ptr(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY: in_blob 指向本栈上有效的密文字节；out_blob 是出参，成功时由
    // CryptUnprotectData 经 LocalAlloc 填充，下方 LocalFree 释放。
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 || out_blob.pbData.is_null() {
        return None;
    }

    // SAFETY: 成功后 out_blob.pbData 指向 cbData 字节的有效缓冲。
    let plain =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    // SAFETY: out_blob.pbData 由 CryptUnprotectData 经 LocalAlloc 分配，LocalFree 配对释放。
    unsafe {
        LocalFree(out_blob.pbData as _);
    }

    String::from_utf8(plain).ok()
}

static SETTINGS_STORE: OnceLock<RwLock<AppSettings>> = OnceLock::new();

fn settings_store() -> &'static RwLock<AppSettings> {
    SETTINGS_STORE.get_or_init(|| RwLock::new(AppSettings::load_from_file()))
}

pub(crate) fn resolve_override_path(raw: &str) -> PathBuf {
    let join_home = |home: PathBuf, suffix: &str| {
        suffix
            .split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .fold(home, |path, component| path.join(component))
    };

    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return join_home(home, stripped);
        }
    } else if let Some(stripped) = raw.strip_prefix("~\\") {
        if let Some(home) = dirs::home_dir() {
            return join_home(home, stripped);
        }
    }

    PathBuf::from(raw)
}

pub fn get_settings() -> AppSettings {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .clone()
}

pub fn get_settings_for_frontend() -> AppSettings {
    let mut settings = get_settings();
    if let Some(sync) = &mut settings.webdav_sync {
        sync.password.clear();
        // The E2E passphrase must never leave the backend; the UI shows a
        // placeholder and preserves the stored value on save when left blank.
        sync.encryption_password.clear();
    }
    if let Some(s3) = &mut settings.s3_sync {
        s3.secret_access_key.clear();
        s3.encryption_password.clear();
    }
    settings.webdav_backup = None;
    settings
}

pub fn update_settings(mut new_settings: AppSettings) -> Result<(), AppError> {
    new_settings.normalize_paths();
    save_settings_file(&new_settings)?;

    let mut guard = settings_store().write().unwrap_or_else(|e| {
        log::warn!("设置锁已毒化，使用恢复值: {e}");
        e.into_inner()
    });
    *guard = new_settings;
    Ok(())
}

fn mutate_settings<F>(mutator: F) -> Result<(), AppError>
where
    F: FnOnce(&mut AppSettings),
{
    let mut guard = settings_store().write().unwrap_or_else(|e| {
        log::warn!("设置锁已毒化，使用恢复值: {e}");
        e.into_inner()
    });
    let mut next = guard.clone();
    mutator(&mut next);
    next.normalize_paths();
    save_settings_file(&next)?;
    *guard = next;
    Ok(())
}

pub fn is_codex_third_party_history_provider_bucket_migrated() -> bool {
    get_settings()
        .local_migrations
        .as_ref()
        .and_then(|migrations| {
            migrations
                .codex_third_party_history_provider_bucket_v1
                .as_ref()
        })
        .is_some_and(|m| m.scanned_history_files)
}

pub fn mark_codex_third_party_history_provider_bucket_migrated(
    migration: CodexThirdPartyHistoryProviderBucketMigration,
) -> Result<(), AppError> {
    mutate_settings(|settings| {
        let migrations = settings
            .local_migrations
            .get_or_insert_with(Default::default);
        migrations.codex_third_party_history_provider_bucket_v1 = Some(migration);
    })
}

pub fn is_codex_provider_template_migrated() -> bool {
    get_settings()
        .local_migrations
        .as_ref()
        .and_then(|migrations| migrations.codex_provider_template_v1.as_ref())
        .is_some()
}

pub fn mark_codex_provider_template_migrated(
    migration: CodexProviderTemplateMigration,
) -> Result<(), AppError> {
    mutate_settings(|settings| {
        let migrations = settings
            .local_migrations
            .get_or_insert_with(Default::default);
        migrations.codex_provider_template_v1 = Some(migration);
    })
}

/// 统一会话迁移标记是否覆盖指定目录。标记里没记目录（不应出现的旧格式）
/// 视为不匹配——重跑迁移是幂等的，宁可重迁也不漏迁。
pub fn is_codex_official_history_unify_migrated_for_dir(codex_dir: &str) -> bool {
    get_settings()
        .local_migrations
        .as_ref()
        .and_then(|migrations| migrations.codex_official_history_unify_v1.as_ref())
        .is_some_and(|migration| migration.codex_config_dir.as_deref() == Some(codex_dir))
}

/// 条件写入迁移完成标记：仅当此刻开关仍开启且迁移意愿仍在时才写。
/// 检查与写入在 settings 写锁内原子完成，与关闭开关路径
/// （`update_settings` / 清标记）串行，消除"迁移线程复查开关后、写标记前
/// 用户恰好关闭开关"的竞态窗口。返回是否实际写入。
pub fn mark_codex_official_history_unify_migrated_if_enabled(
    migration: CodexOfficialHistoryUnifyMigration,
) -> Result<bool, AppError> {
    let mut written = false;
    mutate_settings(|settings| {
        if settings.unify_codex_session_history
            && settings.unify_codex_migrate_existing.unwrap_or(false)
        {
            settings
                .local_migrations
                .get_or_insert_with(Default::default)
                .codex_official_history_unify_v1 = Some(migration);
            written = true;
        }
    })?;
    Ok(written)
}

pub fn clear_codex_official_history_unify_migration() -> Result<(), AppError> {
    mutate_settings(|settings| {
        if let Some(migrations) = settings.local_migrations.as_mut() {
            migrations.codex_official_history_unify_v1 = None;
        }
    })
}

pub fn unify_codex_migrate_existing_requested() -> bool {
    get_settings().unify_codex_migrate_existing.unwrap_or(false)
}

pub fn clear_codex_unify_migrate_existing() -> Result<(), AppError> {
    mutate_settings(|settings| {
        settings.unify_codex_migrate_existing = None;
    })
}

/// 从文件重新加载设置到内存缓存
/// 用于导入配置等场景，确保内存缓存与文件同步
pub fn reload_settings() -> Result<(), AppError> {
    let fresh_settings = AppSettings::load_from_file();
    let mut guard = settings_store().write().unwrap_or_else(|e| {
        log::warn!("设置锁已毒化，使用恢复值: {e}");
        e.into_inner()
    });
    *guard = fresh_settings;
    Ok(())
}

pub fn get_claude_override_dir() -> Option<PathBuf> {
    let settings = settings_store().read().ok()?;
    settings
        .claude_config_dir
        .as_ref()
        .map(|p| resolve_override_path(p))
}

pub fn get_codex_override_dir() -> Option<PathBuf> {
    let settings = settings_store().read().ok()?;
    settings
        .codex_config_dir
        .as_ref()
        .map(|p| resolve_override_path(p))
}

pub fn get_pi_override_dir() -> Option<PathBuf> {
    let settings = settings_store().read().ok()?;
    settings
        .pi_config_dir
        .as_ref()
        .map(|path| resolve_override_path(path))
}

pub fn preserve_codex_official_auth_on_switch() -> bool {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .preserve_codex_official_auth_on_switch
}

pub fn unify_codex_session_history() -> bool {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .unify_codex_session_history
}

// ===== 当前供应商管理函数 =====

/// 获取指定应用类型的当前供应商 ID（从本地 settings 读取）
///
/// 这是设备级别的设置，不随数据库同步。
/// 如果本地没有设置，调用者应该 fallback 到数据库的 `is_current` 字段。
pub fn get_current_provider(app_type: &AppType) -> Option<String> {
    let settings = settings_store().read().ok()?;
    match app_type {
        AppType::Claude => settings.current_provider_claude.clone(),
        AppType::Codex => settings.current_provider_codex.clone(),
        AppType::Pi => None,
    }
}

/// 设置指定应用类型的当前供应商 ID（保存到本地 settings）
///
/// 这是设备级别的设置，不随数据库同步。
/// 传入 `None` 会清除当前供应商设置。
pub fn set_current_provider(app_type: &AppType, id: Option<&str>) -> Result<(), AppError> {
    let id_owned = id.map(|s| s.to_string());
    mutate_settings(|settings| match app_type {
        AppType::Claude => settings.current_provider_claude = id_owned.clone(),
        AppType::Codex => settings.current_provider_codex = id_owned.clone(),
        AppType::Pi => {}
    })
}

/// 获取有效的当前供应商 ID（验证存在性）
///
/// 逻辑：
/// 1. 从本地 settings 读取当前供应商 ID
/// 2. 验证该 ID 在数据库中存在
/// 3. 如果不存在则清理本地 settings，fallback 到数据库的 is_current
///
/// 这确保了返回的 ID 一定是有效的（在数据库中存在）。
/// 多设备云同步场景下，配置导入后本地 ID 可能失效，此函数会自动修复。
pub fn get_effective_current_provider(
    db: &crate::database::Database,
    app_type: &AppType,
) -> Result<Option<String>, AppError> {
    // 1. 从本地 settings 读取
    if let Some(local_id) = get_current_provider(app_type) {
        // 2. 验证该 ID 在数据库中存在
        let providers = db.get_all_providers(app_type.as_str())?;
        if providers.contains_key(&local_id) {
            // 存在，直接返回
            return Ok(Some(local_id));
        }

        // 3. 不存在，清理本地 settings
        log::warn!(
            "本地 settings 中的供应商 {} ({}) 在数据库中不存在，将清理并 fallback 到数据库",
            local_id,
            app_type.as_str()
        );
        let _ = set_current_provider(app_type, None);
    }

    // Fallback 到数据库的 is_current
    db.get_current_provider(app_type.as_str())
}

// ===== Skill 同步方式管理函数 =====

/// 获取 Skill 同步方式配置
pub fn get_skill_sync_method() -> SyncMethod {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .skill_sync_method
}

// ===== Skill 存储位置管理函数 =====

/// 获取 Skill 存储位置配置
pub fn get_skill_storage_location() -> SkillStorageLocation {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .skill_storage_location
}

/// 设置 Skill 存储位置
pub fn set_skill_storage_location(location: SkillStorageLocation) -> Result<(), AppError> {
    mutate_settings(|s| {
        s.skill_storage_location = location;
    })
}

// ===== 备份策略管理函数 =====

/// Get the effective auto-backup interval in hours (default 24)
pub fn effective_backup_interval_hours() -> u32 {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .backup_interval_hours
        .unwrap_or(24)
}

/// Get the effective backup retain count (default 10, minimum 1)
pub fn effective_backup_retain_count() -> usize {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .backup_retain_count
        .map(|n| (n as usize).max(1))
        .unwrap_or(10)
}

// ===== 终端设置管理函数 =====

/// 获取首选终端应用
pub fn get_preferred_terminal() -> Option<String> {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .preferred_terminal
        .clone()
}

// ===== WebDAV 同步设置管理函数 =====

/// 获取 WebDAV 同步设置
pub fn get_webdav_sync_settings() -> Option<WebDavSyncSettings> {
    settings_store().read().ok()?.webdav_sync.clone()
}

/// 保存 WebDAV 同步设置
pub fn set_webdav_sync_settings(settings: Option<WebDavSyncSettings>) -> Result<(), AppError> {
    mutate_settings(|current| {
        current.webdav_sync = settings;
    })
}

/// 仅更新 WebDAV 同步状态，避免覆写 credentials/root/profile 等字段
pub fn update_webdav_sync_status(status: WebDavSyncStatus) -> Result<(), AppError> {
    mutate_settings(|current| {
        if let Some(sync) = current.webdav_sync.as_mut() {
            sync.status = status;
        }
    })
}

// ===== S3 同步设置管理函数 =====

pub fn get_s3_sync_settings() -> Option<S3SyncSettings> {
    settings_store().read().ok()?.s3_sync.clone()
}

pub fn set_s3_sync_settings(settings: Option<S3SyncSettings>) -> Result<(), AppError> {
    mutate_settings(|current| {
        current.s3_sync = settings;
    })
}

pub fn update_s3_sync_status(status: WebDavSyncStatus) -> Result<(), AppError> {
    mutate_settings(|current| {
        if let Some(s3) = current.s3_sync.as_mut() {
            s3.status = status;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_paths_expand_windows_style_tilde_separators() {
        let home = dirs::home_dir().expect("home directory");
        assert_eq!(
            resolve_override_path(r"~\pi\agent"),
            home.join("pi").join("agent")
        );
    }

    // =====================================================================
    // H2 验证：DPAPI 存→读往返
    // =====================================================================

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_returns_original_plaintext() {
        // 原语级往返：protect 产出带标记密文，unprotect 还原出逐字节相同的明文。
        let plain = "w3bdav-pa55w0rd!with;metas";
        let marked = dpapi_protect(plain).expect("DPAPI protect should succeed in a user session");
        assert!(
            marked.starts_with(DPAPI_MARKER),
            "ciphertext must carry the migration marker"
        );
        assert_ne!(marked, plain, "stored form must not be plaintext");
        assert!(
            !marked[DPAPI_MARKER.len()..].contains(plain),
            "no plaintext leak in blob"
        );
        assert_eq!(
            dpapi_unprotect(&marked).as_deref(),
            Some(plain),
            "round trip must be lossless"
        );
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_unprotect_rejects_unmarked_and_corrupt_input() {
        // 无标记 → 视为历史明文，不解密（返回 None 由调用方按"非密文"处理路径兜底）。
        assert_eq!(dpapi_unprotect("plain-password"), None);
        // 有标记但 base64 损坏 → None（调用方清空字段让用户重输，而非 panic）。
        assert_eq!(
            dpapi_unprotect(&format!("{DPAPI_MARKER}!!!not-base64!!!")),
            None
        );
        // 有标记、base64 合法但不是 DPAPI blob → None。
        assert_eq!(
            dpapi_unprotect(&format!("{DPAPI_MARKER}aGVsbG8gd29ybGQ=")),
            None
        );
    }

    #[test]
    fn credential_fields_round_trip_through_save_load_transforms() {
        // 字段级往返：encrypt → decrypt 必须还原原值（Windows 走 DPAPI；
        // 其他平台为 no-op，字段原样保留）。不触碰磁盘，纯内存验证。
        let mut settings = AppSettings {
            webdav_sync: Some(WebDavSyncSettings {
                password: "dav-secret".to_string(),
                ..Default::default()
            }),
            s3_sync: Some(S3SyncSettings {
                secret_access_key: "s3-secret".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let original_dav = settings.webdav_sync.as_ref().unwrap().password.clone();
        let original_s3 = settings.s3_sync.as_ref().unwrap().secret_access_key.clone();

        // 模拟落盘前的加密变换
        encrypt_credentials_in_place(&mut settings);

        #[cfg(windows)]
        {
            // Windows：落盘形态必须是带标记的密文，而非明文。
            assert_ne!(
                settings.webdav_sync.as_ref().unwrap().password,
                original_dav
            );
            assert!(settings
                .webdav_sync
                .as_ref()
                .unwrap()
                .password
                .starts_with(DPAPI_MARKER));
            assert!(settings
                .s3_sync
                .as_ref()
                .unwrap()
                .secret_access_key
                .starts_with(DPAPI_MARKER));
        }
        #[cfg(not(windows))]
        {
            // 非 Windows：no-op，字段保持明文（0o600 是该平台的标准防线）。
            assert_eq!(
                settings.webdav_sync.as_ref().unwrap().password,
                original_dav
            );
            assert_eq!(
                settings.s3_sync.as_ref().unwrap().secret_access_key,
                original_s3
            );
        }

        // 模拟读入后的解密变换：必须无损还原
        decrypt_credentials_in_place(&mut settings);
        assert_eq!(
            settings.webdav_sync.as_ref().unwrap().password,
            original_dav
        );
        assert_eq!(
            settings.s3_sync.as_ref().unwrap().secret_access_key,
            original_s3
        );
    }

    #[test]
    fn encrypt_skips_empty_credentials() {
        // 空凭据不应产生"ccswitch-dpapi-v1:"空密文，保持空串以便 is_empty 判定。
        let mut settings = AppSettings {
            webdav_sync: Some(WebDavSyncSettings::default()),
            ..Default::default()
        };
        encrypt_credentials_in_place(&mut settings);
        assert_eq!(settings.webdav_sync.as_ref().unwrap().password, "");
    }
}
