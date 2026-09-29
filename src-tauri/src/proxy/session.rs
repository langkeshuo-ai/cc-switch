//! Proxy Session - 请求会话管理
//!
//! 为每个代理请求创建会话上下文，在整个请求生命周期中跟踪状态和元数据。
//!
//! ## Session ID 提取
//!
//! 支持从客户端请求中提取 Session ID，用于关联同一对话的多个请求：
//! - Claude: 从 `metadata.user_id` (格式: `user_xxx_session_yyy`) 或 `metadata.session_id` 提取
//! - Codex: 从 headers 中的 `session_id` / `x-session-id` 或 `metadata.session_id` 提取
//! - Grok Build: 从 headers 中的 `x-grok-conv-id` / `x-grok-session-id` 提取
//! - 其他: 生成新的 UUID

use axum::http::HeaderMap;
use uuid::Uuid;

// ============================================================================
// Session ID 提取器
// ============================================================================

/// Session ID 长度上限
const SESSION_ID_MAX_LEN: usize = 128;

/// 规范化/校验客户端提供的 Session ID
///
/// 唯一入口：session id 提取与会话粘性 key 派生都必须经过此函数。
/// 仅允许 `[A-Za-z0-9._-]` 且长度不超过 `SESSION_ID_MAX_LEN`；
/// 不合法（为空、超长、含其他字符）返回 `None`，视为未提供，不参与粘性。
pub(super) fn normalize_session_id(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > SESSION_ID_MAX_LEN
        || !trimmed
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some(trimmed)
}

/// Session ID 来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIdSource {
    /// 从 metadata.user_id 提取 (Claude)
    MetadataUserId,
    /// 从 metadata.session_id 提取
    MetadataSessionId,
    /// 从 headers 提取
    Header,
    /// 新生成
    Generated,
}

/// Session ID 提取结果
#[derive(Debug, Clone)]
pub struct SessionIdResult {
    /// 提取或生成的 Session ID
    pub session_id: String,
    /// Session ID 来源
    pub source: SessionIdSource,
    /// 是否为客户端提供的 ID（非新生成）
    pub client_provided: bool,
}

/// 从请求中提取或生成 Session ID
///
/// 轻量化实现，仅提取 session_id 用于日志记录，不做复杂的 Session 管理。
///
/// ## 提取优先级
///
/// ### Claude 请求
/// 1. `metadata.user_id` (格式: `user_xxx_session_yyy`) → 提取 `yyy` 部分
/// 2. `metadata.session_id` → 直接使用
/// 3. 生成新 UUID
///
/// ### Codex 请求
/// 1. Headers: `session_id` 或 `x-session-id`
/// 2. `metadata.session_id`
/// 3. 生成新 UUID
///
/// ### Grok Build 请求
/// 1. Headers: `x-grok-conv-id` 或 `x-grok-session-id`
/// 2. `metadata.session_id`
/// 3. 生成新 UUID
///
/// ## 示例
///
/// ```ignore
/// let result = extract_session_id(&headers, &body, "claude");
/// println!("Session ID: {} (from {:?})", result.session_id, result.source);
/// ```
pub fn extract_session_id(
    headers: &HeaderMap,
    body: &serde_json::Value,
    client_format: &str,
) -> SessionIdResult {
    if client_format == "claude" {
        if let Some(result) = extract_claude_session(headers, body) {
            return result;
        }
    }

    // Responses 请求特殊处理。Grok Build 使用与 Codex 相同的客户端协议，
    // 但保留独立前缀，避免统计和缓存键跨应用碰撞。
    if matches!(client_format, "codex" | "openai" | "grokbuild") {
        let prefix = if client_format == "grokbuild" {
            "grokbuild"
        } else {
            "codex"
        };
        if let Some(result) = extract_responses_session(headers, body, prefix) {
            return result;
        }
    }

    // Claude 请求：从 metadata 提取
    if let Some(result) = extract_from_metadata(body) {
        return result;
    }

    // 兜底：生成新 Session ID
    generate_new_session_id()
}

