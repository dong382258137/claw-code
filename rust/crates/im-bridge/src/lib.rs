//! IM Bridge: Unified gateway for IM platforms (Feishu, WeCom) to the claw engine.
//!
//! Architecture:
//! ```text
//! IM Platform → axum HTTP server → SessionManager → ClawAgent → IM Platform
//! ```
//!
//! Features:
//! - Multi-platform: Feishu (Lark) + WeCom (企业微信)
//! - Session management: one agent session per chat, auto-reuse
//! - Chat commands: /help, /new, /status (intercepted before agent)
//! - Session persistence: save/restore on restart
//! - Error recovery: graceful handling of agent failures
//!
//! Reuses `spawn_claw_shell` for the agent, and ACP channels for communication.

pub mod api_adapter;
pub mod commands;
pub mod config;
pub mod connectors;
pub mod persistence;
pub mod response;
pub mod server;
pub mod session;
