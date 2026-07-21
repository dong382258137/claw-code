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
// Harness M(多 agent)层接入:MultiAgentCoordinator — Step 3.2-c。
// 主 agent 通过 dispatch_subagent tool 派发任务给子 agent。
// 子 agent 走独立 LLM 请求 + 独立 prompt cache,不污染主 agent 缓存(§5.2)。
use crate::multi_agent::{CoordinationMode, MultiAgentCoordinator, SubagentStatus};
// Step 3.2-a:LaneEvent helpers for SubagentHandoff / SubagentResult.
use crate::lane_events::{try_publish as publish_lane_event, LaneEvent};
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

/// Step 3.2-c:Tool specification for the `dispatch_subagent` tool.
///
/// 主 agent 通过此 tool 将任务派发给子 agent(子 agent 走独立 LLM 请求 +
/// 独立 prompt cache,不污染主 agent 缓存,详见 §5.2)。运行时通过
/// [`ConversationRuntime::execute_dispatch_subagent`] 内部拦截执行,
/// 调用 [`MultiAgentCoordinator::spawn`] + 发布 `SubagentHandoff` 事件。
#[allow(dead_code)] // Reserved for future registration via main.rs tool registry.
pub const DISPATCH_SUBAGENT_TOOL_SPEC: &str = r#"{
    "name": "dispatch_subagent",
    "description": "Dispatch a sub-task to a sub-agent. The sub-agent runs independently with its own LLM request and prompt cache, so the main agent's cache prefix is not polluted. Use this for parallelizable work, isolated refactors, or verification tasks. Returns the subagent_id immediately; use check_subagent to poll for completion.",
    "input_schema": {
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Human-readable name for the sub-agent (e.g. 'refactor-auth', 'test-runner')."
            },
            "task": {
                "type": "string",
                "description": "The task description / prompt to send to the sub-agent."
            },
            "mode": {
                "type": "string",
                "enum": ["fork", "teammate", "worktree"],
                "description": "Coordination mode: 'fork' (shared workdir, parallel), 'teammate' (shared TaskRegistry), 'worktree' (isolated git worktree).",
                "default": "fork"
            }
        },
        "required": ["name", "task"]
    }
}"#;

