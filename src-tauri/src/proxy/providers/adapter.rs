//! Provider Adapter Trait
//!
//! 定义供应商适配器的统一接口，抽象不同上游供应商的处理逻辑。

use super::auth::AuthInfo;
use crate::provider::Provider;
use crate::proxy::error::ProxyError;
use crate::proxy::key_pool::KeySelection;
use serde_json::Value;

/// 供应商适配器 Trait
///
/// 所有供应商适配器都需要实现此 trait，提供统一的接口来处理：
/// - URL 构建
/// - 认证信息提取和头部注入
/// - 请求/响应格式转换（可选）
pub trait ProviderAdapter: Send + Sync {
    /// 适配器名称（用于日志和调试）
    fn name(&self) -> &'static str;

    /// 从 Provider 配置中提取 base_url
    fn extract_base_url(&self, provider: &Provider) -> Result<String, ProxyError>;

    /// 从 Provider 配置中提取认证信息
    fn extract_auth(&self, provider: &Provider) -> Option<AuthInfo>;

    /// 构建请求 URL
    fn build_url(&self, base_url: &str, endpoint: &str) -> String;

    /// Return auth headers as `(name, value)` pairs.
    ///
    /// The forwarder inserts these at the position of the original auth header
    /// so that header order is preserved.
    ///
    /// Returns `ProxyError::AuthError` when the credential contains characters
    /// that cannot be encoded as an HTTP header value (e.g. control chars,
    /// CR/LF), which would otherwise panic inside `HeaderValue::from_str`.
    fn get_auth_headers(
        &self,
        auth: &AuthInfo,
    ) -> Result<Vec<(http::HeaderName, http::HeaderValue)>, ProxyError>;

    /// 是否需要格式转换
    fn needs_transform(&self, _provider: &Provider) -> bool {
        false
    }

    /// 转换请求体
    fn transform_request(&self, body: Value, _provider: &Provider) -> Result<Value, ProxyError> {
        Ok(body)
    }

    /// 转换响应体
    #[allow(dead_code)]
    fn transform_response(&self, body: Value) -> Result<Value, ProxyError> {
        Ok(body)
    }

    /// Key 解析（带池）：provider 配置了 `meta.keyPool` 时按策略选池中一把
    /// 未冷却的 key；无池时回落适配器自己的单 key 提取逻辑。
    ///
    /// 返回 [`KeySelection`]（含 `key_id`）而非裸 `String`：`extract_auth` 需要
    /// 把 key_id 装进 `AuthInfo`，usage 落库才能按 key 归属计量。
    ///
    /// 默认实现让所有适配器免费获得 key 池能力；`extract_auth` 应改调本方法
    /// 而非私有 `extract_key`（OAuth 类 placeholder 分支先行返回，天然不走池）。
    fn resolve_key_with_pool(&self, provider: &Provider) -> Option<KeySelection> {
        match crate::proxy::key_pool::select_for_provider(provider) {
            Some(selection) => {
                log::debug!(
                    "[KeyPool] provider `{}` 选用池 key `{}`",
                    provider.id,
                    selection.key_id
                );
                Some(selection)
            }
            None => self.extract_key(provider).map(|key| KeySelection {
                key,
                key_id: String::new(),
            }),
        }
    }

    /// 适配器的单 key 提取（无池路径）。由 [`Self::resolve_key_with_pool`]
    /// 在无池时回落调用；各适配器必须实现。
    fn extract_key(&self, provider: &Provider) -> Option<String>;
}

/// Build an HTTP `HeaderValue` from a credential / token string.
///
/// Returns `ProxyError::AuthError` when the string contains characters that
/// cannot live in an HTTP header value (control bytes, CR/LF, non-ASCII).
/// Adapters call this for every header value derived from user-pasted
/// material so a malformed key surfaces as a 401 instead of panicking
/// the worker via `HeaderValue::from_str(...).unwrap()`.
pub fn auth_header_value(s: &str) -> Result<http::HeaderValue, ProxyError> {
    http::HeaderValue::from_str(s)
        .map_err(|e| ProxyError::AuthError(format!("invalid auth header value: {e}")))
}
