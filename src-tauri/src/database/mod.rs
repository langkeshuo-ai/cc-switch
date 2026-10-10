//! 数据库模块 - SQLite 数据持久化
//!
//! 此模块提供应用的核心数据存储功能，包括：
//! - 供应商配置管理
//! - MCP 服务器配置
//! - 提示词管理
//! - Skills 管理
//! - 通用设置存储
//!
//! ## 架构设计
//!
//! ```text
//! database/
//! ├── mod.rs        - Database 结构体 + 初始化
//! ├── schema.rs     - 表结构定义 + Schema 迁移
//! ├── backup.rs     - SQL 导入导出 + 快照备份
//! ├── migration.rs  - JSON → SQLite 数据迁移
//! └── dao/          - 数据访问对象
//!     ├── providers.rs
//!     ├── mcp.rs
//!     ├── prompts.rs
//!     ├── skills.rs
//!     └── settings.rs
//! ```

pub(crate) mod backup;
mod dao;
mod migration;
mod schema;

#[cfg(test)]
mod tests;

// DAO 类型导出供外部使用
pub(crate) use dao::providers_seed::{is_official_seed_id, CODEX_OFFICIAL_PROVIDER_ID};
pub(crate) use dao::proxy::{
    validate_cost_multiplier, validate_pricing_source, PRICING_SOURCE_REQUEST,
    PRICING_SOURCE_RESPONSE,
};
pub use dao::snapshots::AppSnapshot;
pub use dao::FailoverQueueItem;
pub use dao::Profile;

use crate::config::get_app_config_dir;
use crate::error::AppError;
use rusqlite::{hooks::Action, Connection};
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 打开 SQLite，并在锁竞争时最多等待 5 秒。
///
/// rusqlite 默认 `busy_timeout = 0`。主库和导入用的暂存库都会在打开后立刻执行
/// PRAGMA；杀毒扫描或 WSL2 的 drvfs 经常让这条语句立刻返回 `database is locked`。
/// 5 秒只覆盖瞬时竞争，真死锁仍会失败，而不是一直卡住。
pub(crate) fn open_sqlite_waiting(path: &Path) -> Result<Connection, AppError> {
    let conn = Connection::open(path).map_err(|e| AppError::Database(e.to_string()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(conn)
}

// DAO 方法通过 impl Database 提供，无需额外导出

/// 当前 Schema 版本号
///
/// **schema 不变量（BASELINE.md 第 2 节）**：本值必须等于基线上游
/// （farion1231/cc-switch v3.20.4）的 `SCHEMA_VERSION`（=19）。
/// fork 不得自造迁移、不得让本值高于同版本上游——否则每次跟上游都要
/// 手改数据库，且与官方构建互不兼容。旧版 fork 曾顶到 22，存量库由
/// `schema::normalize_legacy_trim_schema` 在打开时自动降级归一。
pub(crate) const SCHEMA_VERSION: i32 = 19;

/// 安全地序列化 JSON，避免 unwrap panic
pub(crate) fn to_json_string<T: Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value)
        .map_err(|e| AppError::Config(format!("JSON serialization failed: {e}")))
}

/// 安全地获取 Mutex 锁，避免 unwrap panic
macro_rules! lock_conn {
    ($mutex:expr) => {
        $mutex
            .lock()
            .map_err(|e| AppError::Database(format!("Mutex lock failed: {}", e)))?
    };
}

// 导出宏供子模块使用
pub(crate) use lock_conn;

/// 回滚到 savepoint 并释放它，失败时**记录日志而非静默**。
///
/// 背景：迁移/裁剪这类"部分步骤失败仍要继续跑完"的事务，用 savepoint 隔离。
/// 收尾时若 `ROLLBACK TO` 自身失败（磁盘满、锁冲突），savepoint 状态即被破坏，
/// 后续 DDL 会连带失败；旧代码用 `.ok()` 吞掉，表现为"某功能突然报
/// no such table"却无任何日志可查。
///
/// 为什么不直接 `?` 传播：调用方此时已有原始错误要返回（那才是根因），
/// 回滚失败是次生信息，覆盖它反而会丢掉真正的失败原因。
pub(crate) fn rollback_savepoint(conn: &Connection, name: &str) {
    if let Err(e) = conn.execute(&format!("ROLLBACK TO {name}"), []) {
        log::error!(
            "回滚 savepoint '{name}' 失败：{e}。该事务内后续语句可能连带失败，\
             schema 可能停在半迁移状态。"
        );
    }
    if let Err(e) = conn.execute(&format!("RELEASE {name}"), []) {
        log::error!("释放 savepoint '{name}' 失败：{e}。连接可能残留未释放的保存点。");
    }
}

/// 数据库连接封装
///
/// 使用 Mutex 包装 Connection 以支持在多线程环境（如 Tauri State）中共享。
/// rusqlite::Connection 本身不是 Sync 的，因此需要这层包装。
/// 外层 Arc 允许 DAO 方法把连接克隆进 `spawn_blocking` 闭包（见 [`Database::db_blocking`]）。
pub struct Database {
    pub(crate) conn: Arc<Mutex<Connection>>,
}

