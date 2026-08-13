//! [`acp::Agent`] trait 实现,适配本地 [`runtime::ConversationRuntime`]。
//!
//! 架构参考:grok-build `xai-grok-shell/src/agent/mvp_agent/acp_agent.rs`。
//!
//! 关键差异:
//! - v3:本地 `run_turn_async` 是 async API,在 LocalSet 的 async 上下文中直接 `.await`
//!   (原 `run_turn` 同步包装会在 tokio context 中返回 Err,不再适用)
//! - 本地 `run_turn_async` 需要 `&mut self`,通过 `RefCell` 提供内部可变性
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
use runtime::{
    ApiClient, AssistantEvent, ConversationRuntime, PermissionPolicy, Session, StaticToolExecutor,
};

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
    /// 用于 cancel 当前 turn 的 abort signal。
    /// 在 new_session 时创建并 clone 一份注入 runtime,
    /// 保留一份供 cancel() 调用 abort()。
    /// 在 prompt 入口调用 reset() 清除上一个 turn 的 sticky 状态。
    turn_abort_signal: RefCell<Option<runtime::HookAbortSignal>>,
    /// IDE 编辑器上下文（当前活动文件 + 选区 + 诊断），由前端经
    /// `ExtNotification("session/context")` 推送。prompt 时附加到 user_input
    /// 之前，让 agent 感知用户当前编辑位置与诊断状态（IDE 独有优势）。
    pending_editor_context: RefCell<Option<String>>,
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
    #[allow(clippy::type_complexity)] // builder 模式常见,Fnonce 闭包类型无法简化
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
            turn_abort_signal: RefCell::new(None),
            pending_editor_context: RefCell::new(None),
        }
    }
}

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
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

    /// 把单个 [`AssistantEvent`] 映射为 ACP `SessionNotification`（P0 IDE 流式）。
    ///
    /// - `TextDelta` → `AgentMessageChunk`（逐 delta 流式文本）
    /// - `Thinking` → `AgentMessageChunk`（带 `[thinking]` 前缀，前端渲染为折叠卡片）
    /// - `ToolUse` → `ToolCall`（InProgress，前端显示 ⏳）
    /// - `Usage` / `MessageStop` / `PromptCache` → `None`（不推送，由 turn 结束统一处理）
    fn assistant_event_to_session_update(
        session_id: &acp::SessionId,
        event: &AssistantEvent,
    ) -> Option<acp::SessionNotification> {
        match event {
            AssistantEvent::TextDelta(delta) => {
                if delta.is_empty() {
                    return None;
                }
                Some(acp::SessionNotification::new(
                    session_id.clone(),
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(delta.clone())),
                    )),
                ))
            }
            AssistantEvent::Thinking { thinking, .. } => {
                if thinking.is_empty() {
                    return None;
                }
                Some(acp::SessionNotification::new(
                    session_id.clone(),
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(format!(
                            "[thinking] {thinking}"
                        ))),
                    )),
                ))
            }
            AssistantEvent::ToolUse { id, name, .. } => {
                let mut call = acp::ToolCall::new(id.clone(), name.clone());
                call.kind = acp::ToolKind::Other;
                call.status = acp::ToolCallStatus::InProgress;
                Some(acp::SessionNotification::new(
                    session_id.clone(),
                    acp::SessionUpdate::ToolCall(call),
                ))
            }
            _ => None,
        }
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

        // 创建 abort signal:一份注入 runtime 供 run_turn 主循环检查,
        // 一份保留在 self 供 cancel() 调用 abort()。
        let abort_signal = runtime::HookAbortSignal::new();
        runtime = runtime.with_hook_abort_signal(abort_signal.clone());
        *self.turn_abort_signal.borrow_mut() = Some(abort_signal);

        // P0 IDE 流式：注入流式事件回调，把 `AssistantEvent` 实时推给前端。
        // 闭包捕获可 Clone 的 gateway sender + session_id（均 Send + 'static），
        // 规避 run_turn_async 中 `&mut self` 借用与 `notify(&self)` 的冲突。
        let stream_gateway = self.client_gateway.clone();
        let stream_session_id = session_id.clone();
        runtime = runtime.with_stream_event_callback(Box::new(move |event| {
            if let Some(notification) =
                Self::assistant_event_to_session_update(&stream_session_id, &event)
            {
                stream_gateway.forward_fire_and_forget(notification);
            }
        }));

        *self.runtime.borrow_mut() = Some(runtime);

        // Session Bus(设计文档 §2.4):面板会话注册为 Ide 对等会话,
        // 使主会话可经 `/bus send ide:<id>` 向其发消息。
        {
            let bus = runtime::global_session_bus();
            let _ = bus.register(runtime::BusPeer {
                session_id: session_id.0.to_string(),
                label: format!("ide:{}", session_id.0),
                kind: runtime::PeerKind::Ide,
                status: runtime::PeerStatus::Idle,
                unread: 0,
                last_seen_ms: runtime::bus_now_ms(),
                config_path: None,
            });
        }

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
        arguments: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        // plan/act 模式切换：映射到 runtime 的 plan_mode_enabled 标志。
        // `SessionModeId` 是字符串包装（如 "plan"/"act"），"plan" 开启规划模式。
        let mode_str = arguments.mode_id.0.as_ref();
        let plan = mode_str.eq_ignore_ascii_case("plan");
        if let Some(runtime) = self.runtime.borrow_mut().as_mut() {
            runtime.set_plan_mode_enabled(plan);
            tracing::info!("claw-agent: session mode set to {mode_str} (plan={plan})");
        } else {
            tracing::debug!("claw-agent: set_session_mode ignored, no active runtime");
        }
        Ok(acp::SetSessionModeResponse::new())
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
        // IDE 上下文：若前端推送了当前活动文件/选区/诊断，附加到 user_input 之前，
        // 让 agent 感知用户当前编辑位置（IDE 独有优势，TUI 无此能力）。
        let user_input = match self.pending_editor_context.borrow_mut().take() {
            Some(ctx) if !ctx.trim().is_empty() => {
                format!("[IDE context]\n{ctx}\n\n[User prompt]\n{}", Self::extract_user_text(&arguments.prompt))
            }
            _ => Self::extract_user_text(&arguments.prompt),
        };

        // 清除上一个 turn 可能残留的 sticky abort 状态,
        // 否则 cancel signal 从 turn N 会立即 abort turn N+1。
        if let Some(signal) = self.turn_abort_signal.borrow().as_ref() {
            signal.reset();
        }

        // v3:run_turn_async — 直接在 LocalSet 的 async 上下文中 await,
        // 避免嵌套 runtime 开销(原 run_turn 会因检测到 tokio context 返回 Err)。
        // current_thread + LocalSet 下,await 期间可被 cancel() 触发的 HookAbortSignal
        // 中断;由于 prompt 本身是 async fn,LocalSet 上的其他 task(如 channel forwarder)
        // 在 await 点有机会调度执行。
        let turn_result = runtime_rc.run_turn_async(user_input, None).await;

        let turn_summary = match turn_result {
            Ok(summary) => summary,
            Err(e) => {
                // 失败:runtime 放回,返回错误
                *self.runtime.borrow_mut() = Some(runtime_rc);
                let err_msg = e.to_string();
                // 检测是否为用户取消(cancel 触发 HookAbortSignal,
                // run_turn 在检查点返回 "turn interrupted by user")
                if err_msg.contains("turn interrupted by user") {
                    tracing::info!("claw-agent: turn cancelled by user");
                    return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
                }
                return Err(acp::Error::new(
                    acp::ErrorCode::InternalError.into(),
                    err_msg,
                ));
            }
        };

        // 推送 LaneEvent → SessionNotification 桥接事件(工具调用进度、子 agent 状态等)
        // 在 turn 完成后一次性 drain 并 forward 给 IDE 端,激活实时推送能力。
        let flushed =
            crate::lane_bridge::flush_lane_events_to_acp(&self.client_gateway, &session_id);
        if flushed > 0 {
            tracing::debug!("claw-agent: flushed {flushed} lane events to ACP gateway");
        }

        // Session Bus(设计文档 §2.4):把发给本面板 session 的未读 bus 消息
        // 作为 `session/peer_message` 通知推送(复用 lane_bridge 推送模式)。
        let bus = runtime::global_session_bus();
        let panel_unread = bus.unread_messages(session_id.0.as_ref());
        if !panel_unread.is_empty() {
            bus.mark_read(session_id.0.as_ref());
            for m in panel_unread {
                crate::lane_bridge::push_bus_message_to_acp(&self.client_gateway, &m);
            }
        }

        // P0 IDE 流式：assistant 文本已通过 stream_event_callback 实时逐 delta 推送，
        // 此处不再重复一次性推送（否则前端会看到重复内容）。

        // 推送 tool call 完成通知（闭合流式阶段 ToolUse 推的 InProgress 卡片）。
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
        // 调用 HookAbortSignal::abort(),run_turn_async 主循环在下一次迭代
        // 顶部或工具调用边界检测到后返回 RuntimeError("turn interrupted by user"),
        // prompt 方法据此返回 StopReason::Cancelled。
        //
        // v3:由于 LLM stream 现使用 stream_async(原生 async),
        // await 点会立即让出控制权,LocalSet 上的本 cancel 方法可在 stream 还在进行时
        // 被调度执行,触发 abort signal 后下一次检查点会立刻中断。
        // 这比原同步 stream 路径(必须等流完成)响应更及时。
        if let Some(signal) = self.turn_abort_signal.borrow().as_ref() {
            signal.abort();
            tracing::info!("claw-agent: cancel signal fired for current turn");
        } else {
            tracing::debug!("claw-agent: cancel received but no active turn");
        }
        Ok(())
    }

    async fn ext_method(
        &self,
        _arguments: acp::ExtRequest,
    ) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, arguments: acp::ExtNotification) -> Result<(), acp::Error> {
        // Session Bus(设计文档 2026-08-11-session-bus-design.md §2.4):
        // IDE 面板经 `session/broadcast` ExtNotification 向总线发布消息,
        // 复用 lane_bridge 的广播处理(注册面板为 Ide peer + publish)。
        if arguments.method.as_ref() == "session/broadcast" {
            crate::lane_bridge::handle_broadcast_notification(&arguments.params);
            return Ok(());
        }
        // IDE 编辑器上下文：前端推送当前活动文件/选区/诊断，prompt 时注入。
        // params 为 JSON：{"activeFile": "...", "selection": "...", "diagnostics": "..."}
        if arguments.method.as_ref() == "session/context" {
            let raw = arguments.params.get();
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) {
                let mut parts: Vec<String> = Vec::new();
                if let Some(f) = json.get("activeFile").and_then(|v| v.as_str()) {
                    if !f.is_empty() {
                        parts.push(format!("Active file: {f}"));
                    }
                }
                if let Some(s) = json.get("selection").and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        parts.push(format!("Selection: {s}"));
                    }
                }
                if let Some(d) = json.get("diagnostics").and_then(|v| v.as_str()) {
                    if !d.is_empty() {
                        parts.push(format!("Diagnostics:\n{d}"));
                    }
                }
                if !parts.is_empty() {
                    *self.pending_editor_context.borrow_mut() = Some(parts.join("\n"));
                }
            }
        }
        Ok(())
    }
}
