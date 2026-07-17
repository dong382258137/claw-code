use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde_json::{Map, Value};
use telemetry::SessionTracer;

use crate::compact::{
    compact_session, estimate_session_tokens, CompactionConfig, CompactionResult,
};
use crate::config::RuntimeFeatureConfig;
use crate::hooks::{HookAbortSignal, HookProgressReporter, HookRunResult, HookRunner};
use crate::memory::{
    extract_nudge_actions, should_nudge, NudgeAction, NudgeConfig, PersistentMemory,
};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter,
};
use crate::prompt::SystemPromptSplit;
use crate::session::{ContentBlock, ConversationMessage, Session};
use crate::usage::{TokenUsage, UsageTracker};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 100_000;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";
/// Number of recent tool results kept verbatim by the microcompact pass that
/// runs at the end of every turn, before auto-compaction is considered.
const MICROCOMPACT_PRESERVE_RECENT: usize = 4;
/// More aggressive preserve window used when recovering from a prompt-too-long
/// error. Only the two most recent tool results are kept verbatim.
const REACTIVE_MICROCOMPACT_PRESERVE_RECENT: usize = 2;

/// Tool specification for the `session_search` tool.
///
/// Exposed as a `pub const` so external integrators (e.g. `main.rs`'s
/// tool registry) can register the tool with the model using the exact
/// same schema the runtime expects when it intercepts the call. The
/// runtime handles execution internally via
/// [`ConversationRuntime::execute_session_search`]; the registry only
/// needs to surface the tool's name, description, and input schema to
/// the model.
pub const SESSION_SEARCH_TOOL_SPEC: &str = r#"{
    "name": "session_search",
    "description": "Search the conversation history using full-text search. Use this to recall specific past discussions, decisions, or file references that may not be in the current context window. Returns ranked matches with session ID, role, and content snippet.",
    "input_schema": {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Full-text search query. Supports FTS5 syntax: phrases, AND, OR, NOT, and prefix queries (term*)."
            },
            "top_k": {
                "type": "integer",
                "description": "Maximum number of results to return (default: 10).",
                "default": 10
            }
        },
        "required": ["query"]
    }
}"#;

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: SystemPromptSplit,
    pub messages: Vec<ConversationMessage>,
}

/// Streamed events emitted while processing a single assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantEvent {
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    TextDelta(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    Usage(TokenUsage),
    PromptCache(PromptCacheEvent),
    MessageStop,
}

/// Prompt-cache telemetry captured from the provider response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheEvent {
    pub unexpected: bool,
    pub reason: String,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

/// Minimal streaming API contract required by [`ConversationRuntime`].
pub trait ApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError>;
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
pub trait ToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;
}

/// Error returned when a tool invocation fails locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// Error returned when a conversation turn cannot be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns true when the error message indicates the upstream API rejected
    /// the request because the prompt exceeded its maximum length. Used by the
    /// reactive-compaction recovery path in [`ConversationRuntime::run_turn`].
    #[must_use]
    pub fn is_prompt_too_long(&self) -> bool {
        let lowered = self.message.to_ascii_lowercase();
        lowered.contains("prompt")
            && (lowered.contains("too long")
                || lowered.contains("exceeds")
                || lowered.contains("maximum"))
    }

    /// Returns the underlying error message, primarily for test assertions.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Summary of one completed runtime turn, including tool results and usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub prompt_cache_events: Vec<PromptCacheEvent>,
    pub iterations: usize,
    pub usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
}

/// Details about automatic session compaction applied during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
}

