use std::path::Path;
use std::sync::PoisonError;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("配置错误: {0}")]
    Config(String),
    #[error("无效输入: {0}")]
    InvalidInput(String),
    /// Native files changed after CC Switch last read them.
    #[error("并发冲突: {0}")]
    Conflict(String),
    #[error("IO 错误: {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{context}: {source}")]
    IoContext {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON 解析错误: {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("JSON 序列化失败: {source}")]
    JsonSerialize {
        #[source]
        source: serde_json::Error,
    },
    #[error("TOML 解析错误: {path}: {source}")]
    Toml {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("锁获取失败: {0}")]
    Lock(String),
    #[error("MCP 校验失败: {0}")]
    McpValidation(String),
    #[error("{0}")]
    Message(String),
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("{zh} ({en})")]
    Localized {
        key: &'static str,
        zh: String,
        en: String,
    },
    #[error("数据库错误: {0}")]
    Database(String),
    #[error("OMO 配置文件不存在")]
    OmoConfigNotFound,
    #[error("所有供应商已熔断，无可用渠道")]
    AllProvidersCircuitOpen,
    #[error("未配置供应商")]
    NoProvidersConfigured,
}

impl AppError {
    pub fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn json(path: impl AsRef<Path>, source: serde_json::Error) -> Self {
        Self::Json {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn toml(path: impl AsRef<Path>, source: toml::de::Error) -> Self {
        Self::Toml {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn localized(key: &'static str, zh: impl Into<String>, en: impl Into<String>) -> Self {
        Self::Localized {
            key,
            zh: zh.into(),
            en: en.into(),
        }
    }
}

impl<T> From<PoisonError<T>> for AppError {
    fn from(err: PoisonError<T>) -> Self {
        Self::Lock(err.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Database(err.to_string())
    }
}

impl From<AppError> for String {
    fn from(err: AppError) -> Self {
        err.to_string()
    }
}

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// 结构化错误的统一形状：`{"code","message"?,"context","suggestion"?}`
///
/// 渐进式错误码：后端仍返回 `Result<T, String>`，只是把字符串荷载换成可机读的
/// JSON。前端 `extractErrorMessage` 先尝试 `JSON.parse`，命中 `code` 后取
/// `message` 作为人读文案，取不到就回退原文；`context` 供 i18n 插值，
/// `suggestion` 是可机读的修复提示码。
///
/// `message` 为 `None` 时不写入该键——skill 域的历史荷载就是不含 message 的
/// 形状，`format_skill_error` 依赖这一点保持输出不变。
pub fn format_structured_error(
    code: &str,
    message: Option<&str>,
    context: &[(&str, &str)],
    suggestion: Option<&str>,
) -> String {
    use serde_json::json;

    let mut ctx_map = serde_json::Map::new();
    for (key, value) in context {
        ctx_map.insert(key.to_string(), json!(value));
    }

    let mut error_obj = serde_json::Map::new();
    error_obj.insert("code".to_string(), json!(code));
    if let Some(message) = message {
        error_obj.insert("message".to_string(), json!(message));
    }
    error_obj.insert("context".to_string(), json!(ctx_map));
    error_obj.insert("suggestion".to_string(), json!(suggestion));

    serde_json::to_string(&error_obj).unwrap_or_else(|_| {
        // 如果 JSON 序列化失败，返回简单格式
        format!("ERROR:{code}")
    })
}

/// 格式化为 JSON 错误字符串，前端可解析为结构化错误
///
/// skill 域入口：沿用不含 `message` 的历史荷载形状。
pub fn format_skill_error(
    code: &str,
    context: &[(&str, &str)],
    suggestion: Option<&str>,
) -> String {
    format_structured_error(code, None, context, suggestion)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_payload_keeps_legacy_shape_without_message() {
        let payload =
            format_skill_error("SKILL_NOT_FOUND", &[("name", "demo")], Some("retryLater"));
        assert_eq!(
            payload,
            r#"{"code":"SKILL_NOT_FOUND","context":{"name":"demo"},"suggestion":"retryLater"}"#
        );
    }

    #[test]
    fn structured_payload_carries_code_message_context_and_suggestion() {
        let payload = format_structured_error(
            "PROVIDER_NOT_FOUND",
            Some("供应商 p1 不存在"),
            &[("id", "p1"), ("app", "claude")],
            Some("refreshProviders"),
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid json");
        assert_eq!(parsed["code"], "PROVIDER_NOT_FOUND");
        assert_eq!(parsed["message"], "供应商 p1 不存在");
        assert_eq!(parsed["context"]["id"], "p1");
        assert_eq!(parsed["context"]["app"], "claude");
        assert_eq!(parsed["suggestion"], "refreshProviders");
    }

    #[test]
    fn structured_payload_omits_message_when_absent() {
        let payload = format_structured_error("TAKEOVER_FAILED", None, &[], None);
        assert!(!payload.contains("\"message\""));
        assert!(payload.contains("\"suggestion\":null"));
    }
}
