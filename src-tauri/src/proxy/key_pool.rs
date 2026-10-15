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
    /// 单把 key 的日限额（USD 字符串，与 provider 级 limitDailyUsd 同格式）。
    ///
    /// N2：多 key 摊平的真实动机常是「每把 key 各有日免费额度」——usage 策略
    /// 会把流量推向最空的 key 直到它撞限流。软限制让超限 key 直接视同冷却
    /// （走既有 is_live 路径），不用等上游 429 被动反馈。None = 不限。
    #[serde(
        default,
        rename = "dailyLimitUsd",
        skip_serializing_if = "Option::is_none"
    )]
    pub daily_limit_usd: Option<String>,
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
    /// 按实际消耗：选窗口内 token 累计最少的未冷却 key（用多少补多少）
    Usage,
}

/// 一次池选择的结果。`key_id` 供冷却反馈归属（当前 P-C1 仅用于日志）。
///
/// `reason` 是 N1 的路由决策留痕：本次为什么选了这把（如 `usage:b=0,a=5001->b`）。
/// 与 key_id 同行写入 `proxy_request_logs.route_reason`，让策略行为可解释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySelection {
    pub key: String,
    pub key_id: String,
    pub reason: String,
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

/// 进程级 usage 累计表：`provider_id::key_id` -> 窗口内 input+output tokens。
///
/// 与冷却注册表同构（同样是 `LazyLock` + 全局键），因为 `select_for_provider`
/// 不持有数据库句柄，而 order/rotate 本来就是从全局状态取输入的。把 usage
/// 也放进全局表，Usage 策略才能在不改 trait 签名的前提下生效。
fn usage_totals() -> &'static Mutex<HashMap<String, u64>> {
    static USAGE: LazyLock<Mutex<HashMap<String, u64>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &USAGE
}

/// 把一次请求的 token 消耗累加到指定 key 上（usage 落库后调用）。
///
/// 由 `proxy_request_logs` 的写入方调用（成功行与错误行都算——限流同样
/// 是真实消耗）。累加到进程级表，供 [`select_usage`] 决策。
pub fn record_usage(provider_id: &str, key_id: &str, tokens: u64) {
    if key_id.is_empty() {
        return; // 未走池：无归属可记
    }
    if let Ok(mut guard) = usage_totals().lock() {
        *guard.entry(cooldown_key(provider_id, key_id)).or_insert(0) += tokens;
    }
}

/// 进程级当日成本表：`provider_id::key_id` -> 今日已花 USD（f64 累计）。
///
/// N2：`select_for_provider` 不持 db 句柄（与 usage_totals 同构的根因），
/// per-key 日限额的「今日已花」也走进程级表。跨进程重启后归零——软限制
/// 少算半个进程生命周期的量属可接受误差，等下一次上游反馈会修正。
fn daily_cost_totals() -> &'static Mutex<HashMap<String, f64>> {
    static DAILY: LazyLock<Mutex<HashMap<String, f64>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &DAILY
}

/// 记录日期的进程级锚点：跨天时整表作废重来（day-stamped 表的自然替代——
/// 存「日期 + 表」二元组成本最低）。
fn daily_cost_day() -> &'static Mutex<i64> {
    static DAY: LazyLock<Mutex<i64>> =
        LazyLock::new(|| Mutex::new(chrono::Utc::now().timestamp_millis() / 86_400_000));
    &DAY
}

/// 把一次请求的成本累加到指定 key 的当日累计上（N2）。
///
/// 与 [`record_usage`] 同一调用点（usage 落库成功后）——落库失败不记，
/// 不污染限额判断。跨天自动清零。
pub fn record_daily_cost(provider_id: &str, key_id: &str, cost_usd: f64) {
    if key_id.is_empty() {
        return;
    }
    let today = chrono::Utc::now().timestamp_millis() / 86_400_000;
    if let Ok(mut day) = daily_cost_day().lock() {
        if *day != today {
            *day = today;
            if let Ok(mut guard) = daily_cost_totals().lock() {
                guard.clear();
            }
        }
    }
    if let Ok(mut guard) = daily_cost_totals().lock() {
        *guard
            .entry(cooldown_key(provider_id, key_id))
            .or_insert(0.0) += cost_usd;
    }
}

/// 进程级上游限流压力表（R8）：`provider_id::key_id` -> 上游报告的已用百分比（0-100）。
///
/// 来自上游响应头（`x-codex-primary-used-percent` / `anthropic-ratelimit-*` 等）。
/// 让冷却机制从「被动 429 反馈」升级为「主动预判」：≥ [`PRESSURE_AVOID_PCT`]
/// 的 key 在路由时视作软冷却，提前避开限流。无头/无数据 = 无影响。
fn upstream_pressure() -> &'static Mutex<HashMap<String, u8>> {
    static PRESSURE: LazyLock<Mutex<HashMap<String, u8>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &PRESSURE
}