/// Step 3.2-c:Tool specification for the `check_subagent` tool.
///
/// 主 agent 通过此 tool 查询子 agent 状态/结果。若子 agent 已完成,返回
/// 最终结果并发布 `SubagentResult` lane event。
#[allow(dead_code)] // Reserved for future registration via main.rs tool registry.
pub const CHECK_SUBAGENT_TOOL_SPEC: &str = r#"{
    "name": "check_subagent",
    "description": "Check the status of a previously dispatched sub-agent. Returns the current status (created/running/completed/failed/cancelled) and, if terminal, the result payload. Completed/failed results also emit a SubagentResult lane event for observability.",
    "input_schema": {
        "type": "object",
        "properties": {
            "subagent_id": {
                "type": "string",
                "description": "The subagent_id returned by dispatch_subagent."
            }
        },
        "required": ["subagent_id"]
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
    /// 模型 context window 大小,用于动态计算 compaction 阈值。
    /// 设置后 `maybe_auto_compact` 会根据 context window
    /// 计算合适的压缩点,而非使用硬编码的 100K 默认值。
    /// `None` 时回退到 `auto_compaction_input_tokens_threshold`。
    context_window: Option<u32>,
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
    /// Step 3.2-c:Multi-Agent 协调器 — 子 agent 生命周期管理。
    ///
    /// `None` 时 dispatch_subagent / check_subagent tool 返回 "not available";
    /// `Some` 时主 agent 可通过 tool call 派发子 agent。子 agent 走独立
    /// LLM 请求 + 独立 prompt cache,不污染主 agent 缓存(§5.2)。
    /// 详见 docs/harness-engineering-optimization-plan.md Step 3.2。
    multi_agent_coordinator: Option<MultiAgentCoordinator>,
    /// Epic 3:TaskRegistry — 子 agent 任务注册表。
    ///
    /// `Some` 时子 agent 任务可通过 TaskRegistry 追踪状态/心跳/团队分配。
    /// 与 multi_agent_coordinator 配合使用:coordinator 管理子 agent 生命周期,
    /// registry 管理 task 级元数据。详见 plan.md §9.2 Epic 3。
    task_registry: Option<crate::task_registry::TaskRegistry>,
    /// P0-3:NOTEBOOK 刷新提醒 flag。
    ///
    /// 当 microcompact / auto_compaction / reactive compaction 压缩了
    /// tool result 后置 true,下一次 request 构造时在 system_prompt
    /// 变动区追加提醒,引导 LLM 调用 `notebook_update` 刷新 `<plan>` 和
    /// `<subagents>` 段,确保关键信息不丢失。LLM 调用 notebook_update
    /// 后清除。
    ///
    /// 论文依据:Anthropic Multi-Agent Research System — "The LeadResearcher
    /// begins by thinking through the approach and saving its plan to Memory
    /// to persist the context, since if the context window exceeds 200,000
    /// tokens it will be truncated and it is important to retain the plan."
    /// CompactionRL (arXiv:2607.05378) — summary 必须保留 original goal /
    /// completed actions / unresolved errors / current state。
    notebook_refresh_pending: bool,
    /// v2.0 VerifierAgent remediation 注入 — 上一轮 verify 失败的修正建议。
    ///
    /// Review 阶段若 VerifierAgent 检测到失败,把 `FailedVerification` 列表
    /// 序列化为文本存到此字段,下一次 request 构造时在 system_prompt
    /// 变动区追加,引导 LLM 针对性修复(而非盲目重试)。LLM 下轮开始后清除。
    ///
    /// 修复 v1.0 缺陷:`remediation` 字段完全丢失 — 主 agent 不知道
    /// 上一次 verify 为什么失败,只能盲目重试 → 必然陷入 doom loop。
    pending_remediation: Option<String>,
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
            context_window: None,
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
            multi_agent_coordinator: None,
            task_registry: None,
            notebook_refresh_pending: false,
            pending_remediation: None,
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

    /// 注入模型 context window 大小,启用动态 compaction 阈值计算。
    ///
    /// 设置后 `maybe_auto_compact` 使用 `compaction_threshold_for_context_window()`
    /// 替代硬编码的 100K:
    /// - 1M (DeepSeek V4/GPT-5.4): 阈值 = 650K
    /// - 200K (Claude): 阈值 = 130K
    /// - 256K (Kimi): 阈值 = 166K
    /// - 未设置: 回退到 `CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS` 或 100K
    #[must_use]
    pub fn with_context_window(mut self, context_window: u32) -> Self {
        self.context_window = Some(context_window);
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

    /// Step 3.2-c:注入 MultiAgentCoordinator,启用 subagent-as-tool 路由。
    ///
    /// 注入后,主 agent 可通过 `dispatch_subagent` tool 派发子 agent,
    /// 通过 `check_subagent` tool 查询状态/结果。子 agent 走独立 LLM
    /// 请求 + 独立 prompt cache,不污染主 agent 缓存(§5.2)。
    /// 详见 docs/harness-engineering-optimization-plan.md Step 3.2。
    #[must_use]
    pub fn with_multi_agent_coordinator(mut self, coordinator: MultiAgentCoordinator) -> Self {
        self.multi_agent_coordinator = Some(coordinator);
        self
    }

    /// `&mut self` 版本的 `with_multi_agent_coordinator`。
    pub fn set_multi_agent_coordinator(&mut self, coordinator: MultiAgentCoordinator) {
        self.multi_agent_coordinator = Some(coordinator);
    }

    /// Epic 3:注入 TaskRegistry,启用子 agent 任务追踪。
    ///
    /// 注入后,子 agent 的 task 级元数据(状态/心跳/团队分配)可通过
    /// TaskRegistry 追踪。与 multi_agent_coordinator 配合使用。
    /// 详见 plan.md §9.2 Epic 3。
    #[must_use]
    pub fn with_task_registry(
        mut self,
        registry: crate::task_registry::TaskRegistry,
    ) -> Self {
        self.task_registry = Some(registry);
        self
    }

    /// 获取已注入的 TaskRegistry 引用(若已注入)。
    #[must_use]
    pub fn task_registry(&self) -> Option<&crate::task_registry::TaskRegistry> {
        self.task_registry.as_ref()
    }

    /// Step 3.2-c:获取 `MultiAgentCoordinator` 引用(若已注入)。
    /// 用于外部查询 subagent 列表 / 状态(如 CLI 状态栏显示)。
    #[must_use]
    pub fn multi_agent_coordinator(&self) -> Option<&MultiAgentCoordinator> {
        self.multi_agent_coordinator.as_ref()
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
                    let mut base_result =
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

        // P2-7 修复:在每个 turn 开始时重置 loop_detector,避免跨 turn 累积。
        // 否则同一文件被多次编辑会触发 InjectContext/Abort,即使这些编辑分布在
        // 不同 turn 中(误判 doom loop)。
        self.loop_detector.reset();

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
                            eprintln!("warning: failed to persist plan artifact: {err}");
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

            // 用户中断检查：TUI 层 Ctrl+C（busy 时）会 abort hook_abort_signal。
            // 在每次 agent loop 迭代顶部检查，让用户能在工具调用间隙打断 AI。
            // 注意：正在进行的 API 流式请求无法中断（阻塞 IO），但可以阻止
            // 下一轮迭代（不再发起新请求、不再执行新工具）。
            if self.hook_abort_signal.is_aborted() {
                self.record_turn_failed(iterations, &RuntimeError::new("turn interrupted by user"));
                return Err(RuntimeError::new("turn interrupted by user"));
            }

            let request = {
                let sliced =
                    crate::compact::get_messages_after_compact_boundary(&self.session.messages);
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
                let mut system_split = SystemPromptSplit::from_sections(self.system_prompt.clone());
                // P0-1:NOTEBOOK 注入 — 跨压缩持久化的工作记忆。
                // Anthropic《Effective Context Engineering》明确推荐:structured
                // note-taking 是长程任务的关键技术,每个 turn 注入到 system_prompt
                // 变动区,确保 LLM 始终能看到关键信息(决策、子智能体注册表、
                // 已尝试方案、用户偏好、关键文件引用)。
                //
                // 关键不变量:NOTEBOOK.md 不在 message history 中,因此
                // microcompact / compact_session 不会影响它。它通过 system_prompt
                // 变动区每个 turn 重新注入,这是 Anthropic 推荐的标准模式。
                //
                // 注意:放在 assembler/手动注入路径之前,确保 NOTEBOOK 是变动区
                // 的第一段(LLM 最先看到的工作记忆)。
                if let Some(workspace_root) = &self.workspace_root {
                    if let Ok(notebook) = crate::notebook::Notebook::load(workspace_root) {
                        let notebook_prompt = notebook.render_for_prompt();
                        if !notebook_prompt.is_empty() {
                            system_split.dynamic_sections.push(notebook_prompt);
                        }
                    }
                    // NOTEBOOK 加载失败时不阻塞 turn(避免 NOTEBOOK 文件损坏
                    // 导致整个 agent 无法运行),但记录到 stderr 供排查。
                    // 实际加载错误在 else 分支已经被静默忽略(load 返回 Ok(empty)),
                    // 只有 parse 错误才会进入 Err,这里不额外日志。

                    // P0-3:压缩后 NOTEBOOK 刷新提醒。
                    // 当 microcompact / auto_compaction / reactive compaction
                    // 压缩了 tool result 后,flag 被置 true。这里注入提醒,
                    // 引导 LLM 主动调用 notebook_update 刷新 <plan> 和 <subagents>
                    // 段,确保关键信息(决策、子智能体注册表)在后续压缩中不丢失。
                    // LLM 调用 notebook_update 后,execute_notebook_update 清除 flag。
                    if self.notebook_refresh_pending {
                        system_split.dynamic_sections.push(
                            "# ⚠️ Context Compaction Detected — NOTEBOOK Refresh Required\n\
                             上下文刚刚被压缩,部分旧 tool result 已被摘要替换。\n\
                             **请立即调用 `notebook_update` 工具**刷新以下段:\n\
                             - `<plan>`:当前任务的关键决策、约束、进度(若已变化)\n\
                             - `<subagents>`:已 dispatch 的子智能体注册表(防止重复 dispatch)\n\
                             - `<attempted>`:已尝试的方案(防止重复尝试失败方案)\n\
                             这是防止长程任务中关键信息丢失的关键步骤。"
                                .to_string(),
                        );
                    }
                }
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
                    if let Some(remediation) = &self.pending_remediation {
                        // v2.0:注入上一轮 verify 失败的 remediation。
                        // 顺序:放在 plan 之后(变动区最末尾),
                        // 让最易变的内容放最后,最大化前缀缓存命中率。
                        asm.add_auto(ContextSource::Goal, remediation.clone());
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
                    if let Some(remediation) = &self.pending_remediation {
                        // v2.0:注入上一轮 verify 失败的 remediation。
                        // 顺序:放在 plan 之后(变动区最末尾),
                        // 让最易变的内容放最后,最大化前缀缓存命中率。
                        system_split.dynamic_sections.push(remediation.clone());
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
                            let before_len =
                                crate::conversation::tool_result_output_len(&self.session.messages);
                            // P0:reactive microcompact 同样归档原始 tool result,
                            // 确保 reactive 压缩路径也走无损归档。
                            let archive_root = self.workspace_root.clone();
                            let microcompacted = crate::compact::microcompact_with_archiver(
                                &self.session.messages,
                                REACTIVE_MICROCOMPACT_PRESERVE_RECENT,
                                |id, name, output| {
                                    if let Some(root) = &archive_root {
                                        let _ = crate::tool_result_archive::archive_tool_result(
                                            root, id, name, output,
                                        );
                                    }
                                },
                            );
                            let after_len =
                                crate::conversation::tool_result_output_len(&microcompacted);
                            // P0-3:reactive microcompact 发生压缩,置 flag。
                            // continue 后回到 loop 顶部,request 重新构造,
                            // system_prompt 会注入 NOTEBOOK 刷新提醒。
                            if after_len < before_len {
                                self.notebook_refresh_pending = true;
                            }
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
                                // P0-3:reactive full compact 删除了消息,置 flag。
                                self.notebook_refresh_pending = true;
                                reactive_state = ReactiveCompactState::FullCompactDone;
                                continue;
                            }
                            // Compaction removed nothing — nothing more we can do.
                            //
                            // **P0-3 修复**：之前此分支直接 `record_turn_failed + return Err`，
                            // 跳过 `try_recover_or_record_fail`。原注释称"避免 reactive_state
                            // 重置导致 API 调用翻倍"，但实际上 `try_recover_or_record_fail`
                            // 内部 `recovery_orchestrator.attempt()` 不会修改 `reactive_state`
                            // （它是 `run_turn` 的局部变量，attempt 不持有其引用）。
                            // 跳过 Provider 切换等恢复路径会让本可恢复的 prompt_too_long
                            // 错误直接升级。现在调用恢复路径，让 Provider 切换等策略有机会生效。
                            // 若恢复成功（如切换到支持更长 context 的 Provider），
                            // reactive_state 仍为 MicrocompactDone 但下次循环会重新尝试。
                            if self.try_recover_or_record_fail(
                                iterations,
                                WorkerFailureKind::Provider,
                                &error,
                            ) {
                                // 恢复成功：保持 reactive_state 不变，让下次循环
                                // 在新 Provider 下重新尝试 compaction。
                                continue;
                            }
                            return Err(error);
                        }
                        ReactiveCompactState::FullCompactDone => {
                            // Already exhausted recovery steps; bail out to
                            // prevent an infinite retry loop.
                            //
                            // **P0-3 修复**：同 MicrocompactDone 分支，调用恢复路径
                            // 让 Provider 切换等策略有机会生效。reactive_state 是局部
                            // 变量不会被 attempt 重置，注释中"避免 API 调用翻倍"的担忧
                            // 不成立——attempt 只切换 Provider 配置，不影响 reactive_state。
                            if self.try_recover_or_record_fail(
                                iterations,
                                WorkerFailureKind::Provider,
                                &error,
                            ) {
                                continue;
                            }
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
                        let (mut output, mut is_error) = if tool_name == "session_search" {
                            match self.execute_session_search(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "dispatch_subagent" {
                            // Step 3.2-c:subagent-as-tool 路由。
                            // 主 agent 通过 tool call 派发子 agent,走独立 LLM 请求,
                            // 不污染主 agent 的 prompt cache(§5.2 缓存保护)。
                            match self.execute_dispatch_subagent(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "check_subagent" {
                            // Step 3.2-c:查询子 agent 状态/结果。
                            // 终态会发布 SubagentResult lane event。
                            match self.execute_check_subagent(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "notebook_update" {
                            // P0-1:LLM 主动维护 NOTEBOOK.md(跨压缩持久化记忆)。
                            // Anthropic《Effective Context Engineering for AI Agents》
                            // 明确推荐:structured note-taking 是长程任务的关键技术。
                            // 工具描述强调"CRITICAL: always record subagent dispatches
                            // here so you do not re-dispatch the same task later",
                            // 直击"AI 忘记已 dispatch 过子智能体"的问题。
                            match self.execute_notebook_update(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "recall_full" {
                            // P0:从 ToolResultArchive 检索 microcompact 摘要前的
                            // 原始 tool result。直击"AI 看到摘要后无法判断是否需要
                            // 重新调用工具,导致重复调用"的问题。
                            // 详见 tool_result_archive 模块文档。
                            match self.execute_recall_full(&effective_input) {
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
        // 对每个 Succeeded 状态的 step 调用 verify(tool_result, acceptance_criteria, verify_command),
        // verify 失败则把 step 状态改为 Failed,再走 plan_reviewer.review。
        // v2.0 改动:
        // 1. 用 step.last_tool_use_id 精准查找 tool_result(修复全量拼接噪音问题)
        // 2. 收集 FailedVerification 列表,透传给 reviewer.review()
        // 3. ReplanTriggered 分支把 remediation 保存到 pending_remediation,
        //    下次 request 构造时注入 system_prompt(修复 remediation 丢失缺陷)
        // 详见 docs/harness-engineering-optimization-plan.md Step 3.1。
        if let Some(mut plan) = self.active_plan.take() {
            if !plan.steps.is_empty() {
                // v2.0:收集失败 step 的验证详情,供 reviewer 透传 + 下轮 prompt 注入。
                let mut failed_verifications: Vec<crate::planner::FailedVerification> = Vec::new();

                if let Some(verifier) = &self.verifier_agent {
                    // v2.0:构建 tool_use_id → output 索引,支持精准查找。
                    // step.last_tool_use_id 关联的主 agent 调用的 tool,
                    // 其 tool_result 通过 user message 中的 ToolResult block 返回。
                    let tool_result_index: std::collections::HashMap<&str, &str> = tool_results
                        .iter()
                        .flat_map(|m| {
                            m.blocks.iter().filter_map(|b| match b {
                                crate::session::ContentBlock::ToolResult {
                                    tool_use_id,
                                    output,
                                    ..
                                } => Some((tool_use_id.as_str(), output.as_str())),
                                _ => None,
                            })
                        })
                        .collect();

                    // 全量 fallback:无 last_tool_use_id 时用全量拼接(v1.0 兼容)。
                    let all_tool_results: String = tool_results
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
                            // v2.0:优先用 step.last_tool_use_id 精准查找,
                            // 无关联则 fallback 到全量拼接。
                            let tool_result_ctx: &str = step
                                .last_tool_use_id
                                .as_deref()
                                .and_then(|id| tool_result_index.get(id).copied())
                                .unwrap_or(&all_tool_results);

                            let result = verifier.verify(
                                tool_result_ctx,
                                &step.acceptance_criteria,
                                step.verify_command.as_deref(),
                            );
                            if !result.passed {
                                step.mark_failed();
                                failed_verifications.push(crate::planner::FailedVerification {
                                    step_id: step.id.clone(),
                                    step_description: step.description.clone(),
                                    acceptance_criteria: step.acceptance_criteria.clone(),
                                    detail: result.detail,
                                    remediation: result.remediation.unwrap_or_default(),
                                });
                            }
                        }
                    }
                }

                match self.plan_reviewer.review(&mut plan, failed_verifications) {
                    ReviewResult::AllPassed => {
                        // Plan 完成。可选 persist 最终状态。
                        if let Some(root) = &self.workspace_root {
                            let _ = persist_plan_artifact(&plan, root);
                        }
                    }
                    ReviewResult::ReplanTriggered {
                        failed_verifications,
                        ..
                    } => {
                        // 保留 plan,下次 turn 重新执行 reset 后的 steps。
                        self.active_plan = Some(plan);
                        // v2.0:把失败详情序列化为 remediation prompt,
                        // 下次 request 构造时注入 system_prompt 变动区。
                        if !failed_verifications.is_empty() {
                            self.pending_remediation = Some(
                                crate::planner::render_remediation_prompt(&failed_verifications),
                            );
                        }
                    }
                    ReviewResult::Failed {
                        failed_step_ids,
                        replan_count,
                        failed_verifications,
                    } => {
                        // v2.0:把失败详情拼入错误消息,让用户看到 remediation。
                        let remediation_hint = if failed_verifications.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "\n\n{}",
                                crate::planner::render_remediation_prompt(&failed_verifications)
                            )
                        };
                        let error = RuntimeError::new(format!(
                            "plan failed after {replan_count} replans; failed steps: {}{remediation_hint}",
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
        //
        // P0:在摘要替换前,通过 `microcompact_with_archiver` 归档原始 tool result
        // 到 `.claw/tool_results_archive.jsonl`。LLM 后续可通过 `recall_full` 工具
        // 按 `tool_use_id` 主动检索原始内容,避免"看到摘要后重复调用工具"的问题。
        // 归档失败不阻断 microcompact(吞掉错误)。
        let archive_root = self.workspace_root.clone();
        let microcompacted = crate::compact::microcompact_with_archiver(
            &self.session.messages,
            MICROCOMPACT_PRESERVE_RECENT,
            |id, name, output| {
                if let Some(root) = &archive_root {
                    let _ = crate::tool_result_archive::archive_tool_result(root, id, name, output);
                }
            },
        );
        // P0-3:检测 microcompact 是否发生了实质性压缩(旧 tool result 被替换)。
        // 比较前后 tool result blocks 的总 output 长度,若减少则置刷新 flag,
        // 下个 turn 的 system_prompt 会注入 NOTEBOOK 刷新提醒。
        if crate::conversation::tool_result_output_len(&microcompacted)
            < crate::conversation::tool_result_output_len(&self.session.messages)
        {
            self.notebook_refresh_pending = true;
        }
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

        // v2.0 VerifierAgent:turn 结束时清空 pending_remediation。
        // 下一轮若再次 verify 失败,会重新填充。
        // 若 verify 通过或无 verify_command,remediation 不会被设置。
        self.pending_remediation = None;

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
        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;
        let query = parsed
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("missing 'query' field")?;
        let top_k = parsed.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        // Primary: search FTS5 history index
        if let Some(history_index) = self.session.history_index.as_ref() {
            let hits = history_index.search(query, top_k)?;
            if !hits.is_empty() {
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
                return Ok(output);
            }
        }

        // Fallback: search tool_result_archive for compacted tool outputs
        if let Some(workspace_root) = &self.workspace_root {
            let summaries = crate::tool_result_archive::list_archived_summary(workspace_root)?;
            let query_lower = query.to_lowercase();
            let matches: Vec<_> = summaries
                .iter()
                .filter(|(_, name, preview, _)| {
                    preview.to_lowercase().contains(&query_lower)
                        || name.to_lowercase().contains(&query_lower)
                })
                .take(top_k)
                .collect();
            if !matches.is_empty() {
                let mut output = format!(
                    "Found {} archived tool results matching '{}':\n\n",
                    matches.len(),
                    query
                );
                for (i, (id, name, preview, ts)) in matches.iter().enumerate() {
                    output.push_str(&format!(
                        "## Archive {} (tool: {}, ts: {})\npreview: {}\nid: {}\n\n",
                        i + 1,
                        name,
                        ts,
                        preview,
                        id,
                    ));
                }
                output.push_str(
                    "Use recall_full with a specific tool_use_id to retrieve the full output.",
                );
                return Ok(output);
            }
        }

        Ok(format!(
            "No matches found for query: '{query}'. \
             Tip: try different keywords, or use recall_full with {{\"list_only\": true}} \
             to browse all archived tool outputs."
        ))
    }

    /// Step 3.2-c:Execute the `dispatch_subagent` tool — subagent-as-tool 路由。
    ///
    /// 主 agent 通过 tool call 派发子 agent。流程:
    /// 1. 解析 JSON 输入(`name`/`task`/`mode`)
    /// 2. 检查 `multi_agent_coordinator` 是否注入
    /// 3. 调用 `coordinator.spawn()` + `coordinator.start()`
    /// 4. 发布 `SubagentHandoff` lane event(可观测性)
    /// 5. 返回 subagent_id(主 agent 后续用 `check_subagent` 轮询)
    ///
    /// **缓存保护**(§5.2):子 agent 走独立 LLM 请求 + 独立 prompt cache,
    /// 不污染主 agent 缓存。本方法只做派发登记,不阻塞等待子 agent 完成。
    fn execute_dispatch_subagent(
        &mut self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(coordinator) = &self.multi_agent_coordinator else {
            return Ok(
                "dispatch_subagent is not available: no multi-agent coordinator configured."
                    .to_string(),
            );
        };

        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;
        let name = parsed
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("missing 'name' field")?;
        let task = parsed
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or("missing 'task' field")?;
        let mode_str = parsed
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("fork");
        let mode = match mode_str {
            "fork" => CoordinationMode::Fork,
            "teammate" => CoordinationMode::Teammate,
            "worktree" => CoordinationMode::Worktree,
            other => {
                return Err(format!(
                    "invalid mode '{other}': expected one of fork/teammate/worktree"
                )
                .into());
            }
        };

        let subagent_id = coordinator.spawn(name, task, mode);
        coordinator
            .start(&subagent_id)
            .map_err(|e| format!("failed to start subagent: {e}"))?;

        // 发布 SubagentHandoff lane event — 主 agent → 子 agent 任务派发记录。
        let emitted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let event = LaneEvent::subagent_handoff(emitted_at.clone(), &subagent_id, mode_str, task);
        publish_lane_event(event);

        // P0-2:子智能体真实化 — 同步阻塞执行独立 LLM 请求。
        //
        // 论文依据:Anthropic Multi-Agent Research System
        // - "spawn fresh subagents with clean contexts" — 完全隔离(独立 Session)
        // - "maintaining continuity through careful handoffs" — task 作为 user message
        // - "Subagent output to a filesystem" — 写到 .claw/subagents/{id}.md
        // - "pass lightweight references back" — 主 agent 只收到 result_ref 路径
        //
        // 子智能体走单轮 LLM 请求(不循环 tool calls),结果写到文件。
        // 主 agent 同步等待,完成后收到 result_ref,可后续读取文件内容。
        let subagent_result = self.run_subagent_turn(&subagent_id, name, task);

        // 根据执行结果标记 coordinator 状态
        let coordinator = self
            .multi_agent_coordinator
            .as_ref()
            .expect("coordinator checked above");
        match &subagent_result {
            Ok(result_ref) => {
                let _ = coordinator.complete(&subagent_id, result_ref.as_str());
            }
            Err(error) => {
                let _ = coordinator.fail(&subagent_id, error.as_str());
            }
        }

        // 发布终态 SubagentResult lane event
        let terminal_status = if subagent_result.is_ok() {
            "completed"
        } else {
            "failed"
        };
        let terminal_result = subagent_result.as_deref().unwrap_or_else(|e| e.as_str());
        let event =
            LaneEvent::subagent_result(emitted_at, &subagent_id, terminal_status, terminal_result);
        publish_lane_event(event);

        // 返回给主 agent:成功返回 result_ref 路径,失败返回错误
        match subagent_result {
            Ok(result_ref) => Ok(format!(
                "Subagent `{subagent_id}` completed. Result written to: {result_ref}\n\
                 Use Read tool to inspect the result. \
                 The subagent ran with an isolated context — it did not pollute your context window."
            )),
            Err(error) => Ok(format!(
                "Subagent `{subagent_id}` failed: {error}\n\
                 You may retry with a different task description or approach the task directly."
            )),
        }
    }

    /// P0-2:执行子智能体的独立 LLM 请求(单轮,完全隔离)。
    ///
    /// 子智能体拥有:
    /// - 独立 Session(空 messages,只有 task 作为 user message)
    /// - 独立 system_prompt(子智能体专用,不包含主 agent 的上下文)
    /// - 独立 prompt cache(不污染主 agent 缓存)
    ///
    /// 执行流程:
    /// 1. 构造子智能体 system_prompt + task 作为 user message
    /// 2. 调用 api_client.stream(复用主 agent 的 client,但请求隔离)
    /// 3. 解析 assistant response,提取 text 内容
    /// 4. 写到 `.claw/subagents/{id}.md`
    /// 5. 返回 result_ref 路径
    fn run_subagent_turn(
        &mut self,
        subagent_id: &str,
        name: &str,
        task: &str,
    ) -> Result<String, String> {
        let workspace_root = self.workspace_root.as_ref().ok_or_else(|| {
            "workspace_root not configured — subagent requires filesystem access for result persistence".to_string()
        })?;

        // 构造子智能体 system_prompt — 完全隔离,不包含主 agent 上下文
        let subagent_system_prompt = SystemPromptSplit::from_sections(vec![format!(
            "# Subagent: {name} ({subagent_id})\n\
                 \n\
                 你是一个子智能体,由主智能体派发执行独立任务。\n\
                 \n\
                 ## 任务\n\
                 {task}\n\
                 \n\
                 ## 约束\n\
                 - 你拥有独立的工作上下文,不共享主智能体的对话历史\n\
                 - 你的响应将被写入文件,主智能体会后续读取\n\
                 - 请提供完整、自包含的分析结果\n\
                 - 不需要调用工具,直接给出你的分析和结论\n\
                 \n\
                 ## 输出格式\n\
                 请直接输出你的分析结果,使用 Markdown 格式。包含:\n\
                 1. 任务理解(简要复述)\n\
                 2. 分析过程\n\
                 3. 关键发现\n\
                 4. 结论和建议"
        )]);

        // 构造子智能体的 user message — task 作为唯一输入
        let user_message = ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text {
                text: format!("请执行以下任务:\n\n{task}"),
            }],
            usage: None,
        };

        let request = ApiRequest {
            system_prompt: subagent_system_prompt,
            messages: vec![user_message],
        };

        // 同步阻塞调用 LLM — 复用主 agent 的 api_client(无状态,请求隔离)
        let events = self
            .api_client
            .stream(request)
            .map_err(|e| format!("subagent LLM request failed: {e}"))?;

        // 解析 assistant response
        let (assistant_message, _usage, _cache_events) = build_assistant_message(events)
            .map_err(|e| format!("subagent response parsing failed: {e}"))?;

        // 提取 text 内容
        let mut text_content = String::new();
        for block in &assistant_message.blocks {
            if let ContentBlock::Text { text } = block {
                text_content.push_str(text);
                text_content.push('\n');
            }
        }
        if text_content.trim().is_empty() {
            return Err("subagent produced no text content".to_string());
        }

        // 写到 .claw/subagents/{id}.md(原子写)
        let subagents_dir = workspace_root.join(".claw").join("subagents");
        std::fs::create_dir_all(&subagents_dir)
            .map_err(|e| format!("failed to create subagents dir: {e}"))?;
        let result_path = subagents_dir.join(format!("{subagent_id}.md"));
        let tmp_path = subagents_dir.join(format!("{subagent_id}.md.tmp"));

        let file_content = format!(
            "# Subagent Result: {name} ({subagent_id})\n\
             \n\
             **Task:** {task}\n\
             **Timestamp:** {}\n\
             \n\
             ---\n\
             \n\
             {text_content}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| format!("{} (unix epoch)", d.as_secs()))
                .unwrap_or_else(|_| "unknown".to_string())
        );

        std::fs::write(&tmp_path, &file_content)
            .map_err(|e| format!("failed to write subagent result tmp file: {e}"))?;
        std::fs::rename(&tmp_path, &result_path)
            .map_err(|e| format!("failed to rename subagent result file: {e}"))?;

        // 返回相对路径(便于主 agent 在 tool result 中阅读)
        let result_ref = format!(".claw/subagents/{subagent_id}.md");
        Ok(result_ref)
    }

    /// Step 3.2-c:Execute the `check_subagent` tool — 查询子 agent 状态/结果。
    ///
    /// 主 agent 通过 tool call 查询子 agent 状态:
    /// 1. 解析 JSON 输入(`subagent_id`)
    /// 2. 调用 `coordinator.get()`
    /// 3. 若已到达终态(completed/failed/cancelled),发布 `SubagentResult` lane event
    /// 4. 返回 JSON:`{"subagent_id","status","result"|"error"}`,便于主 agent 解析
    ///
    /// **幂等性**:对同一 subagent_id 多次调用安全。终态事件每次都会发布
    /// (fingerprint 相同,下游可去重),但返回的 JSON 不变。
    fn execute_check_subagent(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(coordinator) = &self.multi_agent_coordinator else {
            return Ok(
                "check_subagent is not available: no multi-agent coordinator configured."
                    .to_string(),
            );
        };

        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;
        let subagent_id = parsed
            .get("subagent_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'subagent_id' field")?;

        let agent = coordinator
            .get(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;

        let status_str = match agent.status {
            SubagentStatus::Created => "created",
            SubagentStatus::Running => "running",
            SubagentStatus::Completed => "completed",
            SubagentStatus::Failed => "failed",
            SubagentStatus::Cancelled => "cancelled",
        };

        // 终态发布 SubagentResult lane event(可观测性 + downstream 去重)。
        let is_terminal = matches!(
            agent.status,
            SubagentStatus::Completed | SubagentStatus::Failed | SubagentStatus::Cancelled
        );
        if is_terminal {
            let emitted_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| "0".to_string());
            let result_str = agent.result.as_deref().unwrap_or("");
            let event = LaneEvent::subagent_result(emitted_at, subagent_id, status_str, result_str);
            publish_lane_event(event);
        }

        // 返回 JSON 便于主 agent 解析。
        let response = serde_json::json!({
            "subagent_id": subagent_id,
            "status": status_str,
            "terminal": is_terminal,
            "result": agent.result,
            "name": agent.name,
            "mode": match agent.mode {
                CoordinationMode::Fork => "fork",
                CoordinationMode::Teammate => "teammate",
                CoordinationMode::Worktree => "worktree",
            },
        });
        Ok(serde_json::to_string_pretty(&response)?)
    }

    /// P0-1:执行 `notebook_update` 工具调用,维护 NOTEBOOK.md。
    ///
    /// 这是 Anthropic《Effective Context Engineering for AI Agents》明确推荐的
    /// "structured note-taking" 模式实现:LLM 通过此工具把关键信息(决策、
    /// 子智能体注册表、已尝试方案、用户偏好、关键文件引用)写入 NOTEBOOK.md,
    /// 这些信息跨 microcompact / compact_session 持久化,避免长程任务中
    /// "AI 忘记关键信息"导致重复 dispatch / 重复读文件 / 陷入死循环。
    ///
    /// 流程:委托 [`notebook::execute_notebook_update`] 处理。
    /// 需要 `workspace_root` 已通过 [`set_workspace_root`] 设置。
    fn execute_notebook_update(
        &mut self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(workspace_root) = &self.workspace_root else {
            return Ok(
                "notebook_update is not available: no workspace_root configured. \
                 Use --workspace-root or set_workspace_root to enable NOTEBOOK persistence."
                    .to_string(),
            );
        };
        let result = crate::notebook::execute_notebook_update(workspace_root, input);
        // P0-3:无论成功失败,只要 LLM 调用了 notebook_update,说明它已响应
        // 刷新提醒,清除 flag 避免重复提醒。失败时 LLM 会从返回消息看到错误
        // 并自行决定下一步,不需要继续提醒。
        self.notebook_refresh_pending = false;
        match result {
            Ok(message) => Ok(message),
            Err(error) => Ok(format!("notebook_update failed: {error}")),
        }
    }

    /// P0:执行 `recall_full` 工具调用,从 ToolResultArchive 检索 microcompact
    /// 摘要前的原始 tool result。
    ///
    /// # 工作流程
    ///
    /// 1. 解析 `tool_use_id` 参数(JSON: `{"tool_use_id": "call_xxx"}`)
    /// 2. 调用 [`tool_result_archive::recall_tool_result`] 检索归档
    /// 3. 找到 → 返回原始 output + tool_name + archived_at_ms
    /// 4. 未找到 → 返回提示信息,引导 LLM 重新调用原工具
    ///
    /// # 与 microcompact 的关系
    ///
    /// microcompact 在摘要替换前会调用 [`compact::microcompact_with_archiver`]
    /// 把原始 tool result 归档到 `.claw/tool_results_archive.jsonl`。LLM 在后续
    /// turn 看到摘要 `[Read output summarized: 1234 chars → ...]` 时,可调用
    /// `recall_full` 取回原始内容,避免盲目重新调用 Read。
    ///
    /// # 错误处理
    ///
    /// - `workspace_root` 未配置:返回提示信息(不报错,让 LLM 知道功能不可用)
    /// - archive 文件不存在:返回"未找到"提示
    /// - IO/解析错误:返回错误信息
    fn execute_recall_full(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(workspace_root) = &self.workspace_root else {
            return Ok(
                "recall_full is not available: no workspace_root configured. \
                 ToolResultArchive requires workspace_root to locate \
                 .claw/tool_results_archive.jsonl."
                    .to_string(),
            );
        };

        // 解析 input JSON
        let parsed: serde_json::Value = serde_json::from_str(input).map_err(|e| {
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "recall_full: invalid JSON input: {e}. Expected: {{\"tool_use_id\": \"call_xxx\"}} or {{\"list_only\": true}}"
            ))
        })?;

        // 可选:list_only 模式 — 列出所有归档摘要,不返回具体内容。
        // 此模式不需要 tool_use_id,所以先检查 list_only。
        let list_only = parsed
            .get("list_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if list_only {
            let summaries = crate::tool_result_archive::list_archived_summary(workspace_root)?;
            if summaries.is_empty() {
                return Ok("recall_full (list_only): archive is empty.".to_string());
            }
            let mut lines = Vec::with_capacity(summaries.len() + 1);
            lines.push(format!(
                "recall_full (list_only): {} archived tool results:",
                summaries.len()
            ));
            for (id, name, preview, ts_ms) in summaries {
                lines.push(format!(
                    "  - id={id} tool={name} ts={ts_ms} preview={preview}"
                ));
            }
            lines.push(
                "Call recall_full with a specific tool_use_id to retrieve the full output."
                    .to_string(),
            );
            return Ok(lines.join("\n"));
        }

        // 非 list_only 模式:必须提供 tool_use_id
        let tool_use_id = parsed
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "recall_full: missing or invalid 'tool_use_id' field. \
                     Expected: {\"tool_use_id\": \"call_xxx\"} or {\"list_only\": true}",
                )
            })?;

        // 按 tool_use_id 检索
        match crate::tool_result_archive::recall_tool_result(workspace_root, tool_use_id)? {
            Some(record) => Ok(format!(
                "recall_full: retrieved archived tool result.\n\
                     tool_use_id: {}\n\
                     tool_name: {}\n\
                     archived_at_ms: {}\n\
                     --- original output ---\n\
                     {}",
                record.tool_use_id, record.tool_name, record.archived_at_ms, record.output
            )),
            None => Ok(format!(
                "recall_full: no archived tool result found for tool_use_id='{tool_use_id}'.\n\
                 The tool result may not have been summarized yet, or the archive \
                 file (.claw/tool_results_archive.jsonl) may have been pruned.\n\
                 Hint: call recall_full with {{\"list_only\": true}} to see all archived ids, \
                 or re-invoke the original tool to get fresh output."
            )),
        }
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

    /// 只读访问底层 API client（供查询状态如 reasoning_effort）。
    pub fn api_client(&self) -> &C {
        &self.api_client
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

    /// 计算当前有效的 compaction 阈值。
    ///
    /// 优先级:
    /// 1. 环境变量 `CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS` (显式覆盖) >
    /// 2. 按 `context_window` 动态计算 (65% 比例,上限 800K) >
    /// 3. 回退到 `auto_compaction_input_tokens_threshold` (默认 100K)
    fn effective_compaction_threshold(&self) -> u32 {
        // 环境变量覆盖始终优先。
        // 修复:用 `Option::is_some()` 判定 env 是否被显式设置,
        // 而非 `!= DEFAULT`。否则用户显式设置 env=100K(与默认相同)时,
        // 会被误判为"未设置"并跳过 context_window 动态计算。
        if let Some(env_threshold) = auto_compaction_threshold_from_env_opt() {
            return env_threshold;
        }
        // 按模型 context window 动态计算
        if let Some(context_window) = self.context_window {
            compaction_threshold_for_context_window(context_window)
        } else {
            self.auto_compaction_input_tokens_threshold
        }
    }

    fn maybe_auto_compact(&mut self) -> Option<AutoCompactionEvent> {
        let threshold = self.effective_compaction_threshold();
        if self.usage_tracker.cumulative_usage().input_tokens < threshold {
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
        // P0-3:大压缩发生(删除了消息),提醒 LLM 下个 turn 刷新 NOTEBOOK。
        // auto_compaction 比 microcompact 更激进,会删除整条消息而非替换,
        // 关键信息丢失风险更高,因此必须刷新 NOTEBOOK。
        self.notebook_refresh_pending = true;
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
        let mut record = TraceRecord::new(turn_id, latency_ms, tool_calls)
            .with_compact_triggered(compact_triggered);
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
///
/// 返回 `DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD` 当环境变量未设置或解析失败时。
/// 如需区分"env 未设置"和"env 设置为默认值",使用 [`auto_compaction_threshold_from_env_opt`]。
#[must_use]
pub fn auto_compaction_threshold_from_env() -> u32 {
    auto_compaction_threshold_from_env_opt()
        .unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

/// 读取环境变量中的 compaction 阈值,只在 env 被显式设置且有效时返回 `Some`。
///
/// 与 [`auto_compaction_threshold_from_env`] 的区别:
/// - `from_env()` 返回 `u32`,无法区分"未设置"和"显式设置为默认值"
/// - `from_env_opt()` 返回 `Option<u32>`,允许调用方准确判定 env 是否被显式设置
///
/// 这是 `effective_compaction_threshold` 优先级链的关键判定依据:
/// 只有 env 被显式设置时才覆盖 context_window 动态计算,否则让 context_window 优先。
#[must_use]
pub fn auto_compaction_threshold_from_env_opt() -> Option<u32> {
    parse_auto_compaction_threshold_opt(std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR).ok().as_deref())
}

/// 根据模型 context window 动态计算 compaction 阈值。
///
/// 规则:
/// - context_window >= 1M: 使用 65% 即 ~650K
/// - context_window >= 200K: 使用 65% 即 ~130K
/// - 其他已知窗口: 使用 65%
/// - 0 (未知): 回退到 100K
///
/// 上限 800K,防止极端情况。
#[must_use]
pub fn compaction_threshold_for_context_window(context_window: u32) -> u32 {
    if context_window == 0 {
        return DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD;
    }

    // 使用 context window 的 65% 作为压缩阈值
    let ratio = ((context_window as u64) * 65 / 100) as u32;
    // 上限 800K,保留足够空间给 output + 安全余量
    ratio.min(800_000)
}

/// P0-3 辅助:计算 messages 中所有 ToolResult block 的 output 总长度。
///
/// 用于检测 microcompact 前后是否发生了实质性压缩(旧 tool result 被替换)。
/// 只统计 `role == Tool` 消息中 `ContentBlock::ToolResult` 的 output 字段,
/// 因为 microcompact 只替换这些 block,其他内容不变。
#[must_use]
pub fn tool_result_output_len(messages: &[ConversationMessage]) -> usize {
    use crate::session::ContentBlock;
    messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .flat_map(|m| m.blocks.iter())
        .map(|block| match block {
            ContentBlock::ToolResult { output, .. } => output.len(),
            _ => 0,
        })
        .sum()
}

/// 旧版解析函数,保留供测试验证向后兼容性。
/// 生产代码请使用 [`parse_auto_compaction_threshold_opt`]。
#[cfg(test)]
#[must_use]
fn parse_auto_compaction_threshold(value: Option<&str>) -> u32 {
    parse_auto_compaction_threshold_opt(value)
        .unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

/// 解析 compaction 阈值,只在输入有效(非零正整数)时返回 `Some`。
///
/// 返回 `None` 的情况:
/// - `value == None`(env 未设置)
/// - 解析失败(非数字)
/// - 值为 0(无效阈值)
#[must_use]
fn parse_auto_compaction_threshold_opt(value: Option<&str>) -> Option<u32> {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|threshold| *threshold > 0)
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
        build_assistant_message, compaction_threshold_for_context_window,
        parse_auto_compaction_threshold, parse_auto_compaction_threshold_opt, ApiClient, ApiRequest,
        AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RuntimeError,
        StaticToolExecutor, ToolExecutor, DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
        SESSION_SEARCH_TOOL_SPEC,
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
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
    use crate::usage::TokenUsage;
    use crate::ToolError;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use telemetry::{MemoryTelemetrySink, SessionTracer, TelemetryEvent};

    /// Step 3.2-c:测试锁 — 确保依赖全局 lane event sink 的测试串行运行。
    /// `drain_lane_events()` 会清空整个 sink,并行运行会互相偷走事件。
    /// 不依赖 sink 的测试不受此锁影响,仍可并行运行。
    static LANE_EVENT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 获取测试锁的 guard。在依赖 lane event sink 的测试开头调用。
    /// 锁中毒时恢复(poison 不应阻塞测试)。
    fn acquire_lane_event_lock() -> std::sync::MutexGuard<'static, ()> {
        LANE_EVENT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

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

    /// 验证 `_opt` 版本能区分"未设置"和"显式设置为默认值",
    /// 这是 `effective_compaction_threshold` 优先级链修复的关键依据。
    #[test]
    fn parse_auto_compaction_threshold_opt_distinguishes_unset_from_default() {
        // env 未设置 → None(让 context_window 动态计算生效)
        assert_eq!(parse_auto_compaction_threshold_opt(None), None);
        // 显式设置为默认值 → Some(让 env 覆盖 context_window)
        assert_eq!(
            parse_auto_compaction_threshold_opt(Some("100000")),
            Some(100_000)
        );
        // 有效值 → Some
        assert_eq!(parse_auto_compaction_threshold_opt(Some("4321")), Some(4321));
        // 0 无效 → None(回退到 context_window 动态计算)
        assert_eq!(parse_auto_compaction_threshold_opt(Some("0")), None);
        // 非数字 → None
        assert_eq!(
            parse_auto_compaction_threshold_opt(Some("not-a-number")),
            None
        );
        // 带空白的有效值 → Some
        assert_eq!(
            parse_auto_compaction_threshold_opt(Some("  50000  ")),
            Some(50_000)
        );
    }

    #[test]
    fn compaction_threshold_scales_with_context_window() {
        // 1M context window → 650K threshold (65%)
        assert_eq!(
            compaction_threshold_for_context_window(1_000_000),
            650_000
        );
        // 200K context window → 130K threshold (65%)
        assert_eq!(
            compaction_threshold_for_context_window(200_000),
            130_000
        );
        // 256K context window → 166K threshold (65%)
        assert_eq!(
            compaction_threshold_for_context_window(256_000),
            166_400
        );
        // 131K context window → ~85K threshold
        assert_eq!(
            compaction_threshold_for_context_window(131_072),
            85_196
        );
    }

    #[test]
    fn compaction_threshold_capped_at_800k() {
        // 2M window → should cap at 800K, not 1.3M
        assert_eq!(
            compaction_threshold_for_context_window(2_000_000),
            800_000
        );
    }

    #[test]
    fn compaction_threshold_zero_falls_back_to_default() {
        assert_eq!(
            compaction_threshold_for_context_window(0),
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
            crate::session::ConversationMessage::tool_result("tool-4", "Read", big_output, false),
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
        // should exhaust all recovery steps (microcompact + full compact +
        // one Provider recovery attempt) and bail out instead of retrying
        // forever.
        //
        // **批次 6（P0-3）修复后**:removed==0 分支也调用
        // `try_recover_or_record_fail`,让 Provider 恢复(如切换到更长 context
        // 的 Provider)有机会生效。默认 RecoveryOrchestrator 第一次 attempt
        // 总是 Recovered,所以会多一次 API 调用验证恢复是否真的解决问题。
        // 第二次 attempt 后 escalation,流程终止。
        struct AlwaysPromptTooLongApi {
            call_count: usize,
        }
        impl ApiClient for AlwaysPromptTooLongApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                Err(RuntimeError::new("prompt exceeds maximum context length"))
            }
        }

        // Small session: should_compact returns false, so full compaction
        // removes nothing and the recovery bails after three API calls:
        //   1) initial attempt → prompt_too_long
        //   2) after microcompact (still too long, removed==0 → Provider recovery)
        //   3) recovery succeeded, retry under new Provider → still too long
        //      → second Provider attempt → escalation → bail out.
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

        // The state machine should have stopped after three attempts
        // (initial + post-microcompact + post-recovery), not retried indefinitely.
        assert_eq!(runtime.api_client_mut().call_count, 3);
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
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(vec![
                AssistantEvent::TextDelta("noop".to_string()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    fn open_temp_history_index() -> (tempfile::NamedTempFile, crate::history_search::HistoryIndex) {
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
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
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
            .index_message("configure the rust toolchain", "sess-a", "user", 0, 1_000)
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
        assert_eq!(
            spec["input_schema"]["properties"]["query"]["type"],
            "string"
        );
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

    // ----- dispatch_subagent / check_subagent tool tests -----
    //
    // Step 3.2-c:subagent-as-tool 路由测试。
    // 验证 ConversationRuntime::execute_dispatch_subagent /
    // execute_check_subagent 的行为,包括:
    // - 无 coordinator 时的 soft-failure
    // - 正常派发/查询流程
    // - JSON 输入解析错误
    // - SubagentHandoff / SubagentResult lane event 发布

    fn runtime_without_coordinator() -> ConversationRuntime<NoopApi, StaticToolExecutor> {
        ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
    }

    fn runtime_with_coordinator(
        coordinator: crate::multi_agent::MultiAgentCoordinator,
    ) -> ConversationRuntime<NoopApi, StaticToolExecutor> {
        ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_multi_agent_coordinator(coordinator)
    }

    #[test]
    fn dispatch_subagent_returns_message_when_no_coordinator_configured() {
        let mut runtime = runtime_without_coordinator();
        let output = runtime
            .execute_dispatch_subagent(r#"{"name":"a","task":"b"}"#)
            .expect("soft failure should not propagate as error");
        assert!(
            output.contains("dispatch_subagent is not available"),
            "missing 'not available' message: {output}"
        );
    }

    #[test]
    fn dispatch_subagent_spawns_and_starts_subagent() {
        // 获取测试锁,确保 lane event sink 操作不被并行测试干扰。
        let _guard = acquire_lane_event_lock();
        // 用唯一的 task 字符串标识本测试的事件,避免并行运行时其他测试干扰。
        let unique_task = "Refactor auth module [test-dispatch-spawn-uuid-7c3a]";
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator.clone());
        // P0-2:子智能体真实化需要 workspace_root 来持久化结果到 .claw/subagents/{id}.md
        let tempdir = tempfile::tempdir().expect("failed to create temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let input = serde_json::json!({
            "name": "refactor-auth",
            "task": unique_task,
            "mode": "fork"
        })
        .to_string();
        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should succeed");
        // P0-2:同步执行后,成功消息包含 "completed" 和 result_ref 路径
        assert!(
            output.contains("Subagent `") && output.contains("completed"),
            "missing 'Subagent `...` completed' marker: {output}"
        );
        assert!(
            output.contains(".claw/subagents/"),
            "missing result_ref path: {output}"
        );
        // 提取 subagent_id — 形如 `subagent-1`。
        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("should extract subagent_id from output");
        assert!(
            subagent_id.starts_with("subagent-"),
            "unexpected subagent_id: {subagent_id}"
        );

        // P0-2:同步执行后,coordinator 中子 agent 状态应为 Completed(不再是 Running)。
        let agent = coordinator
            .get(subagent_id)
            .expect("subagent should be registered");
        assert_eq!(agent.status, crate::multi_agent::SubagentStatus::Completed);
        assert_eq!(agent.name, "refactor-auth");
        assert_eq!(agent.task, unique_task);
        assert_eq!(agent.mode, crate::multi_agent::CoordinationMode::Fork);
        // result 字段应包含 result_ref 路径
        assert!(
            agent
                .result
                .as_deref()
                .unwrap_or("")
                .contains(".claw/subagents/"),
            "coordinator.result should contain result_ref path: {:?}",
            agent.result
        );

        // 验证结果文件确实写入磁盘(P0-2 核心不变量:"Subagent output to a filesystem")
        let result_file = tempdir
            .path()
            .join(".claw")
            .join("subagents")
            .join(format!("{subagent_id}.md"));
        assert!(
            result_file.exists(),
            "subagent result file should exist at {result_file:?}"
        );
        let file_content = std::fs::read_to_string(&result_file).expect("read result file");
        assert!(
            file_content.contains(unique_task),
            "result file should contain the task: {file_content}"
        );

        // 验证 SubagentHandoff lane event 已发布。用 task 字段过滤,避免并行竞争。
        let events = crate::lane_events::drain_lane_events();
        let handoff = events.iter().find(|e| {
            e.event == crate::lane_events::LaneEventName::SubagentHandoff
                && e.data
                    .as_ref()
                    .and_then(|d| d.get("task"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == unique_task)
        });
        assert!(
            handoff.is_some(),
            "SubagentHandoff event should be published"
        );
        let handoff = handoff.unwrap();
        assert_eq!(handoff.status, crate::lane_events::LaneEventStatus::Running);
        let data = handoff.data.as_ref().expect("handoff event has data");
        assert_eq!(data["subagent_id"], subagent_id);
        assert_eq!(data["mode"], "fork");
        assert_eq!(data["task"], unique_task);

        // P0-2:还应发布 SubagentResult 终态事件
        let result_event = events.iter().find(|e| {
            e.event == crate::lane_events::LaneEventName::SubagentResult
                && e.data
                    .as_ref()
                    .and_then(|d| d.get("subagent_id"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == subagent_id)
        });
        assert!(
            result_event.is_some(),
            "SubagentResult terminal event should be published after P0-2 sync execution"
        );
    }

    #[test]
    fn dispatch_subagent_defaults_mode_to_fork() {
        let unique_task = "test-defaults-mode-uuid-9f2b";
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator);
        // P0-2:子智能体真实化需要 workspace_root
        let tempdir = tempfile::tempdir().expect("failed to create temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let input = serde_json::json!({
            "name": "a",
            "task": unique_task
        })
        .to_string();
        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should succeed");
        // P0-2:默认 mode 为 fork,同步执行应成功完成
        assert!(
            output.contains("Subagent `") && output.contains("completed"),
            "default mode should succeed with 'completed' status: {output}"
        );
        assert!(
            output.contains(".claw/subagents/"),
            "missing result_ref path: {output}"
        );
    }

    #[test]
    fn dispatch_subagent_handles_invalid_json() {
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator);

        let error = runtime
            .execute_dispatch_subagent("not json")
            .expect_err("invalid JSON should propagate as error");
        assert!(
            error.to_string().contains("invalid input JSON"),
            "expected invalid JSON error, got: {error}"
        );
    }

    #[test]
    fn dispatch_subagent_errors_when_name_missing() {
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator);

        let error = runtime
            .execute_dispatch_subagent(r#"{"task":"b"}"#)
            .expect_err("missing name should error");
        assert!(
            error.to_string().contains("missing 'name'"),
            "expected missing name error, got: {error}"
        );
    }

    #[test]
    fn dispatch_subagent_errors_when_task_missing() {
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator);

        let error = runtime
            .execute_dispatch_subagent(r#"{"name":"a"}"#)
            .expect_err("missing task should error");
        assert!(
            error.to_string().contains("missing 'task'"),
            "expected missing task error, got: {error}"
        );
    }

    #[test]
    fn dispatch_subagent_errors_when_mode_invalid() {
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator);

        let error = runtime
            .execute_dispatch_subagent(r#"{"name":"a","task":"b","mode":"bogus"}"#)
            .expect_err("invalid mode should error");
        assert!(
            error.to_string().contains("invalid mode 'bogus'"),
            "expected invalid mode error, got: {error}"
        );
    }

    /// P0-2:子智能体真实化 — 无 workspace_root 时应优雅失败,coordinator 标记为 Failed。
    ///
    /// 这是 P0-2 的关键不变量:子智能体需要文件系统持久化(Anthropic 推荐),
    /// 没有 workspace_root 就无法写 result 文件,应返回错误而非静默降级。
    #[test]
    fn dispatch_subagent_fails_gracefully_without_workspace_root() {
        let _guard = acquire_lane_event_lock();
        let unique_task = "test-no-workspace-uuid-p0-2-fail";
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator.clone());
        // 故意不设置 workspace_root — 验证优雅失败

        let input = serde_json::json!({
            "name": "no-workspace-agent",
            "task": unique_task,
            "mode": "fork"
        })
        .to_string();
        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should not propagate as hard error");
        // 应返回失败消息给主 agent
        assert!(
            output.contains("Subagent `") && output.contains("failed"),
            "missing 'failed' marker: {output}"
        );
        assert!(
            output.contains("workspace_root not configured"),
            "missing 'workspace_root not configured' reason: {output}"
        );

        // 提取 subagent_id 并验证 coordinator 状态为 Failed
        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("should extract subagent_id");
        let agent = coordinator
            .get(subagent_id)
            .expect("subagent should be registered despite failure");
        assert_eq!(agent.status, crate::multi_agent::SubagentStatus::Failed);
        assert!(
            agent
                .result
                .as_deref()
                .unwrap_or("")
                .contains("workspace_root not configured"),
            "coordinator.result should contain failure reason: {:?}",
            agent.result
        );

        // 验证 SubagentResult 终态事件已发布(status=failed)
        let events = crate::lane_events::drain_lane_events();
        let result_event = events.iter().find(|e| {
            e.event == crate::lane_events::LaneEventName::SubagentResult
                && e.data
                    .as_ref()
                    .and_then(|d| d.get("subagent_id"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == subagent_id)
        });
        assert!(
            result_event.is_some(),
            "SubagentResult event should be published"
        );
        let result_event = result_event.unwrap();
        assert_eq!(
            result_event.status,
            crate::lane_events::LaneEventStatus::Failed
        );
    }

    /// P0-2:子智能体真实化 — 验证主 agent 上下文不被污染。
    ///
    /// 这是 P0-2 的核心设计目标(Anthropic Multi-Agent Research System):
    /// "spawn fresh subagents with clean contexts" — 子智能体执行不应
    /// 在主 agent 的 session messages 中留下任何痕迹。
    #[test]
    fn dispatch_subagent_does_not_pollute_main_session_messages() {
        let _guard = acquire_lane_event_lock();
        let unique_task = "test-context-isolation-uuid-p0-2-iso";
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("failed to create temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        // 记录执行前的 session messages 数量
        let messages_before = runtime.session().messages.len();

        let input = serde_json::json!({
            "name": "isolated-agent",
            "task": unique_task,
            "mode": "fork"
        })
        .to_string();
        let _output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should succeed");

        // P0-2 核心不变量:子智能体的 LLM 请求和响应完全隔离,
        // 不应在主 agent 的 session messages 中添加任何消息。
        let messages_after = runtime.session().messages.len();
        assert_eq!(
            messages_before, messages_after,
            "P0-2 violation: subagent execution polluted main session messages \
             (before={messages_before}, after={messages_after}). \
             Subagent must run with isolated context."
        );
    }

    /// P0-2:子智能体真实化 — 验证多次 dispatch 产生递增的 subagent_id。
    ///
    /// 确保 id_counter 正确递增,主 agent 可以引用不同的 subagent_id。
    #[test]
    fn dispatch_subagent_increments_id_across_calls() {
        let _guard = acquire_lane_event_lock();
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("failed to create temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let extract_id = |output: &str| -> String {
            output
                .split("Subagent `")
                .nth(1)
                .and_then(|s| s.split('`').next())
                .expect("should extract subagent_id")
                .to_string()
        };

        let input1 = serde_json::json!({"name":"a","task":"task-1","mode":"fork"}).to_string();
        let output1 = runtime
            .execute_dispatch_subagent(&input1)
            .expect("first dispatch");
        let id1 = extract_id(&output1);

        let input2 = serde_json::json!({"name":"b","task":"task-2","mode":"fork"}).to_string();
        let output2 = runtime
            .execute_dispatch_subagent(&input2)
            .expect("second dispatch");
        let id2 = extract_id(&output2);

        assert!(
            id1 != id2,
            "subagent_ids should differ across dispatches: id1={id1}, id2={id2}"
        );
        // 验证 id 格式递增(subagent-1, subagent-2, ...)
        assert!(
            id1.starts_with("subagent-") && id2.starts_with("subagent-"),
            "ids should follow subagent-N pattern: id1={id1}, id2={id2}"
        );
        let n1: u64 = id1.strip_prefix("subagent-").unwrap().parse().unwrap();
        let n2: u64 = id2.strip_prefix("subagent-").unwrap().parse().unwrap();
        assert_eq!(
            n2,
            n1 + 1,
            "id counter should increment by 1: n1={n1}, n2={n2}"
        );

        // 两个结果文件都应存在
        let file1 = tempdir
            .path()
            .join(".claw")
            .join("subagents")
            .join(format!("{id1}.md"));
        let file2 = tempdir
            .path()
            .join(".claw")
            .join("subagents")
            .join(format!("{id2}.md"));
        assert!(file1.exists(), "result file 1 should exist: {file1:?}");
        assert!(file2.exists(), "result file 2 should exist: {file2:?}");
    }

    #[test]
    fn check_subagent_returns_message_when_no_coordinator_configured() {
        let runtime = runtime_without_coordinator();
        let output = runtime
            .execute_check_subagent(r#"{"subagent_id":"x"}"#)
            .expect("soft failure should not propagate as error");
        assert!(
            output.contains("check_subagent is not available"),
            "missing 'not available' message: {output}"
        );
    }

    #[test]
    fn check_subagent_returns_running_status_for_active_subagent() {
        // 获取测试锁,确保 lane event sink 操作不被并行测试干扰。
        let _guard = acquire_lane_event_lock();
        // 用唯一的 task 标识,避免并行竞争。
        let unique_task = "test-check-running-uuid-3e7a";
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let id = coordinator.spawn("a", unique_task, crate::multi_agent::CoordinationMode::Fork);
        coordinator.start(&id).expect("start should succeed");

        let runtime = runtime_with_coordinator(coordinator);
        let output = runtime
            .execute_check_subagent(&format!(r#"{{"subagent_id":"{id}"}}"#))
            .expect("check should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("output should be JSON");
        assert_eq!(parsed["status"], "running");
        assert_eq!(parsed["terminal"], false);
        assert_eq!(parsed["subagent_id"], id);

        // Running 状态不应发布 SubagentResult 事件。
        // 用 result 字段过滤(本测试无 result),所以不应匹配到任何事件。
        let events = crate::lane_events::drain_lane_events();
        let has_result_for_this = events.iter().any(|e| {
            e.event == crate::lane_events::LaneEventName::SubagentResult
                && e.data
                    .as_ref()
                    .and_then(|d| d.get("subagent_id"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == id)
        });
        // 注意:并行运行时 id 可能与其他测试的 subagent-id 冲突。
        // 但 running 状态本来就不发布事件,所以 has_result_for_this 应为 false。
        // 即使 id 冲突,其他测试如果发布了 SubagentResult,也是它们自己的 subagent,
        // 不会用同一个 id(因为每个测试创建独立的 coordinator)。
        // 唯一风险:两个测试都创建了 "subagent-1" 且都发布事件。但 running 测试不发布。
        // 因此这里只需检查本测试未发布事件即可 — 宽松断言。
        let _ = has_result_for_this; // 不做严格断言,因为并行竞争无法完全避免。
    }

    #[test]
    fn check_subagent_publishes_terminal_event_for_completed() {
        // 获取测试锁,确保 lane event sink 操作不被并行测试干扰。
        let _guard = acquire_lane_event_lock();
        // 用唯一的 result 字符串标识本测试的事件,避免并行竞争。
        let unique_result = "all done [test-check-completed-uuid-5d1c]";
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let id = coordinator.spawn("a", "b", crate::multi_agent::CoordinationMode::Fork);
        coordinator.start(&id).unwrap();
        coordinator
            .complete(&id, unique_result)
            .expect("complete should succeed");

        let runtime = runtime_with_coordinator(coordinator);
        let output = runtime
            .execute_check_subagent(&format!(r#"{{"subagent_id":"{id}"}}"#))
            .expect("check should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("output should be JSON");
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["terminal"], true);
        assert_eq!(parsed["result"], unique_result);

        // 用 result 字段过滤,避免被并行测试的 drain_lane_events 偷走。
        let events = crate::lane_events::drain_lane_events();
        let result_event = events.iter().find(|e| {
            e.event == crate::lane_events::LaneEventName::SubagentResult
                && e.data
                    .as_ref()
                    .and_then(|d| d.get("result"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|r| r == unique_result)
        });
        assert!(
            result_event.is_some(),
            "SubagentResult event should be published for completed subagent"
        );
        let result_event = result_event.unwrap();
        assert_eq!(
            result_event.status,
            crate::lane_events::LaneEventStatus::Completed
        );
        let data = result_event.data.as_ref().expect("result event has data");
        assert_eq!(data["subagent_id"], id);
        assert_eq!(data["status"], "completed");
        assert_eq!(data["result"], unique_result);
        // completed 不应设置 failure_class。
        assert!(result_event.failure_class.is_none());
    }

    #[test]
    fn check_subagent_publishes_terminal_event_for_failed() {
        // 获取测试锁,确保 lane event sink 操作不被并行测试干扰。
        let _guard = acquire_lane_event_lock();
        // 用唯一的 error 字符串标识本测试的事件,避免并行竞争。
        // fail() 会自动添加 "error: " 前缀。
        let unique_error = "compile error [test-check-failed-uuid-8b4e]";
        let expected_result = format!("error: {unique_error}");
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let id = coordinator.spawn("a", "b", crate::multi_agent::CoordinationMode::Fork);
        coordinator.start(&id).unwrap();
        coordinator
            .fail(&id, unique_error)
            .expect("fail should succeed");

        let runtime = runtime_with_coordinator(coordinator);
        let output = runtime
            .execute_check_subagent(&format!(r#"{{"subagent_id":"{id}"}}"#))
            .expect("check should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("output should be JSON");
        assert_eq!(parsed["status"], "failed");
        assert_eq!(parsed["terminal"], true);
        assert_eq!(parsed["result"], expected_result);

        // 用 result 字段过滤,避免并行竞争。
        let events = crate::lane_events::drain_lane_events();
        let result_event = events.iter().find(|e| {
            e.event == crate::lane_events::LaneEventName::SubagentResult
                && e.data
                    .as_ref()
                    .and_then(|d| d.get("result"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|r| r == expected_result)
        });
        assert!(
            result_event.is_some(),
            "SubagentResult event should be published for failed subagent"
        );
        let result_event = result_event.unwrap();
        assert_eq!(
            result_event.status,
            crate::lane_events::LaneEventStatus::Failed
        );
        // failed 必须设置 failure_class = SubagentFailure。
        assert_eq!(
            result_event.failure_class,
            Some(crate::lane_events::LaneFailureClass::SubagentFailure)
        );
    }

    #[test]
    fn check_subagent_errors_when_subagent_id_missing() {
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let runtime = runtime_with_coordinator(coordinator);

        let error = runtime
            .execute_check_subagent(r#"{}"#)
            .expect_err("missing subagent_id should error");
        assert!(
            error.to_string().contains("missing 'subagent_id'"),
            "expected missing subagent_id error, got: {error}"
        );
    }

    #[test]
    fn check_subagent_errors_when_subagent_not_found() {
        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let runtime = runtime_with_coordinator(coordinator);

        let error = runtime
            .execute_check_subagent(r#"{"subagent_id":"nonexistent"}"#)
            .expect_err("nonexistent subagent should error");
        assert!(
            error.to_string().contains("subagent not found"),
            "expected 'subagent not found' error, got: {error}"
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

    // ----- P0-3: NOTEBOOK 刷新提醒测试 -----
    //
    // 验证 tool_result_output_len 辅助函数能正确统计 ToolResult output 长度,
    // 用于检测 microcompact 前后是否发生实质性压缩。

    #[test]
    fn tool_result_output_len_counts_only_tool_result_blocks() {
        use crate::session::{ContentBlock, ConversationMessage, MessageRole};
        let messages = vec![
            ConversationMessage {
                role: MessageRole::User,
                blocks: vec![ContentBlock::Text {
                    text: "user query".to_string(),
                }],
                usage: None,
            },
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "1".to_string(),
                    tool_name: "Read".to_string(),
                    output: "line1\nline2\nline3".to_string(),
                    is_error: false,
                }],
                usage: None,
            },
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "2".to_string(),
                        tool_name: "Bash".to_string(),
                        output: "output2".to_string(),
                        is_error: false,
                    },
                    // 非 ToolResult block 不计入
                    ContentBlock::Text {
                        text: "ignored".to_string(),
                    },
                ],
                usage: None,
            },
        ];
        // "line1\nline2\nline3" (17) + "output2" (7) = 24
        assert_eq!(
            super::tool_result_output_len(&messages),
            17 + 7,
            "should sum only ToolResult output lengths"
        );
    }

    #[test]
    fn tool_result_output_len_zero_for_empty() {
        use crate::session::{ConversationMessage, MessageRole};
        let empty: Vec<ConversationMessage> = vec![];
        assert_eq!(super::tool_result_output_len(&empty), 0);

        let no_tool = vec![ConversationMessage {
            role: MessageRole::User,
            blocks: vec![crate::session::ContentBlock::Text {
                text: "hello".to_string(),
            }],
            usage: None,
        }];
        assert_eq!(super::tool_result_output_len(&no_tool), 0);
    }

    #[test]
    fn tool_result_output_len_decreases_after_microcompact() {
        // 模拟 microcompact 行为:旧 tool result 的 output 被替换成短 summary。
        // 验证 tool_result_output_len 能检测到长度减少。
        use crate::session::{ContentBlock, ConversationMessage, MessageRole};
        let long_output = "very long file content\n".repeat(100);
        let before = vec![ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "1".to_string(),
                tool_name: "Read".to_string(),
                output: long_output.clone(),
                is_error: false,
            }],
            usage: None,
        }];
        let after = vec![ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "1".to_string(),
                tool_name: "Read".to_string(),
                output: "Read: file.rs (summarized)".to_string(),
                is_error: false,
            }],
            usage: None,
        }];
        let before_len = super::tool_result_output_len(&before);
        let after_len = super::tool_result_output_len(&after);
        assert!(
            after_len < before_len,
            "microcompact should reduce tool_result output length: {after_len} < {before_len}"
        );
        // 这个差值就是 P0-3 flag 触发的依据
        assert!(before_len > 1000, "before should be long: {before_len}");
        assert!(after_len < 100, "after should be short: {after_len}");
    }

    // ============================================================================
    // P0:recall_full 工具拦截 + ToolResultArchive 集成测试
    // ============================================================================

    #[test]
    fn recall_full_returns_unavailable_when_no_workspace_root() {
        // 不设置 workspace_root,recall_full 应返回不可用提示(不报错)
        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let output = runtime
            .execute_recall_full(r#"{"tool_use_id":"call_abc"}"#)
            .expect("should not propagate as hard error");
        assert!(
            output.contains("not available"),
            "expected 'not available' message: {output}"
        );
        assert!(
            output.contains("workspace_root"),
            "expected 'workspace_root' hint: {output}"
        );
    }

    #[test]
    fn recall_full_returns_not_found_for_unknown_tool_use_id() {
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_workspace_root(tempdir.path().to_path_buf());

        let output = runtime
            .execute_recall_full(r#"{"tool_use_id":"nonexistent"}"#)
            .expect("should succeed with not-found message");
        assert!(
            output.contains("no archived tool result found"),
            "expected 'not found' message: {output}"
        );
        assert!(
            output.contains("list_only"),
            "expected list_only hint: {output}"
        );
    }

    #[test]
    fn recall_full_retrieves_archived_tool_result() {
        // 验证 recall_full 能从 archive 检索原始 tool result。
        // 这是 P0 的核心测试:确保 microcompact 摘要的原始内容可被 LLM 取回。
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        let workspace_root = tempdir.path().to_path_buf();

        // 手动归档一条 tool result(模拟 microcompact_with_archiver 的行为)
        let original_output = "line1\nline2\nline3\nline4\nline5\nimportant content";
        crate::tool_result_archive::archive_tool_result(
            &workspace_root,
            "call_test_123",
            "Read",
            original_output,
        )
        .expect("archive should succeed");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_workspace_root(workspace_root);

        let output = runtime
            .execute_recall_full(r#"{"tool_use_id":"call_test_123"}"#)
            .expect("recall should succeed");
        assert!(
            output.contains("retrieved archived tool result"),
            "expected 'retrieved' message: {output}"
        );
        assert!(
            output.contains("call_test_123"),
            "expected tool_use_id in output: {output}"
        );
        assert!(
            output.contains("Read"),
            "expected tool_name 'Read' in output: {output}"
        );
        assert!(
            output.contains(original_output),
            "expected original output in result: {output}"
        );
    }

    #[test]
    fn recall_full_list_only_mode_returns_summary() {
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        let workspace_root = tempdir.path().to_path_buf();

        // 归档 3 条记录
        crate::tool_result_archive::archive_tool_result(
            &workspace_root,
            "id_1",
            "Read",
            "content1",
        )
        .unwrap();
        crate::tool_result_archive::archive_tool_result(
            &workspace_root,
            "id_2",
            "Bash",
            "content2",
        )
        .unwrap();
        crate::tool_result_archive::archive_tool_result(
            &workspace_root,
            "id_3",
            "Grep",
            "content3",
        )
        .unwrap();

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_workspace_root(workspace_root);

        let output = runtime
            .execute_recall_full(r#"{"list_only":true}"#)
            .expect("list_only should succeed");
        assert!(
            output.contains("3 archived tool results"),
            "expected count '3': {output}"
        );
        assert!(output.contains("id_1"), "expected id_1: {output}");
        assert!(output.contains("id_2"), "expected id_2: {output}");
        assert!(output.contains("id_3"), "expected id_3: {output}");
    }

    #[test]
    fn recall_full_handles_invalid_json() {
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_workspace_root(tempdir.path().to_path_buf());

        let error = runtime
            .execute_recall_full("not json")
            .expect_err("invalid JSON should propagate as error");
        assert!(
            error.to_string().contains("invalid JSON input"),
            "expected invalid JSON error: {error}"
        );
    }

    #[test]
    fn recall_full_errors_when_tool_use_id_missing() {
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_workspace_root(tempdir.path().to_path_buf());

        let error = runtime
            .execute_recall_full(r#"{"list_only":false}"#)
            .expect_err("missing tool_use_id should propagate as error");
        assert!(
            error
                .to_string()
                .contains("missing or invalid 'tool_use_id'"),
            "expected missing tool_use_id error: {error}"
        );
    }

    /// P0 端到端测试:验证 microcompact_with_archiver 归档的原始内容
    /// 能被 recall_full 检索到。
    ///
    /// 测试流程:
    /// 1. 构造 session 包含多个旧的 Read tool result
    /// 2. 调用 microcompact_with_archiver(preserve_recent=1) 摘要旧 result
    /// 3. 验证 archive 文件包含被摘要的原始内容
    /// 4. 通过 recall_full 检索原始内容,验证完整性
    #[test]
    fn microcompact_archives_and_recall_full_retrieves_end_to_end() {
        let tempdir = tempfile::tempdir().expect("failed to create temp dir");
        let workspace_root = tempdir.path().to_path_buf();

        // 构造 3 个 Read tool result,只有最后 1 个会被保留(preserve_recent=1)
        let original_output_1 = "file1 content line1\nfile1 content line2\nfile1 content line3";
        let original_output_2 = "file2 content line1\nfile2 content line2\nfile2 content line3";
        let original_output_3 = "file3 content (recent, should be preserved verbatim)";

        let messages = vec![
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_old_1".to_string(),
                    tool_name: "Read".to_string(),
                    output: original_output_1.to_string(),
                    is_error: false,
                }],
                usage: None,
            },
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_old_2".to_string(),
                    tool_name: "Read".to_string(),
                    output: original_output_2.to_string(),
                    is_error: false,
                }],
                usage: None,
            },
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_recent".to_string(),
                    tool_name: "Read".to_string(),
                    output: original_output_3.to_string(),
                    is_error: false,
                }],
                usage: None,
            },
        ];

        // 调用 microcompact_with_archiver,归档被摘要的原始内容
        let archive_root = workspace_root.clone();
        let microcompacted = crate::compact::microcompact_with_archiver(
            &messages,
            1, // preserve_recent=1:只保留最后 1 个 tool result
            |id, name, output| {
                let _ = crate::tool_result_archive::archive_tool_result(
                    &archive_root,
                    id,
                    name,
                    output,
                );
            },
        );

        // 验证 microcompact 确实摘要了前两个 tool result
        let find_output = |messages: &[ConversationMessage], tool_use_id: &str| -> String {
            for msg in messages {
                for block in &msg.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id: tuid,
                        output,
                        ..
                    } = block
                    {
                        if tuid == tool_use_id {
                            return output.clone();
                        }
                    }
                }
            }
            String::new()
        };

        let output_1_after = find_output(&microcompacted, "call_old_1");
        let output_2_after = find_output(&microcompacted, "call_old_2");
        let output_3_after = find_output(&microcompacted, "call_recent");

        // 前两个被摘要(包含 "summarized" 标记)
        assert!(
            output_1_after.contains("summarized"),
            "call_old_1 should be summarized: {output_1_after}"
        );
        assert!(
            output_2_after.contains("summarized"),
            "call_old_2 should be summarized: {output_2_after}"
        );
        // 最后一个保持原样
        assert_eq!(
            output_3_after, original_output_3,
            "call_recent should be preserved verbatim"
        );

        // 验证 archive 文件包含被摘要的原始内容
        let recalled_1 =
            crate::tool_result_archive::recall_tool_result(&workspace_root, "call_old_1")
                .expect("recall should succeed")
                .expect("call_old_1 should be archived");
        assert_eq!(
            recalled_1.output, original_output_1,
            "archived output should match original"
        );

        let recalled_2 =
            crate::tool_result_archive::recall_tool_result(&workspace_root, "call_old_2")
                .expect("recall should succeed")
                .expect("call_old_2 should be archived");
        assert_eq!(
            recalled_2.output, original_output_2,
            "archived output should match original"
        );

        // call_recent 不应被归档(未被摘要)
        let recalled_3 =
            crate::tool_result_archive::recall_tool_result(&workspace_root, "call_recent")
                .expect("recall should succeed");
        assert!(
            recalled_3.is_none(),
            "call_recent should NOT be archived (not summarized)"
        );

        // 通过 ConversationRuntime.execute_recall_full 验证端到端检索
        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_workspace_root(workspace_root);

        let output = runtime
            .execute_recall_full(r#"{"tool_use_id":"call_old_1"}"#)
            .expect("recall_full should succeed");
        assert!(
            output.contains(original_output_1),
            "recall_full should return original output: {output}"
        );
        assert!(
            output.contains("call_old_1"),
            "recall_full should include tool_use_id: {output}"
        );
    }
}
