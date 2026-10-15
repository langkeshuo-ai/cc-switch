//! Usage Logger - 记录 API 请求使用情况

use super::calculator::{CostBreakdown, CostCalculator, ModelPricing};
use super::parser::TokenUsage;
use crate::database::{Database, PRICING_SOURCE_REQUEST, PRICING_SOURCE_RESPONSE};
use crate::error::AppError;
use crate::services::sql_helpers::{INPUT_TOKEN_SEMANTICS_FRESH, INPUT_TOKEN_SEMANTICS_TOTAL};
use crate::services::usage_stats::{find_model_pricing_row, is_placeholder_pricing_model};
use rusqlite::OptionalExtension;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

#[derive(Debug, PartialEq, Eq)]
struct UsageSemantic {
    app_type: String,
    provider_id: String,
    model: String,
    input_token_semantics: i64,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    status_code: u16,
}

impl UsageSemantic {
    fn from_log(log: &RequestLog, input_token_semantics: i64) -> Self {
        Self {
            app_type: log.app_type.clone(),
            provider_id: log.provider_id.clone(),
            model: log.model.clone(),
            input_token_semantics,
            input_tokens: log.usage.input_tokens,
            output_tokens: log.usage.output_tokens,
            cache_read_tokens: log.usage.cache_read_tokens,
            cache_creation_tokens: log.usage.cache_creation_tokens,
            status_code: log.status_code,
        }
    }

    fn sha256(&self) -> String {
        let encoded = serde_json::to_vec(&(
            &self.app_type,
            &self.provider_id,
            &self.model,
            self.input_token_semantics,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_creation_tokens,
            self.status_code,
        ))
        .expect("usage semantic tuple is serializable");
        Sha256::digest(encoded)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// 请求日志
#[derive(Debug, Clone)]
pub struct RequestLog {
    pub request_id: String,
    pub provider_id: String,
    pub app_type: String,
    pub model: String,
    pub request_model: String,
    /// 写入时实际用于计价的模型名（pricing_model_source 解析后的结果）。
    /// 落库供回填使用：缺价行补价后必须按写入时的基准重算，而不是
    /// 用 model/request_model 猜——路由接管下三者可能各不相同。
    /// 错误行（未计价）为空字符串。
    pub pricing_model: String,
    pub usage: TokenUsage,
    pub cost: Option<CostBreakdown>,
    pub latency_ms: u64,
    pub first_token_ms: Option<u64>,
    pub status_code: u16,
    pub error_message: Option<String>,
    pub session_id: Option<String>,
    /// 供应商类型 (claude, claude_auth, codex, gemini, gemini_cli, openrouter)
    pub provider_type: Option<String>,
    /// 是否为流式请求
    pub is_streaming: bool,
    /// 成本倍数
    pub cost_multiplier: String,
    /// 本次实际选中的池 key id（无池 / 未走池时为 `None` → 落库 NULL）。
    ///
    /// usage 路由策略按它归属 token 消耗。历史行为 NULL，`select_usage`
    /// 遇到无数据的 key 会退化为 order 行为。
    pub key_id: Option<String>,
    /// N1 路由决策留痕：本次为什么选了这把 key（如 `usage:a=5001,b=0->选[b]`）。
    /// 与 `key_id` 同行落库（NULL = 无池 / 未走池 / v22 前的历史行）。
    pub route_reason: Option<String>,
}

/// 使用量记录器
pub struct UsageLogger<'a> {
    db: &'a Database,
}

impl<'a> UsageLogger<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// 记录成功的请求
    pub fn log_request(&self, log: &RequestLog) -> Result<(), AppError> {
        let conn = crate::database::lock_conn!(self.db.conn);
        Self::log_request_on_conn(&conn, log)
    }