/// 软冷却阈值：上游报已用 ≥ 90% 时路由提前避开。
const PRESSURE_AVOID_PCT: u8 = 90;

/// 记录上游限流头报告的已用百分比（R8）。
///
/// forwarder 收到上游响应头时调用（成功与 429 都算——429 的头往往带着更准的余量）。
/// 值截到 0-100；解析失败静默忽略（各厂头名不统一，未知头本来就不传进来）。
pub fn note_upstream_pressure(provider_id: &str, key_id: &str, used_pct: u8) {
    if key_id.is_empty() {
        return;
    }
    if let Ok(mut guard) = upstream_pressure().lock() {
        guard.insert(cooldown_key(provider_id, key_id), used_pct.min(100));
    }
}

/// 从上游响应头提取已用百分比（R8）。
///
/// 已知头（按优先级）：
/// - `x-codex-primary-used-percent`（ChatGPT backend，Codex 0.160.1 RateLimitEvent 同源）
/// - `anthropic-ratelimit-unified-5h-utilization`（Anthropic unified 限流头的百分比）
/// - `x-ratelimit-remaining-requests` + `x-ratelimit-limit-requests`（OpenAI 风格，算余量）
pub fn parse_upstream_pressure(headers: &http::HeaderMap) -> Option<u8> {
    if let Some(v) = headers
        .get("x-codex-primary-used-percent")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().trim_end_matches('%').parse::<f64>().ok())
    {
        return Some(v.clamp(0.0, 100.0) as u8);
    }
    if let Some(v) = headers
        .get("anthropic-ratelimit-unified-5h-utilization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().trim_end_matches('%').parse::<f64>().ok())
    {
        // Anthropic 的 utilization 是 0-1 小数（0.95 = 95%）；>1 的值按百分数容错
        let pct = if v <= 1.0 { v * 100.0 } else { v };
        return Some(pct.clamp(0.0, 100.0) as u8);
    }
    // OpenAI 风格：remaining/limit → 已用百分比
    let remaining = headers
        .get("x-ratelimit-remaining-requests")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok());
    let limit = headers
        .get("x-ratelimit-limit-requests")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok());
    if let (Some(remaining), Some(limit)) = (remaining, limit) {
        if limit > 0.0 {
            let used = ((limit - remaining) / limit * 100.0).clamp(0.0, 100.0);
            return Some(used as u8);
        }
    }
    None
}

/// 某把 key 今日已花是否达到其日限额（N2）。
///
/// 未设限额 / 解析失败（非法数字视作未设，不静默把 key 锁死）→ false。
/// select 路径同样校验锚点日期：表是「昨天」的就视作 0——否则跨天后
/// 昨日超限的 key 永远恢复不了（锚点只在 record 路径更新，select 时
/// 可能尚未有任何新落库来触发清零）。
fn over_daily_limit(entry: &KeyPoolEntry, provider_id: &str) -> bool {
    let Some(limit) = entry
        .daily_limit_usd
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
    else {
        return false;
    };
    let today = chrono::Utc::now().timestamp_millis() / 86_400_000;
    let spent = match (daily_cost_day().lock(), daily_cost_totals().lock()) {
        (Ok(day), Ok(guard)) if *day == today => guard
            .get(&cooldown_key(provider_id, &entry.id))
            .copied()
            .unwrap_or(0.0),
        _ => 0.0, // 无数据 / 表已过期 / 锁中毒：视作今日未花
    };
    spent >= limit
}

/// 读取某个 provider 的池 usage 表（key_id -> tokens）。
///
/// 只返回池内 key 的累计；缺失的 key 不放入表（`select_usage` 视为 0）。
fn usage_snapshot(provider_id: &str, pool: &[KeyPoolEntry]) -> HashMap<String, u64> {
    let Ok(guard) = usage_totals().lock() else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for entry in pool {
        if let Some(total) = guard.get(&cooldown_key(provider_id, &entry.id)).copied() {
            out.insert(entry.id.clone(), total);
        }
    }
    out
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
    if let Ok(mut guard) = usage_totals().lock() {
        guard.clear();
    }
    if let Ok(mut guard) = daily_cost_totals().lock() {
        guard.clear();
    }
    if let Ok(mut guard) = upstream_pressure().lock() {
        guard.clear();
    }
    if let Ok(mut guard) = AFFINITY_SESSION.lock() {
        *guard = None;
    }
}

fn is_live(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    idx: usize,
    now: Instant,
) -> bool {
    if cooldown
        .get(&cooldown_key(provider_id, &pool[idx].id))
        .is_some_and(|until| *until > now)
    {
        return false;
    }
    // N2 软限制：达到日限额的 key 视同冷却（走同一条跳过路径）。
    // 全部超限时各 select 的「回落第一把」兜底仍然生效——宁可让超限 key
    // 再打一次（上游会 429 反馈），也不让请求无 key 可用。
    if over_daily_limit(&pool[idx], provider_id) {
        return false;
    }
    // R8 上游压力：≥90% 已用的 key 提前避开（同一软冷却路径）。
    if is_under_pressure(pool, provider_id, idx) {
        return false;
    }
    true
}

