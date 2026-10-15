//! HTTP代理服务器
//!
//! 基于Axum的HTTP服务器，处理代理请求
//!
//! Uses a manual hyper HTTP/1.1 accept loop with `preserve_header_case(true)` so
//! that the original header-name casing from the CLI client is captured in a
//! `HeaderCaseMap` extension.  This map is later forwarded to the upstream via
//! the hyper-based HTTP client, producing wire-level header casing identical to
//! a direct (non-proxied) CLI request.

use super::{
    failover_switch::FailoverSwitchManager,
    handlers,
    log_codes::srv as log_srv,
    provider_router::ProviderRouter,
    providers::{codex_chat_history::CodexChatHistoryStore, gemini_shadow::GeminiShadowStore},
    types::*,
    ProxyError,
};
use crate::database::Database;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;

/// 代理服务器状态（共享）
#[derive(Clone)]
pub struct ProxyState {
    pub db: Arc<Database>,
    pub config: Arc<RwLock<ProxyConfig>>,
    pub status: Arc<RwLock<ProxyStatus>>,
    pub start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// 每个应用类型当前使用的 provider (app_type -> (provider_id, provider_name))
    pub current_providers: Arc<RwLock<std::collections::HashMap<String, (String, String)>>>,
    /// 共享的 ProviderRouter（持有熔断器状态，跨请求保持）
    pub provider_router: Arc<ProviderRouter>,
    /// Gemini Native shadow state，用于 thoughtSignature / tool call 回放
    pub gemini_shadow: Arc<GeminiShadowStore>,
    /// Codex Chat bridge history，用于恢复 previous_response_id 指向的 tool call
    pub codex_chat_history: Arc<CodexChatHistoryStore>,
    /// AppHandle，用于发射事件和更新托盘菜单
    pub app_handle: Option<tauri::AppHandle>,
    /// 故障转移切换管理器
    pub failover_manager: Arc<FailoverSwitchManager>,
}

/// 代理HTTP服务器
pub struct ProxyServer {
    config: ProxyConfig,
    state: ProxyState,
    shutdown_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
    /// 服务器任务句柄，用于等待服务器实际关闭
    server_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

#[cfg(test)]
impl ProxyState {
    /// 测试用：用这个数据库、其余都是默认值的状态。
    pub(crate) fn for_test(db: Arc<Database>) -> Self {
        ProxyServer::new(ProxyConfig::default(), db, None).state
    }
}

impl ProxyServer {
    pub fn new(
        config: ProxyConfig,
        db: Arc<Database>,
        app_handle: Option<tauri::AppHandle>,
    ) -> Self {
        // 创建共享的 ProviderRouter（熔断器状态将跨所有请求保持）
        let provider_router = Arc::new(ProviderRouter::new(db.clone()));
        // 创建故障转移切换管理器
        let failover_manager = Arc::new(FailoverSwitchManager::new());

        let state = ProxyState {
            db,
            config: Arc::new(RwLock::new(config.clone())),
            status: Arc::new(RwLock::new(ProxyStatus::default())),
            start_time: Arc::new(RwLock::new(None)),
            current_providers: Arc::new(RwLock::new(std::collections::HashMap::new())),
            provider_router,
            gemini_shadow: Arc::new(GeminiShadowStore::default()),
            codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
            app_handle,
            failover_manager,
        };

        Self {
            config,
            state,
            shutdown_tx: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start(&self) -> Result<ProxyServerInfo, ProxyError> {
        // 检查是否已在运行
        if self.shutdown_tx.read().await.is_some() {
            return Err(ProxyError::AlreadyRunning);
        }

        let addr: SocketAddr =
            format!("{}:{}", self.config.listen_address, self.config.listen_port)
                .parse()
                .map_err(|e| ProxyError::BindFailed(format!("无效的地址: {e}")))?;

        // 创建关闭通道
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // 构建路由
        let app = self.build_router();

        // 绑定监听器
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| ProxyError::BindFailed(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ProxyError::BindFailed(e.to_string()))?;
        let actual_port = local_addr.port();

        log::info!("[{}] 代理服务器启动于 {local_addr}", log_srv::STARTED);

        // 更新全局代理端口，用于系统代理检测
        crate::proxy::http_client::set_proxy_port(actual_port);

        // 保存关闭句柄
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        // 更新状态
        let mut status = self.state.status.write().await;
        status.running = true;
        status.address = self.config.listen_address.clone();
        status.port = actual_port;
        drop(status);

        // 记录启动时间
        *self.state.start_time.write().await = Some(std::time::Instant::now());

        // 启动服务器 — 使用手动 hyper HTTP/1.1 accept loop
        // 开启 preserve_header_case 以捕获客户端请求头的原始大小写
        let state = self.state.clone();
        let handle = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _remote_addr) = match result {
                            Ok(v) => v,
                            Err(e) => {
                                log::error!("[{SRV}] accept 失败: {e}", SRV = log_srv::ACCEPT_ERR);
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                continue;
                            }
                        };

                        let app = app.clone();
                        tokio::spawn(async move {
                            // Peek raw TCP bytes to capture original header casing
                            // before hyper parses (and lowercases) the header names.
                            let original_cases = {
                                let mut peek_buf = vec![0u8; 8192];
                                match stream.peek(&mut peek_buf).await {
                                    Ok(n) => {
                                        let cases = super::hyper_client::OriginalHeaderCases::from_raw_bytes(&peek_buf[..n]);
                                        log::debug!(
                                            "[ProxyServer] Peeked {} bytes, captured {} header casings",
                                            n, cases.cases.len()
                                        );
                                        cases
                                    }
                                    Err(e) => {
                                        log::debug!("[ProxyServer] peek failed (non-fatal): {e}");
                                        super::hyper_client::OriginalHeaderCases::default()
                                    }
                                }
                            };

                            // service_fn 将 axum Router（tower::Service）桥接到 hyper
                            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                                let mut router = app.clone();
                                let cases = original_cases.clone();
                                async move {
                                    // 将 hyper::body::Incoming 转为 axum::body::Body，保留 extensions
                                    let (mut parts, body) = req.into_parts();

                                    // Insert our own header case map alongside hyper's internal one
                                    parts.extensions.insert(cases);

                                    let body = axum::body::Body::new(body);
                                    let axum_req = http::Request::from_parts(parts, body);
                                    <Router as tower::Service<http::Request<axum::body::Body>>>::call(&mut router, axum_req).await
                                }
                            });

                            if let Err(e) = hyper::server::conn::http1::Builder::new()
                                .preserve_header_case(true)
                                .serve_connection(TokioIo::new(stream), service)
                                .await
                            {
                                // Connection reset / broken pipe 等在代理场景下很常见，debug 级别
                                log::debug!("[{SRV}] connection error: {e}", SRV = log_srv::CONN_ERR);
                            }
                        });
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }

