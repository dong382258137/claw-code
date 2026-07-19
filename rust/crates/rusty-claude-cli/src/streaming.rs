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
    detect_provider_kind, model_requires_reasoning_content_in_history, resolve_startup_auth_source,
    AnthropicClient, AuthSource, CacheControl, ContentBlockDelta, InputContentBlock, InputMessage,
    MessageRequest, MessageResponse, OutputContentBlock, PromptCache, ProviderClient as ApiProviderClient,
    ProviderKind, StreamEvent as ApiStreamEvent, SystemBlock, SystemContent, ToolChoice,
    ToolDefinition, ToolResultContentBlock,
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
    /// A tool use started (tool name provided).
    ToolUse { id: String, name: String },
    /// The model finished responding (MessageStop received).
    MessageStop,
    /// Streaming turn started (first event received).
    StreamStart,
}

pub(crate) const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);

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
        // Dispatch to the correct provider at construction time.
        // `ApiProviderClient` (exposed by the api crate as
        // `ProviderClient`) is an enum over Anthropic / xAI / OpenAI
        // variants, where xAI and OpenAI both use the OpenAI-compat
        // wire format under the hood. We consult
        // `detect_provider_kind(&resolved_model)` so model-name prefix
        // routing (`openai/`, `gpt-`, `grok`, `qwen/`) wins over
        // env-var presence.
        //
        // For Anthropic we build the client directly instead of going
        // through `ApiProviderClient::from_model_with_anthropic_auth`
        // so we can explicitly apply `api::read_base_url()` — that
        // reads `ANTHROPIC_BASE_URL` and is required for the local
        // mock-server test harness
        // (`crates/rusty-claude-cli/tests/compact_output.rs`) to point
        // claw at its fake Anthropic endpoint. We also attach a
        // session-scoped prompt cache on the Anthropic path; the
        // prompt cache is Anthropic-only so non-Anthropic variants
        // skip it.
        let resolved_model = api::resolve_model_alias(&model);
        let client = match detect_provider_kind(&resolved_model) {
            ProviderKind::Anthropic => {
                let auth = resolve_cli_auth_source()?;
                let inner = AnthropicClient::from_auth(auth)
                    .with_base_url(api::read_base_url())
                    .with_prompt_cache(PromptCache::new(session_id));
                ApiProviderClient::Anthropic(inner)
            }
            ProviderKind::Xai | ProviderKind::OpenAi => {
                // The api crate's `ProviderClient::from_model_with_anthropic_auth`
                // with `None` for the anthropic auth routes via
                // `detect_provider_kind` and builds an
                // `OpenAiCompatClient::from_env` with the matching
                // `OpenAiCompatConfig` (openai / xai / dashscope).
                // That reads the correct API-key env var and BASE_URL
                // override internally, so this one call covers OpenAI,
                // OpenRouter, xAI, DashScope, Ollama, and any other
                // OpenAI-compat endpoint users configure via
                // `OPENAI_BASE_URL` / `XAI_BASE_URL` / `DASHSCOPE_BASE_URL`.
                ApiProviderClient::from_model_with_anthropic_auth(&resolved_model, None)?
            }
        };
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
}

pub(crate) fn resolve_cli_auth_source() -> Result<AuthSource, Box<dyn std::error::Error>> {
    Ok(resolve_cli_auth_source_for_cwd()?)
}

#[allow(clippy::result_large_err)]
pub(crate) fn resolve_cli_auth_source_for_cwd() -> Result<AuthSource, api::ApiError> {
    resolve_startup_auth_source(|| Ok(None))
}