/// Tracks how far the reactive-compaction recovery has progressed within a
/// single [`ConversationRuntime::run_turn`] call. The state machine prevents
/// infinite retry loops when the upstream API keeps returning prompt-too-long
/// errors despite compaction attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactiveCompactState {
    /// No recovery attempted yet — microcompact is the first step.
    NotAttempted,
    /// Aggressive microcompact has been applied; full compaction is next.
    MicrocompactDone,
    /// Full compaction has been applied; no further recovery attempts will be
    /// made for this turn. Any further prompt-too-long error is returned as-is.
    FullCompactDone,
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    system_prompt: Vec<String>,
    max_iterations: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    auto_compaction_input_tokens_threshold: u32,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<Box<dyn HookProgressReporter>>,
    session_tracer: Option<SessionTracer>,
    /// Optional persistent memory surface. When present, the runtime runs a
    /// rule-based nudge pass every `NudgeConfig::interval_turns` turns to keep
    /// the memory layer fresh without an LLM call.
    persistent_memory: Option<PersistentMemory>,
    /// Turns elapsed since the last nudge fired. Reset to 0 whenever a nudge
    /// runs.
    turns_since_last_nudge: usize,
}

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
    ) -> Self {
        Self::new_with_features(
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            &RuntimeFeatureConfig::default(),
        )
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new_with_features(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
        feature_config: &RuntimeFeatureConfig,
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        Self {
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            max_iterations: usize::MAX,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
            persistent_memory: None,
            turns_since_last_nudge: 0,
        }
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_auto_compaction_input_tokens_threshold(mut self, threshold: u32) -> Self {
        self.auto_compaction_input_tokens_threshold = threshold;
        self
    }

    #[must_use]
    pub fn with_hook_abort_signal(mut self, hook_abort_signal: HookAbortSignal) -> Self {
        self.hook_abort_signal = hook_abort_signal;
        self
    }

    #[must_use]
    pub fn with_hook_progress_reporter(
        mut self,
        hook_progress_reporter: Box<dyn HookProgressReporter>,
    ) -> Self {
        self.hook_progress_reporter = Some(hook_progress_reporter);
        self
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.session_tracer = Some(session_tracer);
        self
    }

    /// Attach a persistent memory surface to the runtime.
    ///
    /// When set, the runtime runs a rule-based nudge pass at the end of every
    /// `NudgeConfig::interval_turns` turns, scanning recent user messages for
    /// `remember` / `prefer` / correction phrases and applying them to the
    /// memory layer. The snapshot captured at load time is the only view
    /// surfaced to the system prompt within the current session, so mid-turn
    /// mutations do not destabilize the prompt-cache prefix.
    #[must_use]
    pub fn with_persistent_memory(mut self, memory: PersistentMemory) -> Self {
        self.persistent_memory = Some(memory);
        self
    }

    /// Borrow the attached persistent memory surface, if any.
    #[must_use]
    pub fn persistent_memory(&self) -> Option<&PersistentMemory> {
        self.persistent_memory.as_ref()
    }

    fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_failure_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        let user_input = user_input.into();

        self.record_turn_started(&user_input);
        self.session
            .push_user_text(user_input)
            .map_err(|error| RuntimeError::new(error.to_string()))?;

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        let mut reactive_state = ReactiveCompactState::NotAttempted;

        loop {
            iterations += 1;
            if iterations > self.max_iterations {
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                );
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }

            let request = {
                let sliced = crate::compact::get_messages_after_compact_boundary(
                    &self.session.messages,
                );
                ApiRequest {
                    system_prompt: SystemPromptSplit::from_sections(self.system_prompt.clone()),
                    messages: sliced.to_vec(),
                }
            };
            let events = match self.api_client.stream(request) {
                Ok(events) => events,
                Err(error) => {
                    // Non-recoverable errors propagate immediately.
                    if !error.is_prompt_too_long() {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                    // Reactive compaction recovery: progressively shrink the
                    // transcript until the upstream accepts it or we exhaust
                    // the recovery steps.
                    match reactive_state {
                        ReactiveCompactState::NotAttempted => {
                            // Step 1: aggressive microcompact (preserve_recent=2).
                            let microcompacted = crate::compact::microcompact(
                                &self.session.messages,
                                REACTIVE_MICROCOMPACT_PRESERVE_RECENT,
                            );
                            self.session.messages = microcompacted;
                            reactive_state = ReactiveCompactState::MicrocompactDone;
                            continue;
                        }
                        ReactiveCompactState::MicrocompactDone => {
                            // Step 2: full compaction with Reactive trigger.
                            let result = crate::compact::compact_session_with_trigger(
                                &self.session,
                                CompactionConfig::default(),
                                crate::compact::CompactTrigger::Reactive,
                            );
                            if result.removed_message_count > 0 {
                                self.session = result.compacted_session;
                                reactive_state = ReactiveCompactState::FullCompactDone;
                                continue;
                            }
                            // Compaction removed nothing — nothing more we can do.
                            self.record_turn_failed(iterations, &error);
                            return Err(error);
                        }
                        ReactiveCompactState::FullCompactDone => {
                            // Already exhausted recovery steps; bail out to
                            // prevent an infinite retry loop.
                            self.record_turn_failed(iterations, &error);
                            return Err(error);
                        }
                    }
                }
            };
            let (assistant_message, usage, turn_prompt_cache_events) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            if let Some(usage) = usage {
                self.usage_tracker.record(usage);
            }
            prompt_cache_events.extend(turn_prompt_cache_events);
            let pending_tool_uses = assistant_message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.record_assistant_iteration(
                iterations,
                &assistant_message,
                pending_tool_uses.len(),
            );

            self.session
                .push_message(assistant_message.clone())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            assistant_messages.push(assistant_message);

            if pending_tool_uses.is_empty() {
                break;
            }

            for (tool_use_id, tool_name, input) in pending_tool_uses {
                let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);
                let effective_input = pre_hook_result
                    .updated_input()
                    .map_or_else(|| input.clone(), ToOwned::to_owned);
                let permission_context = PermissionContext::new(
                    pre_hook_result.permission_override(),
                    pre_hook_result.permission_reason().map(ToOwned::to_owned),
                );

                let permission_outcome = if pre_hook_result.is_cancelled() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook cancelled tool `{tool_name}`"),
                        ),
                    }
                } else if pre_hook_result.is_failed() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook failed for tool `{tool_name}`"),
                        ),
                    }
                } else if pre_hook_result.is_denied() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook denied tool `{tool_name}`"),
                        ),
                    }
                } else if let Some(prompt) = prompter.as_mut() {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        Some(*prompt),
                    )
                } else {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        None,
                    )
                };

                let result_message = match permission_outcome {
                    PermissionOutcome::Allow => {
                        self.record_tool_started(iterations, &tool_name);
                        // Intercept `session_search` and route it directly to
                        // the session's `HistoryIndex`. The tool is implemented
                        // inside the runtime (not registered with the external
                        // `ToolExecutor`) so it can read from the session's
                        // `Arc<HistoryIndex>` without going through a foreign
                        // dispatcher. All other tool names fall through to the
                        // standard executor.
                        let (mut output, mut is_error) =
                            if tool_name == "session_search" {
                                match self.execute_session_search(&effective_input) {
                                    Ok(output) => (output, false),
                                    Err(error) => (error.to_string(), true),
                                }
                            } else {
                                match self.tool_executor.execute(&tool_name, &effective_input) {
                                    Ok(output) => (output, false),
                                    Err(error) => (error.to_string(), true),
                                }
                            };
                        output = merge_hook_feedback(pre_hook_result.messages(), output, false);

                        let post_hook_result = if is_error {
                            self.run_post_tool_use_failure_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                            )
                        } else {
                            self.run_post_tool_use_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                                false,
                            )
                        };
                        if post_hook_result.is_denied()
                            || post_hook_result.is_failed()
                            || post_hook_result.is_cancelled()
                        {
                            is_error = true;
                        }
                        output = merge_hook_feedback(
                            post_hook_result.messages(),
                            output,
                            post_hook_result.is_denied()
                                || post_hook_result.is_failed()
                                || post_hook_result.is_cancelled(),
                        );

                        ConversationMessage::tool_result(tool_use_id, tool_name, output, is_error)
                    }
                    PermissionOutcome::Deny { reason } => ConversationMessage::tool_result(
                        tool_use_id,
                        tool_name,
                        merge_hook_feedback(pre_hook_result.messages(), reason, true),
                        true,
                    ),
                };
                self.session
                    .push_message(result_message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                self.record_tool_finished(iterations, &result_message);
                tool_results.push(result_message);
            }
        }

        // Apply microcompact to summarize aged tool results before considering
        // full auto-compaction. This is a lighter pass that replaces old
        // Read/Bash/Grep/Glob/LS outputs with one-line summaries, keeping the
        // recent `MICROCOMPACT_PRESERVE_RECENT` tool results verbatim. Edit /
        // Write / Delete and error results are always preserved.
        let microcompacted =
            crate::compact::microcompact(&self.session.messages, MICROCOMPACT_PRESERVE_RECENT);
        self.session.messages = microcompacted;

        let auto_compaction = self.maybe_auto_compact();

        let summary = TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            usage: self.usage_tracker.cumulative_usage(),
            auto_compaction,
        };
        self.record_turn_completed(&summary);

        // Periodic nudge: if enough turns have elapsed and we have a
        // persistent memory surface, scan recent messages for actionable
        // patterns (user corrections, "remember" keywords, etc.) and apply
        // them to the memory. This keeps the memory layer fresh without an
        // LLM call. The frozen snapshot is not touched, so the prompt-cache
        // prefix stays stable within the session — new facts only surface in
        // the next session.
        self.turns_since_last_nudge += 1;
        let nudge_config = NudgeConfig::default();
        if let Some(memory) = &mut self.persistent_memory {
            if should_nudge(self.turns_since_last_nudge, &nudge_config) {
                let lookback_msgs: Vec<_> = self
                    .session
                    .messages
                    .iter()
                    .rev()
                    .take(nudge_config.lookback_turns * 2)
                    .rev()
                    .cloned()
                    .collect();
                let actions = extract_nudge_actions(&lookback_msgs, memory, &nudge_config);
                for action in actions {
                    match action {
                        NudgeAction::Add { content, source } => {
                            memory.add_entry(&content, &source);
                        }
                        NudgeAction::Replace {
                            old_pattern,
                            new_content,
                            source,
                        } => {
                            memory.replace_entry(&old_pattern, &new_content, &source);
                        }
                        NudgeAction::Remove { pattern: _ } => {
                            // Removal not implemented in the rule-based
                            // version; skip silently.
                        }
                    }
                }
                self.turns_since_last_nudge = 0;
            }
        }

        Ok(summary)
    }

    /// Execute the `session_search` tool: query the FTS5 history index.
    ///
    /// Parses a JSON input of the form `{"query": "...", "top_k": 10}`,
    /// forwards the query to the session's [`HistoryIndex`], and returns
    /// a human-readable string of ranked matches. Each hit is rendered
    /// with its session ID, role, FTS5 rank, and a content snippet
    /// truncated to 500 characters so large tool outputs do not blow up
    /// the model's context window.
    ///
    /// When no `HistoryIndex` is attached to the session, this returns a
    /// soft-failure message (rather than an error) so the model can
    /// gracefully fall back to other strategies. Hard errors (invalid
    /// JSON, missing `query` field, SQLite failures) propagate as
    /// `Err(Box<dyn Error>)` and the runtime converts them into error
    /// tool results.
    fn execute_session_search(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(history_index) = self.session.history_index.as_ref() else {
            return Ok(
                "session_search is not available: no history index configured.".to_string(),
            );
        };

        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;
        let query = parsed
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("missing 'query' field")?;
        let top_k = parsed
            .get("top_k")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;

        let hits = history_index.search(query, top_k)?;

        if hits.is_empty() {
            return Ok(format!("No matches found for query: '{query}'"));
        }

        let mut output = format!("Found {} matches for '{}':\n\n", hits.len(), query);
        for (i, hit) in hits.iter().enumerate() {
            let snippet: String = hit.content.chars().take(500).collect();
            output.push_str(&format!(
                "## Match {} (session: {}, role: {}, rank: {:.3})\n{}\n\n",
                i + 1,
                hit.session_id,
                hit.role,
                hit.rank,
                snippet,
            ));
        }
        Ok(output)
    }

    #[must_use]
    pub fn compact(&self, config: CompactionConfig) -> CompactionResult {
        compact_session(&self.session, config)
    }

    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_session_tokens(&self.session)
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTracker {
        &self.usage_tracker
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn api_client_mut(&mut self) -> &mut C {
        &mut self.api_client
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    #[must_use]
    pub fn fork_session(&self, branch_name: Option<String>) -> Session {
        self.session.fork(branch_name)
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }

    fn maybe_auto_compact(&mut self) -> Option<AutoCompactionEvent> {
        if self.usage_tracker.cumulative_usage().input_tokens
            < self.auto_compaction_input_tokens_threshold
        {
            return None;
        }

        // Use the default CompactionConfig (max_estimated_tokens: 10_000) so that
        // small sessions are not pointlessly compacted. The auto-compact trigger
        // above (input_tokens >= threshold) already decided compaction is needed;
        // max_estimated_tokens just prevents compacting a session whose estimated
        // token footprint is still small (which would generate a summary for no
        // benefit). With CJK-aware estimation (Task 10), this check is now reliable.
        let result = compact_session(&self.session, CompactionConfig::default());

        if result.removed_message_count == 0 {
            return None;
        }

        self.session = result.compacted_session;
        Some(AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
        })
    }

    fn record_turn_started(&self, user_input: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "user_input".to_string(),
            Value::String(user_input.to_string()),
        );
        session_tracer.record("turn_started", attributes);
    }

    fn record_assistant_iteration(
        &self,
        iteration: usize,
        assistant_message: &ConversationMessage,
        pending_tool_use_count: usize,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "assistant_blocks".to_string(),
            Value::from(assistant_message.blocks.len() as u64),
        );
        attributes.insert(
            "pending_tool_use_count".to_string(),
            Value::from(pending_tool_use_count as u64),
        );
        session_tracer.record("assistant_iteration_completed", attributes);
    }

    fn record_tool_started(&self, iteration: usize, tool_name: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        session_tracer.record("tool_execution_started", attributes);
    }

    fn record_tool_finished(&self, iteration: usize, result_message: &ConversationMessage) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let Some(ContentBlock::ToolResult {
            tool_name,
            is_error,
            ..
        }) = result_message.blocks.first()
        else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("tool_name".to_string(), Value::String(tool_name.clone()));
        attributes.insert("is_error".to_string(), Value::Bool(*is_error));
        session_tracer.record("tool_execution_finished", attributes);
    }

    fn record_turn_completed(&self, summary: &TurnSummary) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "iterations".to_string(),
            Value::from(summary.iterations as u64),
        );
        attributes.insert(
            "assistant_messages".to_string(),
            Value::from(summary.assistant_messages.len() as u64),
        );
        attributes.insert(
            "tool_results".to_string(),
            Value::from(summary.tool_results.len() as u64),
        );
        attributes.insert(
            "prompt_cache_events".to_string(),
            Value::from(summary.prompt_cache_events.len() as u64),
        );
        session_tracer.record("turn_completed", attributes);
    }

    fn record_turn_failed(&self, iteration: usize, error: &RuntimeError) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("turn_failed", attributes);
    }
}

