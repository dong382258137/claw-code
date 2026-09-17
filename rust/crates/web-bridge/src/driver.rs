//! 最小 agent 驱动器:复用 claw-shell 的 `spawn_claw_shell` + ACP 握手,
//! 跑一个完整 prompt(内部是多轮工具循环),返回最终回复文本。
//!
//! im-bridge 的 `SessionManager` 已封装同样的流程,但其 `api_client` 字段
//! 被固定为 `BridgeApiClient`,无法注入网页版客户端;此处是它的通用化最小版,
//! 只保留 M0 需要的部分(会话管理 / 命令 / 持久化留到 M2)。

use std::time::Duration;

use agent_client_protocol as acp;
use claw_acp::{acp_send, AcpClientMessage};
use claw_shell::{spawn_claw_shell, ClawAgentBuilder};
use runtime::{ApiClient, PermissionMode, PermissionPolicy};
use tokio_util::sync::CancellationToken;

/// 完成信号:通知通道静默多久判定本轮结束(不猜固定时长)。
const SETTLE_TIMEOUT_MS: u64 = 300;

/// 驱动一个 claw agent 完成一轮完整 prompt(含内部多轮工具调用)。
/// 返回 agent 最终输出文本。
///
/// # 错误
/// ACP 握手或 prompt 失败时返回错误描述字符串。
pub async fn run_prompt<C>(
    api_client: C,
    system_prompt: Vec<String>,
    user_text: &str,
) -> Result<String, String>
where
    C: ApiClient + Send + 'static,
{
    let policy = PermissionPolicy::new(PermissionMode::Allow);
    let builder = ClawAgentBuilder::new(api_client, policy, system_prompt)
        .with_tool_setup(crate::tools::register_default_tools);
    let cancel = CancellationToken::new();
    let spawned = spawn_claw_shell(builder, &cancel).map_err(|e| format!("spawn agent: {e}"))?;

    let tx = spawned.channel.tx.clone();
    let mut rx = spawned.channel.rx;

    // ACP 握手:initialize → authenticate → new_session(agent 要求 new_session
    // 之后才能 prompt,否则返回 "no active session")。
    acp_send(
        acp::InitializeRequest::new(acp::ProtocolVersion::LATEST),
        &tx,
    )
    .await
    .map_err(|e| format!("initialize failed: {e}"))?;
    acp_send(
        acp::AuthenticateRequest::new(acp::AuthMethodId::new("api_key")),
        &tx,
    )
    .await
    .map_err(|e| format!("authenticate failed: {e}"))?;
    let session = acp_send(
        acp::NewSessionRequest::new(std::env::current_dir().unwrap_or_default()),
        &tx,
    )
    .await
    .map_err(|e| format!("new_session failed: {e}"))?;

    // prompt 为 oneshot 语义:阻塞到整轮完成(agent.rs::prompt 内部多轮工具循环,
    // 每轮经 ApiClient::stream 取回复、解析工具调用、本地执行、直到 MessageStop)。
    let prompt_req = acp::PromptRequest::new(
        session.session_id,
        vec![acp::ContentBlock::Text(acp::TextContent::new(user_text))],
    );
    acp_send(prompt_req, &tx)
        .await
        .map_err(|e| format!("prompt failed: {e}"))?;

    // 最终文本不装在 PromptResponse 里,而是经 SessionNotification 流式推送。
    // 完成信号:通道静默 300ms 判定本轮结束。
    let mut final_text = String::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(SETTLE_TIMEOUT_MS), rx.recv()).await {
            Ok(Some(msg)) => {
                if let AcpClientMessage::SessionNotification(args) = &msg {
                    if let acp::SessionUpdate::AgentMessageChunk(chunk) = &args.request.update {
                        if let acp::ContentBlock::Text(text) = &chunk.content {
                            final_text.push_str(&text.text);
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    cancel.cancel();
    Ok(final_text)
}
