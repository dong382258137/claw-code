//! Streaming API client: SSE consumption, message conversion, error formatting.
//!
//! This module groups the API client wiring (`AnthropicRuntimeClient`), the
//! streaming SSE consumption loop (`consume_stream`), the request building
//! helpers (`build_system_blocks`, `mark_last_tool_with_cache_control`,
//! `convert_messages`, `extract_system_messages`), and the error formatting
//! helpers (`format_user_visible_api_error`, `format_context_window_blocked_error`).
//!
//! These pieces are co-located so that the `impl ApiClient for AnthropicRuntimeClient`
//! and its private `consume_stream` helper can share the message conversion
//! and event-emission utilities without sprinkling `pub(crate)` across
//! every other file.

#![allow(dead_code, unused_imports, unused_variables)]

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use api::{
    model_requires_reasoning_content_in_history, CacheControl, ContentBlockDelta,
    InputContentBlock, InputMessage, MessageRequest, MessageResponse, OutputContentBlock,
    ProviderClient as ApiProviderClient, StreamEvent as ApiStreamEvent, SystemBlock, SystemContent,
    ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, ContentBlock, ConversationMessage, MessageRole,
    PermissionMode, PermissionPolicy, PromptCacheEvent, RuntimeError, SystemPromptSplit,
    TokenUsage,
};
use serde_json::json;
use tools::GlobalToolRegistry;

use crate::render::{MarkdownStreamState, TerminalRenderer};
use crate::tool_display::{format_tool_call_start, truncate_for_summary};
use crate::ultraplan::InternalPromptProgressReporter;
use crate::{filter_tool_specs, max_tokens_for_model, AllowedToolSet};

/// Callback type for emitting streaming events to a status observer.
/// Receives a snapshot of the runtime's turn-usage accumulator and
/// an elapsed millis counter, so the observer can update its display.
/// Set via `AnthropicRuntimeClient::with_status_emitter`. No-op by default.
pub(crate) type StatusEmitter = Arc<dyn Fn(StatusEvent) + Send + Sync>;

/// Events emitted during streaming for the status bar to consume.
#[derive(Debug, Clone)]
pub(crate) enum StatusEvent {
    /// A usage delta arrived (input/output tokens updated).
    Usage(TokenUsage),
    /// A text delta arrived (incremental assistant output).
    TextDelta(String),
    /// A tool use started (tool name + input JSON provided).
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    /// A tool finished executing (id, name, output, is_error).
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// A thinking block was observed during streaming. `char_count` is the
    /// number of thinking chars hidden from the user (None when the provider
    /// redacted the content entirely). `redacted` is true for
    /// `RedactedThinking` blocks. The TUI renders a short summary like
    /// "▶ Thinking (N chars hidden)" so users know reasoning happened.
    Thinking {
        char_count: Option<usize>,
        redacted: bool,
    },
    /// The model finished responding (MessageStop received).
    MessageStop,
    /// Streaming turn started (first event received).
    StreamStart,
    /// P0-1 修复：流式过程中发生错误（API 5xx / 网络断开 / 写入失败等）。
    /// 之前所有错误返回路径都不 emit 事件，TUI 在错误发生时收不到任何信号，
    /// `streaming: true` 一直保留导致 UI 假死。现在在每个 `return Err(...)` 前
    /// emit 此事件，让 TUI 能即时调用 `finish_turn()` 并向用户显示错误。
    /// `recoverable` 为 true 表示错误可重试（如 429 限流），false 表示致命错误。
    StreamError { message: String, recoverable: bool },
}

pub(crate) const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// P3:事件间超时 — 两个相邻 SSE 事件之间的最大等待时间。
///
/// 设计依据:
/// - 流式响应(SSE)中,两个事件之间的间隔通常 < 1s(模型逐 token 生成)
/// - 如果 60s 内无任何新事件,说明:
///   * 网络静默挂起(TCP 连接存活但无数据)
///   * 服务端处理卡住(模型推理死锁)
///   * 中间代理(如 Cloudflare)缓冲异常
/// - 60s 是经验值:足够宽容以容纳模型长思考(如 extended thinking),
///   又足够严格以避免用户无限等待
/// - 与 `http_client.rs` 故意不设 `.timeout()` 的设计协同:
///   总请求超时会错误中止合法的长流式响应,而事件间超时只检测
///   "卡住"状态,不影响正常的长流式传输
/// - 超时标记为 recoverable=true,允许上层重试
pub(crate) const INTER_EVENT_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct HookAbortMonitor {
    stop_tx: Option<Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl HookAbortMonitor {
    pub(crate) fn spawn(abort_signal: runtime::HookAbortSignal) -> Self {
        Self::spawn_with_waiter(abort_signal, move |stop_rx, abort_signal| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };

            runtime.block_on(async move {
                let wait_for_stop = tokio::task::spawn_blocking(move || {
                    let _ = stop_rx.recv();
                });

                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            abort_signal.abort();
                        }
                    }
                    _ = wait_for_stop => {}
                }
            });
        })
    }

    pub(crate) fn spawn_with_waiter<F>(
        abort_signal: runtime::HookAbortSignal,
        wait_for_interrupt: F,
    ) -> Self
    where
        F: FnOnce(Receiver<()>, runtime::HookAbortSignal) + Send + 'static,
    {
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = thread::spawn(move || wait_for_interrupt(stop_rx, abort_signal));

        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }
    }

    pub(crate) fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

