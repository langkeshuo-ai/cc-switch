//! 会话粘性（Session Affinity）
//!
//! 记录「会话 → 上次成功使用的供应商」映射，供故障转移路由参考：
//! 同一会话的请求优先粘在它上次成功使用的供应商上，除非该供应商
//! 不可用（不在候选/已熔断）才按队列顺序回落。
//!
//! 设计要点：
//! - key 格式 `app:session_id`，避免跨应用碰撞
//! - TTL 30 分钟，读取时惰性过期（无后台任务）
//! - 容量上限 1024，超限淘汰最旧 `last_seen`
//! - 仅客户端提供的 session id 参与粘性（生成的 UUID 每次请求都不同）

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::session::normalize_session_id;

/// 默认 TTL：30 分钟
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);
/// 默认容量上限
const DEFAULT_CAPACITY: usize = 1024;

/// 会话粘性映射表
///
/// 线程安全：内部 `Mutex<HashMap>`，读写均为短临界区。
pub struct SessionAffinityMap {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<String, (String, Instant)>>,
}

impl Default for SessionAffinityMap {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionAffinityMap {
    /// 使用默认 TTL（30 分钟）与容量上限（1024）创建
    pub fn new() -> Self {
        Self::with_config(DEFAULT_TTL, DEFAULT_CAPACITY)
    }

    /// 使用自定义 TTL 与容量创建（测试用）
    pub fn with_config(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn key(app_type: &str, session_id: &str) -> String {
        format!("{app_type}:{session_id}")
    }

    /// 查询会话上次成功使用的供应商
    ///
    /// TTL 过期（惰性判断）、session id 不合法或未记录时返回 `None`。
    pub fn get(&self, app_type: &str, session_id: &str) -> Option<String> {
        let session_id = normalize_session_id(session_id)?;
        let entries = self.entries.lock().expect("session affinity lock poisoned");
        let (provider_id, last_seen) = entries.get(&Self::key(app_type, session_id))?;
        // 惰性过期：读时判断，不主动清理（容量上限兜底内存）
        if Instant::now().duration_since(*last_seen) >= self.ttl {
            return None;
        }
        Some(provider_id.clone())
    }

    /// 记录会话成功使用某供应商（同时刷新 last_seen）
    ///
    /// session id 不合法时静默忽略（不参与粘性）。
    pub fn record(&self, app_type: &str, session_id: &str, provider_id: &str) {
        let Some(session_id) = normalize_session_id(session_id) else {
            return;
        };
        let mut entries = self.entries.lock().expect("session affinity lock poisoned");
        let key = Self::key(app_type, session_id);

        // 超容量时淘汰最旧 last_seen，为新键腾位
        if !entries.contains_key(&key) && entries.len() >= self.capacity {
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, (_, last_seen))| *last_seen)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest_key);
            }
        }

        entries.insert(key, (provider_id.to_string(), Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn record_then_get_hits_same_provider() {
        let map = SessionAffinityMap::new();
        map.record("claude", "sess-1", "provider_a");
        assert_eq!(map.get("claude", "sess-1"), Some("provider_a".to_string()));
    }

    #[test]
    fn ttl_expiry_returns_none() {
        // TTL = 0：record 后立即视为过期（duration_since >= 0 恒成立），无需 sleep
        let map = SessionAffinityMap::with_config(Duration::ZERO, 16);
        map.record("claude", "sess-1", "provider_a");
        assert_eq!(map.get("claude", "sess-1"), None);
    }

    #[test]
    fn over_capacity_evicts_oldest_entry() {
        let map = SessionAffinityMap::with_config(DEFAULT_TTL, 2);
        map.record("claude", "sess-1", "p1");
        map.record("claude", "sess-2", "p2");
        // 第 3 条插入触发淘汰：最旧 last_seen 的 sess-1 被移除
        map.record("claude", "sess-3", "p3");

        assert_eq!(map.get("claude", "sess-1"), None);
        assert_eq!(map.get("claude", "sess-2"), Some("p2".to_string()));
        assert_eq!(map.get("claude", "sess-3"), Some("p3".to_string()));
    }

    #[test]
    fn keys_are_isolated_per_app() {
        let map = SessionAffinityMap::new();
        map.record("claude", "sess-1", "p_claude");
        map.record("codex", "sess-1", "p_codex");

        assert_eq!(map.get("claude", "sess-1"), Some("p_claude".to_string()));
        assert_eq!(map.get("codex", "sess-1"), Some("p_codex".to_string()));
    }

    #[test]
    fn invalid_session_ids_are_ignored() {
        let map = SessionAffinityMap::new();

        // record：非法 id 静默忽略，不占用容量
        map.record("claude", "bad id with spaces", "p1");
        map.record("claude", "bad/../id", "p1");
        map.record("claude", &"a".repeat(129), "p1");
        assert_eq!(map.get("claude", "bad id with spaces"), None);
        assert_eq!(map.get("claude", "bad/../id"), None);

        // 合法 id 不受影响
        map.record("claude", "sess-1", "p2");
        assert_eq!(map.get("claude", "sess-1"), Some("p2".to_string()));
    }
}
