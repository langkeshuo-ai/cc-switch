//! 供应商 Key 池：同一 provider 多把 API key 的顺序故障转移。
//!
//! 背景：单一供应商常配多把 key（多账号/多额度），某把被限流（429）或欠费
//! （402）后，后续请求应改用池中其他 key，而不是整体切走 provider。
//!
//! 策略与 magpie routing group 对齐：`order`（每个请求选第一把未冷却的
//! key，故障转移语义）与 `rotate`（轮转，主动分摊负载）。`usage`（最少使用
//! 优先）暂缓——usage/ 计量当前无 per-key 粒度，见 ADR MAG-002。
//!
//! 落点选择：池存放在 `ProviderMeta.keyPool`（cc-switch 自有元数据，合并引擎
//! 不写入 agent 配置），不放 `settings_config`（会被 PROXY_MANAGED 合并引擎
//! 处理，有泄漏进 agent 配置的风险）。
//!
//! 反馈路径：上游 429/402 经 `categorize_proxy_error` 回调
//! [`note_rate_limited`]，对「该 provider 上次请求实际使用的池 key」记冷却。
//! 归属方式按 provider 的策略预览（order=首活 key，确定性；rotate=游标所记
//! key），不推进 rotate 游标。并发下同一 provider 多请求在途时，rotate 的
//! 游标可能已被后续请求推进——近似归属，该近似在真实场景成立。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::provider::{Provider, ProviderMeta};

/// 池中单把 key。`id` 稳定（冷却状态归属用），`label` 仅供 UI 备注。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyPoolEntry {
    pub id: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// 池路由策略。缺省 `Order`（与 P-C1 首发行为一致，存量池零感知升级）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyPoolStrategy {
    /// 每个请求选第一把未冷却的 key（故障转移）
    #[default]
    Order,
    /// 轮转：每次选下一把未冷却的 key（主动分摊）
    Rotate,
}

/// 一次池选择的结果。`key_id` 供冷却反馈归属（当前 P-C1 仅用于日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySelection {
    pub key: String,
    pub key_id: String,
}

/// 429（限流）默认冷却 60s；402（欠费）冷却 1h——与 magpie 默认值对齐。
///
/// 上游若在 `Retry-After` 里明确给了时长，以服务端值为准（见 [`parse_retry_after`]）；
/// 头缺失或畸形时才回落到这两个默认值。
const COOLDOWN_429: Duration = Duration::from_secs(60);
const COOLDOWN_402: Duration = Duration::from_secs(3600);

/// 冷却时长上限 24h。
///
/// 存在的意义是防上游（或中间人）给一个超大 `Retry-After`，把某把 key 事实上永久
/// 冷掉——那等价于悄悄减少可用容量，且没有任何界面能说明原因。截到 24h 后，
/// 最坏也只是这把 key 一天内不被选中。
const MAX_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

fn cooldown_key(provider_id: &str, key_id: &str) -> String {
    format!("{provider_id}::{key_id}")
}

/// 进程级冷却注册表：`provider_id::key_id` -> 冷却到期时刻。
fn registry() -> &'static Mutex<HashMap<String, Instant>> {
    static REGISTRY: LazyLock<Mutex<HashMap<String, Instant>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &REGISTRY
}

/// 进程级轮转游标：`provider_id` -> 上次选中的池下标（rotate 策略用）。
fn cursors() -> &'static Mutex<HashMap<String, usize>> {
    static CURSORS: LazyLock<Mutex<HashMap<String, usize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &CURSORS
}

