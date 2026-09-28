//! 应用配置快照数据访问对象
//!
//! app_snapshots 表存放「一键整体恢复」用的命名快照：每个快照记录三个
//! 应用（claude/codex/pi）各自的当前供应商 id 与代理接管状态。data 为
//! 原始 JSON 文本（结构见 services/snapshots.rs），解析在 service 层进行。
//!
//! 命名说明：`profiles` 表已被「项目 Profile」功能占用，本表使用
//! app_snapshots 以示区分。

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::params;

/// 应用配置快照记录
#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub name: String,
    /// 原始 JSON 快照文本（AppSnapshotData），解析在 service 层
    pub data: String,
    pub created_at: i64,
}

impl Database {
    /// 获取所有快照（按创建时间倒序）
    pub fn get_all_app_snapshots(&self) -> Result<Vec<AppSnapshot>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT name, data, created_at
                 FROM app_snapshots
                 ORDER BY created_at DESC, name",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(AppSnapshot {
                    name: row.get(0)?,
                    data: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut snapshots = Vec::new();
        for row in rows {
            snapshots.push(row.map_err(|e| AppError::Database(e.to_string()))?);
        }
        Ok(snapshots)
    }

    /// 获取单个快照
    pub fn get_app_snapshot(&self, name: &str) -> Result<Option<AppSnapshot>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare("SELECT name, data, created_at FROM app_snapshots WHERE name = ?1")
            .map_err(|e| AppError::Database(e.to_string()))?;

        match stmt.query_row(params![name], |row| {
            Ok(AppSnapshot {
                name: row.get(0)?,
                data: row.get(1)?,
                created_at: row.get(2)?,
            })
        }) {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AppError::Database(e.to_string())),
        }
    }

    /// 保存快照（同名覆盖；覆盖时 created_at 由调用方决定是否沿用旧值）
    pub fn save_app_snapshot(&self, snapshot: &AppSnapshot) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT OR REPLACE INTO app_snapshots (name, data, created_at) VALUES (?1, ?2, ?3)",
            params![snapshot.name, snapshot.data, snapshot.created_at],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 删除快照，返回是否实际删除了记录
    pub fn delete_app_snapshot(&self, name: &str) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let affected = conn
            .execute("DELETE FROM app_snapshots WHERE name = ?1", params![name])
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
        db.save_app_snapshot(&sample("home", r#"{"apps":{}}"#, 2_000))?;

        // 按创建时间倒序
        let all = db.get_all_app_snapshots()?;
        assert_eq!(
            all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["home", "work"]
        );

        let got = db.get_app_snapshot("work")?.expect("snapshot exists");
        assert_eq!(got.created_at, 1_000);
        assert!(got.data.contains("created_at"));
        assert!(db.get_app_snapshot("missing")?.is_none());

        // 同名覆盖
        db.save_app_snapshot(&sample("work", r#"{"apps":{}}"#, 3_000))?;
        let got = db.get_app_snapshot("work")?.expect("snapshot exists");
        assert_eq!(got.created_at, 3_000);
        assert_eq!(db.get_all_app_snapshots()?.len(), 2);

        assert!(db.delete_app_snapshot("work")?);
        assert!(!db.delete_app_snapshot("work")?);
        assert!(db.get_app_snapshot("work")?.is_none());
        Ok(())
    }
}
