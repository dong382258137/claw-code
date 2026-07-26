//! ACP 1.3 stdio 服务器入口:在 LocalSet 上运行 agent,通过 stdin/stdout 桥接 JSON-RPC。
//!
//! 与 0.10.4 的 `stdio.rs` 对应,但使用 1.3 的 `Agent.builder() + Stdio` API。
//!
//! ## 关键差异
//! - 0.10.4 用 `AgentSideConnection::new(agent, ...)` 直接构造连接
//! - 1.3 用 `Agent.builder().on_receive_dispatch(...).connect_to(Stdio::new())` 链式构造
//! - 1.3 的 `on_receive_dispatch` 闭包接收 `ConnectionTo<Client>`,用于反向请求
//!
//! ## 阶段 2 状态
//!
//! 当前 dispatch handler 是 stub:捕获 `ConnectionTo<Client>` 后,对所有 request
//! 返回 `internal_error`。stage 3 会注册实际的 `InitializeRequest` /
//! `NewSessionRequest` / `PromptRequest` 等 handler。

#![cfg(feature = "acp-1_5")]

use std::io;

use agent_client_protocol_v1 as acp;
use tokio_util::sync::CancellationToken;

use crate::agent_v1_3::ClawAgentV13;

/// 在调用方线程上运行 stdio ACP 1.3 服务器。
///
/// 阻塞当前线程直到:
/// - stdin EOF(客户端关闭)
/// - `cancel` 触发
/// - 发生不可恢复的 IO 错误
///
/// # 流程
/// 1. 创建 `current_thread` runtime + `LocalSet`(agent 持有 `RefCell`,非 `Send`)
/// 2. 在 LocalSet 内调用 [`run_agent_on_io_v1_3`],transport 用 `acp::Stdio::new()`
///
/// # 参数
/// - `agent`:1.3 `ClawAgentV13`(持有 `RefCell`,非 `Send`,必须 LocalSet)
/// - `cancel`:取消令牌;触发时退出 LocalSet
///
/// # 返回
/// - `Ok(())`:stdin EOF 或 cancel 触发,正常退出
/// - `Err(io::Error)`:runtime 创建失败或连接错误
///
/// # 阻塞
/// 此函数会阻塞调用方线程直到退出。调用方应在专用线程或 main 中调用,
/// 不要在 async 上下文中调用(会阻塞 runtime)。
pub fn run_stdio_agent_v1_3<C>(
    agent: ClawAgentV13<C>,
    cancel: CancellationToken,
) -> Result<(), io::Error>
where
    C: runtime::ApiClient + Send + 'static,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, async move {
        run_agent_on_io_v1_3(agent, cancel, acp::Stdio::new()).await
    })
}