/// R8：该 key 的上游报告压力是否达到避开阈值。
fn is_under_pressure(pool: &[KeyPoolEntry], provider_id: &str, idx: usize) -> bool {
    upstream_pressure()
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .get(&cooldown_key(provider_id, &pool[idx].id))
                .copied()
        })
        .is_some_and(|pct| pct >= PRESSURE_AVOID_PCT)
}

/// order 选择：池中第一把未冷却的 key。
///
/// 纯函数（时刻由调用方注入），便于测试：
/// - 空池 -> `None`
/// - 全部冷却 -> 回落第一把（宁可撞一次限流也不阻塞请求；请求内的
///   provider 级 failover 仍会照常工作）
///
/// 返回 `(下标, 决策理由)`：理由区分「首活」与「全冷却回落」两种。
pub fn select_order(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    now: Instant,
) -> Option<(usize, String)> {
    if pool.is_empty() {
        return None;
    }
    match (0..pool.len()).find(|&idx| is_live(pool, cooldown, provider_id, idx, now)) {
        Some(idx) => Some((idx, format!("order:首活[{}]", pool[idx].id))),
        // 全冷却回落：理由要能和正常首活区分开，排查时一眼看出这不是健康状态
        None => Some((0, format!("order:全冷却回落[{}]", pool[0].id))),
    }
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
) -> Option<(usize, String)> {
    if pool.is_empty() {
        return None;
    }
    let last = cursors()
        .lock()
        .ok()
        .and_then(|g| g.get(provider_id).copied());
    let start = last.map_or(0, |i| (i + 1) % pool.len());
    let (idx, reason) = select_rotate_from(pool, cooldown, provider_id, now, start)?;
    if let Ok(mut guard) = cursors().lock() {
        guard.insert(provider_id.to_string(), idx);
    }
    Some((idx, reason))
}

/// rotate 的纯函数核心：从 `start` 起环绕找第一把未冷却的 key。
pub fn select_rotate_from(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    now: Instant,
    start: usize,
) -> Option<(usize, String)> {
    if pool.is_empty() {
        return None;
    }
    let start = start % pool.len();
    match (0..pool.len())
        .map(|step| (start + step) % pool.len())
        .find(|&idx| is_live(pool, cooldown, provider_id, idx, now))
    {
        Some(idx) => Some((idx, format!("rotate:轮转[{}]", pool[idx].id))),
        None => Some((0, format!("rotate:全冷却回落[{}]", pool[0].id))),
    }
}

/// 取 provider 的 key 池（无 meta 或空池时为空切片）。
pub fn pool_of(provider: &Provider) -> &[KeyPoolEntry] {
    provider
        .meta
        .as_ref()
        .map(|meta: &ProviderMeta| meta.key_pool.as_slice())
        .unwrap_or(&[])
}

/// 本请求的 session id 槽：forwarder（每请求一个实例）在开始时设置。
/// RequestForwarder 不是跨请求复用的，set-once 语义由调用方保证。
static AFFINITY_SESSION: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

/// forwarder 在请求开始时登记本请求的 session_id（R12）。
pub fn set_affinity_session(session_id: Option<String>) {
    if let Ok(mut guard) = AFFINITY_SESSION.lock() {
        *guard = session_id;
    }
}

fn current_affinity_session() -> Option<String> {
    AFFINITY_SESSION.lock().ok().and_then(|guard| guard.clone())
}

/// 亲和选择：session 有既往 key、该 key 在池里且活着 → 返回它（R12）。
///
/// 亲和只在「key 活着」时生效——冷却/限额/压力等软硬约束照样把它挡掉，
/// 那说明这把 key 现在不能用，缓存损失是约束的代价而非亲和失效。
fn select_affinity_if_live(
    pool: &[KeyPoolEntry],
    provider: &Provider,
    session_id: Option<&str>,
) -> Option<KeySelection> {
    let session_id = session_id?;
    let key_id = crate::proxy::session_affinity::SessionAffinity::global().get(session_id)?;
    let idx = pool.iter().position(|entry| entry.id == key_id)?;
    let cooldown = cooldown_snapshot();
    let now = Instant::now();
    if !is_live(pool, &cooldown, &provider.id, idx, now) {
        return None; // 既往 key 被约束挡住：回落正常策略
    }
    log::debug!(
        "[KeyPool] provider `{}` session 亲和粘住 key `{key_id}`",
        provider.id
    );
    let entry = &pool[idx];
    Some(KeySelection {
        key: entry.key.clone(),
        key_id: entry.id.clone(),
        reason: format!("affinity:会话粘住[{key_id}]"),
    })
}