/// Reads the automatic compaction threshold from the environment.
#[must_use]
pub fn auto_compaction_threshold_from_env() -> u32 {
    parse_auto_compaction_threshold(
        std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR)
            .ok()
            .as_deref(),
    )
}

#[must_use]
fn parse_auto_compaction_threshold(value: Option<&str>) -> u32 {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|threshold| *threshold > 0)
        .unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<
    (
        ConversationMessage,
        Option<TokenUsage>,
        Vec<PromptCacheEvent>,
    ),
    RuntimeError,
> {
    let mut text = String::new();
    let mut blocks = Vec::new();
    let mut prompt_cache_events = Vec::new();
    let mut finished = false;
    let mut usage = None;

    for event in events {
        match event {
            AssistantEvent::Thinking {
                thinking,
                signature,
            } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature,
                });
            }
            AssistantEvent::TextDelta(delta) => text.push_str(&delta),
            AssistantEvent::ToolUse { id, name, input } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
            AssistantEvent::Usage(value) => usage = Some(value),
            AssistantEvent::PromptCache(event) => prompt_cache_events.push(event),
            AssistantEvent::MessageStop => {
                finished = true;
            }
        }
    }

    flush_text_block(&mut text, &mut blocks);

    if !finished {
        return Err(RuntimeError::new(
            "assistant stream ended without a message stop event",
        ));
    }
    if blocks.is_empty() {
        return Err(RuntimeError::new("assistant stream produced no content"));
    }

    Ok((
        ConversationMessage::assistant_with_usage(blocks, usage),
        usage,
        prompt_cache_events,
    ))
}