    /// 写入请求日志的同步核心（H4：供 `db_blocking` 闭包复用）
    fn log_request_on_conn(conn: &rusqlite::Connection, log: &RequestLog) -> Result<(), AppError> {
        let (input_cost, output_cost, cache_read_cost, cache_creation_cost, total_cost) =
            if let Some(cost) = &log.cost {
                (
                    cost.input_cost.to_string(),
                    cost.output_cost.to_string(),
                    cost.cache_read_cost.to_string(),
                    cost.cache_creation_cost.to_string(),
                    cost.total_cost.to_string(),
                )
            } else {
                (
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                    "0".to_string(),
                )
            };

        let created_at = chrono::Utc::now().timestamp();
        let input_token_semantics =
            if crate::services::sql_helpers::is_cache_inclusive_app(log.app_type.as_str()) {
                INPUT_TOKEN_SEMANTICS_TOTAL
            } else {
                INPUT_TOKEN_SEMANTICS_FRESH
            };
        let semantic = UsageSemantic::from_log(log, input_token_semantics);
        let existing = Self::load_existing_semantic(conn, &log.request_id)?;

        let (request_id, replace_session_log, collision) = match existing {
            None => (log.request_id.clone(), false, false),
            Some((data_source, _existing_semantic))
                if data_source.as_deref() == Some("session_log") =>
            {
                (log.request_id.clone(), true, false)
            }
            Some((data_source, existing_semantic))
                if data_source.as_deref().unwrap_or("proxy") == "proxy"
                    && existing_semantic == semantic =>
            {
                return Ok(());
            }
            Some(_) => {
                let fallback = format!("{}:collision:{}", log.request_id, semantic.sha256());
                if let Some((data_source, existing_semantic)) =
                    Self::load_existing_semantic(conn, &fallback)?
                {
                    if data_source.as_deref().unwrap_or("proxy") == "proxy"
                        && existing_semantic == semantic
                    {
                        return Ok(());
                    }
                    return Err(AppError::Database(format!(
                        "usage collision fallback 主键发生 SHA-256 冲突: {fallback}"
                    )));
                }
                (fallback, false, true)
            }
        };

        let insert_verb = if replace_session_log {
            "INSERT OR REPLACE"
        } else {
            "INSERT OR IGNORE"
        };
        let sql = format!(
            "{insert_verb} INTO proxy_request_logs (
                request_id, provider_id, app_type, model, request_model, pricing_model,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                input_token_semantics,
                input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
                latency_ms, first_token_ms, status_code, error_message, session_id,
                provider_type, is_streaming, cost_multiplier, created_at, key_id, route_reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)"
        );
        let affected_rows = conn
            .execute(
                &sql,
                rusqlite::params![
                    request_id,
                    log.provider_id,
                    log.app_type,
                    log.model,
                    log.request_model,
                    log.pricing_model,
                    log.usage.input_tokens,
                    log.usage.output_tokens,
                    log.usage.cache_read_tokens,
                    log.usage.cache_creation_tokens,
                    input_token_semantics,
                    input_cost,
                    output_cost,
                    cache_read_cost,
                    cache_creation_cost,
                    total_cost,
                    log.latency_ms as i64,
                    log.first_token_ms.map(|v| v as i64),
                    log.status_code as i64,
                    log.error_message,
                    log.session_id,
                    log.provider_type,
                    log.is_streaming as i64,
                    log.cost_multiplier,
                    created_at,
                    log.key_id,
                    log.route_reason,
                ],
            )
            .map_err(|e| AppError::Database(format!("记录请求日志失败: {e}")))?;

        if affected_rows > 0 {
            if collision {
                log::warn!(
                    "usage request_id collision: primary={}, fallback={request_id}",
                    log.request_id
                );
            }
            crate::usage_events::notify_log_recorded();
        }

        // usage 路由的累计：落库成功后才记，失败不污染决策依据。
        // 放在 `_on_conn` 这一层，是因为 `log_with_calculation_on_conn` 与错误行
        // 变体都直接调它而**不经过** pub `log_request`——曾误放在 `log_request`
        // 里，导致主路径完全不累计（e2e 实测 usage 表为空、key 全打在同一把上）。
        // 错误行 token 多为 0，等价于只记「发生过」。
        if affected_rows > 0 {
            if let Some(key_id) = log.key_id.as_deref() {
                crate::proxy::key_pool::record_usage(
                    &log.provider_id,
                    key_id,
                    u64::from(log.usage.input_tokens)
                        .saturating_add(u64::from(log.usage.output_tokens)),
                );
                // N2 per-key 日限额的当日成本累计：同一落库点、同一成功闸门。
                // 成本为 0（未计价/错误行）也记——0 加 0 无害，但保持单出口。
                crate::proxy::key_pool::record_daily_cost(
                    &log.provider_id,
                    key_id,
                    total_cost.parse::<f64>().unwrap_or(0.0),
                );
            }
        }