/// 请求成功后把 session -> key_id 写进亲和表（R12）。
///
/// 由 forwarder 收尾处调用；session_id 为空（客户端没提供）时不记——
/// 生成的临时 id 跨请求不稳定，记了也是死条目。
pub fn record_affinity(session_id: &str, key_id: &str) {
    if session_id.is_empty() || key_id.is_empty() {
        return;
    }
    crate::proxy::session_affinity::SessionAffinity::global().put(session_id, key_id);
}

/// usage 选择：未冷却的 key 里选 token 累计最少的；并列取更靠前的。
///
/// 纯函数（usage 表由调用方注入，与 `cooldown`/`now` 同一风格），便于测试。
/// 契约与 order/rotate 对齐：
/// - 空池 -> `None`
/// - 全冷却 -> 回落第一把
/// - 无 usage 数据 -> 等价于首把（退化成 order 行为，老库不报错）
///
/// `usage` 的键是 **key_id**（不是 key 本身）；缺失的 key 记 0。
///
/// 返回 `(下标, 决策理由)`：理由带各候选 key 的累计值，让「为什么选它」可直接核对。
pub fn select_usage(
    pool: &[KeyPoolEntry],
    cooldown: &HashMap<String, Instant>,
    provider_id: &str,
    now: Instant,
    usage: &HashMap<String, u64>,
) -> Option<(usize, String)> {
    if pool.is_empty() {
        return None;
    }
    let live = (0..pool.len()).find(|&idx| is_live(pool, cooldown, provider_id, idx, now));
    let candidates = live.map_or(0..1, |first| first..pool.len());
    // 全部冷却时 live 为 None，回落第一把；否则在未冷却的 key 里取累计最少。
    let picked = candidates
        .filter(|&idx| idx == 0 || live.is_some_and(|first| idx >= first))
        .min_by_key(|&idx| (usage.get(&pool[idx].id).copied().unwrap_or(0), idx))?;
    let tally = |idx: usize| usage.get(&pool[idx].id).copied().unwrap_or(0);
    let reason = if live.is_none() {
        format!("usage:全冷却回落[{}]", pool[0].id)
    } else {
        let parts: Vec<String> = (0..pool.len())
            .map(|i| format!("{}={}", pool[i].id, tally(i)))
            .collect();
        format!("usage:{}->选[{}]", parts.join(","), pool[picked].id)
    };
    Some((picked, reason))
}

/// 池选择（glue 层）：池非空时返回选中 key，否则 `None`（调用方回落单 key 路径）。
/// 策略取自 `meta.keyPoolStrategy`，缺省 `Order`。
///
/// 三策的输入全部来自进程级状态（冷却注册表 / 轮转游标 / usage 累计表），
/// 本函数不持有数据库句柄——与 order/rotate 的实现同构。
pub fn select_for_provider(provider: &Provider) -> Option<KeySelection> {
    let pool = pool_of(provider);
    // R12 会话亲和性：forwarder 在本请求开始时经 [`set_affinity_session`]
    // 登记了 session_id，亲和表里记着既往 key 且该 key 仍活着 → 直接粘住。
    let session_id = current_affinity_session();
    if let Some(sel) = select_affinity_if_live(pool, provider, session_id.as_deref()) {
        return Some(sel);
    }
    let strategy = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.key_pool_strategy)
        .unwrap_or_default();
    let (idx, reason) = match strategy {
        KeyPoolStrategy::Order => {
            select_order(pool, &cooldown_snapshot(), &provider.id, Instant::now())?
        }
        KeyPoolStrategy::Rotate => {
            select_rotate(pool, &cooldown_snapshot(), &provider.id, Instant::now())?
        }
        KeyPoolStrategy::Usage => {
            // usage 累计来自进程级表（由 usage 落库处 record_usage 更新），
            // 与冷却/游标同源——select_for_provider 不持 db 句柄。
            let usage = usage_snapshot(&provider.id, pool);
            select_usage(
                pool,
                &cooldown_snapshot(),
                &provider.id,
                Instant::now(),
                &usage,
            )?
        }
    };
    let entry = &pool[idx];
    Some(KeySelection {
        key: entry.key.clone(),
        key_id: entry.id.clone(),
        reason,
    })
}