/// 冷却快照（测试与 select 共用）。过期条目不在此处清理，由 select 时跳过。
pub fn cooldown_snapshot() -> HashMap<String, Instant> {
    registry()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// 测试专用：清空冷却注册表与轮转游标。调用方须 `#[serial(global_env)]` 串行。
#[cfg(test)]
pub fn reset_for_tests() {
    if let Ok(mut guard) = registry().lock() {
        guard.clear();
    }
    if let Ok(mut guard) = cursors().lock() {
        guard.clear();
    }
}

fn is_live(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    idx: usize,
    now: Instant,
) -> bool {
    cooldown
        .get(&cooldown_key(provider_id, &pool[idx].id))
        .is_none_or(|until| *until <= now)
}

/// order 选择：池中第一把未冷却的 key。
///
/// 纯函数（时刻由调用方注入），便于测试：
/// - 空池 -> `None`
/// - 全部冷却 -> 回落第一把（宁可撞一次限流也不阻塞请求；请求内的
///   provider 级 failover 仍会照常工作）
pub fn select_order(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    now: Instant,
) -> Option<usize> {
    if pool.is_empty() {
        return None;
    }
    let live = (0..pool.len()).find(|&idx| is_live(pool, cooldown, provider_id, idx, now));
    Some(live.unwrap_or(0))
}

/// rotate 选择：从「上次选中 + 1」起找第一把未冷却的 key（环绕）。
///
/// 纯函数部分（给定起始下标）便于测试；游标读写经 [`cursors`]。
/// 全部冷却时与 order 一致回落第一把。
pub fn select_rotate(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    now: Instant,
) -> Option<usize> {
    if pool.is_empty() {
        return None;
    }
    let last = cursors()
        .lock()
        .ok()
        .and_then(|g| g.get(provider_id).copied());
    let start = last.map_or(0, |i| (i + 1) % pool.len());
    let idx = select_rotate_from(pool, cooldown, provider_id, now, start)?;
    if let Ok(mut guard) = cursors().lock() {
        guard.insert(provider_id.to_string(), idx);
    }
    Some(idx)
}

/// rotate 的纯函数核心：从 `start` 起环绕找第一把未冷却的 key。
pub fn select_rotate_from(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    now: Instant,
    start: usize,
) -> Option<usize> {
    if pool.is_empty() {
        return None;
    }
    let start = start % pool.len();
    let live = (0..pool.len())
        .map(|step| (start + step) % pool.len())
        .find(|&idx| is_live(pool, cooldown, provider_id, idx, now));
    Some(live.unwrap_or(0))
}

/// 取 provider 的 key 池（无 meta 或空池时为空切片）。
pub fn pool_of(provider: &Provider) -> &[KeyPoolEntry] {
    provider
        .meta
        .as_ref()
        .map(|meta: &ProviderMeta| meta.key_pool.as_slice())
        .unwrap_or(&[])
}

/// 池选择（glue 层）：池非空时返回选中 key，否则 `None`（调用方回落单 key 路径）。
/// 策略取自 `meta.keyPoolStrategy`，缺省 `Order`。
pub fn select_for_provider(provider: &Provider) -> Option<KeySelection> {
    let pool = pool_of(provider);
    let strategy = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.key_pool_strategy)
        .unwrap_or_default();
    let idx = match strategy {
        KeyPoolStrategy::Order => {
            select_order(pool, &cooldown_snapshot(), &provider.id, Instant::now())?
        }
        KeyPoolStrategy::Rotate => {
            select_rotate(pool, &cooldown_snapshot(), &provider.id, Instant::now())?
        }
    };
    let entry = &pool[idx];
    Some(KeySelection {
        key: entry.key.clone(),
        key_id: entry.id.clone(),
    })
}

/// 按 provider 的策略「预览」上次请求实际使用的 key（不推进 rotate 游标）。
///
/// - `order`：首活 key——选择确定，与请求时一致。
/// - `rotate`：游标所记的 key——即上次请求选中并写入游标的那把；游标为空
///   （尚无请求发生过）时视作 0。
///
/// 并发下游标可能已被后续请求推进，此时归属是近似的（见模块 doc）。
fn peek_last_used(pool: &[KeyPoolEntry], provider: &Provider, now: Instant) -> Option<usize> {
    let strategy = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.key_pool_strategy)
        .unwrap_or_default();
    let cooldown = cooldown_snapshot();
    match strategy {
        KeyPoolStrategy::Order => select_order(pool, &cooldown, &provider.id, now),
        KeyPoolStrategy::Rotate => {
            let last = cursors()
                .lock()
                .ok()
                .and_then(|g| g.get(&provider.id).copied());
            select_rotate_from(pool, &cooldown, &provider.id, now, last.unwrap_or(0))
        }
    }
}

