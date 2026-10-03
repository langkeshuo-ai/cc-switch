//! 第三方 OAuth 认证的 Tauri State 包装。
//!
//! # 为什么独立成模块
//!
//! 这三个类型只是 `Arc<Manager>` 的 newtype 包装，本身不含命令逻辑。
//! 它们原先定义在 `commands::{codex_oauth, copilot, xai_oauth}` 里，
//! 导致 `proxy::forwarder` 为了读 token 必须 `use crate::commands::…` ——
//! 下层依赖上层，破坏单向依赖。
//!
//! Manager 全都住在 `proxy::providers::*`，故 State 与它们同层；
//! `commands` 侧保留 `pub use` 转发，既有调用方（`commands::auth`、
//! `tray.rs`、`app_setup.rs`）无需改import。

use std::sync::Arc;

use tokio::sync::RwLock;

use super::codex_oauth_auth::CodexOAuthManager;
use super::copilot_auth::CopilotAuthManager;
use super::xai_oauth_auth::XaiOAuthManager;

/// Codex OAuth 认证状态
///
/// `CodexOAuthManager` 内部已使用细粒度锁且所有方法均为 `&self`，因此这里
/// 直接持有 `Arc`，不再包一层 `RwLock`——避免任一命令持有粗粒度锁跨网络刷新
/// 时阻塞其他命令（切换 / 认证中心操作 / token 读取）。
pub struct CodexOAuthState(pub Arc<CodexOAuthManager>);

/// Copilot 认证状态
pub struct CopilotAuthState(pub Arc<RwLock<CopilotAuthManager>>);

/// xAI OAuth 认证状态
pub struct XaiOAuthState(pub Arc<RwLock<XaiOAuthManager>>);
