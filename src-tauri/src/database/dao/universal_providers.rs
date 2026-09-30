//! 统一供应商 (Universal Provider) DAO
//!
//! 提供统一供应商的 CRUD 操作。

use crate::database::{lock_conn, to_json_string, Database};
use crate::error::AppError;
use crate::provider::UniversalProvider;
use std::collections::HashMap;

/// 统一供应商的 Settings Key
const UNIVERSAL_PROVIDERS_KEY: &str = "universal_providers";

impl Database {
    /// 获取所有统一供应商
    pub fn get_all_universal_providers(
        &self,
    ) -> Result<HashMap<String, UniversalProvider>, AppError> {
        let conn = lock_conn!(self.conn);
        read_universal_providers(&conn)
    }

    /// 获取单个统一供应商
    pub fn get_universal_provider(&self, id: &str) -> Result<Option<UniversalProvider>, AppError> {
        let providers = self.get_all_universal_providers()?;
        Ok(providers.get(id).cloned())
    }

    /// 保存统一供应商（添加或更新）
    ///
    /// 读-改-写在同一次持锁内完成，避免与其他写并发交错导致互相覆盖。
    pub fn save_universal_provider(&self, provider: &UniversalProvider) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        let mut providers = read_universal_providers(&conn)?;
        providers.insert(provider.id.clone(), provider.clone());
        write_universal_providers(&conn, &providers)
    }

    /// 删除统一供应商
    pub fn delete_universal_provider(&self, id: &str) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let mut providers = read_universal_providers(&conn)?;
        let existed = providers.remove(id).is_some();
        if existed {
            write_universal_providers(&conn, &providers)?;
        }
        Ok(existed)
    }
}

/// 读取统一供应商（调用方持有连接锁）。
///
/// 仅"键不存在"视为空集合；其余查询错误照常传播——
/// 把数据库错误误判为空集会在随后保存时清空全部数据。
fn read_universal_providers(
    conn: &rusqlite::Connection,
) -> Result<HashMap<String, UniversalProvider>, AppError> {
    let mut stmt = conn
        .prepare("SELECT value FROM settings WHERE key = ?")
        .map_err(|e| AppError::Database(e.to_string()))?;

    match stmt.query_row([UNIVERSAL_PROVIDERS_KEY], |row| row.get::<_, String>(0)) {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|e| AppError::Database(format!("解析统一供应商数据失败: {e}"))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(HashMap::new()),
        Err(e) => Err(AppError::Database(e.to_string())),
    }
}

/// 写回统一供应商（调用方持有连接锁）。
fn write_universal_providers(
    conn: &rusqlite::Connection,
    providers: &HashMap<String, UniversalProvider>,
) -> Result<(), AppError> {
    let json = to_json_string(providers)?;

    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)",
        rusqlite::params![UNIVERSAL_PROVIDERS_KEY, &json],
    )
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}