/// 上游限流/欠费反馈：对该 provider 上次请求实际使用的池 key 记冷却。
///
/// `retry_after` 是上游 `Retry-After` 头解析出的时长（见 [`parse_retry_after`]）：
/// 有值则用它，无值则按状态码取默认冷却。**过期时间由 `Instant::now() + duration`
/// 决定，不信任响应头里的绝对时刻**——本地时钟与上游时钟不保证一致。
///
/// 无池时 no-op（单 key provider 走既有 provider 级 failover）。
pub fn note_rate_limited(provider: &Provider, status: u16, retry_after: Option<Duration>) {
    let pool = pool_of(provider);
    if pool.is_empty() {
        return;
    }
    let duration = retry_after.unwrap_or(match status {
        402 => COOLDOWN_402,
        _ => COOLDOWN_429,
    });
    let Some(idx) = peek_last_used(pool, provider, Instant::now()) else {
        return;
    };
    let entry = &pool[idx];
    let until = Instant::now() + duration;
    if let Ok(mut guard) = registry().lock() {
        guard.insert(cooldown_key(&provider.id, &entry.id), until);
    }
    log::warn!(
        "[KeyPool] provider `{}` key `{}` ({}) 上游 HTTP {status}，冷却 {}s",
        provider.id,
        entry.id,
        entry.label.as_deref().unwrap_or("-"),
        duration.as_secs(),
    );
}

/// 截断冷却时长到 [`MAX_COOLDOWN`]。
fn clamp_cooldown(duration: Duration) -> Duration {
    if duration > MAX_COOLDOWN {
        MAX_COOLDOWN
    } else {
        duration
    }
}