/// 提取 Claude Session ID
fn extract_claude_session(
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Option<SessionIdResult> {
    for header_name in &["x-claude-code-session-id", "claude-code-session-id"] {
        if let Some(value) = headers.get(*header_name) {
            if let Ok(session_id) = value.to_str() {
                if let Some(session_id) = normalize_session_id(session_id) {
                    return Some(SessionIdResult {
                        session_id: session_id.to_string(),
                        source: SessionIdSource::Header,
                        client_provided: true,
                    });
                }
            }
        }
    }

    extract_from_metadata(body)
}

/// 提取 Responses 客户端的 Session ID
fn extract_responses_session(
    headers: &HeaderMap,
    body: &serde_json::Value,
    prefix: &str,
) -> Option<SessionIdResult> {
    // 1. 从 headers 提取
    let header_names: &[&str] = if prefix == "grokbuild" {
        // Conversation ID 跨多轮请求保持稳定；session ID 作为客户端缺少
        // conversation ID 时的回退。x-grok-req-id 是逐请求 ID，不能用于聚合。
        &["x-grok-conv-id", "x-grok-session-id"]
    } else {
        &["session_id", "x-session-id"]
    };
    for header_name in header_names {
        if let Some(value) = headers.get(*header_name) {
            if let Ok(session_id) = value.to_str() {
                // Responses 客户端的 Session ID 通常较长（UUID 格式）
                if let Some(session_id) = normalize_session_id(session_id).filter(|s| s.len() > 20)
                {
                    if let Some(full) = normalize_session_id(&format!("{prefix}_{session_id}")) {
                        return Some(SessionIdResult {
                            session_id: full.to_string(),
                            source: SessionIdSource::Header,
                            client_provided: true,
                        });
                    }
                }
            }
        }
    }

    // 2. 从 body.metadata.session_id 提取
    if let Some(session_id) = body
        .get("metadata")
        .and_then(|m| m.get("session_id"))
        .and_then(|v| v.as_str())
    {
        if let Some(session_id) = normalize_session_id(session_id).filter(|s| s.len() > 10) {
            if let Some(full) = normalize_session_id(&format!("{prefix}_{session_id}")) {
                return Some(SessionIdResult {
                    session_id: full.to_string(),
                    source: SessionIdSource::MetadataSessionId,
                    client_provided: true,
                });
            }
        }
    }

    // previous_response_id 是 Responses 协议里的响应游标，不是稳定会话身份。
    // Chat/Responses 桥接时该值通常来自上游每轮返回的随机 response id；
    // 若把它当 prompt_cache_key 或 Codex session header，会导致每轮请求换缓存 key。

    None
}

/// 从 metadata 提取 Session ID (Claude)
fn extract_from_metadata(body: &serde_json::Value) -> Option<SessionIdResult> {
    let metadata = body.get("metadata")?;

    // 1. 从 metadata.user_id 提取（格式: user_xxx_session_yyy）
    if let Some(user_id) = metadata.get("user_id").and_then(|v| v.as_str()) {
        if let Some(session_id) = parse_session_from_user_id(user_id)
            .as_deref()
            .and_then(normalize_session_id)
        {
            return Some(SessionIdResult {
                session_id: session_id.to_string(),
                source: SessionIdSource::MetadataUserId,
                client_provided: true,
            });
        }
    }

    // 2. 直接从 metadata.session_id 提取
    if let Some(session_id) = metadata
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(normalize_session_id)
    {
        return Some(SessionIdResult {
            session_id: session_id.to_string(),
            source: SessionIdSource::MetadataSessionId,
            client_provided: true,
        });
    }

    None
}

/// 从 user_id 解析 session_id
///
/// 格式: `user_identifier_session_actual_session_id`
pub(super) fn parse_session_from_user_id(user_id: &str) -> Option<String> {
    // 查找 "_session_" 分隔符
    if let Some(pos) = user_id.find("_session_") {
        let session_id = &user_id[pos + 9..]; // "_session_" 长度为 9
        if !session_id.is_empty() {
            return Some(session_id.to_string());
        }
    }
    None
}

/// 生成新的 Session ID
fn generate_new_session_id() -> SessionIdResult {
    SessionIdResult {
        session_id: Uuid::new_v4().to_string(),
        source: SessionIdSource::Generated,
        client_provided: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ========== Session ID 提取测试 ==========

    #[test]
    fn test_extract_session_from_claude_metadata_user_id() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "user_id": "user_john_doe_session_abc123def456"
            }
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "abc123def456");
        assert_eq!(result.source, SessionIdSource::MetadataUserId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_metadata_session_id() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "session_id": "my-session-123"
            }
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "my-session-123");
        assert_eq!(result.source, SessionIdSource::MetadataSessionId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "d937243f-2702-4f20-97b6-c9682235ab81".parse().unwrap(),
        );
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "d937243f-2702-4f20-97b6-c9682235ab81");
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header_precedes_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "header-session-123".parse().unwrap(),
        );
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "session_id": "my-session-123"
            }
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "header-session-123");
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_codex_previous_response_id_is_not_stable_session_identity() {
        let headers = HeaderMap::new();
        let body = json!({
            "input": "Write a function",
            "previous_response_id": "resp_abc123def456789"
        });

        let result = extract_session_id(&headers, &body, "codex");

        assert!(!result.session_id.is_empty());
        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
    }

    #[test]
    fn test_codex_keeps_existing_response_session_headers() {
        let body = json!({ "input": "Write a function" });

        for header_name in ["session_id", "x-session-id"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header_name,
                "d937243f-2702-4f20-97b6-c9682235ab81".parse().unwrap(),
            );

            let result = extract_session_id(&headers, &body, "codex");

            assert_eq!(
                result.session_id,
                "codex_d937243f-2702-4f20-97b6-c9682235ab81"
            );
            assert_eq!(result.source, SessionIdSource::Header);
            assert!(result.client_provided);
        }
    }

    #[test]
    fn test_grokbuild_prefers_conversation_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-grok-conv-id",
            "conv-724f4275-584e-43af-ad46-b5e7509a3ca2".parse().unwrap(),
        );
        headers.insert(
            "x-grok-session-id",
            "session-d937243f-2702-4f20-97b6-c9682235ab81"
                .parse()
                .unwrap(),
        );
        let body = json!({ "input": "Write a function" });

        let result = extract_session_id(&headers, &body, "grokbuild");

        assert_eq!(
            result.session_id,
            "grokbuild_conv-724f4275-584e-43af-ad46-b5e7509a3ca2"
        );
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_grokbuild_falls_back_to_session_header() {
        let body = json!({ "input": "Write a function" });

        for conversation_id in ["", "                         "] {
            let mut headers = HeaderMap::new();
            headers.insert("x-grok-conv-id", conversation_id.parse().unwrap());
            headers.insert(
                "x-grok-session-id",
                "session-d937243f-2702-4f20-97b6-c9682235ab81"
                    .parse()
                    .unwrap(),
            );

            let result = extract_session_id(&headers, &body, "grokbuild");

            assert_eq!(
                result.session_id,
                "grokbuild_session-d937243f-2702-4f20-97b6-c9682235ab81"
            );
            assert_eq!(result.source, SessionIdSource::Header);
            assert!(result.client_provided);
        }
    }

    #[test]
    fn test_grokbuild_ignores_request_and_codex_session_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-grok-req-id",
            "request-724f4275-584e-43af-ad46-b5e7509a3ca2"
                .parse()
                .unwrap(),
        );
        headers.insert(
            "x-session-id",
            "codex-d937243f-2702-4f20-97b6-c9682235ab81"
                .parse()
                .unwrap(),
        );
        let body = json!({ "input": "Write a function" });

        let result = extract_session_id(&headers, &body, "grokbuild");

        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
    }

    #[test]
    fn test_extract_session_generates_new_when_not_found() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert!(!result.session_id.is_empty());
        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
    }

    #[test]
    fn test_parse_session_from_user_id() {
        assert_eq!(
            parse_session_from_user_id("user_john_session_abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_session_from_user_id("my_app_session_xyz789"),
            Some("xyz789".to_string())
        );
        // 注意: "_session_" 是分隔符，所以下面的字符串会匹配
        assert_eq!(
            parse_session_from_user_id("no_session_marker"),
            Some("marker".to_string())
        );
        // 没有 "_session_" 分隔符的情况
        assert_eq!(parse_session_from_user_id("user_john_abc123"), None);
        assert_eq!(parse_session_from_user_id("_session_"), None);
    }

    // ========== Session ID 规范化测试 ==========

    #[test]
    fn test_normalize_session_id_normal_uuid_passes_unchanged() {
        let uuid = "d937243f-2702-4f20-97b6-c9682235ab81";
        assert_eq!(normalize_session_id(uuid), Some(uuid));
        assert_eq!(
            normalize_session_id("my-session-123"),
            Some("my-session-123")
        );
        assert_eq!(normalize_session_id("codex_abc_123"), Some("codex_abc_123"));
        // 前后空白被裁剪
        assert_eq!(normalize_session_id("  sess-1  "), Some("sess-1"));
    }

    #[test]
    fn test_normalize_session_id_rejects_overlong() {
        // 超过 128 字符上限 → 拒绝（视为未提供，不做截断）
        let long_id = "a".repeat(129);
        assert_eq!(normalize_session_id(&long_id), None);
        // 恰好 128 字符 → 通过
        let max_id = "a".repeat(128);
        assert_eq!(normalize_session_id(&max_id), Some(max_id.as_str()));
    }

    #[test]
    fn test_normalize_session_id_rejects_invalid_charset() {
        // 含空格（非裁剪边界）
        assert_eq!(normalize_session_id("sess with space"), None);
        // 含路径分隔符
        assert_eq!(normalize_session_id("sess/../etc"), None);
        assert_eq!(normalize_session_id("sess\\windows"), None);
        // 含其他特殊字符
        assert_eq!(normalize_session_id("sess:1"), None);
        assert_eq!(normalize_session_id("sess#1"), None);
        // 非 ASCII
        assert_eq!(normalize_session_id("会话-1"), None);
        // 纯空白 → None
        assert_eq!(normalize_session_id("     "), None);
        // 空字符串 → None
        assert_eq!(normalize_session_id(""), None);
    }

    #[test]
    fn test_extract_rejects_invalid_session_ids_and_falls_back() {
        // header 中的非法 session id 应被拒绝，回退到生成新 ID
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "bad session/../id".parse().unwrap(),
        );
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
    }

    #[test]
    fn test_extract_rejects_overlong_metadata_session_id() {
        // 超长 metadata.session_id 应被拒绝，回退到生成新 ID
        let headers = HeaderMap::new();
        let long_id = "x".repeat(200);
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": { "session_id": long_id }
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
    }
}