fn flush_text_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text),
        });
    }
}

fn format_hook_message(result: &HookRunResult, fallback: &str) -> String {
    if result.messages().is_empty() {
        fallback.to_string()
    } else {
        result.messages().join("\n")
    }
}

fn merge_hook_feedback(messages: &[String], output: String, is_error: bool) -> String {
    if messages.is_empty() {
        return output;
    }

    let mut sections = Vec::new();
    if !output.trim().is_empty() {
        sections.push(output);
    }
    let label = if is_error {
        "Hook feedback (error)"
    } else {
        "Hook feedback"
    };
    sections.push(format!("{label}:\n{}", messages.join("\n")));
    sections.join("\n\n")
}

type ToolHandler = Box<dyn FnMut(&str) -> Result<String, ToolError>>;

/// Simple in-memory tool executor for tests and lightweight integrations.
#[derive(Default)]
pub struct StaticToolExecutor {
    handlers: BTreeMap<String, ToolHandler>,
}

impl StaticToolExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn register(
        mut self,
        tool_name: impl Into<String>,
        handler: impl FnMut(&str) -> Result<String, ToolError> + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Box::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get_mut(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_assistant_message, parse_auto_compaction_threshold, ApiClient, ApiRequest,
        AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RuntimeError,
        SESSION_SEARCH_TOOL_SPEC, StaticToolExecutor, ToolExecutor,
        DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::memory::PersistentMemory;
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{
        ProjectContext, SystemPromptBuilder, SystemPromptSplit, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
    };
    use crate::session::{ContentBlock, MessageRole, Session};
    use crate::usage::TokenUsage;
    use crate::ToolError;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use telemetry::{MemoryTelemetrySink, SessionTracer, TelemetryEvent};

    struct ScriptedApiClient {
        call_count: usize,
    }

    impl ApiClient for ScriptedApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            match self.call_count {
                1 => {
                    assert!(request
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::User));
                    Ok(vec![
                        AssistantEvent::TextDelta("Let me calculate that.".to_string()),
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: "2,2".to_string(),
                        },
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 6,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 2,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                2 => {
                    let last_message = request
                        .messages
                        .last()
                        .expect("tool result should be present");
                    assert_eq!(last_message.role, MessageRole::Tool);
                    Ok(vec![
                        AssistantEvent::TextDelta("The answer is 4.".to_string()),
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 24,
                            output_tokens: 4,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 3,
                        }),
                        AssistantEvent::PromptCache(PromptCacheEvent {
                            unexpected: true,
                            reason:
                                "cache read tokens dropped while prompt fingerprint remained stable"
                                    .to_string(),
                            previous_cache_read_input_tokens: 6_000,
                            current_cache_read_input_tokens: 1_000,
                            token_drop: 5_000,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                _ => unreachable!("extra API call"),
            }
        }
    }

    struct PromptAllowOnce;

    impl PermissionPrompter for PromptAllowOnce {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "add");
            PermissionPromptDecision::Allow
        }
    }

    #[test]
    fn runs_user_to_tool_to_result_loop_end_to_end_and_tracks_usage() {
        let api_client = ScriptedApiClient { call_count: 0 };
        let tool_executor = StaticToolExecutor::new().register("add", |input| {
            let total = input
                .split(',')
                .map(|part| part.parse::<i32>().expect("input must be valid integer"))
                .sum::<i32>();
            Ok(total.to_string())
        });
        let permission_policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite);
        let system_prompt = SystemPromptBuilder::new()
            .with_project_context(ProjectContext {
                cwd: PathBuf::from("/tmp/project"),
                current_date: "2026-03-31".to_string(),
                git_status: None,
                git_diff: None,
                git_context: None,
                instruction_files: Vec::new(),
            })
            .with_os("linux", "6.8")
            .build();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
        );

        let summary = runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(summary.prompt_cache_events.len(), 1);
        assert_eq!(runtime.session().messages.len(), 4);
        assert_eq!(summary.usage.output_tokens, 10);
        assert_eq!(summary.auto_compaction, None);
        assert!(matches!(
            runtime.session().messages[1].blocks[1],
            ContentBlock::ToolUse { .. }
        ));
        assert!(matches!(
            runtime.session().messages[2].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn records_runtime_session_trace_events() {
        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-runtime", sink.clone());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        )
        .with_session_tracer(tracer);

        runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        let events = sink.events();
        let trace_names = events
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) => Some(trace.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(trace_names.contains(&"turn_started"));
        assert!(trace_names.contains(&"assistant_iteration_completed"));
        assert!(trace_names.contains(&"tool_execution_started"));
        assert!(trace_names.contains(&"tool_execution_finished"));
        assert!(trace_names.contains(&"turn_completed"));
    }

    #[test]
    fn records_denied_tool_results_when_prompt_rejects() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("I could not use the tool.".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("use the tool", Some(&mut RejectPrompter))
            .expect("conversation should continue after denied tool");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "not now"
        ));
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_blocks() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook denies")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'blocked by hook'; exit 2")],
                Vec::new(),
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook denial");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook denial should produce an error result: {output}"
        );
        assert!(
            output.contains("denied tool") || output.contains("blocked by hook"),
            "unexpected hook denial output: {output:?}"
        );
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_fails() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("failed".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook fails")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'broken hook'; exit 1")],
                Vec::new(),
                Vec::new(),
            )),
        );

        // when
        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook failure");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook failure should produce an error result: {output}"
        );
        assert!(
            output.contains("exited with status 1") || output.contains("broken hook"),
            "unexpected hook failure output: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: r#"{"lhs":2,"rhs":2}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'pre hook ran'")],
                vec![shell_snippet("printf 'post hook ran'")],
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use add", None)
            .expect("tool loop succeeds");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            !*is_error,
            "post hook should preserve non-error result: {output:?}"
        );
        assert!(
            output.contains('4'),
            "tool output missing value: {output:?}"
        );
        assert!(
            output.contains("pre hook ran"),
            "tool output missing pre hook feedback: {output:?}"
        );
        assert!(
            output.contains("post hook ran"),
            "tool output missing post hook feedback: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_use_failure_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "fail".to_string(),
                            input: r#"{"path":"README.md"}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new()
                .register("fail", |_input| Err(ToolError::new("tool exploded"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                Vec::new(),
                vec![shell_snippet("printf 'post hook should not run'")],
                vec![shell_snippet("printf 'failure hook ran'")],
            )),
        );

        // when
        let summary = runtime
            .run_turn("use fail", None)
            .expect("tool loop succeeds");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "failure hook path should preserve error result: {output:?}"
        );
        assert!(
            output.contains("tool exploded"),
            "tool output missing failure reason: {output:?}"
        );
        assert!(
            output.contains("failure hook ran"),
            "tool output missing failure hook feedback: {output:?}"
        );
        assert!(
            !output.contains("post hook should not run"),
            "normal post hook should not run on tool failure: {output:?}"
        );
    }

    #[test]
    fn reconstructs_usage_tracker_from_restored_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session
            .messages
            .push(crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "earlier".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                }),
            ));

        let runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
    }

    #[test]
    fn compacts_session_after_turns() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        runtime.run_turn("a", None).expect("turn a");
        runtime.run_turn("b", None).expect("turn b");
        runtime.run_turn("c", None).expect("turn c");

        let result = runtime.compact(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        });
        assert!(result.summary.contains("Conversation summary"));
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert_eq!(
            result.compacted_session.session_id,
            runtime.session().session_id
        );
        assert!(result.compacted_session.compaction.is_some());
    }

    #[test]
    fn persists_conversation_turn_messages_to_jsonl_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("persisted-turn");
        let session = Session::new().with_persistence_path(path.clone());
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("persist this turn", None)
            .expect("turn should succeed");

        let restored = Session::load_from_path(&path).expect("persisted session should reload");
        fs::remove_file(&path).expect("temp session file should be removable");

        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].role, MessageRole::User);
        assert_eq!(restored.messages[1].role, MessageRole::Assistant);
        assert_eq!(restored.session_id, runtime.session().session_id);
    }

    #[test]
    fn forks_runtime_session_without_mutating_original() {
        let mut session = Session::new();
        session
            .push_user_text("branch me")
            .expect("message should append");

        let runtime = ConversationRuntime::new(
            session.clone(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let forked = runtime.fork_session(Some("alt-path".to_string()));

        assert_eq!(forked.messages, session.messages);
        assert_ne!(forked.session_id, session.session_id);
        assert_eq!(
            forked
                .fork
                .as_ref()
                .map(|fork| (fork.parent_session_id.as_str(), fork.branch_name.as_deref())),
            Some((session.session_id.as_str(), Some("alt-path")))
        );
        assert!(runtime.session().fork.is_none());
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-conversation-{label}-{nanos}.json"))
    }

    #[cfg(windows)]
    fn shell_snippet(script: &str) -> String {
        script.replace('\'', "\"")
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[test]
    fn auto_compacts_when_cumulative_input_threshold_is_crossed() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 120_000,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        // The first user message is intentionally large so the session's
        // estimated token count exceeds CompactionConfig::default()
        // (max_estimated_tokens: 10_000). With CJK-aware estimation
        // (chars().count()/2+1), 20_000 chars → 10_001 tokens.
        session.messages = vec![
            crate::session::ConversationMessage::user_text("x".repeat(20_000)),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 2,
            })
        );
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    #[test]
    fn skips_auto_compaction_below_threshold() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 99_999,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[test]
    fn auto_compaction_threshold_defaults_and_parses_values() {
        assert_eq!(
            parse_auto_compaction_threshold(None),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(parse_auto_compaction_threshold(Some("4321")), 4321);
        assert_eq!(
            parse_auto_compaction_threshold(Some("0")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(
            parse_auto_compaction_threshold(Some("not-a-number")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
    }

    #[test]
    fn build_assistant_message_requires_message_stop_event() {
        // given
        let events = vec![AssistantEvent::TextDelta("hello".to_string())];

        // when
        let error = build_assistant_message(events)
            .expect_err("assistant messages should require a stop event");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream ended without a message stop event"));
    }

    #[test]
    fn build_assistant_message_requires_content() {
        // given
        let events = vec![AssistantEvent::MessageStop];

        // when
        let error =
            build_assistant_message(events).expect_err("assistant messages should require content");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream produced no content"));
    }

    #[test]
    fn build_assistant_message_places_thinking_block_before_text_and_tool_use() {
        // given
        let events = vec![
            AssistantEvent::Thinking {
                thinking: "pondering".to_string(),
                signature: Some("sig".to_string()),
            },
            AssistantEvent::TextDelta("hello".to_string()),
            AssistantEvent::ToolUse {
                id: "tool-1".to_string(),
                name: "echo".to_string(),
                input: "payload".to_string(),
            },
            AssistantEvent::MessageStop,
        ];

        // when
        let (message, _, _) = build_assistant_message(events)
            .expect("assistant message should preserve thinking, text, and tool blocks");

        // then
        assert_eq!(
            message.blocks,
            vec![
                ContentBlock::Thinking {
                    thinking: "pondering".to_string(),
                    signature: Some("sig".to_string()),
                },
                ContentBlock::Text {
                    text: "hello".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "echo".to_string(),
                    input: "payload".to_string(),
                },
            ]
        );
    }

    #[test]
    fn static_tool_executor_rejects_unknown_tools() {
        // given
        let mut executor = StaticToolExecutor::new();

        // when
        let error = executor
            .execute("missing", "{}")
            .expect_err("unregistered tools should fail");

        // then
        assert_eq!(error.to_string(), "unknown tool: missing");
    }

    #[test]
    fn run_turn_errors_when_max_iterations_is_exceeded() {
        struct LoopingApi;

        impl ApiClient for LoopingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "echo".to_string(),
                        input: "payload".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            LoopingApi,
            StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_max_iterations(1);

        // when
        let error = runtime
            .run_turn("loop", None)
            .expect_err("conversation loop should stop after the configured limit");

        // then
        assert!(error
            .to_string()
            .contains("conversation loop exceeded the maximum number of iterations"));
    }

    #[test]
    fn run_turn_propagates_api_errors() {
        struct FailingApi;

        impl ApiClient for FailingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        // when
        let error = runtime
            .run_turn("hello", None)
            .expect_err("API failures should propagate");

        // then
        assert_eq!(error.to_string(), "upstream failed");
    }

    #[test]
    fn api_request_carries_system_prompt_split() {
        let split = SystemPromptSplit::from_sections(vec![
            "static".to_string(),
            SYSTEM_PROMPT_DYNAMIC_BOUNDARY.to_string(),
            "dynamic".to_string(),
        ]);
        let request = ApiRequest {
            system_prompt: split,
            messages: Vec::new(),
        };
        assert_eq!(request.system_prompt.static_sections, vec!["static"]);
        assert_eq!(request.system_prompt.dynamic_sections, vec!["dynamic"]);
    }

    #[test]
    fn reactive_compact_retries_on_prompt_too_long() {
        // API returns prompt-too-long on the first call, then succeeds after
        // reactive microcompact summarizes the aged Read tool results.
        struct RetryAfterMicrocompactApi {
            call_count: usize,
        }
        impl ApiClient for RetryAfterMicrocompactApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => Err(RuntimeError::new(
                        "prompt is too long for the model context window",
                    )),
                    2 => Ok(vec![
                        AssistantEvent::TextDelta("recovered".to_string()),
                        AssistantEvent::MessageStop,
                    ]),
                    _ => unreachable!("unexpected extra API call"),
                }
            }
        }

        // Build a session with four Read tool-result turns. The reactive
        // microcompact (preserve_recent=2) should summarize the two oldest
        // results while keeping the two most recent verbatim.
        let big_output = "line-of-content\n".repeat(200);
        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "Read".to_string(),
                input: "old-file-a.txt".to_string(),
            }]),
            crate::session::ConversationMessage::tool_result(
                "tool-1",
                "Read",
                big_output.clone(),
                false,
            ),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-2".to_string(),
                name: "Read".to_string(),
                input: "old-file-b.txt".to_string(),
            }]),
            crate::session::ConversationMessage::tool_result(
                "tool-2",
                "Read",
                big_output.clone(),
                false,
            ),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-3".to_string(),
                name: "Read".to_string(),
                input: "recent-file-c.txt".to_string(),
            }]),
            crate::session::ConversationMessage::tool_result(
                "tool-3",
                "Read",
                big_output.clone(),
                false,
            ),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-4".to_string(),
                name: "Read".to_string(),
                input: "recent-file-d.txt".to_string(),
            }]),
            crate::session::ConversationMessage::tool_result(
                "tool-4",
                "Read",
                big_output,
                false,
            ),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            RetryAfterMicrocompactApi { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed after reactive microcompact");

        // API called twice: first failed, second succeeded after microcompact.
        assert_eq!(runtime.api_client_mut().call_count, 2);
        assert_eq!(summary.iterations, 2);

        // The two oldest Read results should be summarized; the two most
        // recent should be preserved verbatim.
        let tool_result_outputs: Vec<&str> = runtime
            .session()
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_result_outputs.len(), 4);
        assert!(
            tool_result_outputs[0].contains("output summarized"),
            "oldest tool result should be summarized"
        );
        assert!(
            tool_result_outputs[1].contains("output summarized"),
            "second-oldest tool result should be summarized"
        );
        assert!(
            tool_result_outputs[2].contains("line-of-content"),
            "third tool result should be verbatim"
        );
        assert!(
            tool_result_outputs[3].contains("line-of-content"),
            "most recent tool result should be verbatim"
        );
    }

    #[test]
    fn reactive_compact_does_not_loop_infinitely() {
        // API always returns prompt-too-long. The reactive state machine
        // should exhaust both recovery steps (microcompact + full compact)
        // and bail out instead of retrying forever.
        struct AlwaysPromptTooLongApi {
            call_count: usize,
        }
        impl ApiClient for AlwaysPromptTooLongApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                Err(RuntimeError::new(
                    "prompt exceeds maximum context length",
                ))
            }
        }

        // Small session: should_compact returns false, so full compaction
        // removes nothing and the recovery bails after two API calls.
        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("small"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "response".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            AlwaysPromptTooLongApi { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn("trigger", None)
            .expect_err("turn should fail when prompt stays too long");

        // The state machine should have stopped after two attempts, not
        // retried indefinitely.
        assert_eq!(runtime.api_client_mut().call_count, 2);
        assert!(error.is_prompt_too_long());
    }

    #[test]
    fn reactive_compact_falls_back_to_full_compaction() {
        // API fails twice then succeeds: microcompact is tried first, then
        // full compaction, then the request finally goes through.
        struct FailTwiceThenSucceedApi {
            call_count: usize,
        }
        impl ApiClient for FailTwiceThenSucceedApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 | 2 => Err(RuntimeError::new("prompt is too long for the model")),
                    3 => Ok(vec![
                        AssistantEvent::TextDelta("recovered".to_string()),
                        AssistantEvent::MessageStop,
                    ]),
                    _ => unreachable!("unexpected extra API call"),
                }
            }
        }

        // Large session: even after microcompact (which has no tool results
        // to summarize here), should_compact still returns true so the full
        // compaction step actually removes messages.
        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("x".repeat(20_000)),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("five"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "six".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            FailTwiceThenSucceedApi { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed after full reactive compaction");

        // Three API calls: fail → microcompact → fail → full compact → succeed.
        assert_eq!(runtime.api_client_mut().call_count, 3);
        assert_eq!(summary.iterations, 3);

        // The reactive full compaction should have embedded a boundary marker
        // with the Reactive trigger in the session's System message.
        let boundary = crate::compact::extract_compact_boundary(&runtime.session().messages);
        assert!(
            boundary.is_some(),
            "session should contain a compact boundary marker after reactive compaction"
        );
        let boundary = boundary.expect("boundary checked above");
        assert_eq!(
            boundary.trigger,
            crate::compact::CompactTrigger::Reactive,
            "boundary trigger should be Reactive"
        );
        assert!(
            boundary.messages_summarized > 0,
            "reactive compaction should have removed at least one message"
        );
    }

    // ----- session_search tool tests -----
    //
    // The runtime intercepts `session_search` tool calls inside `run_turn`
    // and routes them to the session's `HistoryIndex` (FTS5). The tests
    // below cover both the direct `execute_session_search` helper and the
    // end-to-end interception path through `run_turn`.

    /// Minimal API client that never actually streams a real response —
    /// used for tests that exercise `execute_session_search` directly
    /// without driving a full `run_turn` loop.
    struct NoopApi;
    impl ApiClient for NoopApi {
        fn stream(
            &mut self,
            _request: ApiRequest,
        ) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(vec![
                AssistantEvent::TextDelta("noop".to_string()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    fn open_temp_history_index() -> (tempfile::NamedTempFile, crate::history_search::HistoryIndex)
    {
        let file = tempfile::NamedTempFile::new().expect("create temp db file");
        let index =
            crate::history_search::HistoryIndex::open(file.path()).expect("open history index");
        (file, index)
    }

    #[test]
    fn session_search_returns_message_when_no_history_index_configured() {
        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        // With no `history_index` attached, the helper returns a soft
        // failure message (Ok) rather than an Err so the model can recover.
        let output = runtime
            .execute_session_search(r#"{"query":"anything"}"#)
            .expect("soft failure should not propagate as error");
        assert!(
            output.contains("session_search is not available"),
            "missing 'not available' message: {output}"
        );
    }

    #[test]
    fn session_search_returns_results_when_indexed() {
        let (_file, index) = open_temp_history_index();
        index
            .index_message(
                "How do I configure the rust toolchain?",
                "sess-a",
                "user",
                0,
                1_000,
            )
            .expect("index msg 0");
        index
            .index_message(
                "You can use rustup to configure the rust toolchain.",
                "sess-a",
                "assistant",
                1,
                2_000,
            )
            .expect("index msg 1");

        let session = Session::new().with_history_index(Arc::new(index));
        let runtime = ConversationRuntime::new(
            session,
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let output = runtime
            .execute_session_search(r#"{"query":"rust toolchain","top_k":5}"#)
            .expect("search should succeed");
        assert!(
            output.contains("Found 2 matches"),
            "expected 2 matches in output: {output}"
        );
        assert!(
            output.contains("configure the rust toolchain"),
            "user message missing from output: {output}"
        );
        assert!(
            output.contains("rustup to configure"),
            "assistant message missing from output: {output}"
        );
        assert!(
            output.contains("session: sess-a"),
            "session id missing from output: {output}"
        );
        assert!(
            output.contains("role: user"),
            "user role missing from output: {output}"
        );
        assert!(
            output.contains("role: assistant"),
            "assistant role missing from output: {output}"
        );
        // Each hit should carry a rank (FTS5 BM25 score).
        assert!(
            output.contains("rank:"),
            "rank missing from output: {output}"
        );
    }

    #[test]
    fn session_search_handles_invalid_json() {
        let (_file, index) = open_temp_history_index();

        let session = Session::new().with_history_index(Arc::new(index));
        let runtime = ConversationRuntime::new(
            session,
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let error = runtime
            .execute_session_search("this is not json")
            .expect_err("invalid JSON should propagate as error");
        assert!(
            error.to_string().contains("invalid input JSON"),
            "expected invalid JSON error, got: {error}"
        );
    }

    #[test]
    fn session_search_errors_when_query_field_missing() {
        let (_file, index) = open_temp_history_index();

        let session = Session::new().with_history_index(Arc::new(index));
        let runtime = ConversationRuntime::new(
            session,
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let error = runtime
            .execute_session_search(r#"{"top_k":5}"#)
            .expect_err("missing 'query' field should propagate as error");
        assert!(
            error.to_string().contains("missing 'query' field"),
            "expected missing 'query' error, got: {error}"
        );
    }

    #[test]
    fn session_search_returns_no_matches_message_when_index_empty() {
        let (_file, index) = open_temp_history_index();

        let session = Session::new().with_history_index(Arc::new(index));
        let runtime = ConversationRuntime::new(
            session,
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let output = runtime
            .execute_session_search(r#"{"query":"nonexistentterm"}"#)
            .expect("empty results should be a soft success");
        assert!(
            output.contains("No matches found"),
            "expected 'no matches' message: {output}"
        );
    }

    /// End-to-end test: the API client emits a `session_search` tool_use,
    /// the runtime intercepts it (bypassing `StaticToolExecutor` which has
    /// no handler registered), routes it to the `HistoryIndex`, and
    /// forwards the formatted result back to the model on the next call.
    #[test]
    fn run_turn_intercepts_session_search_tool_call() {
        struct SearchApi {
            calls: usize,
        }
        impl ApiClient for SearchApi {
            fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "session_search".to_string(),
                            input: r#"{"query":"rust toolchain"}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        // The tool result must have been inserted with the
                        // formatted FTS5 hits before the second API call.
                        let last = request.messages.last().expect("tool result present");
                        assert_eq!(last.role, MessageRole::Tool);
                        let output = match &last.blocks[0] {
                            ContentBlock::ToolResult { output, .. } => output.clone(),
                            _ => panic!("expected tool result block"),
                        };
                        assert!(
                            output.contains("Found 2 matches"),
                            "expected matches in tool result: {output}"
                        );
                        Ok(vec![
                            AssistantEvent::TextDelta("here is what I found".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("unexpected extra API call"),
                }
            }
        }

        let (_file, index) = open_temp_history_index();
        index
            .index_message(
                "configure the rust toolchain",
                "sess-a",
                "user",
                0,
                1_000,
            )
            .expect("index msg 0");
        index
            .index_message(
                "use rustup to configure the rust toolchain",
                "sess-a",
                "assistant",
                1,
                2_000,
            )
            .expect("index msg 1");

        let session = Session::new().with_history_index(Arc::new(index));
        let mut runtime = ConversationRuntime::new(
            session,
            SearchApi { calls: 0 },
            // Intentionally empty: session_search must NOT fall through to
            // this executor. If it did, StaticToolExecutor would return
            // "unknown tool: session_search" and the test would fail.
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("find prior rust discussion", None)
            .expect("turn should complete");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            !*is_error,
            "session_search should not produce an error result: {output}"
        );
        assert!(
            output.contains("Found 2 matches"),
            "missing matches in tool result: {output}"
        );
    }

    #[test]
    fn session_search_tool_spec_is_valid_json_with_expected_fields() {
        // The tool spec is exposed as a `pub const` so external registrars
        // (e.g. main.rs's tool registry) can register it with the model.
        // Verify it parses as valid JSON and carries the schema fields the
        // runtime's `execute_session_search` expects to find in the input.
        let spec: serde_json::Value = serde_json::from_str(SESSION_SEARCH_TOOL_SPEC)
            .expect("SESSION_SEARCH_TOOL_SPEC must be valid JSON");
        assert_eq!(spec["name"], "session_search");
        assert!(
            spec["description"]
                .as_str()
                .is_some_and(|d| d.contains("history")),
            "description should mention history: {spec}"
        );
        assert_eq!(spec["input_schema"]["type"], "object");
        assert_eq!(spec["input_schema"]["properties"]["query"]["type"], "string");
        assert_eq!(
            spec["input_schema"]["properties"]["top_k"]["type"],
            "integer"
        );
        assert!(
            spec["input_schema"]["required"]
                .as_array()
                .is_some_and(|arr| arr.iter().any(|v| v == "query")),
            "'query' must be in required array: {spec}"
        );
    }

    // ----- Periodic nudge integration with run_turn -----

    #[test]
    fn nudge_applies_remember_keyword_to_persistent_memory() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let memory_path = temp_session_path("nudge-memory");
        let memory = PersistentMemory::empty(&memory_path);

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_persistent_memory(memory);

        // Pre-warm the turn counter so the very next turn triggers a nudge
        // (NudgeConfig::default().interval_turns == 5).
        runtime.turns_since_last_nudge = 4;

        runtime
            .run_turn("remember to use tabs not spaces", None)
            .expect("turn should succeed");

        let memory = runtime
            .persistent_memory()
            .expect("persistent memory should be attached");
        let has_tabs_entry = memory
            .entries()
            .iter()
            .any(|entry| entry.content.contains("tabs"));
        assert!(
            has_tabs_entry,
            "persistent memory should contain a 'tabs' entry after nudge: {:?}",
            memory.entries()
        );

        let _ = std::fs::remove_file(&memory_path);
    }

    #[test]
    fn nudge_skips_when_no_persistent_memory() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        // No persistent_memory attached — the nudge branch must be skipped
        // without panicking even after the interval elapses.

        for i in 0..6 {
            runtime
                .run_turn(format!("turn {i}"), None)
                .expect("turn should succeed");
        }

        // Sanity check: still no memory surface, and we did not panic.
        assert!(runtime.persistent_memory().is_none());
    }
}