// NOTE: Despite the historical name `AnthropicRuntimeClient`, this struct
// now holds an `ApiProviderClient` which dispatches to Anthropic, xAI,
// OpenAI, or DashScope at construction time based on
// `detect_provider_kind(&model)`. The struct name is kept to avoid
// churning `BuiltRuntime` and every Deref/DerefMut site that references
// it. See ROADMAP #29 for the provider-dispatch routing fix.
pub(crate) struct AnthropicRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: ApiProviderClient,
    session_id: String,
    model: String,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    progress_reporter: Option<InternalPromptProgressReporter>,
    reasoning_effort: Option<String>,
    /// Optional callback for emitting streaming events to a status observer
    /// (e.g., the TUI's persistent status bar). None in non-TUI mode.
    status_emitter: Option<StatusEmitter>,
}

impl AnthropicRuntimeClient {
    pub(crate) fn new(
        session_id: &str,
        model: String,
        enable_tools: bool,
        emit_output: bool,
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        progress_reporter: Option<InternalPromptProgressReporter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // DeepSeek-only build: the api crate's `ProviderClient::from_model`
        // reads `DEEPSEEK_API_KEY` / `DEEPSEEK_BASE_URL` from the environment
        // and builds an `OpenAiCompatClient` with the DeepSeek config. Auth is
        // resolved internally via `from_env`, so no external auth source is
        // needed here.
        let resolved_model = api::resolve_model_alias(&model);
        let client = ApiProviderClient::from_model(&resolved_model)?;
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            session_id: session_id.to_string(),
            model,
            enable_tools,
            emit_output,
            allowed_tools,
            tool_registry,
            progress_reporter,
            reasoning_effort: None,
            status_emitter: None,
        })
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    /// 读取当前 reasoning_effort 设置（供 TUI 侧栏显示）。
    /// None 表示使用模型默认（不发送 reasoning_effort 字段到 API）。
    pub(crate) fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort.clone()
    }

    /// Attach a status emitter callback. The callback is invoked on
    /// each streaming event (Usage, TextDelta, ToolUse, MessageStop)
    /// so the observer can update its display in real-time.
    pub(crate) fn with_status_emitter(mut self, emitter: StatusEmitter) -> Self {
        self.status_emitter = Some(emitter);
        self
    }

    /// Attach a status emitter callback to an already-constructed client.
    /// This is the `&mut self` counterpart to `with_status_emitter` — used
    /// when the client is already wrapped inside a `ConversationRuntime`
    /// and we only have `api_client_mut()` access.
    pub(crate) fn set_status_emitter(&mut self, emitter: StatusEmitter) {
        self.status_emitter = Some(emitter);
    }

    /// Emit a status event if an emitter is attached. No-op otherwise.
    fn emit_status(&self, event: StatusEvent) {
        if let Some(emitter) = &self.status_emitter {
            emitter(event);
        }
    }

    /// P0-1 修复：emit 一个 `StreamError` 事件并构造对应的 `RuntimeError`。
    /// 在所有错误返回路径调用此方法，确保 TUI 能即时收到错误信号，
    /// 调用 `finish_turn()` 退出 streaming 状态并向用户显示错误信息，
    /// 避免状态栏永久显示 `streaming: true` 导致 UI 假死。
    fn emit_stream_error(&self, message: impl Into<String>, recoverable: bool) -> RuntimeError {
        let msg = message.into();
        self.emit_status(StatusEvent::StreamError {
            message: msg.clone(),
            recoverable,
        });
        RuntimeError::new(msg)
    }
}

/// Convert a [`SystemPromptSplit`] into an Anthropic-compatible
/// [`SystemContent`] with prompt-caching markers.
///
/// Uses tiered cache breakpoints (up to 3) computed by
/// [`SystemPromptSplit::static_cache_breakpoints`] to enable layered caching:
/// instruction tier, snapshot tier, and config tier are cached independently,
/// so changes in a later tier don't invalidate the cache of earlier tiers.
/// Dynamic sections are emitted as plain text blocks (no cache marker) so
/// they re-flow every turn.
///
/// Returns `None` if both static and dynamic sections are empty, so
/// `MessageRequest.system` serializes to absent rather than `null`/`[]`.
pub(crate) fn build_system_blocks(split: &SystemPromptSplit) -> Option<SystemContent> {
    let mut blocks: Vec<SystemBlock> = Vec::new();

    let breakpoints = split.static_cache_breakpoints();
    for (index, section) in split.static_sections.iter().enumerate() {
        let mut block = SystemBlock::new(section.clone());
        if breakpoints.contains(&index) {
            block = block.with_cache_control(CacheControl::ephemeral());
        }
        blocks.push(block);
    }

    // Dynamic sections: no cache marker.
    for section in &split.dynamic_sections {
        blocks.push(SystemBlock::new(section.clone()));
    }

    if blocks.is_empty() {
        None
    } else {
        Some(SystemContent::from_blocks(blocks))
    }
}

/// Mark the last tool in a list with `cache_control: {type: "ephemeral"}` so
/// the Anthropic API caches the tools array prefix. No-op for empty lists.
///
/// Tools definitions are large (JSON schemas) and stable across turns within
/// a session, so caching them yields significant input-token savings.
pub(crate) fn mark_last_tool_with_cache_control(tools: &mut [ToolDefinition]) {
    if let Some(last) = tools.last_mut() {
        last.cache_control = Some(CacheControl::ephemeral());
    }
}