fn register_db_change_hook(conn: &Connection) {
    conn.update_hook(Some(
        |action: Action, _database: &str, table: &str, _row_id: i64| match action {
            Action::SQLITE_INSERT | Action::SQLITE_UPDATE | Action::SQLITE_DELETE => {
                crate::services::webdav_auto_sync::notify_db_changed(table);
                crate::services::s3_auto_sync::notify_db_changed(table);
            }
            _ => {}
        },
    ));
}

impl Database {
    /// 初始化数据库连接并创建表
    ///
    /// 数据库文件位于 `~/.cc-switch/cc-switch.db`
    pub fn init() -> Result<Self, AppError> {
        let db_path = get_app_config_dir().join("cc-switch.db");
        let db_exists = db_path.exists();

        // 确保父目录存在
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }

        let conn = open_sqlite_waiting(&db_path)?;

        // 启用外键约束
        conn.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| AppError::Database(e.to_string()))?;
        if !db_exists {
            // For a brand-new database, configure incremental auto-vacuum
            // before creating any tables so no rebuild is needed later.
            conn.execute("PRAGMA auto_vacuum = INCREMENTAL;", [])
                .map_err(|e| AppError::Database(e.to_string()))?;
        }
        register_db_change_hook(&conn);

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.create_tables()?;

        // 存量 fork 数据库（曾被顶到 v20/v21/v22）先降级归一到上游 v19 形状，
        // 否则下方的迁移链会把它当「版本过新」拒绝。必须在备份与迁移之前执行。
        {
            let version = {
                let conn = lock_conn!(db.conn);
                Self::get_user_version(&conn)?
            };
            if (20..=22).contains(&version) {
                log::info!(
                    "Detected legacy trim schema v{version}; normalizing down to upstream v{SCHEMA_VERSION}"
                );
                // 归一是破坏性操作（DROP app_snapshots、删 'pi' 行），
                // 先落一份文件级备份，失败可回滚。
                if let Err(e) = db.backup_database_file() {
                    log::warn!("Pre-normalization backup failed, aborting normalize: {e}");
                    return Err(AppError::Database(format!(
                        "归一旧版数据库前的备份失败，已中止（错误: {e}）。请手动备份数据库后重试"
                    )));
                }
                let conn = lock_conn!(db.conn);
                Self::normalize_legacy_trim_schema_on_conn(&conn, version)?;
            }
        }

        // Pre-migration backup: only when upgrading from an existing database
        {
            let conn = lock_conn!(db.conn);
            let version = Self::get_user_version(&conn)?;
            drop(conn);
            if version > 0 && version < SCHEMA_VERSION {
                log::info!(
                    "Creating pre-migration database backup (v{version} → v{SCHEMA_VERSION})"
                );
                if let Err(e) = db.backup_database_file() {
                    log::warn!("Pre-migration backup failed, continuing migration: {e}");
                }
            }
        }

        db.apply_schema_migrations()?;
        if let Err(e) = db.ensure_incremental_auto_vacuum() {
            log::warn!("Failed to ensure incremental auto-vacuum: {e}");
        }
        db.ensure_model_pricing_seeded()?;
        if let Err(e) = crate::services::model_pricing::sync_local_model_pricing(&db) {
            log::warn!("Failed to sync local model pricing file: {e}");
        }

        // Startup cleanup: prune old logs and reclaim space
        if let Err(e) = db.cleanup_old_stream_check_logs(7) {
            log::warn!("Startup stream_check_logs cleanup failed: {e}");
        }
        if let Err(e) = db.rollup_and_prune(30) {
            log::warn!("Startup rollup_and_prune failed: {e}");
        }
        // Reclaim disk space after cleanup
        {
            let conn = lock_conn!(db.conn);
            if let Err(e) = Self::incremental_vacuum_on_conn(&conn) {
                log::warn!("Startup incremental vacuum failed: {e}");
            }
        }

        Ok(db)
    }

    /// 读取磁盘上数据库的 `user_version`；仅当它比应用支持的 [`SCHEMA_VERSION`]
    /// 更新时返回 `Some(version)`。
    ///
    /// 用于初始化失败后判断是否为「数据库版本过新（应用过旧，需升级应用）」的可恢复
    /// 场景——此时不应反复弹出无效的重试对话框，而应引导用户在应用内升级。
    pub fn stored_user_version_exceeds_supported(
        db_path: &std::path::Path,
    ) -> Result<Option<i32>, AppError> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(db_path).map_err(|e| AppError::Database(e.to_string()))?;
        let version = Self::get_user_version(&conn)?;
        Ok((version > SCHEMA_VERSION).then_some(version))
    }

    /// 创建内存数据库（用于测试）
    pub fn memory() -> Result<Self, AppError> {
        let conn = Connection::open_in_memory().map_err(|e| AppError::Database(e.to_string()))?;

        // 启用外键约束
        conn.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute("PRAGMA auto_vacuum = INCREMENTAL;", [])
            .map_err(|e| AppError::Database(e.to_string()))?;
        register_db_change_hook(&conn);

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.create_tables()?;
        db.ensure_model_pricing_seeded()?;

        Ok(db)
    }

    /// H4：在 tokio blocking 线程池上执行同步 DB 操作。
    ///
    /// rusqlite 是同步驱动，直接在 async 上下文中执行会阻塞 tokio worker 线程。
    /// 本 helper 把闭包移交给 `spawn_blocking` 执行；`Mutex<Connection>` 仍然
    /// 串行化所有访问，并发语义与内联执行完全一致，仅改变执行线程。
    ///
    /// # 约束
    /// - 只能在 tokio runtime 上下文中调用（`spawn_blocking` 的要求；
    ///   会被 `futures::executor::block_on` 在 runtime 外轮询的方法禁止使用）；
    /// - 闭包内不得再调用本 helper 或其他内部使用 `spawn_blocking` 的 DAO 方法；
    /// - 闭包拿到的 `&Connection` 已持有互斥锁，闭包内不要长时间空转。
    pub(crate) async fn db_blocking<T, F>(&self, f: F) -> Result<T, AppError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, AppError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        match tokio::task::spawn_blocking(move || {
            let conn = lock_conn!(conn);
            f(&conn)
        })
        .await
        {
            Ok(result) => result,
            Err(join_err) => Err(AppError::Database(format!(
                "数据库后台任务执行失败: {join_err}"
            ))),
        }
    }

    pub(crate) fn get_auto_vacuum_mode(conn: &Connection) -> Result<i32, AppError> {
        conn.query_row("PRAGMA auto_vacuum;", [], |row| row.get(0))
            .map_err(|e| AppError::Database(format!("读取 auto_vacuum 失败: {e}")))
    }

    /// 回收全部空闲页。`PRAGMA incremental_vacuum` 每释放一页产出一行结果，
    /// 必须把结果读完才会回收完；`execute_batch` 只 step 一次，只能回收 1 页。
    pub(crate) fn incremental_vacuum_on_conn(conn: &Connection) -> Result<(), AppError> {
        let mut stmt = conn
            .prepare("PRAGMA incremental_vacuum;")
            .map_err(|e| AppError::Database(format!("执行 incremental_vacuum 失败: {e}")))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| AppError::Database(format!("执行 incremental_vacuum 失败: {e}")))?;
        while rows
            .next()
            .map_err(|e| AppError::Database(format!("执行 incremental_vacuum 失败: {e}")))?
            .is_some()
        {}
        Ok(())
    }

    fn has_user_tables(conn: &Connection) -> Result<bool, AppError> {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| AppError::Database(format!("读取表数量失败: {e}")))?;
        Ok(count > 0)
    }

    pub(crate) fn ensure_incremental_auto_vacuum_on_conn(
        conn: &Connection,
    ) -> Result<bool, AppError> {
        let mode = Self::get_auto_vacuum_mode(conn)?;
        if mode == 2 {
            return Ok(false);
        }

        let has_tables = Self::has_user_tables(conn)?;
        conn.execute("PRAGMA auto_vacuum = INCREMENTAL;", [])
            .map_err(|e| AppError::Database(format!("设置 auto_vacuum 失败: {e}")))?;

        if !has_tables {
            return Ok(false);
        }

        conn.execute("VACUUM;", [])
            .map_err(|e| AppError::Database(format!("执行 VACUUM 失败: {e}")))?;
        conn.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| AppError::Database(format!("恢复 foreign_keys 失败: {e}")))?;
        Ok(true)
    }

    pub(crate) fn ensure_incremental_auto_vacuum(&self) -> Result<bool, AppError> {
        let mode = {
            let conn = lock_conn!(self.conn);
            Self::get_auto_vacuum_mode(&conn)?
        };
        if mode == 2 {
            return Ok(false);
        }

        let has_tables = {
            let conn = lock_conn!(self.conn);
            Self::has_user_tables(&conn)?
        };
        if has_tables {
            log::info!(
                "Detected auto_vacuum={mode}, rebuilding database to enable incremental vacuum"
            );
            self.backup_database_file()?;
        }

        let rebuilt = {
            let conn = lock_conn!(self.conn);
            Self::ensure_incremental_auto_vacuum_on_conn(&conn)?
        };

        if rebuilt {
            log::info!("Incremental auto-vacuum enabled after database rebuild");
        } else {
            log::info!("Incremental auto-vacuum configured for new database");
        }

        Ok(rebuilt)
    }

    /// 检查 MCP 服务器表是否为空
    pub fn is_mcp_table_empty(&self) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mcp_servers", [], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(count == 0)
    }

    /// 检查提示词表是否为空
    pub fn is_prompts_table_empty(&self) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM prompts", [], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(count == 0)
    }
}