/// 解析 `Retry-After` 响应头的值，返回冷却时长。
///
/// HTTP 规范（RFC 7231 §7.1.3）只允许两种形式：
///
/// - **delta-seconds**：纯十进制秒数，如 `120`
/// - **HTTP-date**：IMF-fixdate，如 `Wed, 21 Oct 2015 07:28:00 GMT`
///   （旧规范里的 RFC 850 与 asctime 已废弃，`parse_from_rfc2822` 覆盖前者，
///   后者形态差异大，实际服务端不发，故不支持）
///
/// **任何畸形输入都返回 `None`**（调用方随之回落按状态码的默认冷却），
/// 不 panic、不返回 0 之外的猜测值。
///
/// 返回值一律经 [`clamp_cooldown`] 截到 24h 上限。
/// 已过期的 HTTP-date 截为 `Duration::ZERO`，等价于「立即恢复」——
/// 那比把 key 冷却一个凭空的时长更符合服务端的原意。
pub fn parse_retry_after(raw: Option<&str>) -> Option<Duration> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }

    // delta-seconds：必须全是 ASCII 数字（负号、小数点、正号都非法）。
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let secs: u64 = text.parse().ok()?;
        return Some(clamp_cooldown(Duration::from_secs(secs)));
    }

    // HTTP-date（IMF-fixdate 与 RFC 2822 同形）。
    let at = chrono::DateTime::parse_from_rfc2822(text).ok()?;
    let delta = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
    let ms = delta.num_milliseconds().max(0) as u64;
    Some(clamp_cooldown(Duration::from_millis(ms)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use serde_json::json;

    fn entry(id: &str, key: &str) -> KeyPoolEntry {
        KeyPoolEntry {
            id: id.to_string(),
            key: key.to_string(),
            label: None,
        }
    }

    fn make_provider(id: &str, meta: Option<ProviderMeta>) -> Provider {
        Provider {
            id: id.to_string(),
            name: format!("Provider {id}"),
            settings_config: json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta,
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    fn provider_with_pool(pool: Vec<KeyPoolEntry>) -> Provider {
        let meta = ProviderMeta {
            key_pool: pool,
            ..Default::default()
        };
        make_provider("p1", Some(meta))
    }

    fn provider_with_pool_and_strategy(
        pool: Vec<KeyPoolEntry>,
        strategy: KeyPoolStrategy,
    ) -> Provider {
        let meta = ProviderMeta {
            key_pool: pool,
            key_pool_strategy: Some(strategy),
            ..Default::default()
        };
        make_provider("p1", Some(meta))
    }

    #[test]
    fn select_order_empty_pool_returns_none() {
        assert_eq!(
            select_order(&[], &HashMap::new(), "p1", Instant::now()),
            None
        );
    }

    #[test]
    fn select_order_picks_first_when_no_cooldown() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        assert_eq!(
            select_order(&pool, &HashMap::new(), "p1", Instant::now()),
            Some(0)
        );
    }

    #[test]
    fn select_order_skips_cooled_key() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now + Duration::from_secs(30));
        assert_eq!(select_order(&pool, &cooldown, "p1", now), Some(1));
    }

    #[test]
    fn select_order_falls_back_to_first_when_all_cooled() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now + Duration::from_secs(30));
        cooldown.insert(cooldown_key("p1", "b"), now + Duration::from_secs(30));
        assert_eq!(select_order(&pool, &cooldown, "p1", now), Some(0));
    }

    #[test]
    fn select_order_expired_cooldown_returns_to_first() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now - Duration::from_secs(1));
        assert_eq!(select_order(&pool, &cooldown, "p1", now), Some(0));
    }

    #[test]
    fn select_order_cooldown_is_scoped_per_provider() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        // p2 的 a 冷却不应影响 p1
        cooldown.insert(cooldown_key("p2", "a"), now + Duration::from_secs(30));
        assert_eq!(select_order(&pool, &cooldown, "p1", now), Some(0));
    }

    #[test]
    fn select_for_provider_without_pool_returns_none() {
        let provider = make_provider("p1", None);
        assert!(pool_of(&provider).is_empty());
        assert!(select_for_provider(&provider).is_none());
    }

    #[test]
    fn select_for_provider_returns_first_pool_key() {
        let provider = provider_with_pool(vec![entry("a", "sk-a"), entry("b", "sk-b")]);
        let selection = select_for_provider(&provider).expect("pool is non-empty");
        assert_eq!(selection.key, "sk-a");
        assert_eq!(selection.key_id, "a");
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn note_rate_limited_moves_selection_to_next_key() {
        reset_for_tests();
        let provider = provider_with_pool(vec![entry("a", "sk-a"), entry("b", "sk-b")]);

        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");

        note_rate_limited(&provider, 429, None);
        let after = select_for_provider(&provider).expect("pool is non-empty");
        assert_eq!(after.key, "sk-b");
        assert_eq!(after.key_id, "b");

        reset_for_tests();
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn note_rate_limited_402_uses_longer_cooldown_but_same_switch() {
        reset_for_tests();
        let provider = provider_with_pool(vec![entry("a", "sk-a"), entry("b", "sk-b")]);

        note_rate_limited(&provider, 402, None);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-b");

        reset_for_tests();
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn note_rate_limited_without_pool_is_noop() {
        reset_for_tests();
        let provider = make_provider("p1", None);
        note_rate_limited(&provider, 429, None);
        assert!(cooldown_snapshot().is_empty());
        reset_for_tests();
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn selection_returns_after_cooldown_expires() {
        reset_for_tests();
        let provider = provider_with_pool(vec![entry("a", "sk-a"), entry("b", "sk-b")]);

        note_rate_limited(&provider, 429, None);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-b");

        // 手工把 a 的冷却时刻拨到过去，验证过期后回到 a
        {
            let mut guard = registry().lock().unwrap();
            guard.insert(
                cooldown_key("p1", "a"),
                Instant::now() - Duration::from_secs(1),
            );
        }
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");

        reset_for_tests();
    }

    #[test]
    fn select_rotate_from_empty_pool_returns_none() {
        assert_eq!(
            select_rotate_from(&[], &HashMap::new(), "p1", Instant::now(), 0),
            None
        );
    }

    #[test]
    fn select_rotate_from_picks_start_when_live() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        assert_eq!(
            select_rotate_from(&pool, &HashMap::new(), "p1", Instant::now(), 1),
            Some(1)
        );
    }

    #[test]
    fn select_rotate_from_wraps_around() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        // start=1（b），b 冷却 → 环绕到 a
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "b"), now + Duration::from_secs(30));
        assert_eq!(select_rotate_from(&pool, &cooldown, "p1", now, 1), Some(0));
    }

    #[test]
    fn select_rotate_from_falls_back_to_first_when_all_cooled() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now + Duration::from_secs(30));
        cooldown.insert(cooldown_key("p1", "b"), now + Duration::from_secs(30));
        assert_eq!(select_rotate_from(&pool, &cooldown, "p1", now, 1), Some(0));
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn rotate_cycles_through_keys() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Rotate,
        );

        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-b");
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");

        reset_for_tests();
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn rotate_skips_cooled_key() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Rotate,
        );

        note_rate_limited(&provider, 429, None); // a 冷却
                                                 // a 冷却期间每次都应落在 b
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-b");
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-b");

        reset_for_tests();
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn note_rate_limited_cools_rotate_cursor_key_not_first_live() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Rotate,
        );

        // 第一次请求选 a（cursor=0），第二次选 b（cursor=1）
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-b");

        // b 的请求收到 429：必须冷却 b 本身，而不是 order 视角下的首活 key a
        note_rate_limited(&provider, 429, None);
        let snapshot = cooldown_snapshot();
        assert!(snapshot.contains_key(&cooldown_key("p1", "b")));
        assert!(!snapshot.contains_key(&cooldown_key("p1", "a")));

        // 冷却期间后续请求应回到 a
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");

        reset_for_tests();
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn rotate_single_key_stays_on_it() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("only", "sk-only")],
            KeyPoolStrategy::Rotate,
        );

        for _ in 0..3 {
            assert_eq!(select_for_provider(&provider).unwrap().key, "sk-only");
        }

        reset_for_tests();
    }

    #[serial_test::serial(global_env)]
    #[test]
    fn strategy_defaults_to_order_when_absent() {
        reset_for_tests();
        // 未设策略（None）→ order：重复选择都落在第一把
        let provider = provider_with_pool(vec![entry("a", "sk-a"), entry("b", "sk-b")]);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");
        reset_for_tests();
    }

    // ── R2: Retry-After 解析 ───────────────────────────────────────────

    /// 类别①：合法 delta-seconds。
    #[test]
    fn parse_retry_after_accepts_delta_seconds() {
        assert_eq!(parse_retry_after(Some("1")), Some(Duration::from_secs(1)));
        assert_eq!(
            parse_retry_after(Some("120")),
            Some(Duration::from_secs(120))
        );
        // 规范允许可选的前后空白（Ows）
        assert_eq!(
            parse_retry_after(Some("  30  ")),
            Some(Duration::from_secs(30))
        );
        assert_eq!(parse_retry_after(Some("0")), Some(Duration::ZERO));
    }

    /// 类别②：合法 HTTP-date（IMF-fixdate）。断言的是「量级合理」，不是精确秒数——
    /// 解析与断言之间存在真实时间流逝，钉死数值会 flaky。
    #[test]
    fn parse_retry_after_accepts_http_date() {
        let at = chrono::Utc::now() + chrono::Duration::seconds(600);
        let header = at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let parsed = parse_retry_after(Some(&header)).expect("IMF-fixdate 应可解析");
        let secs = parsed.as_secs();
        assert!((590..=600).contains(&secs), "期望约 600s，实际 {secs}s");
    }

    /// HTTP-date 已过期 → 0（立即恢复），不是 panic、也不是负数回绕。
    #[test]
    fn parse_retry_after_treats_past_http_date_as_immediate() {
        let at = chrono::Utc::now() - chrono::Duration::seconds(3600);
        let header = at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        assert_eq!(parse_retry_after(Some(&header)), Some(Duration::ZERO));
    }

    /// 类别③：畸形值一律 None（回落默认冷却），且不 panic。
    #[test]
    fn parse_retry_after_rejects_malformed() {
        for bad in [
            "",
            "   ",
            "abc",
            "-5",                            // 负数：规范不合法
            "+5",                            // 正号：规范不合法
            "12.5",                          // 小数：规范不合法
            "1s",                            // 单位后缀：规范不合法
            "99999999999999999999999999",    // 溢出 u64
            "Wed, 32 Oct 2015 07:28:00 GMT", // 非法日期
            "not-a-date",
        ] {
            assert_eq!(parse_retry_after(Some(bad)), None, "畸形值应回落：{bad:?}");
        }
    }

    /// 类别④：头缺失。
    #[test]
    fn parse_retry_after_handles_absent() {
        assert_eq!(parse_retry_after(None), None);
    }

    /// 上限：服务端给超大值不得把 key 事实上永久冷掉。
    #[test]
    fn parse_retry_after_clamps_to_24h() {
        assert_eq!(
            parse_retry_after(Some("999999999")),
            Some(MAX_COOLDOWN),
            "超大 delta-seconds 应截到 24h"
        );
        let far = chrono::Utc::now() + chrono::Duration::days(365);
        let header = far.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        assert_eq!(parse_retry_after(Some(&header)), Some(MAX_COOLDOWN));
    }

    /// 有覆盖值时用服务端值，不叠加状态码默认冷却。
    #[serial_test::serial(global_env)]
    #[test]
    fn note_rate_limited_prefers_server_retry_after() {
        reset_for_tests();
        let provider = provider_with_pool(vec![entry("a", "sk-a"), entry("b", "sk-b")]);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");

        // 429 默认 60s，但服务端说 1s → 1s 后就该恢复可用
        note_rate_limited(&provider, 429, Some(Duration::from_secs(1)));
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            select_for_provider(&provider).unwrap().key,
            "sk-a",
            "1s 后 a 应已恢复，被选中的仍是第一把"
        );
        reset_for_tests();
    }
}
