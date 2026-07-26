//! [`acp::Agent`] trait 实现,适配本地 [`runtime::ConversationRuntime`]。
//!
//! 架构参考:grok-build `xai-grok-shell/src/agent/mvp_agent/acp_agent.rs`。
//!
//! 关键差异:
//! - 本地 `run_turn` 是同步阻塞 API,通过 `tokio::task::block_in_place` 包裹
//! - 本地 `run_turn` 需要 `&mut self`,通过 `RefCell` 提供内部可变性
//! - session 状态保存在 agent 内部,ACP `session_id` 与 `Session::id` 一一对应
//! - 流式输出通过 `AcpGatewaySender<AgentSide>` 主动推送 `SessionNotification`
//!
//! 关于 `AcpGatewaySender<acp::AgentSide>` 的方向说明:
//! - `AcpSide` for `acp::AgentSide`:OutMessage = AcpClientMessage
//! - agent 向 client 推送 notification(SessionNotification 是 AcpClientMessage 一种)
//! - 因此 agent 持有 `AcpGatewaySender<acp::AgentSide>`,其 OutMessage 正好匹配

use std::cell::RefCell;

use agent_client_protocol as acp;
use async_trait::async_trait;
use runtime::{ApiClient, ConversationRuntime, PermissionPolicy, Session, StaticToolExecutor};

use claw_acp::AcpGatewaySender;

/// Agent 构造配置(供 builder 使用)。
pub struct ClawAgentConfig {
    pub system_prompt: Vec<String>,
    pub permission_policy: PermissionPolicy,
}

/// claw agent:封装 `ConversationRuntime` 并实现 `acp::Agent`。
///
/// 持有 `RefCell<Option<ConversationRuntime>>`:
/// - `initialize` / `authenticate` 阶段为 `None`
/// - `new_session` 时创建 runtime 并存入
/// - `prompt` / `cancel` 操作 runtime
///
/// `client_gateway` 用于向前端推送 `SessionNotification`(流式 chunk、tool call 等)。
pub struct ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// 当前活跃的 runtime。initialize 阶段为 None,new_session 后创建。
    runtime: RefCell<Option<ConversationRuntime<C, StaticToolExecutor>>>,
    /// 配置(在 new_session 时使用)。
    config: ClawAgentConfig,
    /// API client(在 new_session 时移入 runtime,故用 Option)。
    api_client: RefCell<Option<C>>,
    /// Tool executor(在 new_session 时移入 runtime,故用 Option)。
    /// 在 build() 内创建,避免 builder 持有非 Send 类型。
    tool_executor: RefCell<Option<StaticToolExecutor>>,
    /// 推送到前端的 gateway sender。
    /// S = acp::AgentSide:agent 视角,OutMessage = AcpClientMessage。
    client_gateway: AcpGatewaySender<acp::AgentSide>,
}

/// Builder:在 spawn 线程外构造,然后 `build()` 在 LocalSet 内完成。
///
/// `ClawAgentBuilder` 只持有 `Send` 数据(api_client + 配置),
/// `StaticToolExecutor` 在 `build()` 内创建(因其内部 `Box<dyn FnMut>` 非 Send)。
pub struct ClawAgentBuilder<C>
where
    C: ApiClient + Send + 'static,
{
    api_client: C,
    config: ClawAgentConfig,
    /// Optional tool handler setup, called inside `build()` (LocalSet context)
    /// to register handlers on the `StaticToolExecutor` before it's consumed.
    tool_setup: Option<Box<dyn FnOnce(&mut StaticToolExecutor) + Send>>,
}

