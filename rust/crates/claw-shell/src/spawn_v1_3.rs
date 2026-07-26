//! 在独立线程中启动 1.3 claw agent,通过 stdio 与前端通信。
//!
//! 与 0.10.4 的 `spawn.rs` 对应,但使用 1.3 的 `Agent.builder() + Stdio` API。
//!
//! ## 与 0.10.4 的关键差异
//!
//! - 0.10.4 的 `spawn_claw_shell` 接受 `ClawAgentBuilder<C>`(Send),在线程内
//!   `builder.build(client_gateway)` 构造 agent(非 Send)。
//! - 1.3 的 `spawn_claw_shell_v1_3` 当前不接受 builder,而是在线程内直接
//!   `ClawAgentV13::<C>::new()`。Stage 2 的 agent 是骨架(无 api_client,
//!   无 tool handler),stage 3 引入 `ClawAgentV13Builder` 后会改为接受 builder。
//!
//! ## 为什么 1.3 暂不提供 in-process channel 入口
//!
//! 1.3 的 `acp::Channel::duplex()` 返回 `(Channel, Channel)`,两端都是
//! `ConnectTo<R>` for any `R`。要让前端(非 agent 线程)与 agent 通信,
//! 前端也需要一个 `Client.builder()...connect_to(channel)` 来驱动消息循环。
//! 这要求前端有线程专门跑 Client 的 `connect_to` future。
//!
//! 在 stage 2,我们的重点是让 1.3 agent 能独立跑起来(stdio 模式)。
//! in-process channel 模式留给 stage 3,届时会与 TUI 集成时一起设计。
//!
//! ## 阶段 2 状态
//!
//! `spawn_claw_shell_v1_3` 内部调用 [`crate::stdio_v1_3::run_stdio_agent_v1_3`],
//! 后者使用 stage 2 stub handler(详见 `stdio_v1_3.rs` 的阶段说明)。

#![cfg(feature = "acp-1_5")]

use std::thread;

use tokio_util::sync::CancellationToken;

use crate::agent_v1_3::ClawAgentV13;
use crate::stdio_v1_3::run_stdio_agent_v1_3;

/// 启动 1.3 claw agent 后返回的句柄。
///
/// 与 0.10.4 的 `SpawnedAgent` 相比:
/// - 没有 `channel` 字段(1.3 stage 2 走 stdio,前端通过 stdin/stdout 通信)
/// - 保留 `cancel` 和 `_thread_handle`
///
/// Stage 3 引入 in-process channel 后,会新增 `channel` 字段。
pub struct SpawnedAgentV13 {
    /// 取消令牌:调用 `cancel.cancel()` 通知 agent 线程退出。
    pub cancel: CancellationToken,
    /// agent 线程句柄(调试用,正常不 join)。
    pub _thread_handle: thread::JoinHandle<()>,
}

/// 启动 1.3 claw agent shell(stdio 模式)。
///
/// 在独立线程中创建 `current_thread` tokio runtime + `LocalSet`,
/// 构造 `ClawAgentV13` 并通过 `acp::Stdio` 与前端通信。
///
/// # 为什么在线程内构造 agent
///
/// `ClawAgentV13<C>` 持有 `RefCell<Option<StaticToolExecutor>>`,而
/// `StaticToolExecutor` 内部有 `Box<dyn FnMut>`(非 Send),所以
/// `ClawAgentV13` 是 `!Send`,不能跨线程 move。
///
/// 0.10.4 用 `ClawAgentBuilder<C>`(Send)解决:builder 只持有 Send 数据,
/// 在线程内 `build()` 构造 agent。Stage 2 的 `ClawAgentV13::new()` 不接受
/// 参数,直接在线程内构造即可。Stage 3 引入 builder 后会改为接受 builder。
///
/// # 参数
/// - `parent_cancel`:父级取消令牌,agent 会注册子令牌,父取消时联动退出
///
/// # 返回
/// 成功时返回 [`SpawnedAgentV13`],调用方通过 `cancel` 通知 agent 退出。
///
/// # Panics
/// 内部 `run_stdio_agent_v1_3` 中的 panic 会传播到线程,导致线程退出但
/// 不会 panic 调用方。
pub fn spawn_claw_shell_v1_3<C>(
    parent_cancel: &CancellationToken,
) -> Result<SpawnedAgentV13, std::io::Error>
where
    C: runtime::ApiClient + Send + 'static,
{
    let agent_cancel = parent_cancel.child_token();
    // Clone cancel token:agent 线程持原 token,调用方持 clone。
    // CancellationToken::clone 共享底层取消状态,任一触发都会取消两者。
    let return_cancel = agent_cancel.clone();

    let handle = thread::Builder::new()
        .name("claw-agent-v1-3-worker".into())
        .spawn(move || {
            // 在线程内构造 agent(因 ClawAgentV13 是 !Send,不能跨线程 move)。
            // run_stdio_agent_v1_3 是 sync 函数:内部创建 runtime + LocalSet,
            // 阻塞当前线程直到 stdin EOF 或 cancel 触发。
            let agent = ClawAgentV13::<C>::new();
            if let Err(e) = run_stdio_agent_v1_3(agent, agent_cancel.clone()) {
                tracing::warn!("claw-agent-v1-3-worker: stdio agent exited with error: {e}");
            }
            tracing::debug!("claw-agent-v1-3-worker: exiting");
        })?;

    Ok(SpawnedAgentV13 {
        cancel: return_cancel,
        _thread_handle: handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ApiRequest, AssistantEvent, RuntimeError};

    /// 测试用 `ApiClient`:返回空事件序列。必须 `Send`(`spawn_claw_shell_v1_3` 要求)。
    struct NullApiClient;
    impl runtime::ApiClient for NullApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(Vec::new())
        }
    }

    /// 验证 `spawn_claw_shell_v1_3` 启动后,触发 cancel 能让线程优雅退出。
    ///
    /// 注意:stage 2 的 agent 用 `acp::Stdio` transport,会读取真实 stdin。
    /// 此测试立即 cancel,让 agent 在 stdin 阻塞前就退出,不依赖 stdin 输入。
    #[test]
    fn spawn_and_cancel_exits_cleanly() {
        let cancel = CancellationToken::new();
        let spawned = spawn_claw_shell_v1_3::<NullApiClient>(&cancel).unwrap();

        // 给 agent 一点时间启动(线程 + runtime + LocalSet)
        std::thread::sleep(std::time::Duration::from_millis(150));

        // 取消并验证线程退出
        cancel.cancel();
        let handle = spawned._thread_handle;
        handle
            .join()
            .expect("agent v1_3 thread should not panic on cancel");
    }
}
