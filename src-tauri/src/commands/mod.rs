#![allow(non_snake_case)]

use std::str::FromStr;

mod auth;
mod balance;
mod codex_oauth;
mod coding_plan;
mod config;
mod copilot;
mod deeplink;
mod env;
mod failover;
mod global_proxy;
mod import_export;
mod mcp;
mod misc;
mod pi;
mod plugin;
mod profile;
mod prompt;
mod provider;
mod proxy;
mod session_manager;
mod settings;
pub mod skill;
mod stream_check;
mod subscription;
mod sync_support;
mod xai_oauth;

mod lightweight;
mod s3_sync;
mod usage;
mod webdav_sync;

pub use auth::*;
pub use balance::*;
pub use codex_oauth::*;
pub use coding_plan::*;
pub use config::*;
pub use copilot::*;
pub use deeplink::*;
pub use env::*;
pub use failover::*;
pub use global_proxy::*;
pub use import_export::*;
pub use mcp::*;
pub use misc::*;
pub(crate) use pi::*;
pub use plugin::*;
pub use profile::*;
pub use prompt::*;
pub use provider::*;
pub use proxy::*;
pub use session_manager::*;
pub use settings::*;
pub use skill::*;
pub use stream_check::*;
pub use subscription::*;
pub use xai_oauth::*;

pub use lightweight::*;
pub use s3_sync::*;
pub use usage::*;
pub use webdav_sync::*;

/// 解析 Tauri 命令的 `app` 字符串参数为 [`AppType`]。
///
/// 各命令模块共用的样板收敛点，等价于
/// `AppType::from_str(s).map_err(|e| e.to_string())`。
pub(crate) fn parse_app(s: &str) -> Result<crate::app_config::AppType, String> {
    crate::app_config::AppType::from_str(s).map_err(|e| e.to_string())
}
