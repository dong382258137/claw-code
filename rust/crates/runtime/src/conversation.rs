use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
// Harness L(生命周期)层接入:run_turn 失败分支调用 RecoveryOrchestrator 尝试一次自动恢复。
// 详见 docs/harness-engineering-optimization-plan.md Step 1.2。
use crate::recovery_orchestrator::RecoveryOrchestrator;
use crate::recovery_recipes::RecoveryResult;
// Harness O(编排)层 + V(验证)层接入:Plan/Execute/Review 三段循环。
// 默认不启用(plan_mode=false),需通过 CLI `--enable-plan-mode` 开启。
// 缓存保护:PlanArtifact 末尾追加到 prompt 变动区(dynamic_sections),
// 不污染 system_prompt + tools_schema 的"绝对稳定区"。详见
// docs/harness-engineering-optimization-plan.md Step 2.1 与 §5.2。
use crate::planner::{
    assess_complexity, persist_plan_artifact, ComplexityAssessment, PlanArtifact,
    PreCompletionChecklistMiddleware, ReviewResult,
};
// Harness O(可观测性)层接入:LoopDetectionMiddleware 打断 Doom Loop。
// 在 PostToolUse hook 中调用 LoopDetector::record_edit,根据 LoopAction
// 决定 Continue / InjectContext / Abort。详见
// docs/harness-engineering-optimization-plan.md Step 2.2。
use crate::loop_detection::{LoopAction, LoopDetector};
// Harness C(Context Management)层接入:ContextAssembler 统一 prompt 注入。
// 当注入时,PlanArtifact render 通过 assembler 收集到 Goal source,
// 取 volatile_content() 作为 dynamic_sections。详见
// docs/harness-engineering-optimization-plan.md Step 2.3。
use crate::context_assembler::{ContextAssembler, ContextSource};
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::trace_analyzer::{TraceAnalyzer, TraceRecord};
use crate::usage::{TokenUsage, UsageTracker};
use crate::worker_boot::WorkerFailureKind;
use std::cell::Cell;
use std::path::PathBuf;

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
#[allow(dead_code)] // Reserved for future registration via main.rs tool registry.
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
    hook_progress_reporter: Option<Box<dyn HookProgressReporter + Send>>,
    session_tracer: Option<SessionTracer>,
    /// Optional persistent memory surface. When present, the runtime runs a
    /// rule-based nudge pass every `NudgeConfig::interval_turns` turns to keep
    /// the memory layer fresh without an LLM call.
    persistent_memory: Option<PersistentMemory>,
    /// Turns elapsed since the last nudge fired. Reset to 0 whenever a nudge
    /// runs.
    turns_since_last_nudge: usize,
    /// Recovery orchestrator invoked on the `run_turn` failure path. Wraps
    /// `recovery_recipes` so callers can request recovery by
    /// [`WorkerFailureKind`] without coupling to recipe lookup. Each scenario
    /// enforces the recipe's `max_attempts` policy (default 1) before
    /// escalation, preventing infinite retry loops.
    recovery_orchestrator: RecoveryOrchestrator,
    /// Harness O(编排)层:Plan/Execute/Review 三段循环开关。
    /// 默认 `false`,需通过 CLI `--enable-plan-mode` 或 settings.json
    /// `planMode: true` 开启。详见
    /// `docs/harness-engineering-optimization-plan.md` Step 2.1。
    plan_mode_enabled: bool,
    /// 当前活跃的 PlanArtifact。`None` 表示当前 turn 无活跃 plan。
    /// 当 `plan_mode_enabled=true` 且 `assess_complexity` 返回 `Complex` 时,
    /// 在 `run_turn` 入口创建并 persist,turn 结束时清空(或 replan 时保留)。
    active_plan: Option<PlanArtifact>,
    /// Review 阶段中间件,决定 AllPassed / Replan / Failed。
    /// 默认 `max_replans=3`,通过 `with_plan_reviewer` 可定制。
    plan_reviewer: PreCompletionChecklistMiddleware,
    /// 用于 `persist_plan_artifact` 的工作区根目录。
    /// `None` 时跳过持久化(仅内存)。生产环境应通过
    /// `with_workspace_root` 注入 `cwd`。
    workspace_root: Option<PathBuf>,
    /// Harness O(可观测性)层:Doom Loop 检测器。
    /// 在 PostToolUse hook 中记录每次 Edit/Write/MultiEdit 工具的文件路径,
    /// 同文件 5 次编辑触发 InjectContext,10 次触发 Abort。详见
    /// docs/harness-engineering-optimization-plan.md Step 2.2。
    loop_detector: LoopDetector,
    /// Harness C(Context Management)层:统一 prompt 注入器。
    /// `None` 时走原 SystemPromptSplit + 手动 push 逻辑;
    /// `Some` 时 PlanArtifact render 通过 assembler 收集到 Goal source,
    /// 取 volatile_content() 作为 dynamic_sections。详见
    /// docs/harness-engineering-optimization-plan.md Step 2.3。
    context_assembler: Option<ContextAssembler>,
    /// BUG-6 修复:语义召回结果,在 run_turn 入口填充,request 构造时注入。
    ///
    /// 当 persistent_memory 存在时,run_turn 入口调用
    /// `persistent_memory.semantic_recall(user_input, k=3)` 获取 top-3 记忆,
    /// 渲染成文本块存到此字段。request 构造时通过 ContextAssembler Memory
    /// source 或手动 push 注入到 dynamic_sections。turn 结束时清空。
    /// 详见 docs/harness-engineering-optimization-plan.md Step 2.4。
    pending_semantic_context: Option<String>,
    /// BUG-7 修复:Harness V(验证)层接入 — VerifierAgent。
    ///
    /// `None` 时 Review 阶段只检查 StepStatus(原逻辑);
    /// `Some` 时对每个 Succeeded 状态的 step 调用
    /// `verifier.verify(tool_result, acceptance_criteria, method)`,
    /// verify 失败则把 step 状态改为 Failed,再走 plan_reviewer.review。
    /// 详见 docs/harness-engineering-optimization-plan.md Step 3.1。
    verifier_agent: Option<crate::verifier::VerifierAgent>,
    /// BUG-9 修复:Harness O(可观测性)层接入 — TraceAnalyzer (Step 3.3)。
    ///
    /// `None` 时 run_turn 不记录 trace;
    /// `Some` 时在 turn 成功/失败出口构造 [`TraceRecord`] 并 `add_record`,
    /// 后续可通过 `trace_analyzer()` 拿到 handle 导出 CSV 或计算 stats。
    /// 用 `Arc<Mutex<TraceAnalyzer>>` 提供 interior mutability,
    /// 让 `&self` 的 `record_turn_*` 钩子能写入。详见
    /// docs/harness-engineering-optimization-plan.md Step 3.3。
    trace_analyzer: Option<Arc<Mutex<TraceAnalyzer>>>,
    /// BUG-9:当前 turn 的开始时间,run_turn 入口 set,record_turn_* 读取。
    /// 用 `Cell` 提供 interior mutability(Instant: Copy)。
    turn_start: Cell<Option<Instant>>,
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
            recovery_orchestrator: RecoveryOrchestrator::default(),
            plan_mode_enabled: false,
            active_plan: None,
            plan_reviewer: PreCompletionChecklistMiddleware::default(),
            workspace_root: None,
            loop_detector: LoopDetector::new(),
            context_assembler: None,
            pending_semantic_context: None,
            verifier_agent: None,
            trace_analyzer: None,
            turn_start: Cell::new(None),
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
        hook_progress_reporter: Box<dyn HookProgressReporter + Send>,
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

    /// Replace the default recovery orchestrator with a custom one. Useful
    /// for tests that need to inspect `RecoveryContext` after a failure, or
    /// for callers that want to pre-seed attempt counters.
    #[must_use]
    pub fn with_recovery_orchestrator(mut self, orchestrator: RecoveryOrchestrator) -> Self {
        self.recovery_orchestrator = orchestrator;
        self
    }

    /// Borrow the recovery orchestrator (for introspection / tests).
    #[must_use]
    pub fn recovery_orchestrator(&self) -> &RecoveryOrchestrator {
        &self.recovery_orchestrator
    }

    /// 启用 Plan/Execute/Review 三段循环(`--enable-plan-mode` 调用)。
    /// 详见 `docs/harness-engineering-optimization-plan.md` Step 2.1。
    /// 启用后,`run_turn` 会:
    /// 1. 入口调用 `assess_complexity(user_input)` 判断复杂任务。
    /// 2. Complex 时创建 `PlanArtifact` 并 persist 到
    ///    `<workspace>/.claw/plans/<id>.json`。
    /// 3. 把 PlanArtifact 末尾追加到 system_prompt 的 dynamic_sections
    ///    (缓存保护,不污染绝对稳定区)。
    /// 4. 主循环退出前调用 `PreCompletionChecklistMiddleware::review`,
    ///    AllPassed/Replan/Failed 决定后续动作。
    #[must_use]
    pub fn with_plan_mode_enabled(mut self, enabled: bool) -> Self {
        self.plan_mode_enabled = enabled;
        self
    }

    /// 注入工作区根目录,用于 `persist_plan_artifact` 写入
    /// `<workspace>/.claw/plans/<id>.json`。生产环境应注入 `cwd`。
    #[must_use]
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
        self
    }

    /// BUG-5 修复:注入 ContextAssembler,启用统一 prompt 注入路径。
    ///
    /// 注入后,每个 turn 构造 request 时会:
    /// 1. clone 一份 assembler(避免污染状态);
    /// 2. clear 所有 source;
    /// 3. 把 PlanArtifact render 后 add 到 Goal source;
    /// 4. 调用 assemble() 取 volatile_content() 作为 dynamic_sections。
    ///
    /// 不注入时走原 SystemPromptSplit + 手动 push 逻辑,保持向后兼容。
    /// 详见 docs/harness-engineering-optimization-plan.md Step 2.3。
    #[must_use]
    pub fn with_context_assembler(mut self, assembler: ContextAssembler) -> Self {
        self.context_assembler = Some(assembler);
        self
    }

    /// `&mut self` 版本的 `with_context_assembler`,用于已构造的 runtime。
    pub fn set_context_assembler(&mut self, assembler: ContextAssembler) {
        self.context_assembler = Some(assembler);
    }

    /// BUG-7 修复:注入 VerifierAgent,启用 acceptance_criteria 真实校验。
    ///
    /// 注入后,Review 阶段会对每个 Succeeded 状态的 step 调用
    /// `verifier.verify(tool_result, acceptance_criteria, method)`。
    /// verify 失败则把 step 状态改为 Failed,再走 plan_reviewer.review。
    /// 详见 docs/harness-engineering-optimization-plan.md Step 3.1。
    #[must_use]
    pub fn with_verifier_agent(mut self, agent: crate::verifier::VerifierAgent) -> Self {
        self.verifier_agent = Some(agent);
        self
    }

    /// `&mut self` 版本的 `with_verifier_agent`。
    pub fn set_verifier_agent(&mut self, agent: crate::verifier::VerifierAgent) {
        self.verifier_agent = Some(agent);
    }

    /// BUG-9 修复:注入 TraceAnalyzer,启用 telemetry 记录(Step 3.3)。
    ///
    /// 注入后,每个 turn 的成功/失败出口会构造一条 [`TraceRecord`] 并
    /// `add_record`。返回的 `Arc<Mutex<TraceAnalyzer>>` 让调用方可继续
    /// 读取(如导出 CSV、计算 stats)。详见
    /// docs/harness-engineering-optimization-plan.md Step 3.3。
    #[must_use]
    pub fn with_trace_analyzer(mut self, analyzer: TraceAnalyzer) -> Self {
        self.trace_analyzer = Some(Arc::new(Mutex::new(analyzer)));
        self
    }

    /// `&mut self` 版本的 `with_trace_analyzer`。
    pub fn set_trace_analyzer(&mut self, analyzer: TraceAnalyzer) {
        self.trace_analyzer = Some(Arc::new(Mutex::new(analyzer)));
    }

    /// 获取已注入的 TraceAnalyzer handle(克隆 `Arc`)。
    ///
    /// 调用方可通过 `handle.lock().stats()` 或 `handle.lock().export_csv(path)`
    /// 读取 trace 数据。`None` 表示未注入。
    #[must_use]
    pub fn trace_analyzer_handle(&self) -> Option<Arc<Mutex<TraceAnalyzer>>> {
        self.trace_analyzer.clone()
    }

    /// `&mut self` 版本的 `with_plan_mode_enabled`,用于已构造的 runtime
    /// (避免 move 出 `cli.runtime` 字段)。Step 2.1 接入时使用。
    pub fn set_plan_mode_enabled(&mut self, enabled: bool) {
        self.plan_mode_enabled = enabled;
    }

    /// `&mut self` 版本的 `with_workspace_root`,同上。
    pub fn set_workspace_root(&mut self, root: PathBuf) {
        self.workspace_root = Some(root);
    }

    /// 替换默认的 `PreCompletionChecklistMiddleware`(自定义 `max_replans`)。
    #[must_use]
    pub fn with_plan_reviewer(mut self, reviewer: PreCompletionChecklistMiddleware) -> Self {
        self.plan_reviewer = reviewer;
        self
    }

    /// Borrow 当前活跃的 PlanArtifact(供测试 / 诊断使用)。
    #[must_use]
    pub fn active_plan(&self) -> Option<&PlanArtifact> {
        self.active_plan.as_ref()
    }

    /// 是否启用了 Plan 模式。
    #[must_use]
    pub fn plan_mode_enabled(&self) -> bool {
        self.plan_mode_enabled
    }

    /// BUG-3 修复:统一的"先尝试恢复,失败再 record_turn_failed"流程。
    ///
    /// 文档要求所有 `record_turn_failed` 调用点都先经过 RecoveryOrchestrator
    /// (Step 1.2)。原实现只在 stream error 分支接入了 Provider 场景恢复,
    /// 其余 4 处失败分支(compaction 各阶段、build_assistant_message、
    /// max_iterations 超限)直接升级,跳过恢复机会。本方法封装统一恢复逻辑,
    /// 调用方只需:
    ///   if self.try_recover_or_record_fail(iterations, kind, &error) {
    ///       continue; // 恢复成功,重试当前操作
    ///   }
    ///   return Err(error); // 恢复失败,升级
    ///
    /// 每个 scenario 受 recipe max_attempts 硬上限保护(默认 1),不会无限重试。
    /// 返回 `true` 表示已恢复(调用方应 `continue` 重试);
    /// 返回 `false` 表示恢复失败,已调用 record_turn_failed,调用方应 `return Err`。
    fn try_recover_or_record_fail(
        &mut self,
        iterations: usize,
        failure_kind: WorkerFailureKind,
        error: &RuntimeError,
    ) -> bool {
        let outcome = self.recovery_orchestrator.attempt(failure_kind);
        if matches!(outcome.result, RecoveryResult::Recovered { .. }) {
            return true;
        }
        self.record_turn_failed(iterations, error);
        false
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
        // BUG-2 修复:在 PostToolUse hook 中接入 LoopDetector。
        // 仅对会修改文件的工具有意义(Edit/Write/MultiEdit/NotebookEdit),
        // 从 tool_input JSON 中提取 file_path 并记录到 loop_detector。
        // 根据 LoopAction 决定:
        // - Continue:正常流程,继续走原 hook_runner。
        // - InjectContext:把警告消息附加到 hook 结果的 messages 中,
        //   让主 agent 在下一轮看到"重新考虑方法"的提示。
        // - Abort:返回 cancelled=true 的 HookRunResult,阻断当前 turn。
        // 详见 docs/harness-engineering-optimization-plan.md Step 2.2。
        if let Some(file_path) = extract_file_path_from_tool_input(tool_name, input) {
            match self.loop_detector.record_edit(&file_path) {
                LoopAction::Abort(reason) => {
                    return HookRunResult::cancelled_with_message(reason);
                }
                LoopAction::InjectContext(msg) => {
                    let mut base_result = if let Some(reporter) = self.hook_progress_reporter.as_mut() {
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
                    };
                    base_result.append_message(msg);
                    return base_result;
                }
                LoopAction::Continue => {}
            }
        }
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

        // BUG-9:记录 turn 开始时间,供 record_turn_* 计算 latency_ms。
        self.turn_start.set(Some(Instant::now()));

        self.record_turn_started(&user_input);
        self.session
            .push_user_text(user_input.clone())
            .map_err(|error| RuntimeError::new(error.to_string()))?;

        // BUG-6 修复:Harness C(Memory)层接入 — 语义召回。
        // 当 persistent_memory 存在时,调用 semantic_recall 获取 top-3 相关记忆,
        // 渲染成文本块存到 pending_semantic_context,供 request 构造时注入。
        // 详见 docs/harness-engineering-optimization-plan.md Step 2.4。
        if let Some(memory) = &self.persistent_memory {
            let hits = memory.semantic_recall(&user_input, 3);
            if !hits.is_empty() {
                let mut rendered = String::from("# Relevant Memories\n\n");
                for (idx, hit) in hits.iter().enumerate() {
                    rendered.push_str(&format!(
                        "{}. [{}] {}\n   source: {}\n   score: {:.3}\n",
                        idx + 1,
                        hit.entry.id,
                        hit.entry.summary,
                        hit.entry.source,
                        hit.score,
                    ));
                }
                self.pending_semantic_context = Some(rendered);
            }
        }

        // Harness O(编排)层接入:Plan/Execute/Review 三段循环入口。
        // 详见 docs/harness-engineering-optimization-plan.md Step 2.1。
        //
        // 缓存保护(§5.2):PlanArtifact 通过末尾追加到 dynamic_sections 注入,
        // 不污染绝对稳定区(system_prompt + tools_schema)与半稳定区
        // (memory/goal/git_context)。预期命中率从 95% 降至 88-92%。
        //
        // 复杂任务检测:用户输入 > 200 字符或包含 "refactor"/"多文件" 等关键词。
        // Complex 时创建空 PlanArtifact(steps 由后续 Stage 3.1 VerifierAgent
        // 或主 agent 自身填充)。Simple 时跳过,不创建 artifact。
        if self.plan_mode_enabled && self.active_plan.is_none() {
            match assess_complexity(&user_input) {
                ComplexityAssessment::Complex { reason: _ } => {
                    let mut artifact = PlanArtifact::new(user_input.clone(), Vec::new());
                    // 尝试持久化(workspace_root 为 None 时跳过,不阻断主流程)。
                    if let Some(root) = &self.workspace_root {
                        if let Err(err) = persist_plan_artifact(&artifact, root) {
                            eprintln!(
                                "warning: failed to persist plan artifact: {err}"
                            );
                        }
                    }
                    artifact.transition_to_executing();
                    self.active_plan = Some(artifact);
                }
                ComplexityAssessment::Simple => {
                    // 简单任务,无需 plan。主 agent 走原生 ReAct 循环。
                }
            }
        }

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
                // Harness O(编排)层:PlanArtifact 末尾追加到 system_prompt。
                // 缓存保护(§5.2):把 PlanArtifact 渲染成文本块,
                // 末尾追加到 dynamic_sections,不破坏前面 4 层缓存。
                //
                // BUG-5 修复:当注入 ContextAssembler 时,通过 assembler
                // 收集 PlanArtifact render 到 Goal source,取 volatile_content()
                // 作为 dynamic_sections;否则走原手动 push 逻辑。
                //
                // BUG-6 修复:语义召回结果(pending_semantic_context)同样
                // 通过 assembler Memory source 或手动 push 注入。
                let mut system_split =
                    SystemPromptSplit::from_sections(self.system_prompt.clone());
                if let Some(assembler) = &self.context_assembler {
                    // 统一注入路径:把所有动态内容通过 assembler 收集。
                    let mut asm = assembler.clone();
                    asm.clear();
                    if let Some(memory_ctx) = &self.pending_semantic_context {
                        asm.add_auto(ContextSource::Memory, memory_ctx.clone());
                    }
                    if let Some(plan) = &self.active_plan {
                        let rendered = plan.render_for_prompt();
                        if !rendered.is_empty() {
                            asm.add_auto(ContextSource::Goal, rendered);
                        }
                    }
                    let volatile = asm.assemble().volatile_content();
                    if !volatile.is_empty() {
                        system_split.dynamic_sections.push(volatile);
                    }
                } else {
                    // 原生路径:手动 push 到 dynamic_sections。
                    if let Some(memory_ctx) = &self.pending_semantic_context {
                        system_split.dynamic_sections.push(memory_ctx.clone());
                    }
                    if let Some(plan) = &self.active_plan {
                        let rendered = plan.render_for_prompt();
                        if !rendered.is_empty() {
                            system_split.dynamic_sections.push(rendered);
                        }
                    }
                }
                ApiRequest {
                    system_prompt: system_split,
                    messages: sliced.to_vec(),
                }
            };
            let events = match self.api_client.stream(request) {
                Ok(events) => events,
                Err(error) => {
                    // Non-recoverable errors propagate immediately.
                    if !error.is_prompt_too_long() {
                        // Harness L(生命周期)层接入:对非 prompt_too_long 的 API
                        // 错误尝试一次自动恢复(默认 ProviderFailure 场景)。
                        // 恢复成功 → continue 重新发请求;失败 → record + 升级。
                        // 详见 docs/harness-engineering-optimization-plan.md Step 1.2。
                        // BUG-3 修复:用统一辅助方法,确保所有失败分支都经过 orchestrator。
                        if self.try_recover_or_record_fail(
                            iterations,
                            WorkerFailureKind::Provider,
                            &error,
                        ) {
                            continue;
                        }
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
                            // BUG-3 修复:compaction 末端失败也尝试一次 Provider 恢复
                            // (上游可能临时不可用,恢复后重试可能成功)。
                            if self.try_recover_or_record_fail(
                                iterations,
                                WorkerFailureKind::Provider,
                                &error,
                            ) {
                                // 重置 reactive_state,重新走 compaction 流程。
                                reactive_state = ReactiveCompactState::NotAttempted;
                                continue;
                            }
                            return Err(error);
                        }
                        ReactiveCompactState::FullCompactDone => {
                            // Already exhausted recovery steps; bail out to
                            // prevent an infinite retry loop.
                            // BUG-3 修复:同样尝试一次 Provider 恢复,失败再升级。
                            if self.try_recover_or_record_fail(
                                iterations,
                                WorkerFailureKind::Provider,
                                &error,
                            ) {
                                reactive_state = ReactiveCompactState::NotAttempted;
                                continue;
                            }
                            return Err(error);
                        }
                    }
                }
            };
            let (assistant_message, usage, turn_prompt_cache_events) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error) => {
                        // BUG-3 修复:SSE events 解析失败也尝试一次 Protocol 恢复,
                        // 恢复成功后 continue 重新发请求(原 events 已消耗,无法重用)。
                        if self.try_recover_or_record_fail(
                            iterations,
                            WorkerFailureKind::Protocol,
                            &error,
                        ) {
                            continue;
                        }
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

        // Harness O(编排)层 + V(验证)层接入:Plan/Execute/Review 中的 Review 阶段。
        // 主循环退出后,若 active_plan 存在且 steps 非空,调用
        // PreCompletionChecklistMiddleware 决定后续:
        // - AllPassed:清空 active_plan,正常返回 TurnSummary。
        // - ReplanTriggered:保留 active_plan(已 reset Failed → Pending),
        //   清空会在下次 turn 的入口评估后处理。当前 turn 仍正常返回。
        // - Failed:返回 RuntimeError,上层(RecoveryOrchestrator)决定升级。
        //
        // 注:当 active_plan.steps 为空时(plan 创建但主 agent 未填充 steps),
        // 跳过 Review,直接清空 active_plan — 避免空 plan 阻塞后续 turn。
        //
        // BUG-7 修复:在 plan_reviewer.review 之前,若注入了 verifier_agent,
        // 对每个 Succeeded 状态的 step 调用 verify(tool_result, acceptance_criteria, method),
        // verify 失败则把 step 状态改为 Failed,再走 plan_reviewer.review。
        // 详见 docs/harness-engineering-optimization-plan.md Step 3.1。
        if let Some(mut plan) = self.active_plan.take() {
            if !plan.steps.is_empty() {
                // BUG-7 修复:VerifierAgent 真实校验 acceptance_criteria。
                if let Some(verifier) = &self.verifier_agent {
                    // 用本轮 tool_results 拼接作为 tool_result 上下文。
                    // PlanStep 当前不存储关联的 tool_result,这是简化处理。
                    // 提取每个 message 的 ToolResult output 文本作为上下文。
                    let tool_result_ctx: String = tool_results
                        .iter()
                        .flat_map(|m| {
                            m.blocks.iter().filter_map(|b| match b {
                                crate::session::ContentBlock::ToolResult { output, .. } => {
                                    Some(output.as_str())
                                }
                                _ => None,
                            })
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    for step in &mut plan.steps {
                        if step.status == crate::planner::StepStatus::Succeeded {
                            let method = match step.verification_method {
                                crate::planner::VerificationMethod::Rule => {
                                    crate::verifier::VerificationMethod::Rule
                                }
                                crate::planner::VerificationMethod::Visual => {
                                    crate::verifier::VerificationMethod::Visual
                                }
                                crate::planner::VerificationMethod::ModelJudge => {
                                    crate::verifier::VerificationMethod::ModelJudge
                                }
                            };
                            let result = verifier.verify(
                                &tool_result_ctx,
                                &step.acceptance_criteria,
                                method,
                            );
                            if !result.passed {
                                step.mark_failed();
                            }
                        }
                    }
                }
                match self.plan_reviewer.review(&mut plan) {
                    ReviewResult::AllPassed => {
                        // Plan 完成。可选 persist 最终状态。
                        if let Some(root) = &self.workspace_root {
                            let _ = persist_plan_artifact(&plan, root);
                        }
                    }
                    ReviewResult::ReplanTriggered { .. } => {
                        // 保留 plan,下次 turn 重新执行 reset 后的 steps。
                        self.active_plan = Some(plan);
                    }
                    ReviewResult::Failed {
                        failed_step_ids,
                        replan_count,
                    } => {
                        let error = RuntimeError::new(format!(
                            "plan failed after {replan_count} replans; failed steps: {}",
                            failed_step_ids.join(", ")
                        ));
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                }
            }
            // else: 空 plan(steps 为空)直接丢弃,不阻塞。
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

        // BUG-6 修复:turn 结束时清空 pending_semantic_context,
        // 下一 turn 重新召回,避免陈旧记忆污染。
        self.pending_semantic_context = None;

        // Periodic nudge: if enough turns have elapsed and we have a
        // persistent memory surface, scan recent messages for actionable
        // patterns (user corrections, "remember" keywords, etc.) and apply
        // them to the memory. This keeps the memory layer fresh without an
        // LLM call. The frozen snapshot is not touched, so the prompt-cache
        // prefix stays stable within the session — new facts only surface in
        // the next session.
        //
        // Tier S #3 穷鬼模式：激活时整体跳过 nudge（虽然 nudge 当前是规则驱动
        // 不消耗 LLM token，但仍会写入 memory.json 增加后续 prompt 体积；
        // 穷鬼模式下用户明确希望最小化副作用）。
        if !crate::poor_mode::is_active() {
            self.turns_since_last_nudge += 1;
            let nudge_config = NudgeConfig::default();
            if let Some(memory) = &mut self.persistent_memory {
                if should_nudge(self.turns_since_last_nudge, &nudge_config) {
                    // B3 fix: previously used `take(lookback_turns * 2)`
                    // assuming 1 turn = 2 messages. With tool calls one turn
                    // can produce 5-10 messages (user → assistant tool_use →
                    // tool_result → ... → assistant text), so `* 2` only
                    // covered the most recent turn and missed the previous
                    // user input entirely. Iterate from the newest message
                    // backwards, counting only `MessageRole::User` messages
                    // until we've collected `lookback_turns` of them.
                    let lookback_msgs: Vec<_> = {
                        let mut picked: Vec<&ConversationMessage> = Vec::new();
                        let mut user_seen = 0usize;
                        for msg in self.session.messages.iter().rev() {
                            picked.push(msg);
                            if msg.role == MessageRole::User {
                                user_seen += 1;
                                if user_seen >= nudge_config.lookback_turns {
                                    break;
                                }
                            }
                        }
                        picked.into_iter().rev().cloned().collect()
                    };
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
                            NudgeAction::Remove { pattern, source: _ } => {
                                // B8 fix: retire the matching active entry
                                // into `archive` (audit history) rather
                                // than leaving the variant as dead code.
                                // No-op if nothing matches.
                                memory.remove_entry(&pattern);
                            }
                        }
                    }
                    // After applying actions, run consolidation if the
                    // surface has crossed a capacity threshold. This
                    // migrates superseded / expired entries into the
                    // archive sub-table (preserving audit history) and
                    // compresses any over-budget block. Without this hook,
                    // `needs_consolidation` / `consolidate` were dead code
                    // and superseded entries accumulated indefinitely in
                    // `entries` (bloating the on-disk file).
                    if memory.needs_consolidation() {
                        memory.consolidate();
                    }
                    self.turns_since_last_nudge = 0;
                }
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

    /// 返回工具执行器的可变引用，用于运行时调整 tool executor 的配置
    /// （例如 `output_verbosity`）。仅在需要动态修改执行器状态时使用。
    pub fn tool_executor_mut(&mut self) -> &mut T {
        &mut self.tool_executor
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
        // BUG-9:TraceAnalyzer 记录 — 独立于 session_tracer,无条件执行。
        self.record_trace(
            summary.iterations,
            summary.tool_results.len() as u32,
            summary.auto_compaction.is_some(),
            None,
        );

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
        // BUG-9:TraceAnalyzer 记录 — 失败 turn。
        // tool_calls/compact 在失败路径无法准确获取,记 0/false。
        let error_msg = error.to_string();
        self.record_trace(
            iteration,
            0,
            false,
            Some(("runtime_error", error_msg.as_str())),
        );

        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("turn_failed", attributes);
    }

    /// BUG-9:构造一条 [`TraceRecord`] 并写入 `trace_analyzer`(若注入)。
    ///
    /// `failure` 为 `Some((kind, msg))` 时记录失败 turn;`None` 记录成功 turn。
    /// 写入后清空 `turn_start`,防止下一 turn 未设置时读到旧值。
    fn record_trace(
        &self,
        iterations: usize,
        tool_calls: u32,
        compact_triggered: bool,
        failure: Option<(&str, &str)>,
    ) {
        let Some(handle) = &self.trace_analyzer else {
            return;
        };
        let latency_ms = self
            .turn_start
            .get()
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let turn_id = format!("{}-{}", self.session.session_id, iterations);
        let mut record =
            TraceRecord::new(turn_id, latency_ms, tool_calls).with_compact_triggered(compact_triggered);
        if let Some((kind, msg)) = failure {
            record = record.with_failure(kind, msg);
        }
        if let Ok(mut analyzer) = handle.lock() {
            analyzer.add_record(record);
        }
        // 清空 turn_start,防止下一 turn 未设置时读到旧值。
        self.turn_start.set(None);
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

/// BUG-2 修复:从 tool_input JSON 中提取文件路径,供 LoopDetector 跟踪。
///
/// 仅对会修改文件的工具有意义:
/// - Edit / Write / NotebookEdit → `file_path` 字段
/// - MultiEdit → `file_path` 字段(单文件多编辑)
/// - 其他工具(Read/Grep/Bash/LS 等)→ 返回 None(不计数)
///
/// tool_input 期望是 JSON 字符串(如 `{"file_path": "/abs/path", ...}`)。
/// 解析失败或字段缺失时返回 None,不阻断主流程。
fn extract_file_path_from_tool_input(tool_name: &str, tool_input: &str) -> Option<String> {
    // 只关心会修改文件的工具,避免 Read/Grep 等只读工具误计数。
    let modifying_tools = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
    if !modifying_tools.contains(&tool_name) {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(tool_input).ok()?;
    // 优先 file_path,次选 path(部分工具历史字段名)。
    if let Some(path) = parsed.get("file_path").and_then(|v| v.as_str()) {
        return Some(path.to_owned());
    }
    if let Some(path) = parsed.get("path").and_then(|v| v.as_str()) {
        return Some(path.to_owned());
    }
    None
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
    fn trace_analyzer_records_failed_turn() {
        use crate::trace_analyzer::TraceAnalyzer;

        struct FailingApi;

        impl ApiClient for FailingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_trace_analyzer(TraceAnalyzer::new());

        let handle = runtime
            .trace_analyzer_handle()
            .expect("trace analyzer should be injected");

        // run_turn 失败后,record_turn_failed 应写入一条 trace 记录。
        let _ = runtime.run_turn("hello", None).expect_err("should fail");

        let records = handle.lock().unwrap().records.clone();
        assert_eq!(records.len(), 1, "exactly one trace record expected");
        let record = &records[0];
        assert!(
            record.turn_id.starts_with(&runtime.session.session_id),
            "turn_id should be prefixed with session_id"
        );
        assert!(record.failure_kind.is_some());
        assert_eq!(record.failure_kind.as_deref(), Some("runtime_error"));
        assert!(record
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("upstream failed"));
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