impl ApiClient for AnthropicRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        let is_post_tool = request_ends_with_tool_result(&request);

        // Extract system-role messages so they route through MessageRequest.system
        // (eligible for prompt caching) instead of being flattened into messages.
        let (system_text, filtered_messages) = extract_system_messages(&request.messages);
        let mut split = request.system_prompt;
        if !system_text.is_empty() {
            split.dynamic_sections.push(system_text);
        }

        // Effort routing (Headroom Output Token Reduction 对标):
        // 当当前 turn 是工具结果续写(is_post_tool)且用户未显式要求 high effort 时,
        // 自动降低 reasoning_effort 到 "low"。模型读完工具结果后的续写通常不需要
        // 深度推理,降低 effort 可节省 output token 和延迟。
        // 新问题或用户显式设置 high 时保持全力。
        // 注意:仅在用户已开启 reasoning(self.reasoning_effort = Some)时才生效;
        // 若为 None 则保持 None,避免为非 reasoning 模型意外注入 reasoning_effort
        // 参数(会触发 DeepSeek thinking 模式并要求回传 reasoning_content)。
        let effective_effort = match (is_post_tool, &self.reasoning_effort) {
            (true, Some(effort)) if effort != "high" => Some("low".to_string()),
            _ => self.reasoning_effort.clone(),
        };

        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model),
            messages: convert_messages(&filtered_messages, &self.model),
            system: build_system_blocks(&split),
            tools: self.enable_tools.then(|| {
                let mut tools = filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref());
                mark_last_tool_with_cache_control(&mut tools);
                tools
            }),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            reasoning_effort: effective_effort,
            ..Default::default()
        };

        self.runtime.block_on(async {
            // Single attempt: re-sending the full request on stall doubles token
            // usage for no reliability gain (the stall is typically a transient
            // server/network issue that a retry of the identical request is
            // unlikely to fix). Let the caller handle the error.
            self.consume_stream(&message_request, is_post_tool).await
        })
    }

    /// v3:async 变体 — 直接 `.await` [`consume_stream`](Self::consume_stream),
    /// 消除 [`stream`](Self::stream) 内部 `runtime.block_on()` 创建的嵌套 runtime。
    ///
    /// 调用方必须已在 tokio runtime 中(如 [`ConversationRuntime::run_turn_async`]
    /// 或 `spawn_parallel_via_dag_async`)。在非 async 上下文调用请使用同步
    /// [`stream`](Self::stream)。
    ///
    /// 与同步版本语义完全一致:
    /// - 触发 `progress_reporter.mark_model_phase()`
    /// - 计算 `is_post_tool` / `effective_effort` 路由
    /// - 单次 `consume_stream` 调用(stall 仍由内部 `tokio::time::timeout` 保护)
    ///
    /// # Send 性
    /// 返回的 `Future` 为 `Send`,因为 `AnthropicRuntimeClient` 是 `Send`
    /// (所有字段均为 `Send + Sync`:Runtime / ApiProviderClient /
    /// GlobalToolRegistry / InternalPromptProgressReporter / StatusEmitter 等)。
    fn stream_async<'a>(
        &'a mut self,
        request: ApiRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<AssistantEvent>, RuntimeError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            if let Some(progress_reporter) = &self.progress_reporter {
                progress_reporter.mark_model_phase();
            }
            let is_post_tool = request_ends_with_tool_result(&request);

            // 与同步 `stream` 一致:抽离 system 角色消息走 system 字段(prompt cache)
            let (system_text, filtered_messages) = extract_system_messages(&request.messages);
            let mut split = request.system_prompt;
            if !system_text.is_empty() {
                split.dynamic_sections.push(system_text);
            }

            // Effort 路由(与同步版本完全一致):
            // post-tool 续写且非 high 时降到 "low",节省 output token / 延迟。
            // 仅在用户已开启 reasoning 时生效,避免为非 reasoning 模型注入参数。
            let effective_effort = match (is_post_tool, &self.reasoning_effort) {
                (true, Some(effort)) if effort != "high" => Some("low".to_string()),
                _ => self.reasoning_effort.clone(),
            };

            let message_request = MessageRequest {
                model: self.model.clone(),
                max_tokens: max_tokens_for_model(&self.model),
                messages: convert_messages(&filtered_messages, &self.model),
                system: build_system_blocks(&split),
                tools: self.enable_tools.then(|| {
                    let mut tools =
                        filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref());
                    mark_last_tool_with_cache_control(&mut tools);
                    tools
                }),
                tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
                stream: true,
                reasoning_effort: effective_effort,
                ..Default::default()
            };

            // 直接 await,无 runtime.block_on
            self.consume_stream(&message_request, is_post_tool).await
        })
    }

    /// Multi-Agent Hardening §4.5.3:构造一个绑定到指定模型的子 agent client。
    ///
    /// 用于 `execute_dispatch_subagent` retry loop 中的模型升级路径:
    /// - `deepseek-v4-flash` 失败 → `with_model("deepseek-v4-pro")` → 重试
    ///
    /// 子 agent client 配置:
    /// - `enable_tools = false`:子 agent 走单轮 LLM 请求,不调用工具
    /// - `emit_output = false`:静默执行,不污染主 agent 的 stdout
    /// - `allowed_tools = None`:无工具白名单
    /// - `progress_reporter = None`:不订阅进度事件
    /// - `status_emitter = None`:不订阅状态事件
    /// - 复用主 agent 的 `session_id`(用于 prompt cache 隔离桶)和 `tool_registry`(共享工具定义)
    fn with_model(&self, model: &str) -> Result<Box<dyn ApiClient>, String> {
        let client = AnthropicRuntimeClient::new(
            &self.session_id,
            model.to_string(),
            false, // enable_tools:子 agent 不调用工具
            false, // emit_output:静默
            None,  // allowed_tools
            self.tool_registry.clone(),
            None, // progress_reporter
        )
        .map_err(|e| format!("failed to construct subagent client for {model}: {e}"))?;
        Ok(Box::new(client))
    }
}

