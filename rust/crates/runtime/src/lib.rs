//! Core runtime primitives for the `claw` CLI and supporting crates.
//!
//! This crate owns session persistence, permission evaluation, prompt assembly,
//! MCP plumbing, tool-facing file operations, and the core conversation loop
//! that drives interactive and one-shot turns.

mod approval_tokens;
mod bash;
pub mod bash_validation;
pub mod bg;
mod bootstrap;
pub mod branch_lock;
mod compact;
mod config;
pub mod config_validate;
mod conversation;
mod file_ops;
pub mod g004_conformance;
pub mod goal;
mod git_context;
pub mod green_contract;
mod hooks;
pub mod history_search;
mod json;
mod lane_events;
pub mod lsp_client;
mod mcp;
mod mcp_client;
pub mod mcp_lifecycle_hardened;
pub mod mcp_server;
mod mcp_stdio;
pub mod mcp_tool_bridge;
pub mod memory;
pub mod memory_store;
// Harness C(上下文管理)层:Memory 语义检索(L1/L2/L3 三级层级 + keyword fallback)。
// 详见 docs/harness-engineering-optimization-plan.md Step 2.4。
pub mod memory_semantic;
mod oauth;
pub mod permission_enforcer;
mod permissions;
pub mod plugin_lifecycle;
mod policy_engine;
pub mod poor_mode;
mod prompt;
// Harness L(生命周期)层接入:在 run_turn 失败分支提供"最多 1 次自动恢复后升级"机制。
// 详见 docs/harness-engineering-optimization-plan.md Step 1.2。
pub mod recovery_orchestrator;
pub mod recovery_recipes;
// Harness O(编排)层 + V(验证)层:Plan/Execute/Review 三段循环。
// 默认不启用,需通过 CLI `--enable-plan-mode` 或 settings.json `planMode: true` 开启。
// 缓存保护:PlanArtifact 末尾追加到 prompt 变动区,详见 docs/harness-engineering-optimization-plan.md §5.2。
pub mod planner;
// Harness C(上下文管理)层:统一 prompt 注入优先级栈,缓存保护(固定顺序,运行时不可变)。
// 详见 docs/harness-engineering-optimization-plan.md Step 2.3。
pub mod context_assembler;
// Harness O(可观测性)层:LoopDetectionMiddleware 打断 Doom Loop(同文件 10+ 次编辑)。
// 详见 docs/harness-engineering-optimization-plan.md Step 2.2。
pub mod loop_detection;
// Harness V(验证)层:VerifierAgent 规则/视觉/模型当裁判三种验证反馈。
// 详见 docs/harness-engineering-optimization-plan.md Step 3.1。
// 子 agent 走独立 LLM 请求 + 独立 prompt cache,不污染主 agent 缓存(§5.2)。
pub mod verifier;
// Harness M(多 agent)层:MultiAgentCoordinator — Fork/Teammate/Worktree 三模式。
// 详见 docs/harness-engineering-optimization-plan.md Step 3.2。
// 每个子 agent 走独立 LLM 请求 + 独立 prompt cache,不污染主 agent 缓存(§5.2)。
pub mod multi_agent;
// Harness O(可观测性)层:TraceAnalyzer 加载/导出 CSV、计算 turn latency /
// tool call count / compact 触发率直方图,简单失败聚类。
// 详见 docs/harness-engineering-optimization-plan.md Step 3.3。
// 阶段 4 将在此之上接入 K-means + Self-Improving Harness 闭环。
pub mod trace_analyzer;
mod remote;
pub mod repomap;
mod report_schema;
pub mod sandbox;
mod session;
pub mod session_control;
pub use session_control::SessionStore;
mod sse;
pub mod stale_base;
pub mod stale_branch;
pub mod summary_compression;
pub mod task_packet;
pub mod task_registry;
pub mod team_cron_registry;
// 生产构建解锁:之前 #[cfg(test)] 导致信任层仅在测试可用,违反 Harness G(治理)层要求。
// 详见 docs/harness-engineering-optimization-plan.md Step 1.1。
pub mod trust_resolver;
mod usage;
pub mod worker_boot;