impl<C> ClawAgentBuilder<C>
where
    C: ApiClient + Send + 'static,
{
    /// 创建 builder。
    ///
    /// # 参数
    /// - `api_client`:上游 API 客户端(如 Anthropic / OpenAI),必须 `Send`
    /// - `permission_policy`:权限策略
    /// - `system_prompt`:系统提示词
    pub fn new(
        api_client: C,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
    ) -> Self {
        Self {
            api_client,
            config: ClawAgentConfig {
                system_prompt,
                permission_policy,
            },
            tool_setup: None,
        }
    }

    /// Register a setup closure that populates the `StaticToolExecutor` with
    /// tool handlers. Called inside `build()` (within the LocalSet), so the
    /// `!Send` `FnMut` handlers are created in the correct thread context.
    ///
    /// The closure itself must be `Send` (it's stored in the builder before
    /// the thread spawn), but the handlers it creates may be `!Send`.
    #[must_use]
    pub fn with_tool_setup<F>(mut self, setup: F) -> Self
    where
        F: FnOnce(&mut StaticToolExecutor) + Send + 'static,
    {
        self.tool_setup = Some(Box::new(setup));
        self
    }

    /// 在 LocalSet 内构造 `ClawAgent`。
    ///
    /// `client_gateway` 由 `spawn_claw_shell` 从 ACP gateway 中取出注入。
    /// `StaticToolExecutor` 在此创建(非 Send,必须在线程内)。
    pub(crate) fn build(self, client_gateway: AcpGatewaySender<acp::AgentSide>) -> ClawAgent<C> {
        let mut tool_executor = StaticToolExecutor::new();
        if let Some(setup) = self.tool_setup {
            setup(&mut tool_executor);
        }
        ClawAgent {
            runtime: RefCell::new(None),
            config: self.config,
            api_client: RefCell::new(Some(self.api_client)),
            tool_executor: RefCell::new(Some(tool_executor)),
            client_gateway,
        }
    }
}

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// 向前端推送 `SessionNotification`(fire-and-forget)。
    fn notify(&self, session_id: &acp::SessionId, update: acp::SessionUpdate) {
        let notif = acp::SessionNotification::new(session_id.clone(), update);
        // 走 gateway 的 fire-and-forget 路径,前端 channel 关闭不阻塞 agent
        self.client_gateway.forward_fire_and_forget(notif);
    }

    /// 从 `acp::PromptRequest.prompt`(Vec<ContentBlock>)中提取纯文本。
    ///
    /// 本地 `run_turn` 只接受 `String`,所以把所有 Text block 拼接。
    /// 非 Text block(图片/资源等)本期忽略。
    fn extract_user_text(prompt: &[acp::ContentBlock]) -> String {
        prompt
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 从 runtime 中提取 assistant 文本(取最后一条 assistant 消息的所有 Text block)。
    fn extract_assistant_text(runtime: &ConversationRuntime<C, StaticToolExecutor>) -> String {
        runtime
            .session()
            .messages
            .iter()
            .rev()
            .find(|m| m.role == runtime::MessageRole::Assistant)
            .map(|m| {
                m.blocks
                    .iter()
                    .filter_map(|b| match b {
                        runtime::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

#[async_trait(?Send)]
impl<C> acp::Agent for ClawAgent<C>
where
    C: ApiClient + 'static,
{
    async fn initialize(
        &self,
        _arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        tracing::debug!("claw-agent: initialize");
        let mut resp = acp::InitializeResponse::new(acp::ProtocolVersion::LATEST);
        // 暴露一个简单的 "api_key" auth method(本地无真实认证)
        resp.auth_methods = vec![acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
            acp::AuthMethodId::new("api_key"),
            "API Key",
        ))];
        Ok(resp)
    }

    async fn authenticate(
        &self,
        _arguments: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        tracing::debug!("claw-agent: authenticate");
        // 本地无真实认证,直接返回 success
        Ok(acp::AuthenticateResponse::new())
    }

    async fn new_session(
        &self,
        arguments: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        tracing::debug!("claw-agent: new_session cwd={:?}", arguments.cwd);

        // 取出 api_client 和 tool_executor(从 Option 中移出)
        let api_client = self.api_client.borrow_mut().take().ok_or_else(|| {
            acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                "api_client already consumed by a previous session",
            )
        })?;
        let tool_executor = self.tool_executor.borrow_mut().take().ok_or_else(|| {
            acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                "tool_executor already consumed by a previous session",
            )
        })?;

        // 构造 Session + Runtime
        let session = Session::new().with_workspace_root(arguments.cwd.clone());
        let session_id = acp::SessionId::new(session.session_id.clone());
        let runtime = ConversationRuntime::new(
            session,
            api_client,
            tool_executor,
            self.config.permission_policy.clone(),
            self.config.system_prompt.clone(),
        );

        // Phase 4 认知外骨骼：ACP 路径同样注入三个实例，确保 ACP 客户端
        // 也能使用 DecisionLog / ProjectTopology / RefactorTransaction 工具。
        // 详见 docs/agent-cognitive-exoskeleton-plan.md 第五章。
        let mut runtime = runtime;
        if let Ok(decision_log) = runtime::DecisionLog::open(&arguments.cwd) {
            runtime = runtime.with_decision_log(decision_log);
        }
        let topology = std::sync::Arc::new(runtime::project_topology::ProjectTopology::new(
            arguments.cwd.clone(),
        ));
        runtime = runtime.with_project_topology(topology);
        let tx = runtime::RefactorTransaction::new(arguments.cwd.clone());
        runtime = runtime.with_refactor_transaction(tx);

        *self.runtime.borrow_mut() = Some(runtime);

        Ok(acp::NewSessionResponse::new(session_id))
    }

    async fn load_session(
        &self,
        _arguments: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        // 本期不支持 load_session
        Err(acp::Error::method_not_found())
    }

    async fn set_session_mode(
        &self,
        _arguments: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn prompt(
        &self,
        arguments: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        tracing::debug!("claw-agent: prompt session_id={:?}", arguments.session_id.0);

        // 取出 runtime(短暂持有 RefCell 借用)
        let runtime_opt = self.runtime.borrow_mut().take();
        let mut runtime_rc = runtime_opt.ok_or_else(|| {
            acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                "no active session: call new_session first",
            )
        })?;

        let session_id = arguments.session_id.clone();
        let user_input = Self::extract_user_text(&arguments.prompt);

        // run_turn 是同步阻塞 API。
        // current_thread + LocalSet 下直接同步调用:会阻塞 LocalSet 直到 turn 完成,
        // 但不会死锁(没有其他 task 需要并发,且 channel 发送是非阻塞的)。
        // 注意:不能用 block_in_place(它要求 multi-threaded runtime)。
        // TODO: 未来将 run_turn 改为 async 后,改用 spawn_blocking 真正并行。
        let turn_result = runtime_rc.run_turn(user_input, None);

        let turn_summary = match turn_result {
            Ok(summary) => summary,
            Err(e) => {
                // 失败:runtime 放回,返回错误
                *self.runtime.borrow_mut() = Some(runtime_rc);
                return Err(acp::Error::new(
                    acp::ErrorCode::InternalError.into(),
                    e.to_string(),
                ));
            }
        };

        // 推流 assistant 文本(本期简化:turn 完成后一次性推送,非真实流式)
        let text = Self::extract_assistant_text(&runtime_rc);
        if !text.is_empty() {
            self.notify(
                &session_id,
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                )),
            );
        }

        // 推送 tool call 通知(简化:仅记日志)
        for tool_msg in &turn_summary.tool_results {
            for block in &tool_msg.blocks {
                if let runtime::ContentBlock::ToolResult { tool_name, .. } = block {
                    tracing::debug!("claw-agent: tool result for {}", tool_name);
                }
            }
        }

        // runtime 放回
        *self.runtime.borrow_mut() = Some(runtime_rc);

        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn cancel(&self, _arguments: acp::CancelNotification) -> Result<(), acp::Error> {
        // 本期 run_turn 不支持中途取消(同步 API 无法中断)
        // TODO: 改造 run_turn 为 async + CancellationToken 后实现
        tracing::warn!("claw-agent: cancel not yet implemented (sync run_turn)");
        Ok(())
    }

    async fn ext_method(
        &self,
        _arguments: acp::ExtRequest,
    ) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _arguments: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}
