//! 在独立线程中启动 claw agent,返回 ACP channel 供前端通信。
//!
//! 参考:grok-build `xai-grok-pager/src/acp/spawn.rs::spawn_agent_thread_direct`。
//! 关键模式:
//! - 独立 OS 线程 + `current_thread` runtime + `LocalSet`
//!   (因为 `ClawAgent` 持有 `Rc<RefCell<...>>`,非 `Send`)
//! - `CancellationToken` 控制生命周期,不 join 线程
//! - mpsc channel 双向通信

use std::rc::Rc;
use std::thread;

use tokio_util::sync::CancellationToken;

use claw_acp::{AcpClientChannel, AcpGatewayReceiver, AcpGatewaySender, acp_channels};

use crate::agent::ClawAgentBuilder;

/// 启动 agent 后返回的句柄。前端持有 `channel` 与 agent 通信,
/// `cancel` 用于优雅停止 agent 线程。
///
/// `_thread_handle` 保留但不 join:agent 线程通过 `cancel.cancelled()`
/// 主动退出,join 会阻塞前端。
pub struct SpawnedAgent {
    /// 与 agent 通信的 ACP channel(前端侧)。
    pub channel: AcpClientChannel,
    /// 取消令牌:调用 `cancel.cancel()` 通知 agent 线程退出。
    pub cancel: CancellationToken,
    /// agent 线程句柄(调试用,正常不 join)。
    pub _thread_handle: thread::JoinHandle<()>,
}

