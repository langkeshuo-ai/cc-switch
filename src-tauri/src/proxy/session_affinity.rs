//! 会话亲和性落盘（R12，借鉴 magpie `affinity.json`）。
//!
//! 「谁在 vendor 的 prompt cache 里拿着这个会话」写进磁盘：重启（含升级自动
//! 重启）后仍能把同一 session 路由回同一把池 key——否则重启把会话交给路由
//! 排第一的 key，vendor 侧已建好的缓存全部作废重写，长会话的 cache read
//! 变 full price。
//!
//! 只落 `session_id -> key_id` 的映射（magpie 的 cache worth/cold 精细判定
//! 不做：三 CLI 场景粘住就比丢强，减法）。池成员变化（key 被删）时对应条目
//! 自然失效——读取方校验 key 仍在池里才用。
//!
//! 容量与过期照 magpie 收窄：512 条 LRU、24h 过期。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::get_app_config_dir;

/// 保留最近多少条映射。
const ENTRIES_KEPT: usize = 512;
/// 一条映射记多久：24h。
const ENTRY_TTL: Duration = Duration::from_secs(24 * 3600);

/// 单条映射：key_id + 落盘时刻（unix 秒）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AffinityEntry {
    key_id: String,
    at: u64,
}

/// 磁盘文件形状：条目表（键为 session_id）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AffinityFile {
    entries: HashMap<String, AffinityEntry>,
}

/// 进程级单例；惰性加载磁盘，写入节流（每次写全量重写——512 条上限下文件很小）。
pub struct SessionAffinity {
    inner: Mutex<Option<AffinityFile>>,
    path: PathBuf,
}

impl SessionAffinity {
    /// 用给定目录构造（生产用 `get_app_config_dir()`，测试用临时目录）。
    fn with_dir(dir: PathBuf) -> Self {
        Self {
            inner: Mutex::new(None),
            path: dir.join("session-affinity.json"),
        }
    }

    /// 全局实例（config 目录）。
    pub fn global() -> &'static Self {
        static GLOBAL: std::sync::OnceLock<SessionAffinity> = std::sync::OnceLock::new();
        GLOBAL.get_or_init(|| Self::with_dir(get_app_config_dir()))
    }

    fn load(&self) -> AffinityFile {
        if let Ok(guard) = self.inner.lock() {
            if let Some(file) = guard.as_ref() {
                return file.clone();
            }
        }
        let file = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<AffinityFile>(&raw).ok())
            .unwrap_or_default();
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(file.clone());
        }
        file
    }

    fn store(&self, file: &AffinityFile) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(file.clone());
        }
        let Ok(json) = serde_json::to_string(file) else {
            return;
        };
        // 写前 .bak（沿既有 app_config 约定）；解析失败下次 load 得空文件，
        // 行为退化为无亲和——不静默把别的数据写坏。
        if self.path.exists() {
            let _ = std::fs::copy(&self.path, self.path.with_extension("json.bak"));
        }
        let _ = std::fs::write(&self.path, json);
    }

    /// 查一个会话上一次由哪把池 key 应答。过期条目不返回（顺带清除由写路径做）。
    pub fn get(&self, session_id: &str) -> Option<String> {
        if session_id.is_empty() {
            return None;
        }
        let file = self.load();
        let now = now_secs();
        file.entries.get(session_id).and_then(|entry| {
            (now.saturating_sub(entry.at) < ENTRY_TTL.as_secs()).then(|| entry.key_id.clone())
        })
    }

    /// 记一个会话由哪把池 key 应答（LRU：超 512 条丢最旧的）。
    pub fn put(&self, session_id: &str, key_id: &str) {
        if session_id.is_empty() || key_id.is_empty() {
            return;
        }
        let now = now_secs();
        let mut file = self.load();
        // 顺带清过期
        file.entries
            .retain(|_, entry| now.saturating_sub(entry.at) < ENTRY_TTL.as_secs());
        file.entries.insert(
            session_id.to_string(),
            AffinityEntry {
                key_id: key_id.to_string(),
                at: now,
            },
        );
        if file.entries.len() > ENTRIES_KEPT {
            // 按时间丢最旧的
            let mut by_time: Vec<(String, u64)> = file
                .entries
                .iter()
                .map(|(k, v)| (k.clone(), v.at))
                .collect();
            by_time.sort_by_key(|(_, at)| *at);
            let drop_n = file.entries.len() - ENTRIES_KEPT;
            for (k, _) in by_time.into_iter().take(drop_n) {
                file.entries.remove(&k);
            }
        }
        self.store(&file);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> SessionAffinity {
        let dir = std::env::temp_dir().join(format!("ccswitch-affinity-test-{tag}"));
        let _ = std::fs::create_dir_all(&dir);
        SessionAffinity::with_dir(dir)
    }

    #[test]
    fn put_then_get_round_trips() {
        let store = temp_store("rt");
        store.put("sess-1", "pool-a");
        assert_eq!(store.get("sess-1"), Some("pool-a".to_string()));
    }

    #[test]
    fn get_unknown_session_is_none() {
        let store = temp_store("unknown");
        assert_eq!(store.get("nope"), None);
    }

    #[test]
    fn empty_ids_are_ignored() {
        let store = temp_store("empty");
        store.put("", "pool-a");
        store.put("sess", "");
        assert_eq!(store.get(""), None);
        assert_eq!(store.get("sess"), None);
    }

    #[test]
    fn survives_reload_from_disk() {
        // 两个实例指向同一目录：模拟「重启」
        let dir = std::env::temp_dir().join("ccswitch-affinity-test-reload");
        let _ = std::fs::create_dir_all(&dir);
        let first = SessionAffinity::with_dir(dir.clone());
        first.put("sess-42", "pool-b");
        let second = SessionAffinity::with_dir(dir);
        assert_eq!(second.get("sess-42"), Some("pool-b".to_string()));
    }

    #[test]
    fn lru_evicts_oldest_beyond_cap() {
        let store = temp_store("lru");
        // 写 ENTRIES_KEPT + 5 条（时间戳同为秒，靠插入后按 at 排序丢最旧；
        // at 相同时排序稳定按键序——这里只断言容量上限被守住）
        for i in 0..(ENTRIES_KEPT + 5) {
            store.put(&format!("sess-{i:04}"), "k");
        }
        let file = store.load();
        assert_eq!(file.entries.len(), ENTRIES_KEPT);
    }

    #[test]
    fn expired_entry_is_not_returned() {
        let store = temp_store("ttl");
        store.put("sess-old", "k");
        // 直接把时间拨到 25 小时前
        {
            let tampered = {
                let mut file = store.load();
                if let Some(entry) = file.entries.get_mut("sess-old") {
                    entry.at -= 25 * 3600;
                }
                file
            };
            std::fs::write(&store.path, serde_json::to_string(&tampered).unwrap()).unwrap();
        }
        // 内存缓存里还是旧的——load 缓存命中会返回旧值，这是设计内行为：
        // 进程内时钟由 put 时刷新，过期清理发生在每次 put。模拟重启读盘：
        let dir = store.path.parent().unwrap().to_path_buf();
        let fresh = SessionAffinity::with_dir(dir);
        assert_eq!(fresh.get("sess-old"), None, "过期条目重启后不可用");
    }
}