impl AnthropicRuntimeClient {
    /// Consume a single streaming response, optionally applying a stall
    /// timeout on the first event for post-tool continuations.
    #[allow(clippy::too_many_lines)]
    async fn consume_stream(
        &self,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut stream = self
            .client
            .stream_message(message_request)
            .await
            .map_err(|error| {
                // P0-1 修复 #1/9：stream_message 失败（API 错误、网络断开等）。
                let msg = format_user_visible_api_error(&self.session_id, &error);
                self.emit_stream_error(msg, false)
            })?;
        let mut stdout = io::stdout();
        let mut sink = io::sink();
        // v3:使用 `dyn Write + Send` 让 `consume_stream` 返回的 Future 为 `Send`,
        // 从而可在 `stream_async` 中直接 `.await`(消除嵌套 runtime)。
        // `io::Stdout` 与 `io::Sink` 均为 `Send + Sync`,满足此 bound。
        let out: &mut (dyn Write + Send) = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        let renderer = TerminalRenderer::new();
        let mut markdown_stream = MarkdownStreamState::default();
        let mut events = Vec::new();
        let mut pending_tool: Option<(String, String, String)> = None;
        // P0 修复：thinking 内容累积器。
        // ContentBlockStart 暂存 thinking 初值，ThinkingDelta 追加文本，
        // SignatureDelta 追加签名，ContentBlockStop 时统一 emit。
        let mut pending_thinking: Option<(String, Option<String>)> = None;
        let mut block_has_thinking_summary = false;
        let mut saw_stop = false;
        let mut received_any_event = false;

        loop {
            // P3:事件间超时保护 — 统一用 tokio::time::timeout 包装 next_event。
            // - post-tool 场景的首事件:POST_TOOL_STALL_TIMEOUT(10s,严格)
            //   理由:post-tool 续写应该很快开始,10s 足够
            // - 其他所有情况(非 post-tool 首事件 + 所有后续事件):
            //   INTER_EVENT_TIMEOUT(60s,宽容)
            //   理由:模型可能进行长思考(extended thinking),60s 容纳正常长流式
            let timeout_duration = if apply_stall_timeout && !received_any_event {
                POST_TOOL_STALL_TIMEOUT
            } else {
                INTER_EVENT_TIMEOUT
            };
            let next = match tokio::time::timeout(timeout_duration, stream.next_event()).await {
                Ok(inner) => inner.map_err(|error| {
                    // P0-1 修复 #2/9：超时分支内 next_event 失败。
                    let msg = format_user_visible_api_error(&self.session_id, &error);
                    self.emit_stream_error(msg, false)
                })?,
                Err(_elapsed) => {
                    // P0-1 修复 #3/9 + P3 扩展:stall 超时。
                    // 区分两种 stall 场景,提供更精确的错误消息:
                    let msg = if apply_stall_timeout && !received_any_event {
                        // post-tool 首事件 stall(10s 内无事件)
                        "post-tool stall: model did not respond within timeout"
                    } else {
                        // P3 新增:事件间 stall(60s 内无新事件)
                        // 这包括:非 post-tool 首事件 stall + 后续事件 stall
                        "inter-event stall: stream stalled waiting for next event (60s timeout)"
                    };
                    return Err(self.emit_stream_error(msg, true));
                }
            };

            let Some(event) = next else {
                break;
            };
            if !received_any_event {
                self.emit_status(StatusEvent::StreamStart);
            }
            received_any_event = true;

            match event {
                ApiStreamEvent::MessageStart(start) => {
                    // P2-5 修复：之前 for 循环每次迭代后都检查 block_has_thinking_summary
                    // 并 emit，但 push_output_block 对非 thinking 块不 reset 标志，
                    // 导致 [Thinking, Text, ToolUse] 序列会重复 emit 3 次 Thinking。
                    // 现在改为先处理所有块，循环结束后只 emit 一次。
                    let mut had_thinking_summary = false;
                    for block in start.message.content {
                        push_output_block(
                            block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                            &mut block_has_thinking_summary,
                            &mut pending_thinking,
                        )?;
                        if block_has_thinking_summary {
                            had_thinking_summary = true;
                        }
                        // P0 修复：MessageStart 携带的 thinking 块没有对应
                        // ContentBlockStop 事件（OpenAI-compat 提供商可能把完整
                        // thinking 放在 MessageStart.content 里）。如果只暂存到
                        // pending_thinking 等待 ContentBlockStop，会被后续
                        // ContentBlockStart 覆盖丢失，且顺序错乱（thinking 应在
                        // text 之前）。这里立即 emit 以保证顺序正确。
                        if let Some((thinking, signature)) = pending_thinking.take() {
                            if !thinking.is_empty() {
                                events.push(AssistantEvent::Thinking {
                                    thinking,
                                    signature,
                                });
                            }
                        }
                    }
                    if had_thinking_summary {
                        self.emit_status(StatusEvent::Thinking {
                            char_count: None,
                            redacted: false,
                        });
                    }
                    events.push(AssistantEvent::Usage(start.message.usage.token_usage()));
                    self.emit_status(StatusEvent::Usage(start.message.usage.token_usage()));
                }
                ApiStreamEvent::ContentBlockStart(start) => {
                    let pre_len = events.len();
                    push_output_block(
                        start.content_block,
                        out,
                        &mut events,
                        &mut pending_tool,
                        true,
                        &mut block_has_thinking_summary,
                        &mut pending_thinking,
                    )?;
                    // P0 修复：OpenAI-compatible 提供商（DeepSeek 等）可能在
                    // ContentBlockStart 中携带完整文本块。push_output_block 只写入
                    // out（TUI 下为 io::sink），需额外 emit TextDelta 给 TUI 渲染。
                    for event in &events[pre_len..] {
                        if let AssistantEvent::TextDelta(text) = event {
                            self.emit_status(StatusEvent::TextDelta(text.clone()));
                        }
                    }
                    // P1 修复：同 MessageStart 分支，ContentBlockStart 携带完整
                    // thinking 块时也需 emit Thinking 事件给 TUI。
                    if block_has_thinking_summary {
                        self.emit_status(StatusEvent::Thinking {
                            char_count: None,
                            redacted: false,
                        });
                    }
                }
                ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                    ContentBlockDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            if let Some(progress_reporter) = &self.progress_reporter {
                                progress_reporter.mark_text_phase(&text);
                            }
                            if let Some(rendered) = markdown_stream.push(&renderer, &text) {
                                write!(out, "{rendered}")
                                    .and_then(|()| out.flush())
                                    .map_err(|error| {
                                        // P0-1 修复 #5/9：TextDelta 写入失败。
                                        self.emit_stream_error(error.to_string(), false)
                                    })?;
                            }
                            self.emit_status(StatusEvent::TextDelta(text.clone()));
                            events.push(AssistantEvent::TextDelta(text));
                        }
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        if let Some((_, _, input)) = &mut pending_tool {
                            input.push_str(&partial_json);
                        }
                    }
                    ContentBlockDelta::ThinkingDelta { thinking } => {
                        // P0 修复：累积 thinking delta 文本到 pending_thinking。
                        // 之前用 `ThinkingDelta { .. }` 直接丢弃 delta 文本，
                        // 导致流式响应后 assistant 消息没有任何 thinking 内容，
                        // 下一轮请求 DeepSeek API 报 400
                        // (reasoning_content must be passed back)。
                        if !block_has_thinking_summary {
                            render_thinking_block_summary(out, None, false)?;
                            block_has_thinking_summary = true;
                            // Phase 3: notify TUI that a thinking block is
                            // happening. char_count is None because streaming
                            // deltas don't give us the total (matches the
                            // stdout summary which also says "hidden").
                            self.emit_status(StatusEvent::Thinking {
                                char_count: None,
                                redacted: false,
                            });
                        }
                        match &mut pending_thinking {
                            Some((pending, _)) => pending.push_str(&thinking),
                            None => {
                                pending_thinking = Some((thinking, None));
                            }
                        }
                    }
                    ContentBlockDelta::SignatureDelta { signature } => {
                        // P0 修复：累积 signature delta，与 thinking 一起在
                        // ContentBlockStop 时 emit。Anthropic extended thinking
                        // 通过 signature 验证 thinking 块完整性。
                        if let Some((_, pending_signature)) = &mut pending_thinking {
                            pending_signature
                                .get_or_insert_with(String::new)
                                .push_str(&signature);
                        }
                    }
                },
                ApiStreamEvent::ContentBlockStop(_) => {
                    block_has_thinking_summary = false;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| {
                                // P0-1 修复 #6/9：ContentBlockStop markdown flush 失败。
                                self.emit_stream_error(error.to_string(), false)
                            })?;
                    }
                    // P0 修复：thinking 块结束时统一 emit AssistantEvent::Thinking。
                    // 这是 DeepSeek thinking 模式不报 400 的关键 — 下一轮请求时
                    // convert_messages 会把这个 thinking 块转换成 reasoning_content
                    // 回传给 API。空 thinking 不 emit，避免发送空 reasoning_content
                    // (空内容会被 DeepSeek 拒绝)。
                    if let Some((thinking, signature)) = pending_thinking.take() {
                        if !thinking.is_empty() {
                            events.push(AssistantEvent::Thinking {
                                thinking,
                                signature,
                            });
                        }
                    }
                    if let Some((id, name, input)) = pending_tool.take() {
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_tool_phase(&name, &input);
                        }
                        // Display tool call now that input is fully accumulated
                        writeln!(out, "\n{}", format_tool_call_start(&name, &input))
                            .and_then(|()| out.flush())
                            .map_err(|error| {
                                // P0-1 修复 #7/9：ToolCall 显示写入失败。
                                self.emit_stream_error(error.to_string(), false)
                            })?;
                        self.emit_status(StatusEvent::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        });
                        events.push(AssistantEvent::ToolUse { id, name, input });
                    }
                }
                ApiStreamEvent::MessageDelta(delta) => {
                    events.push(AssistantEvent::Usage(delta.usage.token_usage()));
                    self.emit_status(StatusEvent::Usage(delta.usage.token_usage()));
                }
                ApiStreamEvent::MessageStop(_) => {
                    saw_stop = true;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| {
                                // P0-1 修复 #8/9：MessageStop markdown flush 失败。
                                self.emit_stream_error(error.to_string(), false)
                            })?;
                    }
                    events.push(AssistantEvent::MessageStop);
                    self.emit_status(StatusEvent::MessageStop);
                }
            }
        }

        if !saw_stop
            && events.iter().any(|event| {
                matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                    || matches!(event, AssistantEvent::ToolUse { .. })
            })
        {
            events.push(AssistantEvent::MessageStop);
            self.emit_status(StatusEvent::MessageStop);
        }

        if events
            .iter()
            .any(|event| matches!(event, AssistantEvent::MessageStop))
        {
            return Ok(events);
        }

        let response = self
            .client
            .send_message(&MessageRequest {
                stream: false,
                ..message_request.clone()
            })
            .await
            .map_err(|error| {
                // P0-1 修复 #9/9：fallback send_message 失败（流式未收到 stop 且回退到非流式也失败）。
                let msg = format_user_visible_api_error(&self.session_id, &error);
                self.emit_stream_error(msg, false)
            })?;
        let events = response_to_events(response, out)?;
        Ok(events)
    }
}