            // 服务器停止后更新状态
            state.status.write().await.running = false;
            *state.start_time.write().await = None;
        });

        // 保存服务器任务句柄
        *self.server_handle.write().await = Some(handle);

        Ok(ProxyServerInfo {
            address: self.config.listen_address.clone(),
            port: actual_port,
            started_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub async fn stop(&self) -> Result<(), ProxyError> {
        // 1. 发送关闭信号
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        } else {
            return Err(ProxyError::NotRunning);
        }

        // 2. 等待服务器任务结束（带 5 秒超时保护）
        if let Some(handle) = self.server_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    log::info!("[{}] 代理服务器已完全停止", log_srv::STOPPED);
                    Ok(())
                }
                Ok(Err(e)) => {
                    log::warn!("[{}] 代理服务器任务异常终止: {e}", log_srv::TASK_ERROR);
                    Err(ProxyError::StopFailed(e.to_string()))
                }
                Err(_) => {
                    log::warn!(
                        "[{}] 代理服务器停止超时（5秒），强制继续",
                        log_srv::STOP_TIMEOUT
                    );
                    Err(ProxyError::StopTimeout)
                }
            }
        } else {
            Ok(())
        }
    }

    pub async fn get_status(&self) -> ProxyStatus {
        let mut status = self.state.status.read().await.clone();

        // 计算运行时间
        if let Some(start) = *self.state.start_time.read().await {
            status.uptime_seconds = start.elapsed().as_secs();
        }

        // 从 current_providers HashMap 获取每个应用类型当前正在使用的 provider
        let current_providers = self.state.current_providers.read().await;
        status.active_targets = current_providers
            .iter()
            .map(|(app_type, (provider_id, provider_name))| ActiveTarget {
                app_type: app_type.clone(),
                provider_id: provider_id.clone(),
                provider_name: provider_name.clone(),
            })
            .collect();

        status
    }

    /// 更新某个应用类型当前“目标供应商”（用于 UI 展示 active_targets）
    ///
    /// 注意：这不代表该供应商一定已经处理过请求，而是用于“热切换/启用故障转移立即切 P1”
    /// 等场景下，让 UI 能立刻反映最新目标。
    pub async fn set_active_target(&self, app_type: &str, provider_id: &str, provider_name: &str) {
        let mut current_providers = self.state.current_providers.write().await;
        current_providers.insert(
            app_type.to_string(),
            (provider_id.to_string(), provider_name.to_string()),
        );
    }

    fn build_router(&self) -> Router {
        Router::new()
            // 健康检查
            .route("/health", get(handlers::health_check))
            .route("/status", get(handlers::get_status))
            // Claude API (支持带前缀和不带前缀两种格式)
            .route("/v1/messages", post(handlers::handle_messages))
            .route("/claude/v1/messages", post(handlers::handle_messages))
            // OpenAI Chat Completions API (Codex CLI，支持带前缀和不带前缀)
            .route("/chat/completions", post(handlers::handle_chat_completions))
            .route(
                "/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            .route(
                "/v1/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            .route(
                "/codex/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            // OpenAI Models API (Codex CLI reachability check)
            .route("/models", get(handlers::handle_models))
            .route("/v1/models", get(handlers::handle_models))
            // OpenAI Responses API (Codex CLI，支持带前缀和不带前缀)。GET 是 WebSocket
            // 握手：Codex 内置的 openai（代理的官方路由）会先试 WebSocket。
            .route(
                "/responses",
                post(handlers::handle_responses).get(handlers::handle_responses_websocket),
            )
            .route(
                "/v1/responses",
                post(handlers::handle_responses).get(handlers::handle_responses_websocket),
            )
            .route(
                "/v1/v1/responses",
                post(handlers::handle_responses).get(handlers::handle_responses_websocket),
            )
            .route(
                "/codex/v1/responses",
                post(handlers::handle_responses).get(handlers::handle_responses_websocket),
            )
            // OpenAI Responses Compact API (Codex CLI 远程压缩，透传)
            .route(
                "/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/v1/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/codex/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            // Codex standalone Alpha Search API. All local aliases normalize to
            // the selected provider's canonical sibling `/alpha/search` route.
            .route("/alpha/search", post(handlers::handle_alpha_search))
            .route("/v1/alpha/search", post(handlers::handle_alpha_search))
            .route("/v1/v1/alpha/search", post(handlers::handle_alpha_search))
            .route(
                "/codex/v1/alpha/search",
                post(handlers::handle_alpha_search),
            )
            // Codex built-in ImageGen still calls the legacy OpenAI Images API.
            .route(
                "/images/generations",
                post(handlers::handle_images_generations),
            )
            .route(
                "/v1/images/generations",
                post(handlers::handle_images_generations),
            )
            .route(
                "/v1/v1/images/generations",
                post(handlers::handle_images_generations),
            )
            .route(
                "/codex/v1/images/generations",
                post(handlers::handle_images_generations),
            )
            // Codex ImageGen posts to `/images/edits` when it references existing images.
            .route("/images/edits", post(handlers::handle_images_edits))
            .route("/v1/images/edits", post(handlers::handle_images_edits))
            .route("/v1/v1/images/edits", post(handlers::handle_images_edits))
            .route(
                "/codex/v1/images/edits",
                post(handlers::handle_images_edits),
            )
            // 提高默认请求体大小限制（避免 413 Payload Too Large）
            .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
            .with_state(self.state.clone())
    }

    /// 在不重启服务的情况下更新运行时配置
    pub async fn apply_runtime_config(&self, config: &ProxyConfig) {
        *self.state.config.write().await = config.clone();
    }

    /// 热更新熔断器配置
    ///
    /// 将新配置应用到所有已创建的熔断器实例
    pub async fn update_circuit_breaker_configs(
        &self,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state.provider_router.update_all_configs(config).await;
    }

    pub async fn update_circuit_breaker_config_for_app(
        &self,
        app_type: &str,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state
            .provider_router
            .update_app_configs(app_type, config)
            .await;
    }

    /// 重置指定 Provider 的熔断器
    pub async fn reset_provider_circuit_breaker(&self, provider_id: &str, app_type: &str) {
        self.state
            .provider_router
            .reset_provider_breaker(provider_id, app_type)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ProviderMeta};
    use crate::AppError;
    use axum::http::{header, HeaderMap, StatusCode};
    use serde_json::{json, Value};
    use tokio::sync::Mutex;

    #[derive(Debug)]
    struct CapturedRequest {
        path_and_query: String,
        authorization: Option<String>,
        body: Value,
    }

    /// P-C1 端到端：key 池 order 策略——上游对池 key `a` 返 429 后（触发冷却），
    /// 同一 provider 的下一个请求应携带池 key `b`。单 provider 无 failover 目标，
    /// 验证的是跨请求的池内切换。
    #[tokio::test]
    #[serial_test::serial(global_env)]
    async fn key_pool_order_switches_to_next_key_after_upstream_429() {
        use crate::proxy::key_pool::{self, KeyPoolEntry};

        key_pool::reset_for_tests();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let mock_app = Router::new().route(
            "/v1/messages",
            post({
                let seen = seen.clone();
                move |request: axum::extract::Request| {
                    let seen = seen.clone();
                    async move {
                        let (parts, _body) = request.into_parts();
                        let auth = parts
                            .headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        seen.lock().await.push(auth.clone());
                        if auth == "Bearer sk-pool-b" {
                            let mut headers = HeaderMap::new();
                            headers.insert(
                                header::CONTENT_TYPE,
                                "application/json".parse().expect("content type"),
                            );
                            (
                                StatusCode::OK,
                                headers,
                                r#"{"id":"msg_pool_ok","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-pool-test","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
                            )
                        } else {
                            let mut headers = HeaderMap::new();
                            headers.insert(
                                header::CONTENT_TYPE,
                                "application/json".parse().expect("content type"),
                            );
                            (
                                StatusCode::TOO_MANY_REQUESTS,
                                headers,
                                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
                            )
                        }
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider {
            id: "pooled-claude".to_string(),
            name: "Pooled Claude".to_string(),
            settings_config: json!({
                "env": {
                    "ANTHROPIC_BASE_URL": format!("http://{mock_addr}"),
                    "ANTHROPIC_AUTH_TOKEN": "sk-primary"
                }
            }),
            website_url: None,
            category: Some("claude".to_string()),
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                key_pool: vec![
                    KeyPoolEntry {
                        id: "a".to_string(),
                        key: "sk-pool-a".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                    KeyPoolEntry {
                        id: "b".to_string(),
                        key: "sk-pool-b".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                ],
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        db.save_provider("claude", &provider)
            .expect("save test provider");
        db.set_current_provider("claude", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: false,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let url = format!("http://127.0.0.1:{}/v1/messages", proxy_info.port);
        let body = json!({
            "model": "claude-pool-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        });

        // 请求 1：池 key a 被上游 429 → 透传 429，同时记冷却
        let first = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("first pooled request");
        assert_eq!(first.status(), StatusCode::TOO_MANY_REQUESTS);

        // 请求 2：a 处于冷却 → 应携带池 key b，拿到 200
        let second = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("second pooled request");
        assert_eq!(second.status(), StatusCode::OK);

        let seen = seen.lock().await;
        assert_eq!(
            seen.as_slice(),
            ["Bearer sk-pool-a", "Bearer sk-pool-b"],
            "上游看到的 Authorization 应先 a（429）后 b（200）"
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
        key_pool::reset_for_tests();
    }

    /// R2 端到端：上游 429 带 `Retry-After: 1` 时，冷却只用服务端给的 1s，
    /// 而不是按状态码兜底的 60s。
    ///
    /// 判据是「第三个请求回到 a」——若 Retry-After 没被采信，a 会冷却 60s，
    /// 第三个请求仍落在 b。这个测试因此能真正区分两种行为，而不是碰巧通过。
    #[tokio::test]
    #[serial_test::serial(global_env)]
    async fn key_pool_honours_upstream_retry_after_seconds() {
        use crate::proxy::key_pool::{self, KeyPoolEntry};

        key_pool::reset_for_tests();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let mock_app = Router::new().route(
            "/v1/messages",
            post({
                let seen = seen.clone();
                move |request: axum::extract::Request| {
                    let seen = seen.clone();
                    async move {
                        let (parts, _body) = request.into_parts();
                        let auth = parts
                            .headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        seen.lock().await.push(auth.clone());
                        if auth == "Bearer sk-pool-b" {
                            let mut headers = HeaderMap::new();
                            headers.insert(
                                header::CONTENT_TYPE,
                                "application/json".parse().expect("content type"),
                            );
                            (
                                StatusCode::OK,
                                headers,
                                r#"{"id":"msg_pool_ok","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-pool-test","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
                            )
                        } else {
                            let mut headers = HeaderMap::new();
                            headers.insert(
                                header::CONTENT_TYPE,
                                "application/json".parse().expect("content type"),
                            );
                            // R2：把服务端要求的冷却时长显式告知
                            headers.insert(
                                header::RETRY_AFTER,
                                "1".parse().expect("retry-after seconds"),
                            );
                            (
                                StatusCode::TOO_MANY_REQUESTS,
                                headers,
                                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
                            )
                        }
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider {
            id: "pooled-claude".to_string(),
            name: "Pooled Claude".to_string(),
            settings_config: json!({
                "env": {
                    "ANTHROPIC_BASE_URL": format!("http://{mock_addr}"),
                    "ANTHROPIC_AUTH_TOKEN": "sk-primary"
                }
            }),
            website_url: None,
            category: Some("claude".to_string()),
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                key_pool: vec![
                    KeyPoolEntry {
                        id: "a".to_string(),
                        key: "sk-pool-a".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                    KeyPoolEntry {
                        id: "b".to_string(),
                        key: "sk-pool-b".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                ],
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        db.save_provider("claude", &provider)
            .expect("save test provider");
        db.set_current_provider("claude", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: false,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let url = format!("http://127.0.0.1:{}/v1/messages", proxy_info.port);
        let body = json!({
            "model": "claude-pool-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        });

        // 请求 1：a 被上游 429 且带了 Retry-After: 1 → 记 1s 冷却
        let first = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("first pooled request");
        assert_eq!(first.status(), StatusCode::TOO_MANY_REQUESTS);

        // 请求 2：a 仍在 1s 冷却内 → 落到 b
        let second = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("second pooled request");
        assert_eq!(second.status(), StatusCode::OK);

        // 等过服务端要求的 1s（略多留余量给调度抖动）
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // 请求 3：冷却已过 → a 重新可选。
        // mock 对 a 恒返 429，所以这里**期望 429**——那正说明请求打的是 a。
        // 若 Retry-After 未被采信，a 会冷却 60s，第三个请求将落到 b（200）。
        let third = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("third pooled request");
        assert_eq!(
            third.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "第三个请求应重新选中 a（mock 对 a 恒返 429）"
        );

        let seen = seen.lock().await;
        assert_eq!(
            seen.as_slice(),
            ["Bearer sk-pool-a", "Bearer sk-pool-b", "Bearer sk-pool-a"],
            "Retry-After: 1 到期后应重新选中 a"
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
        key_pool::reset_for_tests();
    }

    /// R1 端到端：usage 路由策略——按各 key 的实际 token 消耗选累计更少的那把。
    ///
    /// 判据是 Authorization 序列 `[a, b, b]`：
    /// 1. 初始两把都 0 → 选 a；mock 对 a 返 **大 usage**（5000 input）
    /// 2. a 累计 5000 > b 的 0 → 选 b；mock 对 b 返小 usage（1 input）
    /// 3. a 仍 5000 > b 的 1 → 继续选 b
    ///
    /// 若 usage 未被采信（例如退化成 order），序列会变成 [a, b, a] 或
    /// [a, a, ...]，本测试因此能真正区分两种行为。
    #[tokio::test]
    #[serial_test::serial(global_env)]
    async fn key_pool_usage_prefers_least_consumed_key() {
        use crate::proxy::key_pool::{self, KeyPoolEntry};

        key_pool::reset_for_tests();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let mock_app = Router::new().route(
            "/v1/messages",
            post({
                let seen = seen.clone();
                move |request: axum::extract::Request| {
                    let seen = seen.clone();
                    async move {
                        let (parts, _body) = request.into_parts();
                        let auth = parts
                            .headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        seen.lock().await.push(auth.clone());
                        // a 返大 usage，b 返小 usage——制造消耗差。
                        // b 的 input 逐次 +1：规避 UsageSemantic 去重
                        // （同 provider/model/tokens/status 的重复行会被
                        // INSERT OR IGNORE 静默合并，第 2、3 次会变成一行）。
                        let usage = if auth == "Bearer sk-pool-b" {
                            let n = {
                                let seen = seen.clone();
                                let seen = seen.lock().await;
                                seen.len()
                            };
                            format!(r#""usage":{{"input_tokens":{n},"output_tokens":1}}"#)
                        } else {
                            r#""usage":{"input_tokens":5000,"output_tokens":1}"#.to_string()
                        };
                        let mut headers = HeaderMap::new();
                        headers.insert(
                            header::CONTENT_TYPE,
                            "application/json".parse().expect("content type"),
                        );
                        (
                            StatusCode::OK,
                            headers,
                            format!(r#"{{"id":"msg_usage","type":"message","role":"assistant","content":[{{"type":"text","text":"ok"}}],"model":"claude-pool-test","stop_reason":"end_turn",{usage}}}"#),
                        )
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider {
            id: "pooled-claude".to_string(),
            name: "Pooled Claude".to_string(),
            settings_config: json!({
                "env": {
                    "ANTHROPIC_BASE_URL": format!("http://{mock_addr}"),
                    "ANTHROPIC_AUTH_TOKEN": "sk-primary"
                }
            }),
            website_url: None,
            category: Some("claude".to_string()),
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                key_pool: vec![
                    KeyPoolEntry {
                        id: "a".to_string(),
                        key: "sk-pool-a".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                    KeyPoolEntry {
                        id: "b".to_string(),
                        key: "sk-pool-b".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                ],
                key_pool_strategy: Some(crate::proxy::key_pool::KeyPoolStrategy::Usage),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        db.save_provider("claude", &provider)
            .expect("save test provider");
        db.set_current_provider("claude", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                // 必须开：usage 落库（→ record_usage）是 usage 路由的数据来源。
                // P-C 的两个 e2e 不需要 usage 数据故设 false；本测试设 true，
                // 与 1318 行那个验证 usage 落库的既有 e2e 一致。
                enable_logging: true,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let url = format!("http://127.0.0.1:{}/v1/messages", proxy_info.port);
        let body = json!({
            "model": "claude-pool-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        });

        // 三个请求：a（大 usage）→ b（小 usage）→ b（a 累计仍更多）
        for expected in [200u16, 200, 200] {
            let resp = client
                .post(&url)
                .header(header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
                .expect("pooled usage request");
            assert_eq!(resp.status(), expected);
            // usage 落库是 tokio::spawn 异步的，且要拿 DB 阻塞锁做磁盘 IO。
            // 固定 sleep 不可靠（本机实测 10ms 不够），改为轮询等行数达标。
            let want = {
                let conn = db.conn.lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM proxy_request_logs WHERE provider_id = 'pooled-claude'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
            } + 1;
            for _ in 0..100 {
                let got = {
                    let conn = db.conn.lock().unwrap();
                    conn.query_row(
                        "SELECT COUNT(*) FROM proxy_request_logs WHERE provider_id = 'pooled-claude'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                };
                if got >= want {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }

        let seen = seen.lock().await;
        assert_eq!(
            seen.as_slice(),
            ["Bearer sk-pool-a", "Bearer sk-pool-b", "Bearer sk-pool-b"],
            "usage 策略应按累计消耗选 key：a 打满 5000 后转 b 并保持"
        );

        // key_id 归属必须逐行正确：第一行 a，后两行 b。
        // 这一条专门守 `peek_last_used` 的 Usage 分支——它曾用空表预览，
        // 导致「实际用 b、归属写 a」，usage 全记到一把 key 头上。
        let key_ids: Vec<Option<String>> = {
            let conn = db.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT key_id FROM proxy_request_logs ORDER BY rowid")
                .expect("prepare key_id query");
            let rows = stmt
                .query_map([], |row| row.get::<_, Option<String>>(0))
                .expect("query key_ids");
            rows.collect::<Result<_, _>>().expect("collect key_ids")
        };
        assert_eq!(
            key_ids,
            vec![
                Some("a".to_string()),
                Some("b".to_string()),
                Some("b".to_string())
            ],
            "key_id 归属应与实际使用的 key 一致"
        );

        // N1：决策理由与 key_id 同行落库，且能解释选择（usage 场景下含累计值）。
        let reasons: Vec<Option<String>> = {
            let conn = db.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT route_reason FROM proxy_request_logs ORDER BY rowid")
                .expect("prepare route_reason query");
            let rows = stmt
                .query_map([], |row| row.get::<_, Option<String>>(0))
                .expect("query route_reasons");
            rows.collect::<Result<_, _>>()
                .expect("collect route_reasons")
        };
        for (i, reason) in reasons.iter().enumerate() {
            let reason = reason.as_deref().expect("每行都应有决策理由");
            assert!(
                reason.starts_with("usage:") && reason.contains("->选["),
                "第 {i} 行理由应是 usage 决策格式: {reason}"
            );
        }
        assert!(
            reasons[1].as_deref().unwrap_or("").contains("->选[b]"),
            "第二行应解释选了 b: {:?}",
            reasons[1]
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
        key_pool::reset_for_tests();
    }

    /// R8 端到端：上游成功响应带 95% 压力头 → 下一请求应避开该 key（软冷却）。
    #[tokio::test]
    #[serial_test::serial(global_env)]
    async fn key_pool_upstream_pressure_header_avoids_hot_key() {
        use crate::proxy::key_pool::{self, KeyPoolEntry};

        key_pool::reset_for_tests();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let mock_app = Router::new().route(
            "/v1/messages",
            post({
                let seen = seen.clone();
                move |request: axum::extract::Request| {
                    let seen = seen.clone();
                    async move {
                        let (parts, _body) = request.into_parts();
                        let auth = parts
                            .headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        seen.lock().await.push(auth.clone());
                        let mut headers = HeaderMap::new();
                        headers.insert(
                            header::CONTENT_TYPE,
                            "application/json".parse().expect("content type"),
                        );
                        // a 的成功响应报 95% 已用——R8 应让下一请求避开 a
                        if auth == "Bearer sk-pool-a" {
                            headers.insert(
                                "x-codex-primary-used-percent",
                                "95".parse().expect("pressure header"),
                            );
                        }
                        (
                            StatusCode::OK,
                            headers,
                            r#"{"id":"msg_r8","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-pool-test","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
                        )
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("in-memory db"));
        let provider = Provider {
            id: "pooled-claude".to_string(),
            name: "Pooled Claude".to_string(),
            settings_config: serde_json::json!({
                "env": {
                    "ANTHROPIC_BASE_URL": format!("http://{mock_addr}"),
                    "ANTHROPIC_AUTH_TOKEN": "unused"
                }
            }),
            website_url: None,
            category: Some("claude".to_string()),
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                key_pool: vec![
                    KeyPoolEntry {
                        id: "a".to_string(),
                        key: "sk-pool-a".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                    KeyPoolEntry {
                        id: "b".to_string(),
                        key: "sk-pool-b".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                ],
                // order：无 usage/限额干扰，压力软冷却是唯一变量
                key_pool_strategy: Some(crate::proxy::key_pool::KeyPoolStrategy::Order),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        db.save_provider("claude", &provider)
            .expect("save test provider");
        db.set_current_provider("claude", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: true,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let url = format!("http://127.0.0.1:{}/v1/messages", proxy_info.port);
        let body = json!({
            "model": "claude-pool-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        });

        // 请求 1：order 起手选 a，200 + 95% 压力头
        let first = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("first pressure request");
        assert_eq!(first.status(), StatusCode::OK);

        // 请求 2：a 因压力头被软冷却 → 应选 b
        let second = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .expect("second pressure request");
        assert_eq!(second.status(), StatusCode::OK);

        assert_eq!(
            seen.lock().await.as_slice(),
            ["Bearer sk-pool-a", "Bearer sk-pool-b"],
            "R8：压力头 95% 应让下一请求避开 a 改用 b"
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
        key_pool::reset_for_tests();
    }

    /// P-C2 端到端：key 池 rotate 策略——连续三个请求应按 a → b → a 轮转。
    #[tokio::test]
    #[serial_test::serial(global_env)]
    async fn key_pool_rotate_cycles_keys_across_requests() {
        use crate::proxy::key_pool::{self, KeyPoolEntry};

        key_pool::reset_for_tests();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let mock_app = Router::new().route(
            "/v1/messages",
            post({
                let seen = seen.clone();
                move |request: axum::extract::Request| {
                    let seen = seen.clone();
                    async move {
                        let (parts, _body) = request.into_parts();
                        let auth = parts
                            .headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        seen.lock().await.push(auth);
                        let mut headers = HeaderMap::new();
                        headers.insert(
                            header::CONTENT_TYPE,
                            "application/json".parse().expect("content type"),
                        );
                        (
                            StatusCode::OK,
                            headers,
                            r#"{"id":"msg_rot_ok","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-rot-test","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
                        )
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider {
            id: "rotating-claude".to_string(),
            name: "Rotating Claude".to_string(),
            settings_config: json!({
                "env": {
                    "ANTHROPIC_BASE_URL": format!("http://{mock_addr}"),
                    "ANTHROPIC_AUTH_TOKEN": "sk-primary"
                }
            }),
            website_url: None,
            category: Some("claude".to_string()),
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                key_pool: vec![
                    KeyPoolEntry {
                        id: "a".to_string(),
                        key: "sk-rot-a".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                    KeyPoolEntry {
                        id: "b".to_string(),
                        key: "sk-rot-b".to_string(),
                        label: None,
                        daily_limit_usd: None,
                    },
                ],
                key_pool_strategy: Some(crate::proxy::key_pool::KeyPoolStrategy::Rotate),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        db.save_provider("claude", &provider)
            .expect("save test provider");
        db.set_current_provider("claude", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: false,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let url = format!("http://127.0.0.1:{}/v1/messages", proxy_info.port);
        let body = json!({
            "model": "claude-rot-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        });

        for _ in 0..3 {
            let response = client
                .post(&url)
                .header(header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
                .expect("rotating pooled request");
            assert_eq!(response.status(), StatusCode::OK);
        }

        let seen = seen.lock().await;
        assert_eq!(
            seen.as_slice(),
            ["Bearer sk-rot-a", "Bearer sk-rot-b", "Bearer sk-rot-a"],
            "rotate 策略下三个请求应轮转 a → b → a"
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
        key_pool::reset_for_tests();
    }

    /// Codex 内置的 openai 先用 WebSocket 连 Responses：握手回 426 它才立刻改走 HTTP。
    #[tokio::test]
    async fn responses_websocket_handshake_is_answered_with_upgrade_required() {
        let db = Arc::new(Database::memory().expect("memory database"));
        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..ProxyConfig::default()
            },
            db,
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        for path in [
            "/responses",
            "/v1/responses",
            "/v1/v1/responses",
            "/codex/v1/responses",
        ] {
            let response = client
                .get(format!("http://127.0.0.1:{}{path}", proxy_info.port))
                .header(header::CONNECTION, "Upgrade")
                .header(header::UPGRADE, "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .send()
                .await
                .expect("websocket handshake");
            assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED, "{path}");
        }
        proxy.stop().await.expect("stop test proxy");
    }

    /// A base URL pasted as a complete endpoint with the full-URL switch left off
    /// must derive the sibling standalone endpoint instead of having the
    /// standalone path appended to it.
    #[tokio::test]
    async fn codex_standalone_endpoints_derive_from_pasted_full_base_url() {
        let captured = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
        let capture_handler = {
            let captured = captured.clone();
            move |request: axum::extract::Request| {
                let captured = captured.clone();
                async move {
                    let (parts, _body) = request.into_parts();
                    captured.lock().await.push(CapturedRequest {
                        path_and_query: parts
                            .uri
                            .path_and_query()
                            .map(|value| value.as_str().to_string())
                            .unwrap_or_else(|| parts.uri.path().to_string()),
                        authorization: parts
                            .headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(ToString::to_string),
                        body: Value::Null,
                    });

                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"created":1,"data":[{"b64_json":"aW1hZ2U="}],"usage":{"input_tokens":7,"output_tokens":11,"total_tokens":18}}"#,
                    )
                }
            }
        };
        let mock_app = Router::new()
            .route("/v1/images/generations", post(capture_handler.clone()))
            .route("/v1/images/edits", post(capture_handler.clone()))
            .route("/Gateway/v1/images/edits", post(capture_handler.clone()))
            .route("/v1/alpha/search", post(capture_handler));
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();

        let cases = [
            (
                "pasted-mixed-case-chat-completions",
                format!("http://{mock_addr}/v1/Chat/Completions?api-version=CaseValue"),
                "/v1/images/edits",
                "/v1/images/edits?api-version=CaseValue&client_version=0.145.0",
            ),
            (
                "pasted-mixed-case-images-generations",
                format!("http://{mock_addr}/Gateway/v1/Images/Generations/?api-version=CaseValue#fragment"),
                "/v1/images/edits",
                "/Gateway/v1/images/edits?api-version=CaseValue&client_version=0.145.0",
            ),
            (
                "pasted-mixed-case-images-edits",
                format!("http://{mock_addr}/v1/Images/Edits/"),
                "/v1/images/generations",
                "/v1/images/generations?client_version=0.145.0",
            ),
            (
                "pasted-mixed-case-responses-compact",
                format!("http://{mock_addr}/v1/Responses/Compact/"),
                "/v1/alpha/search",
                "/v1/alpha/search?client_version=0.145.0",
            ),
            (
                "pasted-chat-completions",
                format!("http://{mock_addr}/v1/chat/completions"),
                "/v1/images/generations",
                "/v1/images/generations?client_version=0.145.0",
            ),
            (
                "pasted-chat-completions",
                format!("http://{mock_addr}/v1/chat/completions"),
                "/v1/images/edits",
                "/v1/images/edits?client_version=0.145.0",
            ),
            (
                "pasted-images-generations",
                format!("http://{mock_addr}/v1/images/generations?api-version=test"),
                "/v1/images/edits",
                "/v1/images/edits?api-version=test&client_version=0.145.0",
            ),
            (
                "pasted-responses",
                format!("http://{mock_addr}/v1/responses"),
                "/v1/alpha/search",
                "/v1/alpha/search?client_version=0.145.0",
            ),
        ];

        for (provider_id, base_url, local_path, expected_upstream) in cases {
            let provider = Provider::with_id(
                provider_id.to_string(),
                provider_id.to_string(),
                json!({
                    "base_url": base_url,
                    "auth": {"OPENAI_API_KEY": "upstream-secret"}
                }),
                None,
            );
            db.save_provider("codex", &provider)
                .expect("save pasted base URL provider");
            db.set_current_provider("codex", &provider.id)
                .expect("select pasted base URL provider");

            let response = client
                .post(format!(
                    "http://127.0.0.1:{}{local_path}?client_version=0.145.0",
                    proxy_info.port
                ))
                .header(header::AUTHORIZATION, "Bearer client-secret")
                .json(&json!({"model": "gpt-image-1", "prompt": "pasted base URL"}))
                .send()
                .await
                .expect("send images request");

            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{local_path} via {base_url}"
            );
            let request = captured
                .lock()
                .await
                .pop()
                .expect("upstream request captured");
            assert_eq!(
                request.path_and_query, expected_upstream,
                "{local_path} via {base_url}"
            );
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer upstream-secret")
            );
        }

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
    }

    #[tokio::test]
    async fn codex_images_generation_aliases_forward_and_record_usage() {
        let captured = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
        let mock_app = Router::new().route(
            "/v1/images/generations",
            post({
                let captured = captured.clone();
                move |request: axum::extract::Request| {
                    let captured = captured.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = axum::body::to_bytes(body, 1024 * 1024)
                            .await
                            .expect("read mock request body");
                        captured.lock().await.push(CapturedRequest {
                            path_and_query: parts
                                .uri
                                .path_and_query()
                                .map(|value| value.as_str().to_string())
                                .unwrap_or_else(|| parts.uri.path().to_string()),
                            authorization: parts
                                .headers
                                .get(header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .map(ToString::to_string),
                            body: serde_json::from_slice(&body).expect("parse mock request body"),
                        });

                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"created":1,"data":[{"b64_json":"aW1hZ2U="}],"usage":{"input_tokens":7,"output_tokens":11,"total_tokens":18}}"#,
                        )
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider::with_id(
            "images-api-upstream".to_string(),
            "Images API Upstream".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1"),
                "auth": {"OPENAI_API_KEY": "upstream-secret"}
            }),
            None,
        );
        db.save_provider("codex", &provider)
            .expect("save test provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: true,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let aliases = [
            "/images/generations",
            "/v1/images/generations",
            "/v1/v1/images/generations",
            "/codex/v1/images/generations",
        ];

        for (index, path) in aliases.iter().enumerate() {
            let response = client
                .post(format!(
                    "http://127.0.0.1:{}{}?client_version=0.145.0",
                    proxy_info.port, path
                ))
                .header(header::AUTHORIZATION, "Bearer client-secret")
                .json(&json!({
                    "model": "gpt-image-1",
                    "prompt": format!("image generation alias {index}")
                }))
                .send()
                .await
                .expect("send images request");

            assert_eq!(response.status(), StatusCode::OK, "alias {path}");
            assert_eq!(
                response.text().await.expect("read proxy response"),
                r#"{"created":1,"data":[{"b64_json":"aW1hZ2U="}],"usage":{"input_tokens":7,"output_tokens":11,"total_tokens":18}}"#,
                "alias {path}"
            );
        }

        let (log_count, input_tokens, output_tokens): (i64, i64, i64) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let totals = {
                        let conn = crate::database::lock_conn!(db.conn);
                        match conn.query_row(
                            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
                             FROM proxy_request_logs WHERE provider_id = ?1",
                            ["images-api-upstream"],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        ) {
                            Ok(totals) => totals,
                            Err(error) => panic!("query images usage logs: {error}"),
                        }
                    };

                    if totals.0 == aliases.len() as i64 {
                        break Ok::<_, AppError>(totals);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("images usage logs were not recorded")
            .expect("read images usage logs");
        assert_eq!(log_count, aliases.len() as i64);
        assert_eq!(input_tokens, 7 * aliases.len() as i64);
        assert_eq!(output_tokens, 11 * aliases.len() as i64);

        let mut full_url_provider = Provider::with_id(
            "images-api-full-url".to_string(),
            "Images API Full URL".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1/responses?api-version=test"),
                "auth": {"OPENAI_API_KEY": "full-url-secret"}
            }),
            None,
        );
        full_url_provider.meta = Some(ProviderMeta {
            is_full_url: Some(true),
            ..ProviderMeta::default()
        });
        db.save_provider("codex", &full_url_provider)
            .expect("save full URL images provider");
        db.set_current_provider("codex", &full_url_provider.id)
            .expect("select full URL images provider");

        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/images/generations?client_version=0.145.0",
                proxy_info.port
            ))
            .header(header::AUTHORIZATION, "Bearer client-secret")
            .json(&json!({
                "model": "gpt-image-1",
                "prompt": "image generation full URL"
            }))
            .send()
            .await
            .expect("send full URL images request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.text().await.expect("read full URL image response"),
            r#"{"created":1,"data":[{"b64_json":"aW1hZ2U="}],"usage":{"input_tokens":7,"output_tokens":11,"total_tokens":18}}"#
        );

        let captured = captured.lock().await;
        assert_eq!(captured.len(), aliases.len() + 1);
        for (index, request) in captured.iter().take(aliases.len()).enumerate() {
            assert_eq!(
                request.path_and_query,
                "/v1/images/generations?client_version=0.145.0"
            );
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer upstream-secret")
            );
            assert_eq!(request.body["model"], "gpt-image-1");
            assert_eq!(
                request.body["prompt"],
                format!("image generation alias {index}")
            );
        }

        let full_url_request = captured.last().expect("full URL image request captured");
        assert_eq!(
            full_url_request.path_and_query,
            "/v1/images/generations?api-version=test&client_version=0.145.0"
        );
        assert_eq!(
            full_url_request.authorization.as_deref(),
            Some("Bearer full-url-secret")
        );
        assert_eq!(full_url_request.body["model"], "gpt-image-1");
        assert_eq!(full_url_request.body["prompt"], "image generation full URL");

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
    }

    #[tokio::test]
    async fn codex_images_edit_aliases_forward_and_record_usage() {
        // Real Images API responses carry `input_tokens_details` with text/image
        // splits; the shared Codex usage parser must tolerate them.
        const UPSTREAM_BODY: &str = r#"{"created":1,"data":[{"b64_json":"ZWRpdA=="}],"usage":{"input_tokens":13,"output_tokens":17,"total_tokens":30,"input_tokens_details":{"text_tokens":5,"image_tokens":8}}}"#;
        const IMAGE_DATA_URL: &str = "data:image/png;base64,Zm9v";

        let captured = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
        let mock_app = Router::new().route(
            "/v1/images/edits",
            post({
                let captured = captured.clone();
                move |request: axum::extract::Request| {
                    let captured = captured.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = axum::body::to_bytes(body, 1024 * 1024)
                            .await
                            .expect("read mock request body");
                        captured.lock().await.push(CapturedRequest {
                            path_and_query: parts
                                .uri
                                .path_and_query()
                                .map(|value| value.as_str().to_string())
                                .unwrap_or_else(|| parts.uri.path().to_string()),
                            authorization: parts
                                .headers
                                .get(header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .map(ToString::to_string),
                            body: serde_json::from_slice(&body).expect("parse mock request body"),
                        });

                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            UPSTREAM_BODY,
                        )
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider::with_id(
            "images-edit-upstream".to_string(),
            "Images Edit Upstream".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1"),
                "auth": {"OPENAI_API_KEY": "upstream-secret"}
            }),
            None,
        );
        db.save_provider("codex", &provider)
            .expect("save test provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: true,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let aliases = [
            "/images/edits",
            "/v1/images/edits",
            "/v1/v1/images/edits",
            "/codex/v1/images/edits",
        ];

        for (index, path) in aliases.iter().enumerate() {
            let response = client
                .post(format!(
                    "http://127.0.0.1:{}{}?client_version=0.145.0",
                    proxy_info.port, path
                ))
                .header(header::AUTHORIZATION, "Bearer client-secret")
                .json(&json!({
                    "model": "gpt-image-2",
                    "prompt": format!("image edit alias {index}"),
                    "images": [{"image_url": IMAGE_DATA_URL}]
                }))
                .send()
                .await
                .expect("send images edit request");

            assert_eq!(response.status(), StatusCode::OK, "alias {path}");
            assert_eq!(
                response.text().await.expect("read proxy response"),
                UPSTREAM_BODY,
                "alias {path}"
            );
        }

        let (log_count, input_tokens, output_tokens): (i64, i64, i64) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let totals = {
                        let conn = crate::database::lock_conn!(db.conn);
                        match conn.query_row(
                            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
                             FROM proxy_request_logs WHERE provider_id = ?1",
                            ["images-edit-upstream"],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        ) {
                            Ok(totals) => totals,
                            Err(error) => panic!("query images edit usage logs: {error}"),
                        }
                    };

                    if totals.0 == aliases.len() as i64 {
                        break Ok::<_, AppError>(totals);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("images edit usage logs were not recorded")
            .expect("read images edit usage logs");
        assert_eq!(log_count, aliases.len() as i64);
        assert_eq!(input_tokens, 13 * aliases.len() as i64);
        assert_eq!(output_tokens, 17 * aliases.len() as i64);

        // A full URL pasted for the sibling generations route must derive
        // `/images/edits` instead of posting edit payloads to generations.
        let mut full_url_provider = Provider::with_id(
            "images-edit-full-url".to_string(),
            "Images Edit Full URL".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1/images/generations?api-version=test"),
                "auth": {"OPENAI_API_KEY": "full-url-secret"}
            }),
            None,
        );
        full_url_provider.meta = Some(ProviderMeta {
            is_full_url: Some(true),
            ..ProviderMeta::default()
        });
        db.save_provider("codex", &full_url_provider)
            .expect("save full URL images edit provider");
        db.set_current_provider("codex", &full_url_provider.id)
            .expect("select full URL images edit provider");

        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/images/edits?client_version=0.145.0",
                proxy_info.port
            ))
            .header(header::AUTHORIZATION, "Bearer client-secret")
            .json(&json!({
                "model": "gpt-image-2",
                "prompt": "image edit full URL",
                "images": [{"image_url": IMAGE_DATA_URL}]
            }))
            .send()
            .await
            .expect("send full URL images edit request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .text()
                .await
                .expect("read full URL image edit response"),
            UPSTREAM_BODY
        );

        let captured = captured.lock().await;
        assert_eq!(captured.len(), aliases.len() + 1);
        for (index, request) in captured.iter().take(aliases.len()).enumerate() {
            assert_eq!(
                request.path_and_query,
                "/v1/images/edits?client_version=0.145.0"
            );
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer upstream-secret")
            );
            assert_eq!(request.body["model"], "gpt-image-2");
            assert_eq!(request.body["prompt"], format!("image edit alias {index}"));
            assert_eq!(request.body["images"][0]["image_url"], IMAGE_DATA_URL);
        }

        let full_url_request = captured
            .last()
            .expect("full URL image edit request captured");
        assert_eq!(
            full_url_request.path_and_query,
            "/v1/images/edits?api-version=test&client_version=0.145.0"
        );
        assert_eq!(
            full_url_request.authorization.as_deref(),
            Some("Bearer full-url-secret")
        );
        assert_eq!(full_url_request.body["model"], "gpt-image-2");
        assert_eq!(full_url_request.body["prompt"], "image edit full URL");
        assert_eq!(
            full_url_request.body["images"][0]["image_url"],
            IMAGE_DATA_URL
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();
    }

    #[tokio::test]
    async fn alpha_search_routes_forward_to_canonical_upstream() {
        let captured = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
        let mock_app = Router::new().route(
            "/v1/alpha/search",
            post({
                let captured = captured.clone();
                move |request: axum::extract::Request| {
                    let captured = captured.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = axum::body::to_bytes(body, 1024 * 1024)
                            .await
                            .expect("read mock request body");
                        captured.lock().await.push(CapturedRequest {
                            path_and_query: parts
                                .uri
                                .path_and_query()
                                .map(|value| value.as_str().to_string())
                                .unwrap_or_else(|| parts.uri.path().to_string()),
                            authorization: parts
                                .headers
                                .get(header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .map(ToString::to_string),
                            body: serde_json::from_slice(&body).expect("parse mock request body"),
                        });

                        let mut headers = HeaderMap::new();
                        headers.insert(
                            header::CONTENT_TYPE,
                            "application/json".parse().expect("content type"),
                        );
                        headers.insert(
                            "x-upstream-request-id",
                            "search-1".parse().expect("request id"),
                        );
                        (
                            StatusCode::ACCEPTED,
                            headers,
                            r#"{"encrypted_output":"ciphertext"}"#,
                        )
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider::with_id(
            "alpha-search-upstream".to_string(),
            "Alpha Search Upstream".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1"),
                "auth": {"OPENAI_API_KEY": "upstream-secret"}
            }),
            None,
        );
        db.save_provider("codex", &provider)
            .expect("save test provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: false,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = crate::proxy::test_support::local_client();
        let aliases = [
            "/alpha/search",
            "/v1/alpha/search",
            "/v1/v1/alpha/search",
            "/codex/v1/alpha/search",
        ];

        for (index, path) in aliases.iter().enumerate() {
            let response = client
                .post(format!(
                    "http://127.0.0.1:{}{}?client_version=0.144.6",
                    proxy_info.port, path
                ))
                .header(header::AUTHORIZATION, "Bearer client-secret")
                .json(&json!({
                    "id": format!("search-{index}"),
                    "model": "gpt-5.6-sol",
                    "commands": {"search_query": [{"q": "test"}]}
                }))
                .send()
                .await
                .expect("send alpha search request");

            assert_eq!(response.status(), StatusCode::ACCEPTED, "alias {path}");
            assert_eq!(
                response
                    .headers()
                    .get("x-upstream-request-id")
                    .and_then(|value| value.to_str().ok()),
                Some("search-1"),
                "alias {path}"
            );
            assert_eq!(
                response.text().await.expect("read proxy response"),
                r#"{"encrypted_output":"ciphertext"}"#,
                "alias {path}"
            );
        }

        // Full-URL providers were the known flaw in the original PR: without a
        // sibling-endpoint rewrite, this request would be posted back to
        // `/v1/responses` instead of `/v1/alpha/search`.
        let mut full_url_provider = Provider::with_id(
            "alpha-search-full-url".to_string(),
            "Alpha Search Full URL".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1/responses?api-version=test"),
                "auth": {"OPENAI_API_KEY": "full-url-secret"}
            }),
            None,
        );
        full_url_provider.meta = Some(ProviderMeta {
            is_full_url: Some(true),
            ..ProviderMeta::default()
        });
        db.save_provider("codex", &full_url_provider)
            .expect("save full URL provider");
        db.set_current_provider("codex", &full_url_provider.id)
            .expect("select full URL provider");

        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/alpha/search?client_version=0.144.6",
                proxy_info.port
            ))
            .header(header::AUTHORIZATION, "Bearer client-secret")
            .json(&json!({
                "id": "search-full-url",
                "model": "gpt-5.6-sol",
                "commands": {"search_query": [{"q": "full URL"}]}
            }))
            .send()
            .await
            .expect("send full URL alpha search request");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.text().await.expect("read full URL response"),
            r#"{"encrypted_output":"ciphertext"}"#
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();

        let captured = captured.lock().await;
        assert_eq!(captured.len(), aliases.len() + 1);
        for (index, request) in captured.iter().take(aliases.len()).enumerate() {
            assert_eq!(
                request.path_and_query,
                "/v1/alpha/search?client_version=0.144.6"
            );
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer upstream-secret")
            );
            assert_eq!(request.body["id"], format!("search-{index}"));
            assert_eq!(request.body["model"], "gpt-5.6-sol");
            assert_eq!(request.body["commands"]["search_query"][0]["q"], "test");
        }

        let full_url_request = captured.last().expect("full URL request captured");
        assert_eq!(
            full_url_request.path_and_query,
            "/v1/alpha/search?api-version=test&client_version=0.144.6"
        );
        assert_eq!(
            full_url_request.authorization.as_deref(),
            Some("Bearer full-url-secret")
        );
        assert_eq!(full_url_request.body["id"], "search-full-url");
        assert_eq!(
            full_url_request.body["commands"]["search_query"][0]["q"],
            "full URL"
        );
    }
}