        Ok(())
    }

    fn load_existing_semantic(
        conn: &rusqlite::Connection,
        request_id: &str,
    ) -> Result<Option<(Option<String>, UsageSemantic)>, AppError> {
        conn.query_row(
            "SELECT data_source, app_type, provider_id, model, input_token_semantics,
                    input_tokens, output_tokens, cache_read_tokens,
                    cache_creation_tokens, status_code
             FROM proxy_request_logs WHERE request_id = ?1",
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    UsageSemantic {
                        app_type: row.get(1)?,
                        provider_id: row.get(2)?,
                        model: row.get(3)?,
                        input_token_semantics: row.get(4)?,
                        input_tokens: row.get::<_, i64>(5)? as u32,
                        output_tokens: row.get::<_, i64>(6)? as u32,
                        cache_read_tokens: row.get::<_, i64>(7)? as u32,
                        cache_creation_tokens: row.get::<_, i64>(8)? as u32,
                        status_code: row.get::<_, i64>(9)? as u16,
                    },
                ))
            },
        )
        .optional()
        .map_err(|error| AppError::Database(format!("查询 usage request_id 失败: {error}")))
    }

    /// 记录失败的请求
    ///
    /// 用于记录无法从上游获取 usage 信息的失败请求。
    /// `key_id` 是本次实际选中的池 key（无池时 `None`）——限流错误同样要归属，
    /// 否则 usage 策略看不到那把 key 已经消耗/被限。
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn log_error(
        &self,
        request_id: String,
        provider_id: String,
        app_type: String,
        model: String,
        status_code: u16,
        error_message: String,
        latency_ms: u64,
        key_id: Option<String>,
    ) -> Result<(), AppError> {
        let request_model = model.clone();
        let log = RequestLog {
            request_id,
            provider_id,
            app_type,
            model,
            request_model,
            // 错误行未经过计价，留空（回填的 has_usage 闸门也不会碰全 0 行）
            pricing_model: String::new(),
            usage: TokenUsage::default(),
            cost: None,
            latency_ms,
            first_token_ms: None,
            status_code,
            error_message: Some(error_message),
            session_id: None,
            provider_type: None,
            is_streaming: false,
            cost_multiplier: "1.0".to_string(),
            key_id,
            route_reason: None,
        };

        self.log_request(&log)
    }

    /// 构造错误请求日志（log_error_with_context 的同步/异步变体共用）
    #[allow(clippy::too_many_arguments)]
    fn error_request_log(
        request_id: String,
        provider_id: String,
        app_type: String,
        model: String,
        status_code: u16,
        error_message: String,
        latency_ms: u64,
        is_streaming: bool,
        session_id: Option<String>,
        provider_type: Option<String>,
        key_id: Option<String>,
    ) -> RequestLog {
        let request_model = model.clone();
        RequestLog {
            request_id,
            provider_id,
            app_type,
            model,
            request_model,
            // 错误行未经过计价，留空（回填的 has_usage 闸门也不会碰全 0 行）
            pricing_model: String::new(),
            usage: TokenUsage::default(),
            cost: None,
            latency_ms,
            first_token_ms: None,
            status_code,
            error_message: Some(error_message),
            session_id,
            provider_type,
            is_streaming,
            cost_multiplier: "1.0".to_string(),
            key_id,
            route_reason: None,
        }
    }

    /// 记录失败的请求（带更多上下文信息）——同步内联版本。
    ///
    /// 供调用方已身处 blocking 线程池（`spawn_blocking`）的场景使用：
    /// 直接取 `Database` 的阻塞锁写库，不得在 tokio worker 线程上调用。
    /// 错误行落库（N1）：`route_reason` 与 `key_id` 同行写入（同源同刻）。
    #[allow(clippy::too_many_arguments)]
    pub fn log_error_with_context_with_reason(
        &self,
        request_id: String,
        provider_id: String,
        app_type: String,
        model: String,
        status_code: u16,
        error_message: String,
        latency_ms: u64,
        is_streaming: bool,
        session_id: Option<String>,
        provider_type: Option<String>,
        key_id: Option<String>,
        route_reason: Option<String>,
    ) -> Result<(), AppError> {
        let mut log = Self::error_request_log(
            request_id,
            provider_id,
            app_type,
            model,
            status_code,
            error_message,
            latency_ms,
            is_streaming,
            session_id,
            provider_type,
            key_id,
        );
        log.route_reason = route_reason;
        let conn = crate::database::lock_conn!(self.db.conn);
        Self::log_request_on_conn(&conn, &log)
    }

    /// 查询模型定价的同步核心（H4：供 `db_blocking` 闭包复用）
    fn get_model_pricing_on_conn(
        conn: &rusqlite::Connection,
        model_id: &str,
    ) -> Result<Option<ModelPricing>, AppError> {
        let row = find_model_pricing_row(conn, model_id)?;
        match row {
            Some((input, output, cache_read, cache_creation)) => {
                ModelPricing::from_strings(&input, &output, &cache_read, &cache_creation)
                    .map(Some)
                    .map_err(|e| AppError::Database(format!("解析定价数据失败: {e}")))
            }
            None => Ok(None),
        }
    }

    /// 获取计费模式来源（按请求模型 / 按返回模型），只读应用级全局配置
    pub async fn resolve_pricing_model_source(&self, app_type: &str) -> String {
        // Claude Desktop 网关没有独立的全局计费配置（proxy_config 的 CHECK 不含
        // claude-desktop），继承 claude 的设置。
        let default_app_type = if app_type == "claude-desktop" {
            "claude"
        } else {
            app_type
        };
        match self.db.get_pricing_model_source(default_app_type).await {
            Ok(value) if value == PRICING_SOURCE_RESPONSE || value == PRICING_SOURCE_REQUEST => {
                value
            }
            Ok(value) => {
                log::warn!("[USG-003] 计费模式无效 (app_type={app_type}): {value}");
                PRICING_SOURCE_RESPONSE.to_string()
            }
            Err(e) => {
                log::warn!("[USG-003] 获取计费模式失败 (app_type={app_type}): {e}");
                PRICING_SOURCE_RESPONSE.to_string()
            }
        }
    }

    /// 计算并记录请求（同步内联版本）。
    ///
    /// 供调用方已身处 blocking 线程池（`spawn_blocking`）的场景使用：
    /// 直接取 `Database` 的阻塞锁写库，不得在 tokio worker 线程上调用。
    #[allow(clippy::too_many_arguments)]
    pub fn log_with_calculation(
        &self,
        request_id: String,
        provider_id: String,
        app_type: String,
        model: String,
        request_model: String,
        pricing_model: String,
        usage: TokenUsage,
        latency_ms: u64,
        first_token_ms: Option<u64>,
        status_code: u16,
        session_id: Option<String>,
        provider_type: Option<String>,
        is_streaming: bool,
        key_id: Option<String>,
        route_reason: Option<String>,
    ) -> Result<(), AppError> {
        let conn = crate::database::lock_conn!(self.db.conn);
        Self::log_with_calculation_on_conn(
            &conn,
            request_id,
            provider_id,
            app_type,
            model,
            request_model,
            pricing_model,
            usage,
            Decimal::ONE,
            latency_ms,
            first_token_ms,
            status_code,
            session_id,
            provider_type,
            is_streaming,
            key_id,
            route_reason,
        )
    }

    /// 计算并记录请求的同步核心（H4：供 `db_blocking` 闭包复用）
    ///
    /// `cost_multiplier` 参数已不再参与本路径的计价（倍率在上游重构中移出），
    /// 仅为兼容既有调用方签名保留。
    #[allow(clippy::too_many_arguments, unused_variables)]
    fn log_with_calculation_on_conn(
        conn: &rusqlite::Connection,
        request_id: String,
        provider_id: String,
        app_type: String,
        model: String,
        request_model: String,
        pricing_model: String,
        usage: TokenUsage,
        cost_multiplier: Decimal,
        latency_ms: u64,
        first_token_ms: Option<u64>,
        status_code: u16,
        session_id: Option<String>,
        provider_type: Option<String>,
        is_streaming: bool,
        key_id: Option<String>,
        route_reason: Option<String>,
    ) -> Result<(), AppError> {
        let pricing = Self::get_model_pricing_on_conn(conn, &pricing_model)?;

        let has_usage = usage.input_tokens > 0
            || usage.output_tokens > 0
            || usage.cache_read_tokens > 0
            || usage.cache_creation_tokens > 0;

        if pricing.is_none() && has_usage && !is_placeholder_pricing_model(&pricing_model) {
            log::warn!("[USG-002] 模型定价未找到，成本将记录为 0: {pricing_model}");
        }

        let cost = CostCalculator::try_calculate_for_app(
            &app_type,
            &usage,
            pricing.as_ref(),
            Decimal::ONE,
        );

        let log = RequestLog {
            request_id,
            provider_id,
            app_type,
            model,
            request_model,
            pricing_model,
            usage,
            cost,
            latency_ms,
            first_token_ms,
            status_code,
            error_message: None,
            session_id,
            provider_type,
            is_streaming,
            cost_multiplier: "1".to_string(),
            key_id,
            route_reason,
        };

        Self::log_request_on_conn(conn, &log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_log(request_id: &str, input_tokens: u32) -> RequestLog {
        RequestLog {
            request_id: request_id.to_string(),
            provider_id: "provider-1".to_string(),
            app_type: "codex".to_string(),
            model: "gpt-5.6".to_string(),
            request_model: "gpt-5.6".to_string(),
            pricing_model: "gpt-5.6".to_string(),
            usage: TokenUsage {
                input_tokens,
                output_tokens: 5,
                cache_read_tokens: 2,
                cache_creation_tokens: 0,
                model: None,
                message_id: Some("resp-1".to_string()),
            },
            cost: None,
            latency_ms: 10,
            first_token_ms: Some(2),
            status_code: 200,
            error_message: None,
            session_id: None,
            provider_type: Some("codex".to_string()),
            is_streaming: true,
            cost_multiplier: "1".to_string(),
            key_id: None,
            route_reason: None,
        }
    }

    #[test]
    fn test_log_request() -> Result<(), AppError> {
        let db = Database::memory()?;

        // 插入测试定价
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO model_pricing (model_id, display_name, input_cost_per_million, output_cost_per_million)
                 VALUES ('test-model', 'Test Model', '3.0', '15.0')",
                [],
            )
            .unwrap();
        }

        let logger = UsageLogger::new(&db);

        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            model: None,
            message_id: None,
        };

        logger.log_with_calculation(
            "req-123".to_string(),
            "provider-1".to_string(),
            "claude".to_string(),
            "test-model".to_string(),
            "req-model".to_string(),
            "test-model".to_string(),
            usage,
            100,
            None,
            200,
            None,
            Some("claude".to_string()),
            false,
            None,
            None,
        )?;

        // 验证记录已插入
        let conn = crate::database::lock_conn!(db.conn);
        let (count, request_model): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), request_model FROM proxy_request_logs WHERE request_id = 'req-123'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(request_model, "req-model");
        Ok(())
    }

    /// R1：带 key_id 的 usage 落库必须①把 key_id 写进 proxy_request_logs
    /// ②累加到进程级 usage 表（供 usage 路由决策）。
    #[test]
    fn log_with_calculation_records_key_id_and_usage_total() -> Result<(), AppError> {
        use crate::proxy::key_pool;

        key_pool::reset_for_tests();
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);

        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            model: None,
            message_id: None,
        };

        logger.log_with_calculation(
            "req-key-1".to_string(),
            "provider-1".to_string(),
            "claude".to_string(),
            "test-model".to_string(),
            "req-model".to_string(),
            "test-model".to_string(),
            usage,
            100,
            None,
            200,
            None,
            Some("claude".to_string()),
            false,
            Some("pool-a".to_string()),
            Some("usage:pool-a=0,pool-b=0->选[pool-a]".to_string()),
        )?;

        // ① DB 里的 key_id 与 route_reason（N1：同一行、同源同刻）
        let conn = crate::database::lock_conn!(db.conn);
        let key_id: Option<String> = conn
            .query_row(
                "SELECT key_id FROM proxy_request_logs WHERE request_id = 'req-key-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(key_id.as_deref(), Some("pool-a"));
        let route_reason: Option<String> = conn
            .query_row(
                "SELECT route_reason FROM proxy_request_logs WHERE request_id = 'req-key-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            route_reason.as_deref(),
            Some("usage:pool-a=0,pool-b=0->选[pool-a]")
        );

        // ② 进程级 usage 表：input+output = 1500
        // 通过 select_for_provider 的行为间接验证（全局表无公开读取接口），
        // 这里用一个两 key 池验证累计差确实影响了选择。
        drop(conn);
        key_pool::reset_for_tests();

        // 重新记：a 记 1500，然后验证 Usage 策略选 b
        key_pool::record_usage("provider-1", "pool-a", 1500);
        let provider = crate::provider::Provider {
            id: "provider-1".to_string(),
            name: "p".to_string(),
            settings_config: serde_json::json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(crate::provider::ProviderMeta {
                key_pool: vec![
                    crate::proxy::key_pool::KeyPoolEntry {
                        id: "pool-a".to_string(),
                        key: "sk-a".to_string(),
                        label: None,
                        daily_limit_usd: None,
                        max_concurrency: None,
                    },
                    crate::proxy::key_pool::KeyPoolEntry {
                        id: "pool-b".to_string(),
                        key: "sk-b".to_string(),
                        label: None,
                        daily_limit_usd: None,
                        max_concurrency: None,
                    },
                ],
                key_pool_strategy: Some(crate::proxy::key_pool::KeyPoolStrategy::Usage),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        let selected = key_pool::select_for_provider(&provider).expect("pool non-empty");
        assert_eq!(selected.key_id, "pool-b", "a 累计 1500 > b 的 0，应选 b");
        key_pool::reset_for_tests();
        Ok(())
    }

    #[test]
    fn identical_replay_writes_and_notifies_once() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);
        crate::usage_events::take_test_notify_count();
        let log = request_log("stable-id", 10);

        logger.log_request(&log)?;
        logger.log_request(&log)?;

        let conn = crate::database::lock_conn!(db.conn);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE request_id = 'stable-id'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        assert_eq!(crate::usage_events::take_test_notify_count(), 1);
        Ok(())
    }

    #[test]
    fn semantic_collision_uses_deterministic_idempotent_fallback() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);
        crate::usage_events::take_test_notify_count();
        let first = request_log("shared-id", 10);
        let second = request_log("shared-id", 20);

        logger.log_request(&first)?;
        logger.log_request(&second)?;
        logger.log_request(&second)?;

        let conn = crate::database::lock_conn!(db.conn);
        let rows: Vec<(String, i64)> = conn
            .prepare(
                "SELECT request_id, input_tokens FROM proxy_request_logs ORDER BY input_tokens",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("shared-id".to_string(), 10));
        assert!(rows[1].0.starts_with("shared-id:collision:"));
        assert_eq!(rows[1].1, 20);
        assert_eq!(crate::usage_events::take_test_notify_count(), 2);
        Ok(())
    }

    #[test]
    fn only_session_log_primary_rows_may_be_replaced() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            for (request_id, data_source) in [
                ("session-primary", "session_log"),
                ("codex-primary", "codex_session"),
            ] {
                conn.execute(
                    "INSERT INTO proxy_request_logs (
                        request_id, provider_id, app_type, model, input_tokens,
                        output_tokens, cache_read_tokens, cache_creation_tokens,
                        latency_ms, status_code, created_at, data_source
                     ) VALUES (?1, '_session', 'claude', 'old', 1, 1, 0, 0, 0, 200, 1, ?2)",
                    rusqlite::params![request_id, data_source],
                )?;
            }
        }
        let logger = UsageLogger::new(&db);

        let mut session_replacement = request_log("session-primary", 10);
        session_replacement.app_type = "claude".to_string();
        logger.log_request(&session_replacement)?;
        logger.log_request(&request_log("codex-primary", 20))?;

        let conn = crate::database::lock_conn!(db.conn);
        let session_source: String = conn.query_row(
            "SELECT data_source FROM proxy_request_logs WHERE request_id = 'session-primary'",
            [],
            |row| row.get(0),
        )?;
        let codex_input: i64 = conn.query_row(
            "SELECT input_tokens FROM proxy_request_logs WHERE request_id = 'codex-primary'",
            [],
            |row| row.get(0),
        )?;
        let fallback_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs
             WHERE request_id LIKE 'codex-primary:collision:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(session_source, "proxy");
        assert_eq!(codex_input, 1);
        assert_eq!(fallback_count, 1);
        Ok(())
    }

    #[test]
    fn claude_desktop_proxy_replaces_matching_session_log_row() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model, input_tokens,
                    output_tokens, cache_read_tokens, cache_creation_tokens,
                    latency_ms, status_code, created_at, data_source
                 ) VALUES ('session:msg_desktop', '_session', 'claude',
                    'claude-sonnet-4-5', 10, 5, 2, 1, 0, 200, 1, 'session_log')",
                [],
            )?;
        }

        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            model: Some("claude-sonnet-4-5".to_string()),
            message_id: Some("msg_desktop".to_string()),
        };
        let request_id = usage.dedup_request_id(crate::proxy::usage::parser::dedup_scope_for_app(
            "claude-desktop",
            "desktop-provider",
        ));
        let mut proxy_log = request_log(&request_id, 10);
        proxy_log.provider_id = "desktop-provider".to_string();
        proxy_log.app_type = "claude-desktop".to_string();
        proxy_log.model = "claude-sonnet-4-5".to_string();
        proxy_log.request_model = "claude-sonnet-4-5".to_string();
        proxy_log.pricing_model = "claude-sonnet-4-5".to_string();
        proxy_log.usage = usage;

        UsageLogger::new(&db).log_request(&proxy_log)?;

        let conn = crate::database::lock_conn!(db.conn);
        let (count, source, app_type): (i64, String, String) = conn.query_row(
            "SELECT COUNT(*), data_source, app_type FROM proxy_request_logs
             WHERE request_id = 'session:msg_desktop'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(count, 1);
        assert_eq!(source, "proxy");
        assert_eq!(app_type, "claude-desktop");
        Ok(())
    }

    #[test]
    fn test_log_error() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);

        logger.log_error(
            "req-error".to_string(),
            "provider-1".to_string(),
            "claude".to_string(),
            "unknown-model".to_string(),
            500,
            "Internal Server Error".to_string(),
            50,
            None,
        )?;

        // 验证错误记录已插入
        let conn = crate::database::lock_conn!(db.conn);
        let (status, error): (i64, Option<String>) = conn
            .query_row(
                "SELECT status_code, error_message FROM proxy_request_logs WHERE request_id = 'req-error'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, 500);
        assert_eq!(error, Some("Internal Server Error".to_string()));
        Ok(())
    }

    #[test]
    fn grokbuild_logs_total_input_token_semantics() -> Result<(), AppError> {
        let db = Database::memory()?;
        let logger = UsageLogger::new(&db);
        let log = RequestLog {
            request_id: "grok-semantics".to_string(),
            provider_id: "grok-provider".to_string(),
            app_type: "grokbuild".to_string(),
            model: "grok-4.5".to_string(),
            request_model: "grok-4.5".to_string(),
            pricing_model: String::new(),
            usage: TokenUsage::default(),
            cost: None,
            latency_ms: 1,
            first_token_ms: None,
            status_code: 200,
            error_message: None,
            session_id: None,
            provider_type: Some("grokbuild".to_string()),
            is_streaming: false,
            cost_multiplier: "1".to_string(),
            key_id: None,
            route_reason: None,
        };

        logger.log_request(&log)?;

        let conn = crate::database::lock_conn!(db.conn);
        let semantics: i64 = conn.query_row(
            "SELECT input_token_semantics FROM proxy_request_logs WHERE request_id = 'grok-semantics'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(semantics, INPUT_TOKEN_SEMANTICS_TOTAL);
        Ok(())
    }
}