/// 在自定义 transport 上运行 ACP 1.3 agent(可测试版本)。
///
/// 与 [`run_stdio_agent_v1_3`] 的区别:接受任意实现了 `ConnectTo<acp::Agent>`
/// 的 transport,用于测试中用 `acp::Channel::duplex()` 替换 stdin/stdout。
///
/// # 运行环境要求
///
/// 必须在 `tokio::task::LocalSet` 上运行(因 `ClawAgentV13` 持有 `RefCell`,
/// 非 `Send`)。调用方负责创建 runtime + LocalSet。
///
/// # 参数
/// - `agent`:1.3 ClawAgent
/// - `cancel`:取消令牌
/// - `transport`:ACP 1.3 transport(如 `acp::Stdio::new()` 或 `acp::Channel`)
///
/// # 返回
/// - `Ok(())`:`transport` EOF 或 cancel 触发
/// - `Err(io::Error)`:连接错误
pub async fn run_agent_on_io_v1_3<C, T>(
    agent: ClawAgentV13<C>,
    cancel: CancellationToken,
    transport: T,
) -> Result<(), io::Error>
where
    C: runtime::ApiClient + Send + 'static,
    T: acp::ConnectTo<acp::Agent> + 'static,
{
    // 1. 取 connection slot,供 Builder 闭包捕获。
    //    slot 内部是 Arc<Mutex<...>>,Send + Sync,可 move 进 Send 闭包。
    let slot = agent.connection_slot();

    // 2. 保留 agent 在作用域内,使其生命周期覆盖整个连接。
    //    agent 持有 RefCell,非 Send,但本函数的 future 不需要 Send
    //    (调用方在 LocalSet 内运行)。反向请求方法通过 slot 共享的
    //    Arc<Mutex<...>> 读取 connection,不需要直接访问 agent。
    let _agent = agent;

    // 3. 构造 1.3 Builder + dispatch handler。
    //    on_receive_dispatch 是 catch-all handler:接收所有未被子 handler
    //    匹配的消息(Request / Notification / Response)。
    //    Stage 2 stub:捕获 ConnectionTo 后,对所有 Request 返回 internal_error。
    //    Stage 3 会注册实际的 InitializeRequest / NewSessionRequest / PromptRequest 等。
    let connection = acp::Agent
        .builder()
        .name("claw-agent-v1-3")
        .on_receive_dispatch(
            async move |msg: acp::Dispatch, cx: acp::ConnectionTo<acp::Client>| {
                // 捕获 ConnectionTo<Client>,供 ClawAgentV13 的反向请求方法使用。
                // 每次收到消息都更新(实际只需首次,但 clone 代价很低)。
                slot.set_connection(cx.clone()).await;

                // Stage 2 stub:Request → internal_error,Notification/Response 静默消费。
                match msg {
                    acp::Dispatch::Request(req, responder) => {
                        tracing::debug!(
                            method = %req.method,
                            "claw-agent-v1-3: stub handler rejecting request"
                        );
                        responder.respond_with_error(acp::util::internal_error(
                            "claw-agent v1.3: handlers not yet implemented (stage 2 skeleton)",
                        ))
                    }
                    acp::Dispatch::Notification(notif) => {
                        tracing::debug!(
                            method = %notif.method,
                            "claw-agent-v1-3: stub handler consuming notification"
                        );
                        Ok(())
                    }
                    acp::Dispatch::Response(result, _router) => {
                        tracing::debug!(
                            "claw-agent-v1-3: stub handler dropping response (err={:?})",
                            result.as_ref().err()
                        );
                        Ok(())
                    }
                }
            },
            acp::on_receive_dispatch!(),
        )
        .connect_to(transport);

    // 4. 等待连接完成或 cancel
    let result = tokio::select! {
        biased;
        r = connection => r,
        _ = cancel.cancelled() => {
            tracing::debug!("claw-agent-v1-3: cancellation received, exiting");
            return Ok(());
        }
    };

    match result {
        Ok(()) => {
            tracing::debug!("claw-agent-v1-3: connection closed");
            Ok(())
        }
        Err(e) => {
            tracing::warn!("claw-agent-v1-3: connection error: {e}");
            Err(io::Error::other(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ApiRequest, AssistantEvent, RuntimeError};

    /// 测试用 `ApiClient`:返回空事件序列。
    struct NullApiClient;
    impl runtime::ApiClient for NullApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(Vec::new())
        }
    }

    /// 类型检查测试:`acp::Channel` 满足 `ConnectTo<acp::Agent>` bound。
    #[test]
    fn channel_satisfies_connect_to_agent_bound() {
        fn _accept<T: acp::ConnectTo<acp::Agent> + 'static>() {}
        _accept::<acp::Channel>();
    }

    /// 验证 cancel 触发后,agent 优雅退出(不依赖 stdin)。
    ///
    /// 使用 `acp::Channel::duplex()` 作为 in-process transport,避免阻塞 stdin。
    /// 立即触发 cancel,验证 `run_agent_on_io_v1_3` 返回 `Ok(())`。
    ///
    /// 不使用 `#[tokio::test]`:因为 `run_agent_on_io_v1_3` 要求在
    /// `LocalSet` 上运行(`ClawAgentV13` 持有 `RefCell`,非 `Send`),
    /// 而在 `tokio::test` 的多线程 runtime 内 `block_on` 会 panic
    /// ("Cannot start a runtime from within a runtime")。
    /// 用独立线程 + `Builder::new_current_thread` 自建 runtime,与
    /// `run_stdio_agent_v1_3` 的实际运行环境保持一致。
    #[test]
    fn cancel_exits_cleanly_via_channel_transport() {
        let cancel = CancellationToken::new();
        // 立即触发 cancel(在 agent 启动前)
        cancel.cancel();

        // 用独立线程跑 runtime + LocalSet,模拟 `run_stdio_agent_v1_3` 的运行环境
        let handle = std::thread::Builder::new()
            .name("test-cancel-exit-v1_3".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let local = tokio::task::LocalSet::new();

                let agent = ClawAgentV13::<NullApiClient>::new();
                let (_client_channel, agent_channel) = acp::Channel::duplex();

                local.block_on(&rt, async move {
                    run_agent_on_io_v1_3(agent, cancel, agent_channel).await
                })
            })
            .expect("failed to spawn test thread");

        // 闭包返回 `Result<(), io::Error>`(`rt` 构建的 `?` 与 `block_on`
        // 的返回值合并),`join()` 外再包一层 thread::Result。
        // - 外层 `.expect(...)` 解 thread::Result → 拿到闭包返回值
        //   (`Result<(), io::Error>`)
        // - 不再 `.expect()` 内层:保留 `Result` 以便 `.is_ok()` 断言
        let result: Result<(), io::Error> = handle
            .join()
            .expect("test thread should not panic");

        assert!(
            result.is_ok(),
            "cancel should result in clean exit, got: {result:?}"
        );
    }
}