/// 按 provider 的策略「预览」上次请求实际使用的 key（不推进 rotate 游标）。
///
/// - `order`：首活 key——选择确定，与请求时一致。
/// - `rotate`：游标所记的 key——即上次请求选中并写入游标的那把；游标为空
///   （尚无请求发生过）时视作 0。
///
/// 并发下游标可能已被后续请求推进，此时归属是近似的（见模块 doc）。
///
/// 返回 `(下标, 决策理由)`——N1：理由与 key_id 同源同刻，落库时才不会
/// 出现「key_id 是 b、理由却说选了 a」的错位（同一快照里一起取）。
fn peek_last_used(
    pool: &[KeyPoolEntry],
    provider: &Provider,
    now: Instant,
) -> Option<(usize, String)> {
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
        // usage 的选择确定（累计最少的活 key）——预览必须与请求时同源，
        // 因此读真实 usage 表。用空表会永远返回首把，导致归属错记到 a 头上
        // （e2e 实测：第二行明明是 b 的 usage，key_id 却写成 a）。
        KeyPoolStrategy::Usage => {
            let usage = usage_snapshot(&provider.id, pool);
            select_usage(pool, &cooldown, &provider.id, now, &usage)
        }
    }
}

/// 预览「上次请求实际使用的池 key」的 `(key_id, 决策理由)`（N1）。
///
/// 不推进 rotate 游标、不改状态。错误行的 usage 归属与决策留痕都需要它——
/// 错误发生时 `AuthInfo` 早已不在作用域（见 `handlers::log_forward_error`），
/// 而限流错误恰恰是 usage 策略最需要归属的数据。
/// 无池 / 从未请求过时返回 `None`（→ 落库 NULL）。
///
/// 与 [`last_used_key_id`] 同一次 peek（同一快照、同一时刻），保证 id 与
/// 理由不会错位。无池 / 从未请求过时返回 `None`。
pub fn last_used_key_and_reason(provider: &Provider) -> Option<(String, String)> {
    let pool = pool_of(provider);
    let (idx, reason) = peek_last_used(pool, provider, Instant::now())?;
    Some((pool[idx].id.clone(), reason))
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
    let Some((idx, _)) = peek_last_used(pool, provider, Instant::now()) else {
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
            daily_limit_usd: None,
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
            select_order(&pool, &HashMap::new(), "p1", Instant::now()).map(|(i, _)| i),
            Some(0)
        );
    }

    #[test]
    fn select_order_skips_cooled_key() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now + Duration::from_secs(30));
        assert_eq!(
            select_order(&pool, &cooldown, "p1", now).map(|(i, _)| i),
            Some(1)
        );
    }

    #[test]
    fn select_order_falls_back_to_first_when_all_cooled() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now + Duration::from_secs(30));
        cooldown.insert(cooldown_key("p1", "b"), now + Duration::from_secs(30));
        assert_eq!(
            select_order(&pool, &cooldown, "p1", now).map(|(i, _)| i),
            Some(0)
        );
    }

    #[test]
    fn select_order_expired_cooldown_returns_to_first() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now - Duration::from_secs(1));
        assert_eq!(
            select_order(&pool, &cooldown, "p1", now).map(|(i, _)| i),
            Some(0)
        );
    }

    #[test]
    fn select_order_cooldown_is_scoped_per_provider() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        // p2 的 a 冷却不应影响 p1
        cooldown.insert(cooldown_key("p2", "a"), now + Duration::from_secs(30));
        assert_eq!(
            select_order(&pool, &cooldown, "p1", now).map(|(i, _)| i),
            Some(0)
        );
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
            select_rotate_from(&pool, &HashMap::new(), "p1", Instant::now(), 1).map(|(i, _)| i),
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
        assert_eq!(
            select_rotate_from(&pool, &cooldown, "p1", now, 1).map(|(i, _)| i),
            Some(0)
        );
    }

    #[test]
    fn select_rotate_from_falls_back_to_first_when_all_cooled() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now + Duration::from_secs(30));
        cooldown.insert(cooldown_key("p1", "b"), now + Duration::from_secs(30));
        assert_eq!(
            select_rotate_from(&pool, &cooldown, "p1", now, 1).map(|(i, _)| i),
            Some(0)
        );
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

    // ── R1: usage 路由策略 ─────────────────────────────────────────────

    /// 排序核心：未冷却的 key 里选 token 累计最少的；并列取更靠前的（稳定性）。
    #[test]
    fn select_usage_picks_least_consumed_live_key() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b"), entry("c", "sk-c")];
        let cooldown = HashMap::new();
        let now = Instant::now();
        let usage = HashMap::from([
            ("a".to_string(), 500u64),
            ("b".to_string(), 100u64),
            ("c".to_string(), 900u64),
        ]);
        assert_eq!(
            select_usage(&pool, &cooldown, "p1", now, &usage).map(|(i, _)| i),
            Some(1),
            "b 累计最少，应被选中"
        );
    }

    /// 并列时取更靠前的一把——保证行为确定、可复现（不做随机/轮转兜底）。
    #[test]
    fn select_usage_breaks_ties_by_pool_order() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let cooldown = HashMap::new();
        let now = Instant::now();
        let usage = HashMap::from([("a".to_string(), 300u64), ("b".to_string(), 300u64)]);
        assert_eq!(
            select_usage(&pool, &cooldown, "p1", now, &usage).map(|(i, _)| i),
            Some(0)
        );
    }

    /// 全冷却时回落第一把——与 order/rotate 一致（宁可撞一次限流也不阻塞）。
    #[test]
    fn select_usage_falls_back_to_first_when_all_cooled() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let cooldown = HashMap::from([
            (cooldown_key("p1", "a"), now + Duration::from_secs(60)),
            (cooldown_key("p1", "b"), now + Duration::from_secs(60)),
        ]);
        let usage = HashMap::from([("b".to_string(), 1u64)]);
        assert_eq!(
            select_usage(&pool, &cooldown, "p1", now, &usage).map(|(i, _)| i),
            Some(0)
        );
    }

    /// 无 usage 数据时降级为 order 行为（首把）——老库/新表没数据时不报错。
    #[test]
    fn select_usage_without_data_degrades_to_first() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let cooldown = HashMap::new();
        let usage = HashMap::new();
        assert_eq!(
            select_usage(&pool, &cooldown, "p1", Instant::now(), &usage).map(|(i, _)| i),
            Some(0)
        );
    }

    /// 冷却是硬约束：累计最少的 key 若在冷却中，跳过它选下一把。
    #[test]
    fn select_usage_skips_cooled_key_even_if_least_used() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let cooldown = HashMap::from([(cooldown_key("p1", "a"), now + Duration::from_secs(60))]);
        let usage = HashMap::from([("a".to_string(), 1u64), ("b".to_string(), 9999u64)]);
        assert_eq!(
            select_usage(&pool, &cooldown, "p1", now, &usage).map(|(i, _)| i),
            Some(1),
            "a 虽累计最少但在冷却，应选 b"
        );
    }

    /// 空池 → None。
    #[test]
    fn select_usage_empty_pool_returns_none() {
        let pool: Vec<KeyPoolEntry> = vec![];
        let cooldown = HashMap::new();
        let usage = HashMap::new();
        assert_eq!(
            select_usage(&pool, &cooldown, "p1", Instant::now(), &usage),
            None
        );
    }

    /// glue 层全链：record_usage 累计 → select_for_provider 按 Usage 策略
    /// 选中累计更少的那把。这是 R1 的验收核心（任务书 e2e 的单测版）。
    #[serial_test::serial(global_env)]
    #[test]
    fn usage_strategy_selects_least_consumed_via_global_table() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Usage,
        );

        // 初始无数据：两把都记 0 → 取第一把（与 order 同形，保证确定性）
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");

        // a 累计 5000，b 仍 0 → 应选 b
        record_usage(&provider.id, "a", 5000);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-b");

        // b 也累计 6000（比 a 多）→ 选回 a
        record_usage(&provider.id, "b", 6000);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");

        // a 追加 1 后仍少于 b（5001 < 6000）→ 稳定在 a
        record_usage(&provider.id, "a", 1);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");
        reset_for_tests();
    }

    /// 老数据（无任何 usage 记录）时 Usage 策略不报错，退化为首把。
    #[serial_test::serial(global_env)]
    #[test]
    fn usage_strategy_without_any_data_degrades_to_first() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Usage,
        );
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");
        reset_for_tests();
    }

    /// glue 层：策略缺省仍为 Order（存量池零感知），显式 Usage 才走新路径。
    #[serial_test::serial(global_env)]
    #[test]
    fn strategy_defaults_to_order_when_absent() {
        reset_for_tests();
        // 未设策略（None）→ order：重复选择都落在第一把
        let provider = provider_with_pool(vec![entry("a", "sk-a"), entry("b", "sk-b")]);
        assert_eq!(select_for_provider(&provider).unwrap().key, "sk-a");
        reset_for_tests();
    }

    // ── N1: 路由决策留痕 ──────────────────────────────────────────────

    /// order 正常首活的理由能区分出 key。
    #[test]
    fn select_order_reason_names_picked_key() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let (_, reason) = select_order(&pool, &HashMap::new(), "p1", Instant::now()).unwrap();
        assert!(reason.contains("[a]"), "理由应含选中的 key id: {reason}");
        assert!(reason.starts_with("order:"), "理由应带策略前缀: {reason}");
    }

    /// order 全冷却回落的理由必须与正常首活可区分——排查时一眼看出不健康。
    #[test]
    fn select_order_all_cooled_reason_is_distinct() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let now = Instant::now();
        let mut cooldown = HashMap::new();
        cooldown.insert(cooldown_key("p1", "a"), now + Duration::from_secs(30));
        cooldown.insert(cooldown_key("p1", "b"), now + Duration::from_secs(30));
        let (_, reason) = select_order(&pool, &cooldown, "p1", now).unwrap();
        assert!(reason.contains("全冷却回落"), "{reason}");
    }

    /// usage 理由带各候选 key 的累计值——「为什么选它」可直接核对。
    #[test]
    fn select_usage_reason_contains_tally() {
        let pool = vec![entry("a", "sk-a"), entry("b", "sk-b")];
        let cooldown = HashMap::new();
        let usage = HashMap::from([("a".to_string(), 5001u64), ("b".to_string(), 2u64)]);
        let (idx, reason) = select_usage(&pool, &cooldown, "p1", Instant::now(), &usage).unwrap();
        assert_eq!(idx, 1);
        assert!(
            reason.contains("a=5001") && reason.contains("b=2"),
            "{reason}"
        );
        assert!(reason.contains("->选[b]"), "{reason}");
    }

    /// glue 层：select_for_provider 返回的 reason 与所选 key 一致。
    #[serial_test::serial(global_env)]
    #[test]
    fn select_for_provider_carries_reason() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Usage,
        );
        record_usage("p1", "a", 5000);
        let sel = select_for_provider(&provider).unwrap();
        assert_eq!(sel.key_id, "b");
        assert!(sel.reason.contains("->选[b]"), "{}", sel.reason);
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

    // ── N2: per-key 日限额（软限制视同冷却） ──────────────────────────

    fn entry_with_limit(id: &str, key: &str, limit: Option<&str>) -> KeyPoolEntry {
        KeyPoolEntry {
            id: id.to_string(),
            key: key.to_string(),
            label: None,
            daily_limit_usd: limit.map(str::to_string),
        }
    }

    /// 超限 key 被跳过——usage 策略不把流量推向已到日额度的 key。
    #[serial_test::serial(global_env)]
    #[test]
    fn daily_limit_skips_over_limit_key() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![
                entry_with_limit("a", "sk-a", Some("0.001")),
                entry_with_limit("b", "sk-b", None),
            ],
            KeyPoolStrategy::Usage,
        );
        // a 首次被选（都为空时取首把），落库后累计成本达到限额
        assert_eq!(select_for_provider(&provider).unwrap().key_id, "a");
        record_daily_cost("p1", "a", 0.01);
        let sel = select_for_provider(&provider).unwrap();
        assert_eq!(sel.key_id, "b", "a 已超限（0.01 >= 0.001），应选 b");
        assert!(sel.reason.contains("->选[b]"), "{}", sel.reason);
        reset_for_tests();
    }

    /// 未设限额 / 非法数字 / 0 与负数：一律不限，不静默把 key 锁死。
    #[serial_test::serial(global_env)]
    #[test]
    fn daily_limit_invalid_values_mean_unlimited() {
        reset_for_tests();
        for bad in [None, Some("abc"), Some("0"), Some("-5"), Some("  ")] {
            let provider = provider_with_pool_and_strategy(
                vec![
                    entry_with_limit("a", "sk-a", bad),
                    entry_with_limit("b", "sk-b", None),
                ],
                KeyPoolStrategy::Usage,
            );
            record_daily_cost("p1", "a", 999.0);
            assert_eq!(
                select_for_provider(&provider).unwrap().key_id,
                "a",
                "限额值 {bad:?} 应视作不限"
            );
            reset_for_tests();
        }
    }

    /// 全部 key 都超限时回落第一把（与全冷却回落同构）——宁可超限再打一次
    /// 让上游反馈，也不让请求无 key 可用。
    #[serial_test::serial(global_env)]
    #[test]
    fn daily_limit_all_over_limit_falls_back_to_first() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![
                entry_with_limit("a", "sk-a", Some("0.5")),
                entry_with_limit("b", "sk-b", Some("0.5")),
            ],
            KeyPoolStrategy::Usage,
        );
        record_daily_cost("p1", "a", 1.0);
        record_daily_cost("p1", "b", 1.0);
        let sel = select_for_provider(&provider).unwrap();
        assert_eq!(sel.key_id, "a", "全超限回落第一把 a");
        assert!(sel.reason.contains("usage:全冷却回落"), "{}", sel.reason);
        reset_for_tests();
    }

    /// 跨天清零：昨日超限的 key 今天恢复可用（模拟：直接改锚点日期）。
    #[serial_test::serial(global_env)]
    #[test]
    fn daily_limit_resets_next_day() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![
                entry_with_limit("a", "sk-a", Some("0.001")),
                entry_with_limit("b", "sk-b", None),
            ],
            KeyPoolStrategy::Usage,
        );
        record_daily_cost("p1", "a", 1.0);
        assert_eq!(select_for_provider(&provider).unwrap().key_id, "b");
        // 手工把锚点拨到昨天，触发跨天清零
        {
            let mut day = daily_cost_day().lock().unwrap();
            *day -= 1;
        }
        assert_eq!(
            select_for_provider(&provider).unwrap().key_id,
            "a",
            "跨天后 a 的当日成本归零，恢复可用"
        );
        reset_for_tests();
    }

    // ── R8: 上游限流头回读（软冷却阈值 90%） ─────────────────────────

    fn header_map(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    /// 类别①：x-codex-primary-used-percent（含可选 % 后缀）。
    #[test]
    fn parse_upstream_pressure_codex_header() {
        assert_eq!(
            parse_upstream_pressure(&header_map(&[("x-codex-primary-used-percent", "93")])),
            Some(93)
        );
        assert_eq!(
            parse_upstream_pressure(&header_map(&[("x-codex-primary-used-percent", "42.7%")])),
            Some(42)
        );
        // 越界截到 0-100
        assert_eq!(
            parse_upstream_pressure(&header_map(&[("x-codex-primary-used-percent", "150")])),
            Some(100)
        );
    }

    /// 类别②：Anthropic unified 利用率头。
    #[test]
    fn parse_upstream_pressure_anthropic_header() {
        assert_eq!(
            parse_upstream_pressure(&header_map(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.95"
            )])),
            Some(95)
        );
    }

    /// 类别③：OpenAI 风格 remaining/limit 换算已用百分比。
    #[test]
    fn parse_upstream_pressure_openai_pair() {
        assert_eq!(
            parse_upstream_pressure(&header_map(&[
                ("x-ratelimit-remaining-requests", "2"),
                ("x-ratelimit-limit-requests", "20"),
            ])),
            Some(90)
        );
    }

    /// 类别④：无已知头 / 畸形值 → None，不 panic。
    #[test]
    fn parse_upstream_pressure_absent_or_malformed() {
        assert_eq!(parse_upstream_pressure(&header_map(&[])), None);
        assert_eq!(
            parse_upstream_pressure(&header_map(&[("x-codex-primary-used-percent", "abc")])),
            None
        );
        // limit 为 0 时除法保护
        assert_eq!(
            parse_upstream_pressure(&header_map(&[
                ("x-ratelimit-remaining-requests", "1"),
                ("x-ratelimit-limit-requests", "0"),
            ])),
            None
        );
    }

    /// 软冷却行为：≥90% 的 key 被路由避开，另一把接手。
    #[serial_test::serial(global_env)]
    #[test]
    fn upstream_pressure_avoids_hot_key() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Usage,
        );
        note_upstream_pressure("p1", "a", 90);
        let sel = select_for_provider(&provider).unwrap();
        assert_eq!(sel.key_id, "b", "a 已达 90% 压力，应避开选 b");
        reset_for_tests();
    }

    /// 低于阈值不影响；无数据的 key 不受影响。
    #[serial_test::serial(global_env)]
    #[test]
    fn upstream_pressure_below_threshold_is_ignored() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Usage,
        );
        note_upstream_pressure("p1", "a", 89);
        assert_eq!(select_for_provider(&provider).unwrap().key_id, "a");
        reset_for_tests();
    }

    // ── R12: 会话亲和性落盘 ──────────────────────────────────────────

    /// 记了亲和 → 同 session 粘回同 key（绕过策略计算）。
    #[serial_test::serial(global_env)]
    #[test]
    fn affinity_sticks_session_to_last_key() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Rotate, // rotate 会轮到 b；亲和必须压过它
        );
        record_affinity("sess-1", "a");
        set_affinity_session(Some("sess-1".to_string()));
        let sel = select_for_provider(&provider).unwrap();
        assert_eq!(sel.key_id, "a", "亲和应粘住 a，即使 rotate 想轮到 b");
        assert!(sel.reason.contains("affinity"), "{}", sel.reason);
        reset_for_tests();
    }

    /// 既往 key 被冷却 → 亲和失效，回落正常策略。
    #[serial_test::serial(global_env)]
    #[test]
    fn affinity_yields_to_cooldown() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Order,
        );
        record_affinity("sess-2", "a");
        note_rate_limited(&provider, 429, None); // a 进冷却
        set_affinity_session(Some("sess-2".to_string()));
        let sel = select_for_provider(&provider).unwrap();
        assert_eq!(sel.key_id, "b", "a 冷却中，亲和让位给约束");
        reset_for_tests();
    }

    /// 无亲和记录 / session 未登记 → 完全走原策略（存量行为零变化）。
    #[serial_test::serial(global_env)]
    #[test]
    fn affinity_absent_falls_through_to_strategy() {
        reset_for_tests();
        let provider = provider_with_pool_and_strategy(
            vec![entry("a", "sk-a"), entry("b", "sk-b")],
            KeyPoolStrategy::Order,
        );
        set_affinity_session(Some("sess-never-seen".to_string()));
        assert_eq!(select_for_provider(&provider).unwrap().key_id, "a");
        set_affinity_session(None);
        assert_eq!(select_for_provider(&provider).unwrap().key_id, "a");
        reset_for_tests();
    }
}