pub use approval_tokens::{
    ApprovalDelegationHop, ApprovalScope, ApprovalTokenAudit, ApprovalTokenError,
    ApprovalTokenGrant, ApprovalTokenLedger, ApprovalTokenStatus,
};
pub use bash::{execute_bash, BashCommandInput, BashCommandOutput};
pub use bg::{bg_dir, BgError, BgRecord, BgStatus};
pub use bootstrap::{BootstrapPhase, BootstrapPlan};
pub use branch_lock::{detect_branch_lock_collisions, BranchLockCollision, BranchLockIntent};
pub use compact::{
    compact_session, estimate_session_tokens, format_compact_summary,
    get_compact_continuation_message, should_compact, CompactionConfig, CompactionResult,
};
pub use config::{
    ConfigEntry, ConfigError, ConfigLoader, ConfigSource, McpConfigCollection,
    McpManagedProxyServerConfig, McpOAuthConfig, McpRemoteServerConfig, McpSdkServerConfig,
    McpServerConfig, McpStdioServerConfig, McpTransport, McpWebSocketServerConfig, OAuthConfig,
    ProviderFallbackConfig, ResolvedPermissionMode, RuntimeConfig, RuntimeFeatureConfig,
    RuntimeHookConfig, RuntimePermissionRuleConfig, RuntimePluginConfig, ScopedMcpServerConfig,
    CLAW_SETTINGS_SCHEMA_NAME,
};
pub use config_validate::{
    check_unsupported_format, format_diagnostics, validate_config_file, ConfigDiagnostic,
    DiagnosticKind, ValidationResult,
};
pub use conversation::{
    auto_compaction_threshold_from_env, ApiClient, ApiRequest, AssistantEvent, AutoCompactionEvent,
    ConversationRuntime, PromptCacheEvent, RuntimeError, StaticToolExecutor, ToolError,
    ToolExecutor, TurnSummary,
};
pub use file_ops::{
    edit_file, edit_file_in_workspace, edit_file_in_workspace_with_roots, glob_search,
    glob_search_in_workspace, glob_search_in_workspace_with_roots, grep_search,
    grep_search_in_workspace, grep_search_in_workspace_with_roots, read_file,
    read_file_in_workspace, read_file_in_workspace_with_roots, strip_verbatim_prefix,
    write_file, write_file_in_workspace, write_file_in_workspace_with_roots, EditFileOutput,
    GlobSearchOutput, GrepSearchInput, GrepSearchOutput, ReadFileOutput, StructuredPatchHunk,
    TextFilePayload, WorkspacePathScope, WriteFileOutput,
};
pub use git_context::{GitCommitEntry, GitContext};
pub use goal::{goal_json_path, Goal, GoalError, GoalManager, GoalState};
pub use history_search::{HistoryHit, HistoryIndex, HistoryIndexError};
pub use hooks::{
    HookAbortSignal, HookEvent, HookProgressEvent, HookProgressReporter, HookRunResult, HookRunner,
};
pub use lane_events::{
    compute_event_fingerprint, dedupe_superseded_commit_events, dedupe_terminal_events,
    is_terminal_event, BlockedSubphase, EventProvenance, LaneCommitProvenance, LaneEvent,
    LaneEventBlocker, LaneEventBuilder, LaneEventMetadata, LaneEventName, LaneEventStatus,
    LaneFailureClass, LaneOwnership, SessionIdentity, ShipMergeMethod, ShipProvenance,
    WatcherAction,
};
pub use mcp::{
    mcp_server_signature, mcp_tool_name, mcp_tool_prefix, normalize_name_for_mcp,
    scoped_mcp_config_hash, unwrap_ccr_proxy_url,
};
pub use mcp_client::{
    McpClientAuth, McpClientBootstrap, McpClientTransport, McpManagedProxyTransport,
    McpRemoteTransport, McpSdkTransport, McpStdioTransport,
};
pub use mcp_lifecycle_hardened::{
    McpDegradedReport, McpErrorSurface, McpFailedServer, McpLifecyclePhase, McpLifecycleState,
    McpLifecycleValidator, McpPhaseResult,
};
pub use mcp_server::{McpServer, McpServerSpec, ToolCallHandler, MCP_SERVER_PROTOCOL_VERSION};
pub use memory::{
    detect_conflicts, extract_nudge_actions, should_nudge, MemoryBlock, MemoryEntry,
    NudgeAction, NudgeConfig, PersistentMemory, UNVERIFIED_THRESHOLD_MS,
};
pub use memory_store::MemoryStore;
pub use mcp_stdio::{
    spawn_mcp_stdio_process, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    ManagedMcpTool, McpDiscoveryFailure, McpInitializeClientInfo, McpInitializeParams,
    McpInitializeResult, McpInitializeServerInfo, McpListResourcesParams, McpListResourcesResult,
    McpListToolsParams, McpListToolsResult, McpReadResourceParams, McpReadResourceResult,
    McpResource, McpResourceContents, McpServerManager, McpServerManagerError, McpStdioProcess,
    McpTool, McpToolCallContent, McpToolCallParams, McpToolCallResult, McpToolDiscoveryReport,
    UnsupportedMcpServer,
};
pub use oauth::{
    clear_oauth_credentials, code_challenge_s256, credentials_path, generate_pkce_pair,
    generate_state, load_oauth_credentials, loopback_redirect_uri, parse_oauth_callback_query,
    parse_oauth_callback_request_target, save_oauth_credentials, OAuthAuthorizationRequest,
    OAuthCallbackParams, OAuthRefreshRequest, OAuthTokenExchangeRequest, OAuthTokenSet,
    PkceChallengeMethod, PkceCodePair,
};
pub use permissions::{
    PermissionContext, PermissionMode, PermissionOutcome, PermissionOverride, PermissionPolicy,
    PermissionPromptDecision, PermissionPrompter, PermissionRequest,
};
pub use plugin_lifecycle::{
    DegradedMode, DiscoveryResult, PluginHealthcheck, PluginLifecycle, PluginLifecycleEvent,
    PluginState, ResourceInfo, ServerHealth, ServerStatus, ToolInfo,
};
pub use policy_engine::{
    evaluate, evaluate_with_events, ApprovalToken, DiffScope, GreenLevel, LaneBlocker, LaneContext,
    PolicyAction, PolicyCondition, PolicyDecisionEvent, PolicyDecisionKind, PolicyEngine,
    PolicyEvaluation, PolicyRule, ReconcileReason, ReviewStatus,
};
pub use prompt::{
    load_system_prompt, load_system_prompt_with_extras, prepend_bullets, ContextFile,
    ModelFamilyIdentity, ProjectContext, PromptBuildError, SystemPromptBuilder,
    SystemPromptExtras, SystemPromptSplit, FRONTIER_MODEL_NAME,
    SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
pub use repomap::RepoMap;
pub use recovery_orchestrator::{RecoveryOrchestrator, RecoveryOutcome};
pub use recovery_recipes::{
    attempt_recovery, recipe_for, EscalationPolicy, FailureScenario, RecoveryAttemptState,
    RecoveryAttemptType, RecoveryCommandResult, RecoveryContext, RecoveryEvent,
    RecoveryLedgerEntry, RecoveryRecipe, RecoveryResult, RecoveryStatusReport, RecoveryStep,
};
pub use remote::{
    inherited_upstream_proxy_env, no_proxy_list, read_token, upstream_proxy_ws_url,
    RemoteSessionContext, UpstreamProxyBootstrap, UpstreamProxyState, DEFAULT_REMOTE_BASE_URL,
    DEFAULT_SESSION_TOKEN_PATH, DEFAULT_SYSTEM_CA_BUNDLE, NO_PROXY_HOSTS, UPSTREAM_PROXY_ENV_KEYS,
};
pub use report_schema::{
    canonicalize_report, project_report, report_content_hash, report_schema_v1_registry,
    CanonicalReportV1, ClaimKind, ConsumerCapabilities, FieldDelta, FieldDeltaState,
    NegativeEvidence, NegativeFindingStatus, ProjectionProvenance, RedactionProvenance,
    ReportClaim, ReportConfidence, ReportIdentity, ReportProjectionV1, ReportSchemaField,
    ReportSchemaRegistry, SensitivityClass, DEFAULT_PROJECTION_POLICY_V1, REPORT_SCHEMA_V1,
};
pub use sandbox::{
    build_linux_sandbox_command, detect_container_environment, detect_container_environment_from,
    resolve_sandbox_status, resolve_sandbox_status_for_request, ContainerEnvironment,
    FilesystemIsolationMode, LinuxSandboxCommand, SandboxConfig, SandboxDetectionInputs,
    SandboxRequest, SandboxStatus,
};
pub use session::{
    ContentBlock, ConversationMessage, MessageRole, Session, SessionCompaction, SessionError,
    SessionFork, SessionHeartbeat, SessionLiveness, SessionPromptEntry,
};
pub use sse::{IncrementalSseParser, SseEvent};
pub use stale_base::{
    check_base_commit, format_stale_base_warning, read_claw_base_file, resolve_expected_base,
    BaseCommitSource, BaseCommitState,
};
pub use stale_branch::{
    apply_policy, check_freshness, BranchFreshness, StaleBranchAction, StaleBranchEvent,
    StaleBranchPolicy,
};
pub use task_packet::{
    validate_packet, TaskPacket, TaskPacketValidationError, TaskResource, ValidatedPacket,
};
pub use task_registry::{LaneBoard, LaneBoardEntry, LaneFreshness, LaneHeartbeat};
// 生产构建解锁:见 L59 模块注释。补齐 TrustAllowlistEntry/TrustResolution/
// detect_trust_prompt,让 worker_boot 的 TrustGate 分支可在生产构建接入信任层。
pub use trust_resolver::{
    detect_trust_prompt, TrustAllowlistEntry, TrustConfig, TrustDecision, TrustEvent,
    TrustPolicy, TrustResolution, TrustResolver,
};
pub use usage::{
    format_usd, pricing_for_model, ModelPricing, TokenUsage, UsageCostEstimate, UsageTracker,
};
pub use worker_boot::{
    probe_mcp_health, probe_transport_health, StartupHealthSummary,
    Worker, WorkerEvent, WorkerEventKind, WorkerEventPayload, WorkerFailure, WorkerFailureKind,
    WorkerPromptTarget, WorkerReadySnapshot, WorkerRegistry, WorkerStatus, WorkerTrustResolution,
};
pub use loop_detection::{
    LoopAction, LoopDetector, ABORT_THRESHOLD, MCP_TOOLS_MAX, SKILLS_MAX, WARN_THRESHOLD,
};
pub use trace_analyzer::{
    FailureCluster, TraceAnalyzer, TraceRecord, TraceStats, CSV_HEADER as TRACE_CSV_HEADER,
    MAX_SAMPLE_ERRORS_PER_CLUSTER,
};
pub use verifier::{
    ModelJudgeVerifier, ModelJudgeVerdict, RuleVerifier, RuleVerdict, VerificationMethod,
    VerificationResult, VerifierAgent, VisualVerifier, VisualVerdict,
};
pub use multi_agent::{
    CoordinationMode, JoinStats, MultiAgentCoordinator, Subagent, SubagentStatus,
};

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
