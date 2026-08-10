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

    // agent 由本线程持有(Rc),cmd_loop 通过 Rc clone 访问。
    // 关键:若直接把 agent move 进 block_on 闭包,agent 会在 async 上下文被 drop。
    // 生产场景 api_client 内部持有 tokio Runtime(如 AnthropicRuntimeClient),
    // 在 async 上下文 drop 会 panic("Cannot drop a runtime in a context where
    // blocking is not allowed")。
    // 因此:闭包 move `agent`(Rc),block_on 返回后主线程才 drop `agent_held`(另一个
    // Rc clone)。`run_agent_on_io_v1_3` 结束只减少自身引用计数,agent 内容存活到
    // block_on 之后的非 async 上下文,由本线程(主线程)释放。
    let agent = std::rc::Rc::new(agent);
    let agent_held = std::rc::Rc::clone(&agent);
    let result = local.block_on(&rt, async move {
        run_agent_on_io_v1_3(agent, cancel, acp::Stdio::new()).await
    });
    drop(agent_held);
    result
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
/// # agent 所有权
///
/// 参数为 `Rc<ClawAgentV13<C>>`:agent 由调用方(通常是 [`run_stdio_agent_v1_3`])
/// 持有,本函数内部仅保留 Rc clone。这样 agent(及其持有的 api_client 内部
/// tokio Runtime)会在调用方线程、非 async 上下文 drop,避免
/// "Cannot drop a runtime in a context where blocking is not allowed" panic。
///
/// # 参数
/// - `agent`:1.3 ClawAgent(Rc 共享所有权)
/// - `cancel`:取消令牌
/// - `transport`:ACP 1.3 transport(如 `acp::Stdio::new()` 或 `acp::Channel`)
///
/// # 返回
/// - `Ok(())`:`transport` EOF 或 cancel 触发
/// - `Err(io::Error)`:连接错误
pub async fn run_agent_on_io_v1_3<C, T>(
    agent: std::rc::Rc<ClawAgentV13<C>>,
    cancel: CancellationToken,
    transport: T,
) -> Result<(), io::Error>
where
    C: runtime::ApiClient + Send + 'static,
    T: acp::ConnectTo<acp::Agent> + 'static,
{
    use acp::schema::v1 as schema;
    use tokio::sync::{mpsc, oneshot};

    use crate::agent_v1_3::AgentCommand;

    // 1. 取 connection slot,供 Builder 闭包捕获。
    //    slot 内部是 Arc<Mutex<...>>,Send + Sync,可 move 进 Send 闭包。
    let slot = agent.connection_slot();

    // 2. 命令通道:dispatch handler(Send 闭包)通过 channel 把请求转发给
    //    持有 agent 的命令循环(agent 非 Send,不能跨线程 move)。
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();
    let cmd_new_session = cmd_tx.clone();
    let cmd_prompt = cmd_tx.clone();
    let cmd_cancel = cmd_tx.clone();

    // `async move` 闭包会 move 捕获的整个变量,所以为每个需要 slot 的
    // 闭包提前 clone 一份(ClawAgentV13ConnectionSlot 是 cheap Arc clone)。
    let slot_new_session = slot.clone();
    let slot_cancel = slot.clone();
    let slot_dispatch = slot.clone();

    // 3. 构造 1.3 Builder + dispatch handler(Stage 3:真实 handler)。
    //    - initialize / authenticate:直接本地响应,不需要 agent
    //    - session/new / session/prompt:经 channel 转发给命令循环
    //    - session/cancel:经 channel 转发,触发 abort signal
    //    - catch-all dispatch:未匹配消息返回 method_not_found
    let connection = acp::Agent
        .builder()
        .name("claw-agent-v1-3")
        .on_receive_request(
            async move |req: schema::InitializeRequest, responder, cx| {
                // 捕获 ConnectionTo<Client>,供反向请求方法使用。
                slot.set_connection(cx.clone()).await;
                tracing::debug!("claw-agent-v1-3: initialize");
                let resp = schema::InitializeResponse::new(req.protocol_version).auth_methods(
                    vec![schema::AuthMethod::Agent(schema::AuthMethodAgent::new(
                        schema::AuthMethodId::new("api_key"),
                        "API Key",
                    ))],
                );
                responder.respond(resp)
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: schema::AuthenticateRequest, responder, _cx| {
                // 本地无真实认证,直接返回 success(与 0.10.4 一致)。
                responder.respond(schema::AuthenticateResponse::new())
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::NewSessionRequest, responder, cx| {
                slot_new_session.set_connection(cx.clone()).await;
                let (tx, rx) = oneshot::channel();
                if cmd_new_session
                    .send(AgentCommand::NewSession {
                        cwd: req.cwd.clone(),
                        tx,
                    })
                    .is_err()
                {
                    return responder.respond_with_internal_error("agent loop closed");
                }
                match rx.await {
                    Ok(Ok(session_id)) => {
                        responder.respond(schema::NewSessionResponse::new(session_id))
                    }
                    Ok(Err(e)) => responder.respond_with_internal_error(e),
                    Err(_) => {
                        responder.respond_with_internal_error("session creation cancelled")
                    }
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::PromptRequest, responder, _cx| {
                let (tx, rx) = oneshot::channel();
                let text = extract_prompt_text_v1_3(&req.prompt);
                if cmd_prompt
                    .send(AgentCommand::Prompt {
                        session_id: req.session_id.clone(),
                        prompt: text,
                        tx,
                    })
                    .is_err()
                {
                    return responder.respond_with_internal_error("agent loop closed");
                }
                match rx.await {
                    Ok(Ok(stop_reason)) => {
                        responder.respond(schema::PromptResponse::new(stop_reason))
                    }
                    Ok(Err(e)) => responder.respond_with_internal_error(e),
                    Err(_) => responder.respond_with_internal_error("prompt cancelled"),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: schema::CancelNotification, cx| {
                slot_cancel.set_connection(cx.clone()).await;
                let _ = cmd_cancel.send(AgentCommand::Cancel {
                    session_id: notif.session_id,
                });
                Ok(())
            },
            acp::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |msg: acp::Dispatch, cx: acp::ConnectionTo<acp::Client>| {
                slot_dispatch.set_connection(cx.clone()).await;
                tracing::debug!(
                    "claw-agent-v1-3: unhandled dispatch (method={:?})",
                    msg.method()
                );
                msg.respond_with_error(
                    acp::util::internal_error("claw-agent v1.3: unhandled message"),
                    cx,
                )
            },
            acp::on_receive_dispatch!(),
        )
        .connect_to(transport);

    // 4. 命令循环:持有 agent,串行处理 NewSession / Prompt / Cancel。
    //    agent 非 Send,此 future 与 connection 在同一 LocalSet 内并发。
    let cmd_loop = async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                AgentCommand::NewSession { cwd, tx } => {
                    let result = agent.create_session(cwd);
                    let _ = tx.send(result.map_err(|e| e.to_string()));
                }
                AgentCommand::Prompt {
                    session_id,
                    prompt,
                    tx,
                } => {
                    let result = agent.run_prompt(&session_id, prompt).await;
                    let _ = tx.send(result.map_err(|e| e.to_string()));
                }
                AgentCommand::Cancel { session_id } => {
                    agent.cancel(&session_id);
                }
            }
        }
    };

    // 5. 并发运行 connection + 命令循环,响应 cancel token。
    //    connection 借用 cmd_tx(闭包捕获),cmd_loop 借用 agent 与 cmd_rx,
    //    两者无借用冲突。
    tokio::pin!(connection);
    tokio::pin!(cmd_loop);
    let result = tokio::select! {
        biased;
        r = &mut connection => r,
        _ = cancel.cancelled() => {
            tracing::debug!("claw-agent-v1-3: cancellation received, exiting");
            return Ok(());
        }
        _ = &mut cmd_loop => {
            // 所有 sender drop(connection 关闭)时命令循环自然结束。
            tracing::debug!("claw-agent-v1-3: command channel closed, exiting");
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

/// 从 `Vec<ContentBlock>` 中提取纯文本(与 0.10.4 `extract_user_text` 对齐)。
///
/// 本地 `run_turn` 只接受 `String`,所以把所有 Text block 拼接。
/// 非 Text block(图片 / Resource 等)本期忽略。
fn extract_prompt_text_v1_3(prompt: &[acp::schema::v1::ContentBlock]) -> String {
    prompt
        .iter()
        .filter_map(|block| match block {
            acp::schema::v1::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_v1_3::ClawAgentV13Builder;
    use runtime::{ApiRequest, AssistantEvent, RuntimeError};

    /// 测试用 `ApiClient`:返回空事件序列。
    struct NullApiClient;
    impl runtime::ApiClient for NullApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(Vec::new())
        }
    }

    /// 测试用 `ApiClient`:返回固定文本 + MessageStop,让 turn 正常完成。
    struct EchoApiClient;
    impl runtime::ApiClient for EchoApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(vec![
                AssistantEvent::TextDelta("hello from e2e".to_string()),
                AssistantEvent::MessageStop,
            ])
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
                    run_agent_on_io_v1_3(std::rc::Rc::new(agent), cancel, agent_channel).await
                })
            })
            .expect("failed to spawn test thread");

        // 闭包返回 `Result<(), io::Error>`(`rt` 构建的 `?` 与 `block_on`
        // 的返回值合并),`join()` 外再包一层 thread::Result。
        // - 外层 `.expect(...)` 解 thread::Result → 拿到闭包返回值
        //   (`Result<(), io::Error>`)
        // - 不再 `.expect()` 内层:保留 `Result` 以便 `.is_ok()` 断言
        let result: Result<(), io::Error> = handle.join().expect("test thread should not panic");

        assert!(
            result.is_ok(),
            "cancel should result in clean exit, got: {result:?}"
        );
    }

    /// 端到端握手测试:模拟 IDE 客户端,通过 `acp::Channel::duplex()` 走完整
    /// `initialize → session/new → session/prompt` 链路(Stage 3 收尾)。
    ///
    /// 验证:
    /// 1. `initialize` 返回 `protocolVersion` + `authMethods`(本地认证直通);
    /// 2. `session/new` 返回 `sessionId`(命令循环真实创建会话);
    /// 3. `session/prompt` 返回 `stopReason = end_turn`(agent 真实运行 turn)。
    ///
    /// 与 [`cancel_exits_cleanly_via_channel_transport`] 相同,在独立线程 +
    /// `current_thread` runtime + `LocalSet` 中运行(`ClawAgentV13` 非 Send)。
    #[test]
    fn e2e_handshake_initialize_session_new_prompt() {
        use acp::schema::v1 as schema;

        let cancel = CancellationToken::new();

        let handle = std::thread::Builder::new()
            .name("test-e2e-handshake-v1_3".into())
            .spawn(move || -> Result<(), io::Error> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let local = tokio::task::LocalSet::new();

                local.block_on(&rt, async move {
                    let (client_channel, agent_channel) = acp::Channel::duplex();

                    // agent:用 Builder 注入 api_client + tool_executor
                    let cwd = std::env::temp_dir().join("claw-e2e-handshake");
                    let _ = std::fs::create_dir_all(&cwd);
                    let agent = ClawAgentV13Builder::new(
                        EchoApiClient,
                        runtime::PermissionPolicy::new(runtime::PermissionMode::WorkspaceWrite),
                        Vec::new(),
                    )
                    .build();
                    let agent_handle = tokio::task::spawn_local(run_agent_on_io_v1_3(
                        std::rc::Rc::new(agent),
                        cancel,
                        agent_channel,
                    ));

                    let mut client = client_channel;

                    // 1. initialize → 期望 protocolVersion + authMethods
                    let init_msg = acp::RawJsonRpcMessage::request(
                        "initialize".to_string(),
                        serde_json::json!({ "protocolVersion": 1 }),
                        schema::RequestId::Number(1),
                    )
                    .map_err(|e| io::Error::other(e.to_string()))?;
                    client
                        .tx
                        .unbounded_send(Ok(init_msg))
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    let init_result =
                        recv_typed_response(&mut client.rx, &schema::RequestId::Number(1)).await;
                    assert_eq!(
                        init_result["protocolVersion"].as_i64(),
                        Some(1),
                        "initialize protocolVersion mismatch: {init_result}"
                    );
                    assert!(
                        init_result["authMethods"].is_array(),
                        "initialize should return authMethods: {init_result}"
                    );

                    // 2. session/new → 期望 sessionId
                    let cwd_str = cwd.to_string_lossy().replace('\\', "/");
                    let new_msg = acp::RawJsonRpcMessage::request(
                        "session/new".to_string(),
                        serde_json::json!({ "cwd": cwd_str, "mcpServers": [] }),
                        schema::RequestId::Number(2),
                    )
                    .map_err(|e| io::Error::other(e.to_string()))?;
                    client
                        .tx
                        .unbounded_send(Ok(new_msg))
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    let new_result =
                        recv_typed_response(&mut client.rx, &schema::RequestId::Number(2)).await;
                    let session_id = new_result["sessionId"]
                        .as_str()
                        .expect("session/new should return sessionId")
                        .to_string();
                    assert!(!session_id.is_empty(), "sessionId should be non-empty");

                    // 3. session/prompt → 期望 stopReason = end_turn
                    let prompt_msg = acp::RawJsonRpcMessage::request(
                        "session/prompt".to_string(),
                        serde_json::json!({
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": "hello" }],
                        }),
                        schema::RequestId::Number(3),
                    )
                    .map_err(|e| io::Error::other(e.to_string()))?;
                    client
                        .tx
                        .unbounded_send(Ok(prompt_msg))
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    let prompt_result =
                        recv_typed_response(&mut client.rx, &schema::RequestId::Number(3)).await;
                    assert_eq!(
                        prompt_result["stopReason"].as_str(),
                        Some("end_turn"),
                        "prompt should return end_turn: {prompt_result}"
                    );

                    // 关闭 client 侧,agent 应随连接关闭自然退出
                    drop(client);
                    let _ = agent_handle.await;
                    Ok(())
                })
            })
            .expect("failed to spawn test thread");

        let result: Result<(), io::Error> = handle.join().expect("test thread should not panic");
        assert!(result.is_ok(), "e2e handshake failed: {result:?}");
    }

    /// 从 client channel 读取与 `expected_id` 匹配的响应 result。
    ///
    /// 跳过中间可能出现的 `session/update` notification(如 agent 推送的
    /// AgentMessageChunk),只返回匹配 id 的 Response result。
    async fn recv_typed_response(
        rx: &mut (impl futures::Stream<Item = Result<acp::RawJsonRpcMessage, acp::Error>> + Unpin),
        expected_id: &acp::schema::v1::RequestId,
    ) -> serde_json::Value {
        use futures::StreamExt;
        use acp::schema::v1 as schema;

        loop {
            let msg = rx
                .next()
                .await
                .expect("channel closed before receiving response")
                .expect("transport error before receiving response");
            match msg {
                acp::RawJsonRpcMessage::Response(schema::Response::Result { id, result })
                    if &id == expected_id =>
                {
                    return result;
                }
                acp::RawJsonRpcMessage::Response(schema::Response::Error { id, error })
                    if &id == expected_id =>
                {
                    panic!("rpc error for id {id:?}: {error:?}");
                }
                // 其他消息(notification / 其他 id 的响应):跳过
                _ => continue,
            }
        }
    }
}
