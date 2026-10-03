//! Pi coding agent adapter — transparent passthrough.
//!
//! Pi 的请求方言由供应商 `models.json` 节点的 `api` 字段决定，且与上游方言
//! 一致，因此网关不做任何请求体转换：适配器只负责从 Pi 形状的配置
//! （camelCase `baseUrl` / `apiKey` / `api`）提取上游地址与凭据，并按方言
//! 选择认证头风格。
//!
//! - `anthropic-messages` → x-api-key（Anthropic 原生风格）
//! - `openai-completions` / `openai-responses` → Authorization: Bearer

use super::adapter::auth_header_value as hv;
use super::{AuthInfo, AuthStrategy, ProviderAdapter};
use crate::provider::Provider;
use crate::proxy::ProxyError;

pub struct PiAdapter;

impl PiAdapter {
    fn provider_api(provider: &Provider) -> &'static str {
        let api = provider
            .settings_config
            .get("api")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match api {
            "anthropic-messages" => "anthropic-messages",
            "openai-completions" => "openai-completions",
            "openai-responses" => "openai-responses",
            _ => "",
        }
    }
}

impl ProviderAdapter for PiAdapter {
    fn name(&self) -> &'static str {
        "Pi"
    }

    fn extract_base_url(&self, provider: &Provider) -> Result<String, ProxyError> {
        provider
            .settings_config
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ProxyError::Internal("Pi provider settings_config missing baseUrl".to_string())
            })
    }

    fn extract_auth(&self, provider: &Provider) -> Option<AuthInfo> {
        let key = provider
            .settings_config
            .get("apiKey")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        if key.is_empty() {
            return None;
        }
        let strategy = match Self::provider_api(provider) {
            "anthropic-messages" => AuthStrategy::Anthropic,
            _ => AuthStrategy::Bearer,
        };
        Some(AuthInfo::new(key, strategy))
    }

    fn build_url(&self, base_url: &str, endpoint: &str) -> String {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            endpoint.trim_start_matches('/')
        )
    }

    fn get_auth_headers(
        &self,
        auth: &AuthInfo,
    ) -> Result<Vec<(http::HeaderName, http::HeaderValue)>, ProxyError> {
        use http::HeaderName;
        let bearer = format!("Bearer {}", auth.api_key);
        Ok(match auth.strategy {
            AuthStrategy::Anthropic => {
                vec![(HeaderName::from_static("x-api-key"), hv(&auth.api_key)?)]
            }
            AuthStrategy::ClaudeAuth | AuthStrategy::Bearer => {
                vec![(HeaderName::from_static("authorization"), hv(&bearer)?)]
            }
            _ => {
                return Err(ProxyError::Internal(format!(
                    "Pi adapter does not support auth strategy {:?}",
                    auth.strategy
                )))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pi_provider(api: &str, base_url: &str, api_key: &str) -> Provider {
        Provider {
            id: "test-pi".to_string(),
            name: "Test Pi".to_string(),
            settings_config: json!({
                "name": "Test Pi",
                "baseUrl": base_url,
                "apiKey": api_key,
                "api": api,
                "models": [],
            }),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: None,
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[test]
    fn extracts_pi_base_url_and_strips_trailing_slash() {
        let adapter = PiAdapter;
        let provider = pi_provider("anthropic-messages", "https://api.example.com/v1/", "sk-1");
        assert_eq!(
            adapter.extract_base_url(&provider).unwrap(),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn anthropic_dialect_uses_x_api_key() {
        let adapter = PiAdapter;
        let provider = pi_provider("anthropic-messages", "https://api.example.com", "sk-1");
        let auth = adapter.extract_auth(&provider).unwrap();
        let headers = adapter.get_auth_headers(&auth).unwrap();
        assert_eq!(headers[0].0.as_str(), "x-api-key");
    }

    #[test]
    fn openai_dialect_uses_bearer() {
        let adapter = PiAdapter;
        let provider = pi_provider("openai-completions", "https://api.example.com/v1", "sk-2");
        let auth = adapter.extract_auth(&provider).unwrap();
        let headers = adapter.get_auth_headers(&auth).unwrap();
        assert_eq!(headers[0].0.as_str(), "authorization");
        assert!(headers[0].1.to_str().unwrap().starts_with("Bearer "));
    }

    #[test]
    fn build_url_joins_base_and_endpoint() {
        let adapter = PiAdapter;
        assert_eq!(
            adapter.build_url("https://api.example.com/v1/", "/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    /// B4 端到端落点锁定：客户端 `/pi/openai/v1/chat/completions`
    /// → handler 剥前缀得 `/v1/chat/completions` → 拼 base 得最终上游 URL。
    ///
    /// 回归背景：endpoint 曾被硬编码为 `/chat/completions`，对 New API 一类
    /// 兼容网关会命中网页路由返回 HTML（实测 200 + text/html）。
    #[test]
    fn build_url_keeps_v1_for_openai_compatible_gateways() {
        let adapter = PiAdapter;
        // 常见形态：档案 baseUrl 就是裸域名 → 上游路径正确
        assert_eq!(
            adapter.build_url("https://host", "/v1/chat/completions"),
            "https://host/v1/chat/completions"
        );
        // base 自带 /v1 时会拼出 /v1/v1 —— 这是 PiAdapter 纯拼接的**既有语义**
        // （anthropic 侧在 B4 之前同样如此），网关不做"base 是否已含 /v1"的推断，
        // 因为那是用户的配置决策。本测试锁定该行为，防止未来有人无意改动它。
        assert_eq!(
            adapter.build_url("https://host/v1/", "/v1/chat/completions"),
            "https://host/v1/v1/chat/completions"
        );
    }
}