/// 启动 claw agent shell。
///
/// 在独立线程中创建 `current_thread` tokio runtime + `LocalSet`,
/// 构造 `ClawAgent` 并运行 `AcpGatewayReceiver::run()` 消费 ACP 请求。
///
/// # 参数
/// - `builder`:agent 构造器(已配置 api_client + 系统 prompt)
/// - `parent_cancel`:父级取消令牌,agent 会注册子令牌,父取消时联动退出
///
/// # 返回
/// 成功时返回 [`SpawnedAgent`],前端通过 `.channel` 与 agent 通信。
///
/// # 要求
/// - `C: ApiClient + Send`:api_client 必须可跨线程移动(线程 spawn 要求)
/// - `StaticToolExecutor` 在线程内创建(非 Send,不能跨线程)
///
/// # Panics
/// 内部 `LocalSet::block_on` 中的 panic 会传播到线程,导致线程退出但
/// 不会 panic 调用方。前端通过 channel 收到 `SendFailed` 错误感知。
pub fn spawn_claw_shell<C>(
    builder: ClawAgentBuilder<C>,
    parent_cancel: &CancellationToken,
) -> Result<SpawnedAgent, std::io::Error>
where
    C: runtime::ApiClient + Send + 'static,
{
    let (acp_client, acp_agent) = acp_channels();
    let agent_cancel = parent_cancel.child_token();

    // 在 move 到线程前,先从 acp_agent.tx clone 一份用于 agent 回推 notification。
    // acp_agent.tx 发送 AcpClientMessage(SessionNotification 等),正好匹配
    // AcpGatewaySender<acp::AgentSide>::OutMessage(= AcpClientMessage)。
    // grok-build 在 LocalSet 内 clone,这里因 acp_agent 要 move,提前 clone。
    let client_gateway: AcpGatewaySender<agent_client_protocol::AgentSide> =
        AcpGatewaySender::new(acp_agent.tx.clone()).with_tracing(true);

    let handle = thread::Builder::new()
        .name("claw-agent-worker".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build agent runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                // 在 LocalSet 内构造 agent(Rc 非 Send,必须 LocalSet)
                // StaticToolExecutor 在 build() 内创建
                let agent = builder.build(client_gateway);
                let agent_rc = Rc::new(agent);

                let gw_rx = AcpGatewayReceiver::new(acp_agent.rx, agent_rc).with_tracing(true);
                tokio::task::spawn_local(gw_rx.run());

                // yield 一次,让 gateway receiver 先注册到 LocalSet
                tokio::task::yield_now().await;

                // 阻塞直到取消
                agent_cancel.cancelled().await;
                tracing::debug!("claw-agent-worker: cancellation received, exiting");
            });
        })?;

    Ok(SpawnedAgent {
        channel: acp_client,
        cancel: parent_cancel.child_token(),
        _thread_handle: handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use claw_acp::acp_send;
    use agent_client_protocol as acp;
    use runtime::{
        ApiClient, ApiRequest, AssistantEvent, PermissionMode, PermissionPolicy, RuntimeError,
    };

    /// 简单的 mock ApiClient:返回单个空 assistant 事件。
    /// 必须是 `Send`(spawn_claw_shell 要求)。
    struct MockApiClient {
        events: Vec<AssistantEvent>,
    }

    impl MockApiClient {
        fn new() -> Self {
            Self {
                events: vec![
                    AssistantEvent::TextDelta("mock response".into()),
                    AssistantEvent::MessageStop,
                ],
            }
        }
    }

    impl ApiClient for MockApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(self.events.clone())
        }
    }

    #[test]
    fn spawn_and_cancel_exits_cleanly() {
        let cancel = CancellationToken::new();
        let builder = ClawAgentBuilder::new(
            MockApiClient::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["test system prompt".to_string()],
        );
        let spawned = spawn_claw_shell(builder, &cancel).unwrap();

        // 给 agent 一点时间启动
        std::thread::sleep(Duration::from_millis(150));

        // 取消并验证线程退出
        cancel.cancel();
        let handle = spawned._thread_handle;
        handle.join().expect("agent thread should not panic");
    }

    #[tokio::test]
    async fn initialize_handshake_works() {
        let cancel = CancellationToken::new();
        let builder = ClawAgentBuilder::new(
            MockApiClient::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["test system prompt".to_string()],
        );
        let spawned = spawn_claw_shell(builder, &cancel).unwrap();

        // 发送 initialize 请求(InitializeRequest::new 接受 ProtocolVersion)
        let init_req = acp::InitializeRequest::new(acp::ProtocolVersion::LATEST);
        let init_resp = acp_send(init_req, &spawned.channel.tx).await;

        // 取消并清理
        cancel.cancel();
        let _ = spawned._thread_handle.join();

        // 验证 initialize 响应
        let init_resp = init_resp.expect("initialize should succeed");
        assert!(
            !init_resp.auth_methods.is_empty(),
            "agent should expose at least one auth method"
        );
    }

    /// 完整握手流程:initialize → authenticate → new_session → prompt
    /// 并验证能收到 SessionNotification(AgentMessageChunk)。
    #[tokio::test]
    async fn full_handshake_and_prompt_works() {
        let cancel = CancellationToken::new();
        let builder = ClawAgentBuilder::new(
            MockApiClient::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["test system prompt".to_string()],
        );
        let spawned = spawn_claw_shell(builder, &cancel).unwrap();
        let tx = spawned.channel.tx.clone();
        let mut rx = spawned.channel.rx;

        // 后台 task 接收 notification
        let notif_task = tokio::spawn(async move {
            let mut received_text = String::new();
            while let Some(msg) = rx.recv().await {
                if let claw_acp::AcpClientMessage::SessionNotification(args) = msg {
                    if let acp::SessionUpdate::AgentMessageChunk(chunk) = &args.request.update {
                        if let acp::ContentBlock::Text(text) = &chunk.content {
                            received_text.push_str(&text.text);
                        }
                    }
                }
            }
            received_text
        });

        // 1. initialize
        let init_resp = acp_send(
            acp::InitializeRequest::new(acp::ProtocolVersion::LATEST),
            &tx,
        )
        .await
        .expect("initialize should succeed");
        assert!(!init_resp.auth_methods.is_empty());

        // 2. authenticate
        let auth_resp = acp_send(
            acp::AuthenticateRequest::new(acp::AuthMethodId::new("api_key")),
            &tx,
        )
        .await
        .expect("authenticate should succeed");
        let _ = auth_resp;

        // 3. new_session
        let session_resp = acp_send(
            acp::NewSessionRequest::new(std::env::current_dir().unwrap()),
            &tx,
        )
        .await
        .expect("new_session should succeed");
        let session_id = session_resp.session_id;

        // 4. prompt
        let prompt_req = acp::PromptRequest::new(
            session_id.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new("hello"))],
        );
        let prompt_resp = acp_send(prompt_req, &tx)
            .await
            .expect("prompt should succeed");
        assert_eq!(prompt_resp.stop_reason, acp::StopReason::EndTurn);

        // 取消 agent,让 notification channel 关闭
        cancel.cancel();
        let _ = spawned._thread_handle.join();

        // 等待 notification task 完成,验证收到了 assistant 文本
        let received_text = notif_task.await.expect("notif task should not panic");
        assert!(
            !received_text.is_empty(),
            "should receive at least one AgentMessageChunk"
        );
    }

    /// A6.3:验证 `CancelNotification` 能被 agent 接收并返回 `Ok(())`。
    ///
    /// **已知限制**:`ClawAgent::cancel` 当前是 stub(`agent.rs:305`),
    /// 仅记录日志并返回 `Ok(())`,不实际中断正在进行的 `run_turn`(因
    /// `run_turn` 是同步阻塞 API)。此测试验证协议层路径完整(channel
    /// 路由 + response 回传),并固化 stub 契约 —— 未来实现真实取消时,
    /// 此测试无需修改(响应类型仍是 `()`),但应新增"prompt 中途 cancel
    /// 后 prompt 返回 Cancelled stop_reason"的测试。
    ///
    /// 测试流程:initialize → new_session → cancel(无活跃 prompt)→ 验证 Ok。
    #[tokio::test]
    async fn cancel_notification_returns_ok_without_active_prompt() {
        let cancel = CancellationToken::new();
        let builder = ClawAgentBuilder::new(
            MockApiClient::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["test system prompt".to_string()],
        );
        let spawned = spawn_claw_shell(builder, &cancel).unwrap();
        let tx = spawned.channel.tx.clone();

        // 1. initialize
        let _ = acp_send(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST), &tx)
            .await
            .expect("initialize should succeed");

        // 2. authenticate
        let _ = acp_send(
            acp::AuthenticateRequest::new(acp::AuthMethodId::new("api_key")),
            &tx,
        )
        .await
        .expect("authenticate should succeed");

        // 3. new_session
        let session_resp = acp_send(
            acp::NewSessionRequest::new(std::env::current_dir().unwrap()),
            &tx,
        )
        .await
        .expect("new_session should succeed");
        let session_id = session_resp.session_id;

        // 4. 发送 CancelNotification(无活跃 prompt,应立即返回 Ok)
        let cancel_result = acp_send(acp::CancelNotification::new(session_id), &tx).await;
        assert!(
            cancel_result.is_ok(),
            "cancel should return Ok(()) even without active prompt, got: {cancel_result:?}"
        );

        // 清理
        cancel.cancel();
        let _ = spawned._thread_handle.join();
    }
}