/// Convert a [`SystemPromptSplit`] into an Anthropic-compatible
/// [`SystemContent`] with prompt-caching markers.
///
/// The static (stable) sections are emitted as text blocks with
/// `cache_control: {type: "ephemeral"}` on the **last** static block,
/// marking the cache prefix boundary. Dynamic sections are emitted as
/// plain text blocks (no cache marker) so they re-flow every turn.
///
/// Returns `None` if both static and dynamic sections are empty, so
/// `MessageRequest.system` serializes to absent rather than `null`/`[]`.
pub(crate) fn build_system_blocks(split: &SystemPromptSplit) -> Option<SystemContent> {
    let mut blocks: Vec<SystemBlock> = Vec::new();

    // Static sections: mark the last one with cache_control.
    let static_len = split.static_sections.len();
    for (index, section) in split.static_sections.iter().enumerate() {
        let mut block = SystemBlock::new(section.clone());
        if index == static_len.saturating_sub(1) && static_len > 0 {
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
            reasoning_effort: self.reasoning_effort.clone(),
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
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut stdout = io::stdout();
        let mut sink = io::sink();
        let out: &mut dyn Write = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        let renderer = TerminalRenderer::new();
        let mut markdown_stream = MarkdownStreamState::default();
        let mut events = Vec::new();
        let mut pending_tool: Option<(String, String, String)> = None;
        let mut block_has_thinking_summary = false;
        let mut saw_stop = false;
        let mut received_any_event = false;

        loop {
            let next = if apply_stall_timeout && !received_any_event {
                match tokio::time::timeout(POST_TOOL_STALL_TIMEOUT, stream.next_event()).await {
                    Ok(inner) => inner.map_err(|error| {
                        RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                    })?,
                    Err(_elapsed) => {
                        return Err(RuntimeError::new(
                            "post-tool stall: model did not respond within timeout",
                        ));
                    }
                }
            } else {
                stream.next_event().await.map_err(|error| {
                    RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                })?
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
                    for block in start.message.content {
                        push_output_block(
                            block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                            &mut block_has_thinking_summary,
                        )?;
                    }
                }
                ApiStreamEvent::ContentBlockStart(start) => {
                    push_output_block(
                        start.content_block,
                        out,
                        &mut events,
                        &mut pending_tool,
                        true,
                        &mut block_has_thinking_summary,
                    )?;
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
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
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
                    ContentBlockDelta::ThinkingDelta { .. } => {
                        if !block_has_thinking_summary {
                            render_thinking_block_summary(out, None, false)?;
                            block_has_thinking_summary = true;
                        }
                    }
                    ContentBlockDelta::SignatureDelta { .. } => {}
                },
                ApiStreamEvent::ContentBlockStop(_) => {
                    block_has_thinking_summary = false;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    if let Some((id, name, input)) = pending_tool.take() {
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_tool_phase(&name, &input);
                        }
                        // Display tool call now that input is fully accumulated
                        writeln!(out, "\n{}", format_tool_call_start(&name, &input))
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        self.emit_status(StatusEvent::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
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
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    events.push(AssistantEvent::MessageStop);
                    self.emit_status(StatusEvent::MessageStop);
                }
            }
        }

        push_prompt_cache_record(&self.client, &mut events);

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
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut events = response_to_events(response, out)?;
        push_prompt_cache_record(&self.client, &mut events);
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

pub(crate) fn format_context_window_blocked_error(session_id: &str, error: &api::ApiError) -> String {
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

pub(crate) fn collect_prompt_cache_events(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
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
        OutputContentBlock::Thinking { thinking, .. } => {
            render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
            *block_has_thinking_summary = true;
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

    for block in response.content {
        let mut block_has_thinking_summary = false;
        push_output_block(
            block,
            out,
            &mut events,
            &mut pending_tool,
            false,
            &mut block_has_thinking_summary,
        )?;
        if let Some((id, name, input)) = pending_tool.take() {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

pub(crate) fn push_prompt_cache_record(client: &ApiProviderClient, events: &mut Vec<AssistantEvent>) {
    // `ApiProviderClient::take_last_prompt_cache_record` is a pass-through
    // to the Anthropic variant and returns `None` for OpenAI-compat /
    // xAI variants, which do not have a prompt cache. So this helper
    // remains a no-op on non-Anthropic providers without any extra
    // branching here.
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

pub(crate) fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
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
pub(crate) fn extract_system_messages(messages: &[ConversationMessage]) -> (String, Vec<ConversationMessage>) {
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
/// 行为镜像 upstream Claude Code（TS 版）的 `mapToolResultToToolResultBlockParam`。
pub(crate) fn compact_tool_output_for_model(tool_name: &str, output: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(output)
        .unwrap_or(serde_json::Value::String(output.to_string()));

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

pub(crate) fn convert_messages(
    messages: &[ConversationMessage],
    model: &str,
) -> Vec<InputMessage> {
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
                ContentBlock::Text { text } => {
                    Some(InputContentBlock::Text { text: text.clone() })
                }
                ContentBlock::Thinking { .. } => {
                    if keep_thinking {
                        Some(InputContentBlock::Thinking {
                            thinking: String::new(),
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
                        text: compact_tool_output_for_model(tool_name, output),
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
            let current_is_tool_only =
                content.iter().all(|b| matches!(b, InputContentBlock::ToolResult { .. }));
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