/// Returns `true` when the conversation ends with a tool-result message,
/// meaning the model is expected to continue after tool execution.
pub(crate) fn request_ends_with_tool_result(request: &ApiRequest) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Tool)
}

pub(crate) fn format_user_visible_api_error(session_id: &str, error: &api::ApiError) -> String {
    if error.is_context_window_failure() {
        format_context_window_blocked_error(session_id, error)
    } else if error.is_generic_fatal_wrapper() {
        let mut qualifiers = vec![format!("session {session_id}")];
        if let Some(request_id) = error.request_id() {
            qualifiers.push(format!("trace {request_id}"));
        }
        format!(
            "{} ({}): {}",
            error.safe_failure_class(),
            qualifiers.join(", "),
            error
        )
    } else {
        error.to_string()
    }
}

pub(crate) fn format_context_window_blocked_error(
    session_id: &str,
    error: &api::ApiError,
) -> String {
    let mut lines = vec![
        "Context window blocked".to_string(),
        "  Failure class    context_window_blocked".to_string(),
        format!("  Session          {session_id}"),
    ];

    if let Some(request_id) = error.request_id() {
        lines.push(format!("  Trace            {request_id}"));
    }

    match error {
        api::ApiError::ContextWindowExceeded {
            model,
            estimated_input_tokens,
            requested_output_tokens,
            estimated_total_tokens,
            context_window_tokens,
        } => {
            lines.push(format!("  Model            {model}"));
            lines.push(format!(
                "  Input estimate   ~{estimated_input_tokens} tokens (heuristic)"
            ));
            lines.push(format!(
                "  Requested output {requested_output_tokens} tokens"
            ));
            lines.push(format!(
                "  Total estimate   ~{estimated_total_tokens} tokens (heuristic)"
            ));
            lines.push(format!("  Context window   {context_window_tokens} tokens"));
        }
        api::ApiError::Api { message, body, .. } => {
            let detail = message.as_deref().unwrap_or(body).trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        api::ApiError::RetriesExhausted { last_error, .. } => {
            let detail = match last_error.as_ref() {
                api::ApiError::Api { message, body, .. } => message.as_deref().unwrap_or(body),
                other => return format_context_window_blocked_error(session_id, other),
            }
            .trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        _ => {}
    }

    lines.push(String::new());
    lines.push("Recovery".to_string());
    lines.push("  Compact          /compact".to_string());
    lines.push(format!(
        "  Resume compact   claw --resume {session_id} /compact"
    ));
    lines.push("  Fresh session    /clear --confirm".to_string());
    lines.push(
        "  Reduce scope     remove large pasted context/files or ask for a smaller slice"
            .to_string(),
    );
    lines.push("  Retry            rerun after compacting or reducing the request".to_string());

    lines.join("\n")
}

pub(crate) fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(crate) fn collect_tool_uses(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .assistant_messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_tool_results(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .tool_results
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => Some(json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "output": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_prompt_cache_events(
    summary: &runtime::TurnSummary,
) -> Vec<serde_json::Value> {
    summary
        .prompt_cache_events
        .iter()
        .map(|event| {
            json!({
                "unexpected": event.unexpected,
                "reason": event.reason,
                "previous_cache_read_input_tokens": event.previous_cache_read_input_tokens,
                "current_cache_read_input_tokens": event.current_cache_read_input_tokens,
                "token_drop": event.token_drop,
            })
        })
        .collect()
}

/// Tier S #1 Goal 持续驱动：网络错误关键词列表。
/// 在 `run_turn` 错误路径检测，命中时自动 `goal_manager.pause("network error")`。
/// 选用小写匹配（error.to_string().to_ascii_lowercase() 后比较）。
pub(crate) const NETWORK_ERROR_KEYWORDS: &[&str] = &[
    "timeout",
    "timed out",
    "connection reset",
    "connection refused",
    "connection aborted",
    "network is unreachable",
    "network error",
    "broken pipe",
    "dns resolution failed",
    "name resolution",
    "connect error",
    "hyper: error",
    "reqwest: error",
    "eof before",
    "stream closed",
];

pub(crate) fn render_thinking_block_summary(
    out: &mut (impl Write + ?Sized),
    char_count: Option<usize>,
    redacted: bool,
) -> Result<(), RuntimeError> {
    let summary = if redacted {
        "\n▶ Thinking block hidden by provider\n".to_string()
    } else if let Some(char_count) = char_count {
        format!("\n▶ Thinking ({char_count} chars hidden)\n")
    } else {
        "\n▶ Thinking hidden\n".to_string()
    };
    write!(out, "{summary}")
        .and_then(|()| out.flush())
        .map_err(|error| RuntimeError::new(error.to_string()))
}

pub(crate) fn push_output_block(
    block: OutputContentBlock,
    out: &mut (impl Write + ?Sized),
    events: &mut Vec<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String)>,
    streaming_tool_input: bool,
    block_has_thinking_summary: &mut bool,
    // P0 修复：流式响应必须累积 thinking 内容。
    // DeepSeek V4 在 thinking 模式下要求历史中 assistant 消息回传 reasoning_content。
    // 之前流式路径用 `ThinkingDelta { .. }` 直接丢弃 delta 文本，
    // 且 push_output_block 在 streaming_tool_input=true 时不 emit Thinking 事件，
    // 导致下一轮请求历史中完全没有 thinking 块，触发 API 400
    // (reasoning_content must be passed back)。镜像 tools/src/lib.rs 的实现：
    // ContentBlockStart 时把 thinking 暂存到 pending_thinking，
    // 后续 ThinkingDelta/SignatureDelta 追加，ContentBlockStop 时 emit 事件。
    pending_thinking: &mut Option<(String, Option<String>)>,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                let rendered = TerminalRenderer::new().markdown_to_ansi(&text);
                write!(out, "{rendered}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            // During streaming, the initial content_block_start has an empty input ({}).
            // The real input arrives via input_json_delta events. In
            // non-streaming responses, preserve a legitimate empty object.
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            *pending_tool = Some((id, name, initial_input));
        }
        OutputContentBlock::Thinking {
            thinking,
            signature,
        } => {
            render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
            *block_has_thinking_summary = true;
            if streaming_tool_input {
                // 流式路径：暂存到 pending_thinking，等待后续 delta 追加，
                // 由 ContentBlockStop 统一 emit AssistantEvent::Thinking。
                *pending_thinking = Some((thinking, signature));
            } else {
                // G10.5 fix: non-streaming fallback path must emit Thinking
                // event so downstream consumers (planner, TUI status) receive
                // the full event stream — mirrors tools/lib.rs push_output_block.
                events.push(AssistantEvent::Thinking {
                    thinking,
                    signature,
                });
            }
        }
        OutputContentBlock::RedactedThinking { .. } => {
            render_thinking_block_summary(out, None, true)?;
            *block_has_thinking_summary = true;
        }
    }
    Ok(())
}

pub(crate) fn response_to_events(
    response: MessageResponse,
    out: &mut (impl Write + ?Sized),
) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tool = None;
    // 非流式回退路径：streaming_tool_input=false 时 push_output_block 会
    // 直接 push AssistantEvent::Thinking 到 events，无需 pending_thinking。
    // 此变量仅为满足 push_output_block 新签名而存在，永远不会被写入。
    let mut pending_thinking: Option<(String, Option<String>)> = None;

    for block in response.content {
        let mut block_has_thinking_summary = false;
        push_output_block(
            block,
            out,
            &mut events,
            &mut pending_tool,
            false,
            &mut block_has_thinking_summary,
            &mut pending_thinking,
        )?;
        if let Some((id, name, input)) = pending_tool.take() {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
        // 安全网：流式累积漏 emit 时这里兜底（理论上不会触发，
        // 因为 streaming_tool_input=false 时 push_output_block 已经 push 过）。
        if let Some((thinking, signature)) = pending_thinking.take() {
            if !thinking.is_empty() {
                events.push(AssistantEvent::Thinking {
                    thinking,
                    signature,
                });
            }
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

pub(crate) fn permission_policy(
    mode: PermissionMode,
    feature_config: &runtime::RuntimeFeatureConfig,
    tool_registry: &GlobalToolRegistry,
) -> Result<PermissionPolicy, String> {
    Ok(tool_registry.permission_specs(None)?.into_iter().fold(
        PermissionPolicy::new(mode).with_permission_rules(feature_config.permission_rules()),
        |policy, (name, required_permission)| {
            policy.with_tool_requirement(name, required_permission)
        },
    ))
}

/// Extract system-role messages from a conversation and merge their text
/// content into a single string. Returns `(system_text, non_system_messages)`.
///
/// The system text is joined with `\n\n` separators. Non-system messages
/// (User/Assistant/Tool) are returned in their original order, with ownership
/// transferred.
///
/// This lets `stream()` route system content through `MessageRequest.system`
/// (where it can be prompt-cached) instead of flattening it into the messages
/// array as a fake "user" turn.
pub(crate) fn extract_system_messages(
    messages: &[ConversationMessage],
) -> (String, Vec<ConversationMessage>) {
    let mut system_texts: Vec<String> = Vec::new();
    let mut rest: Vec<ConversationMessage> = Vec::new();
    for message in messages {
        if message.role == MessageRole::System {
            for block in &message.blocks {
                if let ContentBlock::Text { text } = block {
                    if !text.is_empty() {
                        system_texts.push(text.clone());
                    }
                }
            }
        } else {
            rest.push(message.clone());
        }
    }
    let system_text = system_texts.join("\n\n");
    (system_text, rest)
}

/// 在送入模型前压缩工具结果的 JSON 外壳，节省 context tokens。
///
/// `edit_file` / `write_file` 等工具会返回包含 `originalFile`、`structuredPatch`
/// 和全文件内容的完整 JSON，回传给模型会浪费上下文预算（一次 500 KB 文件编辑
/// ≈ 135 k tokens）。本函数把这些状态变更型工具的输出剥离为简短人类可读消息；
/// 完整结构化数据仍保留在 session 中用于展示与回放。
///
/// 当 `is_error` 为 `true` 时，工具本身已经返回错误结果，此时不再套用“成功”模板，
/// 而是原样返回错误内容（或从 JSON 错误对象中提取 `.error` 字段），避免把
/// "old_string not found in file" 这种错误包装成 "The file unknown has been updated
/// successfully." 的误导性消息。
///
/// 行为镜像 upstream Claude Code（TS 版）的 `mapToolResultToToolResultBlockParam`。
pub(crate) fn compact_tool_output_for_model(
    tool_name: &str,
    output: &str,
    is_error: bool,
) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(output).unwrap_or(serde_json::Value::String(output.to_string()));

    // 错误路径优先短路，防止错误结果被成功模板覆盖。
    if is_error {
        return parsed
            .get("error")
            .and_then(|v| v.as_str())
            .map_or_else(|| output.to_string(), |s| s.to_string());
    }

    match tool_name {
        "edit_file" | "Edit" => {
            let path = parsed
                .get("filePath")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let user_modified = parsed
                .get("userModified")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let replace_all = parsed
                .get("replaceAll")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let modified_note = if user_modified {
                ". The user modified your proposed changes before accepting them."
            } else {
                ""
            };
            if replace_all {
                format!(
                    "The file {path} has been updated{modified_note}. All occurrences were successfully replaced."
                )
            } else {
                format!("The file {path} has been updated successfully{modified_note}.")
            }
        }
        "write_file" | "Write" => {
            let path = parsed
                .get("filePath")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let kind = parsed
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("update");
            match kind {
                "create" => format!("File created successfully at: {path}"),
                _ => format!("The file {path} has been updated successfully."),
            }
        }
        "NotebookEdit" => {
            let path = parsed
                .get("notebookPath")
                .or_else(|| parsed.get("notebook_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let edit_mode = parsed
                .get("editMode")
                .and_then(|v| v.as_str())
                .unwrap_or("replace");
            match edit_mode {
                "insert" => format!("Cell inserted successfully in {path}."),
                "delete" => format!("Cell deleted successfully in {path}."),
                _ => format!("The notebook {path} has been updated successfully."),
            }
        }
        "WebSearch" => {
            // WebSearch 结果是一个对象数组：第一条是 commentary 字符串，
            // 后续是结构化命中对象。这里只保留 commentary。
            let results = parsed.get("results").and_then(|v| v.as_array());
            let commentary = results.and_then(|arr| arr.iter().find_map(|item| item.as_str()));
            commentary.map_or_else(|| output.to_string(), |c| c.to_string())
        }
        "WebFetch" => {
            // WebFetch 返回 { bytes, code, result, ... }，仅保留 AI 摘要 `result`。
            let result = parsed.get("result").and_then(|v| v.as_str());
            result.map_or_else(|| output.to_string(), |r| r.to_string())
        }
        _ => output.to_string(),
    }
}

pub(crate) fn convert_messages(messages: &[ConversationMessage], model: &str) -> Vec<InputMessage> {
    let keep_thinking = model_requires_reasoning_content_in_history(model);
    let mut result: Vec<InputMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
            MessageRole::Assistant => "assistant",
        };
        let content = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(InputContentBlock::Text { text: text.clone() }),
                ContentBlock::Thinking { thinking, .. } => {
                    // 仅在 keep_thinking 且 thinking 内容非空时才回传。
                    // 空的 thinking 内容会被 DeepSeek 的 thinking 模式拒绝
                    // (400: reasoning_content must be passed back)。
                    if keep_thinking && !thinking.is_empty() {
                        Some(InputContentBlock::Thinking {
                            thinking: thinking.clone(),
                            signature: None,
                        })
                    } else {
                        None
                    }
                }
                ContentBlock::ToolUse { id, name, input } => Some(InputContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::from_str(input)
                        .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                }),
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => Some(InputContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: vec![ToolResultContentBlock::Text {
                        text: compact_tool_output_for_model(tool_name, output, *is_error),
                    }],
                    is_error: *is_error,
                }),
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            continue;
        }

        // Anthropic API 严格要求：一个 assistant 消息里的所有 tool_use 必须在
        // **下一条** user 消息里都有对应的 tool_result。如果 runtime 把多个
        // tool_result push 成多条独立的 tool 消息（每条 1 个 tool_result），
        // API 会认为只有第一个 tool_result 在 "next message"，后续的 tool_use
        // 缺少 tool_result，返回 400 Bad Request。
        //
        // 修复：如果当前消息是 tool 消息（role=Tool，只含 ToolResult blocks），
        // 且前一条已生成的 InputMessage 也是 user 消息且只含 ToolResult blocks，
        // 则把当前的 ToolResult blocks 合并到前一条 InputMessage 里。
        if role == "user" && message.role == MessageRole::Tool {
            let current_is_tool_only = content
                .iter()
                .all(|b| matches!(b, InputContentBlock::ToolResult { .. }));
            if current_is_tool_only {
                if let Some(last) = result.last_mut() {
                    let last_is_user = last.role == "user";
                    let last_is_tool_only = last
                        .content
                        .iter()
                        .all(|b| matches!(b, InputContentBlock::ToolResult { .. }));
                    if last_is_user && last_is_tool_only {
                        last.content.extend(content);
                        continue;
                    }
                }
            }
        }

        result.push(InputMessage {
            role: role.to_string(),
            content,
        });
    }
    result
}

#[cfg(test)]
mod status_emitter_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn emit_status_noop_when_emitter_none() {
        // Construct a minimal client. We can't easily build a real one in unit
        // tests (requires auth), but we can test the `emit_status` no-op path
        // by ensuring `Option::None` doesn't panic when checked.
        let emitter: Option<StatusEmitter> = None;
        // Just verify the Option is None and doesn't panic when checked.
        assert!(emitter.is_none());
    }

    #[test]
    fn emit_status_invokes_callback_when_set() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let emitter: StatusEmitter = Arc::new(move |_event| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
        // Simulate emit
        emitter(StatusEvent::StreamStart);
        emitter(StatusEvent::MessageStop);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }
}
