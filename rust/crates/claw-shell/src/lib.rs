//! ACP shell layer for claw-code.
//!
//! 在 `runtime::ConversationRuntime` 之上封装一层 ACP 适配,通过
//! [`spawn_claw_shell`] 在独立线程 + LocalSet 中启动 agent,前端
//! (TUI / headless) 通过 ACP channel 与之通信。
//!
//! 架构参考:grok-build `xai-grok-shell/src/agent/mvp_agent/acp_agent.rs`。
//! 关键差异:
//! - 本地 `ConversationRuntime::run_turn` 是同步阻塞 API,我们用
//!   `tokio::task::spawn_blocking` 包裹,避免阻塞 LocalSet。
//! - 本地 `run_turn` 需要 `&mut self`,通过 `RefCell` 提供内部可变性。
//! - session 状态保存在 agent 内部,ACP `session_id` 与
//!   `Session::id` 一一对应。
//!
//! ## Feature 分层
//!
//! 本 crate 使用 `acp-0_10`(默认):基于 agent-client-protocol 0.10.4,
//! 实现 `ClawAgent` + `spawn_claw_shell` + `run_stdio_agent` + `lane_bridge`。

// ---- 0.10.4 路径(默认) ----
// agent.rs / spawn.rs / stdio.rs / lane_bridge.rs 都依赖 0.10.4 的
// `agent_client_protocol` crate 和 `claw-acp` 的 0.10.4 feature。
#[cfg(feature = "acp-0_10")]
mod agent;
#[cfg(feature = "acp-0_10")]
mod lane_bridge;
#[cfg(feature = "acp-0_10")]
mod spawn;
#[cfg(feature = "acp-0_10")]
mod stdio;

// ---- 公开 re-export ----

#[cfg(feature = "acp-0_10")]
pub use self::agent::{ClawAgent, ClawAgentBuilder, ClawAgentConfig};
#[cfg(feature = "acp-0_10")]
pub use self::spawn::{spawn_claw_shell, SpawnedAgent};
#[cfg(feature = "acp-0_10")]
pub use self::stdio::{run_agent_on_io, run_stdio_agent};
// 导出 lane_bridge 的公开 API:供 ClawAgent 在 run_turn 后调用,刷新 LaneEvent。
#[cfg(feature = "acp-0_10")]
pub use self::lane_bridge::{flush_lane_events_to_acp, lane_event_to_session_update};
