//! Tauri 应用 setup 阶段（自 lib.rs 拆出，M2）。
//!
//! 按序完成：rustls crypto provider → 日志系统 → 数据库初始化（含 JSON 迁移、
//! 版本预检、恢复模式）→ AppState/manage → 托盘 → 各后台任务 → 代理状态恢复 → 窗口显示策略。
//! 返回 `Box<dyn Error>` 以保持与 Tauri setup 闭包相同的错误语义（`?` 自动转换）。

use std::sync::Arc;

use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri_plugin_deep_link::DeepLinkExt;

use crate::app_lifecycle::exit_with_cleanup;
#[cfg(target_os = "windows")]
use crate::app_lifecycle::set_windows_app_user_model_id;
use crate::deeplink::handle_deeplink_url;
#[cfg(target_os = "linux")]
use crate::linux_fix;
use crate::log_redact::{runtime_log_level_allows, url_for_log};
use crate::services::SkillService;
use crate::startup_restore::{initialize_common_config_snippets, restore_proxy_state_on_startup};
use crate::store::AppState;
#[cfg(target_os = "macos")]
use crate::tray::macos_tray_icon;
use crate::{app_store, commands, panic_hook, startup_dialogs, tray, usage_events};

pub(crate) fn setup_app(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 预先刷新 Store 覆盖配置，确保后续路径读取正确（日志/数据库等）
    app_store::refresh_app_config_dir_override(app.handle());
    panic_hook::init_app_config_dir(crate::config::get_app_config_dir());

    // 初始化日志（输出到 <app_config_dir>/logs/cc-switch.log）
    {
        use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};

        let log_dir = panic_hook::get_log_dir();

        // 确保日志目录存在
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            eprintln!("创建日志目录失败: {e}");
        }

        app.handle().plugin(
            tauri_plugin_log::Builder::default()
                // 底层保留 Trace 能力，便于加载用户配置后动态调高级别。
                // 插件注册后会立即把全局级别收紧到 Info，避免启动阶段全量 Trace。
                .level(log::LevelFilter::Trace)
                // plugin-log 的前端 command 会直达 logger，绕过 log 宏的全局
                // max_level；在分发层补一次过滤，确保动态总开关同样约束前端日志。
                .filter(|metadata| runtime_log_level_allows(metadata.level(), log::max_level()))
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::Folder {
                        path: log_dir,
                        file_name: Some("cc-switch".into()),
                    }),
                ])
                // KeepSome(4) 保留 4 个轮转归档，加上当前文件最多约 100 MiB。
                // 轮转仅按大小触发；跨重启继续追加，不再丢失上一次运行的日志。
                .rotation_strategy(RotationStrategy::KeepSome(4))
                .max_file_size(20 * 1024 * 1024)
                .timezone_strategy(TimezoneStrategy::UseLocal)
                .build(),
        )?;

        // 用户配置存在数据库中，数据库尚未打开时使用保守的 Info 级别。
        log::set_max_level(log::LevelFilter::Info);
        log::info!("=== CC Switch v{} started ===", env!("CARGO_PKG_VERSION"));
    }

    // 首次读取覆盖路径时 logger 尚未可用；此处重放一次，
    // 让 Store 损坏或路径无效等启动警告能够真正落盘。
    let _ = app_store::refresh_app_config_dir_override(app.handle());

    #[cfg(target_os = "windows")]
    set_windows_app_user_model_id(app.handle());

    // 也能向前端推送 `usage-log-recorded`。
    // 放在日志系统初始化之后，确保 init 的日志能正常输出。
    usage_events::init(app.handle().clone());

    // 初始化数据库
    let app_config_dir = crate::config::get_app_config_dir();
    let db_path = app_config_dir.join("cc-switch.db");
    let json_path = app_config_dir.join("config.json");

    // 检查是否需要从 config.json 迁移到 SQLite
    let has_json = json_path.exists();
    let has_db = db_path.exists();

    // 如果需要迁移，先验证 config.json 是否可以加载（在创建数据库之前）
    // 这样如果加载失败用户选择退出，数据库文件还没被创建，下次可以正常重试
    let migration_config = if !has_db && has_json {
        log::info!("检测到旧版配置文件，验证配置文件...");

        // 循环：支持用户重试加载配置文件
        loop {
            match crate::app_config::MultiAppConfig::load() {
                Ok(config) => {
                    log::info!("✓ 配置文件加载成功");
                    break Some(config);
                }
                Err(e) => {
                    log::error!("加载旧配置文件失败: {e}");
                    // 弹出系统对话框让用户选择
                    if !startup_dialogs::show_migration_error_dialog(app.handle(), &e.to_string()) {
                        // 用户选择退出（此时数据库还没创建，下次启动可以重试）
                        log::info!("用户选择退出程序");
                        exit_with_cleanup(app.handle(), 1);
                    }
                    // 用户选择重试，继续循环
                    log::info!("用户选择重试加载配置文件");
                }
            }
        }
    } else {
        None
    };

    // 现在创建数据库（包含 Schema 迁移）
    //
    // 说明：从 v3.8.* 升级的用户通常会走到这里的 SQLite schema 迁移，
    // 若迁移失败（数据库损坏/权限不足/user_version 过新等），需要给用户明确提示，
    // 否则表现可能只是“应用打不开/闪退”。
    //
    // 预检：数据库版本过新时，必须先于任何 schema 写操作（create_tables 内含
    // DROP/ALTER 等 DDL）进入恢复界面，避免旧应用对读不懂的更新版 DB 落写。
    match crate::database::Database::stored_user_version_exceeds_supported(&db_path) {
        Ok(Some(version)) => {
            log::warn!("数据库版本过新（v{version}），引导用户在应用内升级应用");
            crate::init_status::set_init_error(crate::init_status::InitErrorPayload {
                path: db_path.display().to_string(),
                error: format!(
                    "数据库版本过新（{version}），当前应用仅支持 {}，请升级应用后再尝试。",
                    crate::database::SCHEMA_VERSION
                ),
                kind: Some("db_version_too_new".to_string()),
                db_version: Some(version),
                supported_version: Some(crate::database::SCHEMA_VERSION),
            });
            // 主窗口默认 visible:false，恢复界面必须强制显示
            if let Some(window) = app.get_webview_window("main") {
                #[cfg(target_os = "windows")]
                {
                    let _ = window.set_skip_taskbar(false);
                }
                let _ = window.show();
                let _ = window.set_focus();
            }
            return Ok(());
        }
        Ok(None) => {}
        Err(e) => {
            log::warn!("预检数据库版本失败，继续正常初始化流程: {e}");
        }
    }

    let db = loop {
        match crate::database::Database::init() {
            Ok(db) => break Arc::new(db),
            Err(e) => {
                log::error!("Failed to init database: {e}");

                if !startup_dialogs::show_database_init_error_dialog(
                    app.handle(),
                    &db_path,
                    &e.to_string(),
                ) {
                    log::info!("用户选择退出程序");
                    exit_with_cleanup(app.handle(), 1);
                }

                log::info!("用户选择重试初始化数据库");
            }
        }
    };

    // 数据库可用后立即应用持久化日志级别，避免后续服务初始化
    // 继续使用启动阶段的 Info 回退。损坏配置显式 fail-closed 到 Info。
    match db.get_log_config() {
        Ok(log_config) => {
            log::set_max_level(log_config.to_level_filter());
            log::info!(
                "已加载日志配置: enabled={}, level={}",
                log_config.enabled,
                log_config.level
            );
        }
        Err(e) => {
            log::set_max_level(log::LevelFilter::Info);
            log::warn!("读取日志配置失败，已回退到 info: {e}");
        }
    }

    // 如果有预加载的配置，执行迁移
    if let Some(config) = migration_config {
        log::info!("开始执行数据迁移...");

        match db.migrate_from_json(&config) {
            Ok(_) => {
                log::info!("✓ 配置迁移成功");
                // 标记迁移成功，供前端显示 Toast
                crate::init_status::set_migration_success();
                // 归档旧配置文件（重命名而非删除，便于用户恢复）
                let archive_path = json_path.with_extension("json.migrated");
                if let Err(e) = std::fs::rename(&json_path, &archive_path) {
                    log::warn!("归档旧配置文件失败: {e}");
                } else {
                    log::info!("✓ 旧配置已归档为 config.json.migrated");
                }
            }
            Err(e) => {
                // 配置加载成功但迁移失败的情况极少（磁盘满等），仅记录日志
                log::error!("配置迁移失败: {e}，将从现有配置导入");
            }
        }
    }

    let app_state = AppState::new(db);

    // 设置 AppHandle 用于代理故障转移时的 UI 更新
    app_state.proxy_service.set_app_handle(app.handle().clone());

    // ============================================================
    // 按表独立判断的导入逻辑（各类数据独立检查，互不影响）
    // ============================================================

    // 1. 初始化默认 Skills 仓库（已有内置检查：表非空则跳过）
    match app_state.db.init_default_skill_repos() {
        Ok(count) if count > 0 => {
            log::info!("✓ Initialized {count} default skill repositories");
        }
        Ok(_) => {} // 表非空，静默跳过
        Err(e) => log::warn!("✗ Failed to initialize default skill repos: {e}"),
    }

    // 1.1. Skills 统一管理迁移：当数据库迁移到 v3 结构后，自动从各应用目录导入到 SSOT
    // 触发条件由 schema 迁移设置 settings.skills_ssot_migration_pending = true 控制。
    match app_state.db.get_setting("skills_ssot_migration_pending") {
        Ok(Some(flag)) if flag == "true" || flag == "1" => {
            // 安全保护：如果用户已经有 v3 结构的 Skills 数据，就不要自动清空重建。
            let has_existing = app_state
                .db
                .get_all_installed_skills()
                .map(|skills| !skills.is_empty())
                .unwrap_or(false);

            if has_existing {
                log::info!(
                            "Detected skills_ssot_migration_pending but skills table not empty; skipping auto import."
                        );
                let _ = app_state
                    .db
                    .set_setting("skills_ssot_migration_pending", "false");
            } else {
                match crate::services::skill::migrate_skills_to_ssot(&app_state.db) {
                    Ok(count) => {
                        log::info!("✓ Auto imported {count} skill(s) into SSOT");
                        if count > 0 {
                            crate::init_status::set_skills_migration_result(count);
                        }
                        let _ = app_state
                            .db
                            .set_setting("skills_ssot_migration_pending", "false");
                    }
                    Err(e) => {
                        log::warn!("✗ Failed to auto import legacy skills to SSOT: {e}");
                        crate::init_status::set_skills_migration_error(e.to_string());
                        // 保留 pending 标志，方便下次启动重试
                    }
                }
            }
        }
        Ok(_) => {} // 未开启迁移标志，静默跳过
        Err(e) => log::warn!("✗ Failed to read skills migration flag: {e}"),
    }

    // 1.5. 自动导入 live 配置 + seed 官方预设供应商（Claude / Codex / Gemini）
    //
    // 先 import 后 seed 是有意为之：先把用户手动配置的 settings.json / auth.json / .env
    // 落成 "default" provider 设为 current，再追加官方预设（is_current=false）。
    // 这样用户切到官方预设时，回填机制会保护原 live 配置不丢失。
    //
    // 捕获首次运行快照：所有全新装用户都会看到欢迎弹窗介绍 CC Switch 的工作方式。
    // 读失败时默认不弹，宁可漏弹也不要因为故障打扰用户。
    let first_run_already_confirmed = crate::settings::get_settings()
        .first_run_notice_confirmed
        .unwrap_or(false);
    let fresh_install_at_startup = app_state.db.is_providers_empty().unwrap_or(false);

    for app_type in crate::app_config::AppType::all().filter(|t| !t.is_additive_mode()) {
        if !crate::services::provider::should_import_default_config_on_startup(
            &app_state, &app_type,
        )
        .unwrap_or(false)
        {
            log::debug!(
                "○ {} already has providers; live import skipped",
                app_type.as_str()
            );
            continue;
        }

        match crate::services::provider::import_default_config(&app_state, app_type.clone()) {
            Ok(true) => log::info!(
                "✓ Imported live config for {} as default provider",
                app_type.as_str()
            ),
            Ok(false) => log::debug!(
                "○ {} already has providers; live import skipped",
                app_type.as_str()
            ),
            Err(e) => log::debug!("○ No live config to import for {}: {e}", app_type.as_str()),
        }
    }

    match app_state.db.init_default_official_providers() {
        Ok(count) if count > 0 => {
            log::info!("✓ Seeded {count} official provider(s)");
        }
        Ok(_) => {}
        Err(e) => log::warn!("✗ Failed to seed official providers: {e}"),
    }

    {
        let db_for_codex_history_migration = app_state.db.clone();
        tauri::async_runtime::spawn_blocking(move || {
            match crate::codex_history_migration::maybe_migrate_codex_third_party_history_provider_bucket(
                        &db_for_codex_history_migration,
                    ) {
                        Ok(outcome) => {
                            if let Some(reason) = outcome.skipped_reason {
                                log::debug!("○ Codex history provider bucket migration skipped: {reason}");
                            } else {
                                log::info!(
                                    "✓ Codex history provider bucket migration completed: sources={}, jsonl_files={}, state_rows={}",
                                    outcome.source_provider_ids.len(),
                                    outcome.migrated_jsonl_files,
                                    outcome.migrated_state_rows
                                );
                            }
                        }
                        Err(e) => {
                            log::warn!("✗ Codex history provider bucket migration failed: {e}");
                        }
                    }

            match crate::codex_history_migration::maybe_migrate_codex_provider_template_bucket(
                &db_for_codex_history_migration,
            ) {
                Ok(outcome) => {
                    if let Some(reason) = outcome.skipped_reason {
                        log::debug!("○ Codex provider template bucket migration skipped: {reason}");
                    } else if !outcome.migrated_provider_ids.is_empty() {
                        log::info!(
                            "✓ Codex provider template bucket migration completed: providers={}",
                            outcome.migrated_provider_ids.len()
                        );
                    }
                }
                Err(e) => {
                    log::warn!("✗ Codex provider template bucket migration failed: {e}");
                }
            }

            // 统一会话开关的官方历史迁移：开关开启但上次未完成（如文件被占用
            // 中途失败）时在启动期重试；函数内部自门控，开关关闭时直接跳过。
            match crate::codex_history_migration::maybe_migrate_codex_official_history_to_unified_bucket() {
                        Ok(outcome) => {
                            if let Some(reason) = outcome.skipped_reason {
                                log::debug!("○ Codex official history unify migration skipped: {reason}");
                            } else {
                                log::info!(
                                    "✓ Codex official history unify migration completed: jsonl_files={}, state_rows={}",
                                    outcome.migrated_jsonl_files,
                                    outcome.migrated_state_rows
                                );
                            }
                        }
                        Err(e) => {
                            log::warn!("✗ Codex official history unify migration failed: {e}");
                        }
                    }
        });
    }

    // 老用户 / 已确认的路径由 `fresh_install_at_startup` 自行拦截，这里不做写入。
    // 字段只由前端在用户点击"我知道了"时 save_settings 回写，语义是"用户显式确认过"。
    if !first_run_already_confirmed && fresh_install_at_startup {
        log::info!("✓ First-run welcome notice pending");
    }

    // 1.6. 自动同步累加模式应用的原生 providers 到数据库
    //
    // additive 模式的 import 函数按 id 幂等——
    // 新 id 执行导入，已有 id 则更新 settings 和 display name，所以每次
    // 启动都跑是安全的：既保证新装用户开箱可见 live 中的供应商，也让外部
    // 修改的 live 文件能在重启后同步到数据库（与之前依赖前端"导入当前配置"
    // 按钮手动触发不同）。
    //
    // 底层 read_*_config 在文件不存在时返回默认空配置，因此新装且无
    // live 文件的用户走 Ok(0) 路径，不会产生错误日志噪音。
    match crate::services::provider::import_pi_providers_from_live(&app_state) {
        Ok(count) if count > 0 => {
            log::info!("✓ Synced {count} Pi provider(s) from native config");
        }
        Ok(_) => log::debug!("○ No Pi provider changes from native config"),
        Err(e) => log::warn!("✗ Failed to import Pi providers: {e}"),
    }

    // 3. 导入 MCP 服务器配置（表空时触发）
    if app_state.db.is_mcp_table_empty().unwrap_or(false) {
        log::info!("MCP table empty, importing from live configurations...");

        match crate::services::mcp::McpService::import_from_claude(&app_state) {
            Ok(count) if count > 0 => {
                log::info!("✓ Imported {count} MCP server(s) from Claude");
            }
            Ok(_) => log::debug!("○ No Claude MCP servers found to import"),
            Err(e) => log::warn!("✗ Failed to import Claude MCP: {e}"),
        }

        match crate::services::mcp::McpService::import_from_codex(&app_state) {
            Ok(count) if count > 0 => {
                log::info!("✓ Imported {count} MCP server(s) from Codex");
            }
            Ok(_) => log::debug!("○ No Codex MCP servers found to import"),
            Err(e) => log::warn!("✗ Failed to import Codex MCP: {e}"),
        }
    }

    // 4. 导入提示词文件（表空时触发）
    if app_state.db.is_prompts_table_empty().unwrap_or(false) {
        log::info!("Prompts table empty, importing from live configurations...");

        for app in [
            crate::app_config::AppType::Claude,
            crate::app_config::AppType::Codex,
            crate::app_config::AppType::Pi,
        ] {
            match crate::services::prompt::PromptService::import_from_file_on_first_launch(
                &app_state,
                app.clone(),
            ) {
                Ok(count) if count > 0 => {
                    log::info!("✓ Imported {count} prompt(s) for {}", app.as_str());
                }
                Ok(_) => log::debug!("○ No prompt file found for {}", app.as_str()),
                Err(e) => log::warn!("✗ Failed to import prompt for {}: {e}", app.as_str()),
            }
        }
    }

    // 迁移旧的 app_config_dir 配置到 Store
    if let Err(e) = app_store::migrate_app_config_dir_from_settings(app.handle()) {
        log::warn!("迁移 app_config_dir 失败: {e}");
    }

    // 启动阶段不再无条件保存,避免意外覆盖用户配置。

    // 注册 deep-link URL 处理器（使用正确的 DeepLinkExt API）
    log::info!("=== Registering deep-link URL handler ===");

    // Linux 和 Windows 调试模式需要显式注册
    #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
    {
        #[cfg(target_os = "linux")]
        {
            // Use Tauri's path API to get correct path (includes app identifier)
            // tauri-plugin-deep-link writes to: ~/.local/share/com.ccswitch.desktop/applications/cc-switch-handler.desktop
            // Only register if .desktop file doesn't exist to avoid overwriting user customizations
            let should_register = app
                .path()
                .data_dir()
                .map(|d| !d.join("applications/cc-switch-handler.desktop").exists())
                .unwrap_or(true);

            if should_register {
                if let Err(e) = app.deep_link().register_all() {
                    log::error!("✗ Failed to register deep link schemes: {}", e);
                } else {
                    log::info!("✓ Deep link schemes registered (Linux)");
                }
            } else {
                log::info!("⊘ Deep link handler already exists, skipping registration");
            }
        }

        #[cfg(all(debug_assertions, windows))]
        {
            if let Err(e) = app.deep_link().register_all() {
                log::error!("✗ Failed to register deep link schemes: {}", e);
            } else {
                log::info!("✓ Deep link schemes registered (Windows debug)");
            }
        }
    }

    // 注册 URL 处理回调（所有平台通用）
    app.deep_link().on_open_url({
        let app_handle = app.handle().clone();
        move |event| {
            log::info!("=== Deep Link Event Received (on_open_url) ===");
            let urls = event.urls();
            log::info!("Received {} URL(s)", urls.len());

            if crate::lightweight::is_lightweight_mode() {
                if let Err(e) = crate::lightweight::exit_lightweight_mode(&app_handle) {
                    log::error!("退出轻量模式重建窗口失败: {e}");
                }
            }

            for (i, url) in urls.iter().enumerate() {
                let url_str = url.as_str();
                log::debug!("  URL[{i}]: {}", url_for_log(url_str));

                if handle_deeplink_url(&app_handle, url_str, true, "on_open_url") {
                    break; // Process only first ccswitch:// URL
                }
            }
        }
    });
    log::info!("✓ Deep-link URL handler registered");

    // 创建动态托盘菜单
    let menu = tray::create_tray_menu(app.handle(), &app_state)?;

    // 构建托盘
    let mut tray_builder = TrayIconBuilder::with_id(tray::TRAY_ID)
        .tooltip("CC Switch") // 鼠标悬停提示
        .on_tray_icon_event(|tray, event| match event {
            // 鼠标悬停/点击到托盘图标时，后台异步刷新用量缓存，
            // 让用户下一次（或快速打开菜单的那一刻）看到较新的数字。
            // refresh_all_usage_in_tray 内部有 10 秒防抖。
            TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. } => {
                let app = tray.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    crate::tray::refresh_all_usage_in_tray(&app).await;
                });
            }
            _ => log::debug!("unhandled event {event:?}"),
        })
        .menu(&menu)
        .on_menu_event(|app, event| {
            tray::handle_tray_menu_event(app, &event.id.0);
        })
        .show_menu_on_left_click(true);

    // 使用平台对应的托盘图标（macOS 使用模板图标适配深浅色）
    #[cfg(target_os = "macos")]
    {
        if let Some(icon) = macos_tray_icon() {
            tray_builder = tray_builder.icon(icon).icon_as_template(true);
        } else if let Some(icon) = app.default_window_icon() {
            log::warn!("Falling back to default window icon for tray");
            tray_builder = tray_builder.icon(icon.clone());
        } else {
            log::warn!("Failed to load macOS tray icon for tray");
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        if let Some(icon) = app.default_window_icon() {
            tray_builder = tray_builder.icon(icon.clone());
        } else {
            log::warn!("Failed to get default window icon for tray");
        }
    }

    let _tray = tray_builder.build(app)?;
    crate::services::webdav_auto_sync::start_worker(app_state.db.clone(), app.handle().clone());
    crate::services::s3_auto_sync::start_worker(app_state.db.clone(), app.handle().clone());
    // 将同一个实例注入到全局状态，避免重复创建导致的不一致
    app.manage(app_state);

    // 初始化 SkillService
    let skill_service = SkillService::new();
    app.manage(commands::skill::SkillServiceState(Arc::new(skill_service)));

    // 初始化 CopilotAuthManager
    {
        use crate::proxy::providers::copilot_auth::CopilotAuthManager;
        use commands::CopilotAuthState;
        use tokio::sync::RwLock;

        let app_config_dir = crate::config::get_app_config_dir();
        let copilot_auth_manager = CopilotAuthManager::new(app_config_dir);
        app.manage(CopilotAuthState(Arc::new(RwLock::new(
            copilot_auth_manager,
        ))));
        log::info!("✓ CopilotAuthManager initialized");
    }

    // 初始化 CodexOAuthManager (ChatGPT Plus/Pro 反代)
    {
        use commands::CodexOAuthState;

        let codex_oauth_manager = app.state::<AppState>().codex_oauth_manager.clone();
        app.manage(CodexOAuthState(codex_oauth_manager));
        log::info!("✓ CodexOAuthManager initialized");
    }

    // 初始化 xAI OAuthManager (Grok API 反代)
    {
        use crate::proxy::providers::xai_oauth_auth::XaiOAuthManager;
        use commands::XaiOAuthState;
        use tokio::sync::RwLock;

        let app_config_dir = crate::config::get_app_config_dir();
        let xai_oauth_manager = XaiOAuthManager::new(app_config_dir);
        app.manage(XaiOAuthState(Arc::new(RwLock::new(xai_oauth_manager))));
        log::info!("✓ XaiOAuthManager initialized");
    }

    // 初始化全局出站代理 HTTP 客户端
    {
        let db = &app.state::<AppState>().db;
        let proxy_url = db.get_global_proxy_url().ok().flatten();

        if let Err(e) = crate::proxy::http_client::init(proxy_url.as_deref()) {
            log::error!("[GlobalProxy] [GP-005] Failed to initialize with saved config: {e}");

            // 清除无效的代理配置
            if proxy_url.is_some() {
                log::warn!("[GlobalProxy] [GP-006] Clearing invalid proxy config from database");
                if let Err(clear_err) = db.set_global_proxy_url(None) {
                    log::error!(
                        "[GlobalProxy] [GP-007] Failed to clear invalid config: {clear_err}"
                    );
                }
            }

            // 使用直连模式重新初始化
            if let Err(fallback_err) = crate::proxy::http_client::init(None) {
                log::error!(
                    "[GlobalProxy] [GP-008] Failed to initialize direct connection: {fallback_err}"
                );
            }
        }
    }

    // 异常退出恢复 + 代理状态自动恢复
    let app_handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        let state = app_handle.state::<AppState>();

        // 检查是否有 Live 备份（表示上次异常退出时可能处于接管状态）
        let has_backups = match state.db.has_any_live_backup().await {
            Ok(v) => v,
            Err(e) => {
                log::error!("检查 Live 备份失败: {e}");
                false
            }
        };
        // 检查 Live 配置是否仍处于被接管状态（包含占位符）
        let live_taken_over = state.proxy_service.detect_takeover_in_live_configs();

        if has_backups || live_taken_over {
            log::warn!("检测到上次异常退出（存在接管残留），正在恢复 Live 配置...");
            if let Err(e) = state.proxy_service.recover_from_crash().await {
                log::error!("恢复 Live 配置失败: {e}");
            } else {
                log::info!("Live 配置已恢复");
            }
        }

        initialize_common_config_snippets(&state);

        // 检查 settings 表中的代理状态，自动恢复代理服务
        restore_proxy_state_on_startup(&state).await;

        // Periodic backup check (on startup)
        if let Err(e) = state.db.periodic_backup_if_needed() {
            log::warn!("Periodic backup failed on startup: {e}");
        }

        // Periodic maintenance timer: run once per day while the app is running
        let db_for_timer = state.db.clone();
        tauri::async_runtime::spawn(async move {
            const PERIODIC_MAINTENANCE_INTERVAL_SECS: u64 = 24 * 60 * 60;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                PERIODIC_MAINTENANCE_INTERVAL_SECS,
            ));
            interval.tick().await; // skip immediate first tick (already checked above)
            loop {
                interval.tick().await;
                if let Err(e) = db_for_timer.periodic_backup_if_needed() {
                    log::warn!("Periodic maintenance timer failed: {e}");
                }
            }
        });

        // Session log usage sync: 启动时同步一次，之后每 60 秒检查
        let db_for_session_sync = state.db.clone();
        tauri::async_runtime::spawn(async move {
            const SESSION_SYNC_INTERVAL_SECS: u64 = 60;

            async fn run_session_sync(
                db: std::sync::Arc<crate::database::Database>,
                backfill: bool,
            ) {
                // 手动扫描模式下跳过定时扫描；backfill 轮（启动首轮）仍进入，
                // 费用回填只修补数据库既有行（含代理记账行），不读会话文件
                if !backfill && !crate::settings::get_settings().session_auto_sync_enabled {
                    return;
                }
                let _guard = crate::services::session_usage::session_sync_mutex()
                    .lock()
                    .await;
                let task = tauri::async_runtime::spawn_blocking(move || {
                    if backfill {
                        if let Err(error) = db.backfill_missing_usage_costs() {
                            log::warn!("Usage cost startup backfill failed: {error}");
                        }
                    }
                    if !crate::settings::get_settings().session_auto_sync_enabled {
                        return crate::services::session_usage::SessionSyncResult::default();
                    }
                    crate::services::session_usage::sync_all_unlocked(&db)
                });
                match task.await {
                    Ok(result) if !result.errors.is_empty() => {
                        log::warn!(
                            "Session usage sync completed with {} error(s)",
                            result.errors.len()
                        );
                    }
                    Ok(_) => {}
                    Err(error) => log::warn!("Session usage blocking task failed: {error}"),
                }
            }

            // 首次同步（含费用回填）
            run_session_sync(db_for_session_sync.clone(), true).await;

            // 定期同步
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(SESSION_SYNC_INTERVAL_SECS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                run_session_sync(db_for_session_sync.clone(), false).await;
            }
        });
    });

    // Linux: 禁用 WebKitGTK 硬件加速，防止 EGL 初始化失败导致白屏
    #[cfg(target_os = "linux")]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.with_webview(|webview| {
                use webkit2gtk::{HardwareAccelerationPolicy, SettingsExt, WebViewExt};
                let wk_webview = webview.inner();
                if let Some(settings) = WebViewExt::settings(&wk_webview) {
                    SettingsExt::set_hardware_acceleration_policy(
                        &settings,
                        HardwareAccelerationPolicy::Never,
                    );
                    log::info!("已禁用 WebKitGTK 硬件加速");
                }
            });
        }
    }

    // 静默启动：根据设置决定是否显示主窗口
    let settings = crate::settings::get_settings();
    if let Some(window) = app.get_webview_window("main") {
        // 在窗口首次显示前同步装饰状态，避免前端加载后再切换导致标题栏闪烁
        // 仅 Linux 生效：解决 Wayland 下系统窗口按钮不可用的问题
        #[cfg(target_os = "linux")]
        let _ = window.set_decorations(!settings.use_app_window_controls);
        if settings.silent_startup {
            // 静默启动模式：保持窗口隐藏
            let _ = window.hide();
            #[cfg(target_os = "windows")]
            let _ = window.set_skip_taskbar(true);
            #[cfg(target_os = "macos")]
            tray::apply_tray_policy(app.handle(), false);
            log::info!("静默启动模式：主窗口已隐藏");
        } else {
            // 正常启动模式：显示窗口
            #[cfg(not(target_os = "windows"))]
            let _ = window.show();
            #[cfg(target_os = "windows")]
            log::info!("正常启动模式：等待主页面加载完成后显示主窗口");
            #[cfg(not(target_os = "windows"))]
            log::info!("正常启动模式：主窗口已显示");

            // Linux: 解决首次启动 UI 无响应问题（Tauri #10746 + wry #637）。
            // 启动时 webview 未获取焦点 + surface 尺寸协商失败，导致点击无效。
            // 这里做 set_focus + 伪 resize，等价于无视觉版本的"最大化-还原"。
            #[cfg(target_os = "linux")]
            {
                linux_fix::nudge_main_window(window.clone());
            }
        }
    }

    Ok(())
}
