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

mod agent;
mod spawn;
mod stdio;

pub use self::agent::{ClawAgent, ClawAgentBuilder, ClawAgentConfig};
pub use self::spawn::{spawn_claw_shell, SpawnedAgent};
pub use self::stdio::{run_agent_on_io, run_stdio_agent};
