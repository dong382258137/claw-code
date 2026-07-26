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
//! 本 crate 支持两个互斥的 ACP 版本 feature:
//! - `acp-0_10`(默认):使用 agent-client-protocol 0.10.4,完整实现
//!   `ClawAgent` + `spawn_claw_shell` + `run_stdio_agent` + `lane_bridge`。
//! - `acp-1_5`:使用 agent-client-protocol 1.3.0,仅提供 `ClawAgentV13`
//!   骨架(3 个反向请求方法占位 + LaneEvent 桥接 TODO)。
//!
//! 使用 1.3 时必须 `--no-default-features --features acp-1_5`,
//! 否则两个互斥 feature 会同时启用导致 claw-acp 内部 `use` 别名冲突。

// ---- 0.10.4 路径(默认) ----
// agent.rs / spawn.rs / stdio.rs / lane_bridge.rs 都依赖 0.10.4 的
// `agent_client_protocol` crate 和 `claw-acp` 的 0.10.4 feature。
// 当 `acp-1_5` 启用时不编译这些模块,避免依赖图冲突。
#[cfg(feature = "acp-0_10")]
mod agent;
#[cfg(feature = "acp-0_10")]
mod lane_bridge;
#[cfg(feature = "acp-0_10")]
mod spawn;
#[cfg(feature = "acp-0_10")]
mod stdio;

// ---- 1.3 路径(可选) ----
// ACP 1.3 版本的 ClawAgent 骨架(仅在 acp-1_5 feature 启用时编译)。
// 提供 fs/read_text_file、fs/write_text_file、session/request_permission 三个
// 反向请求方法的占位实现。完整接入是阶段 2 后续工作。
#[cfg(feature = "acp-1_5")]
mod agent_v1_3;

// ---- 公开 re-export ----
// 根据启用的 feature 导出对应类型。两个 feature 同时启用时(不推荐),
// 两组类型都可见,但 claw-acp 内部会因 feature 冲突而编译失败。

#[cfg(feature = "acp-0_10")]
pub use self::agent::{ClawAgent, ClawAgentBuilder, ClawAgentConfig};
#[cfg(feature = "acp-0_10")]
pub use self::spawn::{spawn_claw_shell, SpawnedAgent};
#[cfg(feature = "acp-0_10")]
pub use self::stdio::{run_agent_on_io, run_stdio_agent};
// 导出 lane_bridge 的公开 API:供 ClawAgent 在 run_turn 后调用,刷新 LaneEvent。
#[cfg(feature = "acp-0_10")]
pub use self::lane_bridge::{flush_lane_events_to_acp, lane_event_to_session_update};

// 导出 1.3 骨架的公开类型(仅在 acp-1_5 feature 启用时可见)。
#[cfg(feature = "acp-1_5")]
pub use self::agent_v1_3::{
    ClawAgentV13, PermissionError, PermissionOutcome, PermissionRequest, ReadError, WriteError,
};
