//! 应用配置快照数据访问对象
//!
//! 快照存放在 `settings` 键值表中（key 前缀 `app_snapshot:`，value 为原始
//! JSON 文本，结构见 services/snapshots.rs，name/created_at 冗余内嵌在
//! JSON 里）。不使用独立表：fork 硬性约束禁止自造数据库迁移，而 settings
//! 表在基线上游（v3.20.4）即已存在，无需任何 schema 变更。
//!
//! 历史兼容：旧版 fork 曾用独立 `app_snapshots` 表。存量库由
//! `schema::normalize_legacy_trim_schema` 在启动时一次性搬到 settings 并
//! DROP 旧表，本 DAO 只读写 settings。

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::params;

/// settings 表中快照键的统一前缀
pub(crate) const APP_SNAPSHOT_KEY_PREFIX: &str = "app_snapshot:";

/// 应用配置快照记录
#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub name: String,
    /// 原始 JSON 快照文本（AppSnapshotData），解析在 service 层
    pub data: String,
    pub created_at: i64,
}

fn snapshot_key(name: &str) -> String {
    format!("{APP_SNAPSHOT_KEY_PREFIX}{name}")
}

/// 从快照 JSON 中提取 created_at（JSON 损坏或字段缺失时返回 0）。
///
/// 说明：旧表结构有 created_at 列可作排序/兜底；settings 存储后只能依赖
/// JSON 内嵌字段。损坏行的 created_at 降级为 0（排到最后），可接受。
fn created_at_from_json(data: &str) -> i64 {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|v| v.get("created_at").and_then(|t| t.as_i64()))
        .unwrap_or(0)
}

impl Database {
    /// 获取所有快照（按创建时间倒序）
    pub fn get_all_app_snapshots(&self) -> Result<Vec<AppSnapshot>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT key, value FROM settings WHERE key LIKE ?1 ORDER BY key",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let prefix_pattern = format!("{APP_SNAPSHOT_KEY_PREFIX}%");
        let rows = stmt
            .query_map(params![prefix_pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut snapshots = Vec::new();
        for row in rows {
            let (key, data) = row.map_err(|e| AppError::Database(e.to_string()))?;
            let name = key
                .strip_prefix(APP_SNAPSHOT_KEY_PREFIX)
                .unwrap_or(&key)
                .to_string();
            let created_at = created_at_from_json(&data);
            snapshots.push(AppSnapshot { name, data, created_at });
        }
        // 快照数量级极小（用户手工命名），内存排序足够
        snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.name.cmp(&b.name)));
        Ok(snapshots)
    }

    /// 获取单个快照
    pub fn get_app_snapshot(&self, name: &str) -> Result<Option<AppSnapshot>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare("SELECT value FROM settings WHERE key = ?1")
            .map_err(|e| AppError::Database(e.to_string()))?;

        let key = snapshot_key(name);
        match stmt.query_row(params![key], |row| row.get::<_, String>(0)) {
            Ok(data) => Ok(Some(AppSnapshot {
                name: name.to_string(),
                created_at: created_at_from_json(&data),
                data,
            })),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AppError::Database(e.to_string())),
        }
    }

    /// 保存快照（同名覆盖；覆盖时 created_at 由调用方决定是否沿用旧值）
    pub fn save_app_snapshot(&self, snapshot: &AppSnapshot) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![snapshot_key(&snapshot.name), snapshot.data],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 删除快照，返回是否实际删除了记录
    pub fn delete_app_snapshot(&self, name: &str) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let affected = conn
            .execute(
                "DELETE FROM settings WHERE key = ?1",
                params![snapshot_key(name)],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, data: &str, created_at: i64) -> AppSnapshot {
        AppSnapshot {
            name: name.to_string(),
            data: data.to_string(),
            created_at,
        }
    }

    #[test]
    fn test_app_snapshot_crud_roundtrip() -> Result<(), AppError> {
        let db = Database::memory()?;

        assert!(db.get_all_app_snapshots()?.is_empty());

        db.save_app_snapshot(&sample(
            "work",
            r#"{"name":"work","created_at":1000,"apps":{}}"#,
            1_000,
        ))?;
        db.save_app_snapshot(&sample("home", r#"{"apps":{},"created_at":2000}"#, 2_000))?;

        // 按创建时间倒序（取自 JSON 内嵌 created_at）
        let all = db.get_all_app_snapshots()?;
        assert_eq!(
            all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["home", "work"]
        );
        assert_eq!(all[1].created_at, 1_000);

        let got = db.get_app_snapshot("work")?.expect("snapshot exists");
        assert_eq!(got.created_at, 1_000);
        assert!(got.data.contains("created_at"));
        assert!(db.get_app_snapshot("missing")?.is_none());

        // 同名覆盖
        db.save_app_snapshot(&sample("work", r#"{"apps":{},"created_at":3000}"#, 3_000))?;
        let got = db.get_app_snapshot("work")?.expect("snapshot exists");
        assert_eq!(got.created_at, 3_000);
        assert_eq!(db.get_all_app_snapshots()?.len(), 2);

        assert!(db.delete_app_snapshot("work")?);
        assert!(!db.delete_app_snapshot("work")?);
        assert!(db.get_app_snapshot("work")?.is_none());
        Ok(())
    }

    #[test]
    fn test_app_snapshot_corrupt_json_sorts_last_with_zero_timestamp() -> Result<(), AppError> {
        let db = Database::memory()?;
        db.save_app_snapshot(&sample("bad", "not-json", 9_999))?;
        db.save_app_snapshot(&sample(
            "good",
            r#"{"created_at":1234,"apps":{}}"#,
            1_234,
        ))?;
        let all = db.get_all_app_snapshots()?;
        assert_eq!(all[0].name, "good");
        assert_eq!(all[0].created_at, 1_234);
        assert_eq!(all[1].name, "bad");
        assert_eq!(all[1].created_at, 0);
        Ok(())
    }
}
