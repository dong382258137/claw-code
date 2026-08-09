//! Core runtime primitives for the `claw` CLI and supporting crates.
//!
//! This crate owns session persistence, permission evaluation, prompt assembly,
//! MCP plumbing, tool-facing file operations, and the core conversation loop
//! that drives interactive and one-shot turns.

mod approval_tokens;
pub mod bash;
pub mod bash_validation;
pub mod bg;
mod bootstrap;
pub mod branch_lock;
pub mod cache_alignment;
pub mod compact;
mod config;
pub mod config_validate;
pub mod content_classifier;
pub mod content_compression;
mod conversation;
// Multi-Agent Hardening §4.1:统一诊断基础设施(panic hook + DiagLog)。
// 提取自 rusty-claude-cli/src/lib.rs main_entry 内联闭包,供 main/headless/测试入口复用。
pub mod decision_log;
pub mod diag;
mod file_ops;
pub mod g004_conformance;
mod git_context;
pub mod goal;
pub mod green_contract;
pub mod history_search;
mod hooks;
mod json;
pub mod knowledge_freshness;
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
// Harness C(上下文管理)层:NOTEBOOK — Structured Note-taking(Anthropic 推荐)。
// 跨压缩持久化的工作记忆,LLM 通过 `notebook_update` 工具主动维护。
// 详见 docs/harness-engineering-optimization-plan.md P0-1。
pub mod notebook;
// P0:ToolResultArchive — microcompact 真无损化的 Layer 3 持久存储。
// 在 microcompact 摘要前归档原始 tool result,LLM 可通过 `recall_full` 工具
// 按 `tool_use_id` 主动检索原始内容。直击"AI 忘记原始 tool output 导致重复调用"
// 的问题。详见 PiAgent 借鉴分析 P0 方案。
mod oauth;
pub mod permission_enforcer;
mod permissions;
pub mod plugin_lifecycle;
mod policy_engine;
pub mod poor_mode;
mod prompt;
pub mod tool_result_archive;
// Harness L(生命周期)层接入:在 run_turn 失败分支提供"最多 1 次自动恢复后升级"机制。
// 详见 docs/harness-engineering-optimization-plan.md Step 1.2。
// Harness O(编排)层 + V(验证)层:Plan/Execute/Review 三段循环。
// 默认不启用,需通过 CLI `--enable-plan-mode` 或 settings.json `planMode: true` 开启。
// 缓存保护:PlanArtifact 末尾追加到 prompt 变动区,详见
// docs/harness-engineering-optimization-plan.md §5.2。
pub mod project_topology;
pub mod recovery_orchestrator;
pub mod recovery_recipes;
pub mod vcs_snapshot;
// Phase 4-B:DomainTools — 算法级重构建议(建议模式)+ 基准对比。无状态。
pub mod domain_algorithm;
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
// Harness V(验证)层:SlopScanner 内置幻觉/偷懒信号扫描,write_file/edit_file 后
// 扫产物占位标记(unimplemented!/placeholder/TODO),warning 不阻断。
// 仿照 LoopDetectionMiddleware 模式,纯文本扫描不调 LLM,不影响 prompt cache。
pub mod slop_scanner;
// Harness V(验证)层:P3 完成声明校验。LLM 声称"完成"且本轮无工具调用时,
// 自动执行项目验证命令(cargo check 等),失败注入 remediation。
// 四条件严格 gating + 30s 超时,纯子进程执行不调 LLM,不影响 prompt cache。
pub mod completion_verifier;
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
mod remote;
pub mod repomap;
mod report_schema;
pub mod sandbox;
mod session;
pub mod session_control;
pub mod trace_analyzer;
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
    get_compact_continuation_message, microcompact, microcompact_with_archiver, should_compact,
    CompactionConfig, CompactionResult,
};
pub use config::{
    bootstrapped_sentinel_path, default_config_home, is_bootstrapped, load_wizard_settings,
    mark_bootstrapped, save_wizard_settings, ConfigEntry, ConfigError, ConfigLoader, ConfigSource,
    LspConfigCollection, LspServerConfig, McpConfigCollection, McpManagedProxyServerConfig,
    McpOAuthConfig, McpRemoteServerConfig, McpSdkServerConfig, McpServerConfig,
    McpStdioServerConfig, McpTransport, McpWebSocketServerConfig, OAuthConfig,
    ProviderFallbackConfig, ResolvedPermissionMode, RuntimeConfig, RuntimeFeatureConfig,
    RuntimeHookConfig, RuntimePermissionRuleConfig, RuntimePluginConfig, ScopedMcpServerConfig,
    WizardSettings, CLAW_SETTINGS_SCHEMA_NAME,
};
pub use config_validate::{
    check_unsupported_format, format_diagnostics, validate_config_file, ConfigDiagnostic,
    DiagnosticKind, ValidationResult,
};
pub use context_assembler::{
    AssembledPrompt, CacheStrategy, ContextAssembler, ContextBlock, ContextSource, TokenBudget,
};
pub use decision_log::{
    compute_simhash, hamming_distance, is_decision_extractor_client_registered,
    set_global_decision_extractor_client, DecisionExtractorClient, DecisionLog, DecisionLogError,
    DecisionVerification, DetectionStrategy,
};
pub use vcs_snapshot::{RefactorTransaction, TransactionStatus, VcsError};

pub use conversation::{
    auto_compaction_threshold_from_env, auto_compaction_threshold_from_env_opt, ApiClient,
    ApiRequest, AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent,
    RequestKind, RuntimeError, StaticToolExecutor, ToolError, ToolExecutor, TurnSummary,
};
pub use file_ops::{
    edit_file, edit_file_in_workspace, edit_file_in_workspace_with_roots, glob_search,
    glob_search_in_workspace, glob_search_in_workspace_with_roots, grep_search,
    grep_search_in_workspace, grep_search_in_workspace_with_roots, read_file,
    read_file_in_workspace, read_file_in_workspace_with_roots, replace_lines,
    replace_lines_in_workspace_with_roots, run_cargo_check_for_file, strip_verbatim_prefix,
    write_file, write_file_in_workspace, write_file_in_workspace_with_roots, EditFileOutput,
    GlobSearchOutput, GrepSearchInput, GrepSearchOutput, ReadFileOutput, ReplaceLinesOutput,
    StructuredPatchHunk, TextFilePayload, WorkspacePathScope, WriteFileOutput,
};
pub use git_context::{GitCommitEntry, GitContext};
pub use goal::{goal_json_path, Goal, GoalError, GoalManager, GoalState};
pub use history_search::{HistoryHit, HistoryIndex, HistoryIndexError};
pub use hooks::{
    HookAbortSignal, HookEvent, HookProgressEvent, HookProgressReporter, HookRunResult, HookRunner,
};
pub use lane_events::{
    compute_event_fingerprint, dedupe_superseded_commit_events, dedupe_terminal_events,
    drain_lane_events, is_terminal_event, try_publish, BlockedSubphase, EventProvenance,
    LaneCommitProvenance, LaneEvent, LaneEventBlocker, LaneEventBuilder, LaneEventMetadata,
    LaneEventName, LaneEventStatus, LaneFailureClass, LaneOwnership, SessionIdentity,
    ShipMergeMethod, ShipProvenance, WatcherAction,
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
    detect_conflicts, extract_nudge_actions, should_nudge, MemoryBlock, MemoryEntry, NudgeAction,
    NudgeConfig, PersistentMemory, UNVERIFIED_THRESHOLD_MS,
};
pub use memory_store::MemoryStore;
// Step 2.4: Memory 语义检索层 embedding provider 公开 API。
pub use mcp_stdio::{
    spawn_mcp_stdio_process, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    ManagedMcpTool, McpDiscoveryFailure, McpInitializeClientInfo, McpInitializeParams,
    McpInitializeResult, McpInitializeServerInfo, McpListResourcesParams, McpListResourcesResult,
    McpListToolsParams, McpListToolsResult, McpReadResourceParams, McpReadResourceResult,
    McpResource, McpResourceContents, McpServerManager, McpServerManagerError, McpStdioProcess,
    McpTool, McpToolCallContent, McpToolCallParams, McpToolCallResult, McpToolDiscoveryReport,
    UnsupportedMcpServer,
};
#[cfg(feature = "embedding")]
pub use memory_semantic::FastembedProvider;
pub use memory_semantic::{
    cosine_similarity, EmbeddingError, EmbeddingProvider, HashEmbeddingProvider,
};
pub use notebook::{
    execute_notebook_update, Notebook, NotebookError, NotebookUpdateInput, NOTEBOOK_FILENAME,
    NOTEBOOK_HEADER, NOTEBOOK_MAX_CHARS, NOTEBOOK_UPDATE_TOOL_SPEC, SECTION_TAGS,
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
// Epic 5:mcp_tool_bridge 类型顶层 re-export(模块私有,但类型需被 doctor smoke test 消费)。
pub use mcp_tool_bridge::{
    McpConnectionStatus, McpResourceInfo, McpServerState, McpToolInfo, McpToolRegistry,
};
pub use policy_engine::{
    evaluate, evaluate_with_events, ApprovalToken, DiffScope, GreenLevel, LaneBlocker, LaneContext,
    PolicyAction, PolicyCondition, PolicyDecisionEvent, PolicyDecisionKind, PolicyEngine,
    PolicyEvaluation, PolicyRule, ReconcileReason, ReviewStatus,
};
pub use prompt::{
    load_system_prompt, load_system_prompt_with_extras, prepend_bullets, ContextFile,
    ModelFamilyIdentity, ProjectContext, PromptBuildError, SystemPromptBuilder, SystemPromptExtras,
    SystemPromptSplit, FRONTIER_MODEL_NAME, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
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
pub use repomap::RepoMap;
pub use report_schema::{
    canonicalize_report, project_report, report_content_hash, report_schema_v1_registry,
    CanonicalReportV1, ClaimKind, ConsumerCapabilities, FieldDelta, FieldDeltaState,
    NegativeEvidence, NegativeFindingStatus, ProjectionProvenance, RedactionProvenance,
    ReportClaim, ReportConfidence, ReportIdentity, ReportProjectionV1, ReportSchemaField,
    ReportSchemaRegistry, SensitivityClass, DEFAULT_PROJECTION_POLICY_V1, REPORT_SCHEMA_V1,
};
pub use sandbox::{
    build_linux_sandbox_command,
    detect_container_environment,
    detect_container_environment_from,
    platform_sandbox_builder,
    platform_sandbox_supported,
    resolve_sandbox_status,
    resolve_sandbox_status_for_request,
    ContainerEnvironment,
    FilesystemIsolationMode,
    LinuxSandboxBuilder,
    LinuxSandboxCommand,
    // Step 4.1:SandboxBuilder trait + 三平台实现 + 工厂函数 + 常量
    MacOsSandboxBuilder,
    SandboxBuilder,
    SandboxCommand,
    SandboxConfig,
    SandboxDetectionInputs,
    SandboxRequest,
    SandboxStatus,
    WindowsSandboxBuilder,
    CREATE_NEW_PROCESS_GROUP,
    CREATE_NO_WINDOW,
    DETACHED_PROCESS,
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
pub use task_registry::{
    LaneBoard, LaneBoardEntry, LaneFreshness, LaneHeartbeat, Task, TaskRegistry, TaskStatus,
};
// Epic 6:team_cron_registry 类型顶层 re-export(pub mod,但类型需被 doctor smoke test 消费)。
pub use team_cron_registry::{CronEntry, CronRegistry, Team, TeamRegistry, TeamStatus};
pub use tool_result_archive::{
    archive_path, archive_tool_result, list_archived_summary, prune_archive, recall_by_tool_name,
    recall_tool_result, record_count, ArchiveError, ArchivedToolResult, ARCHIVE_FILENAME,
    ARCHIVE_MAX_CHARS, ARCHIVE_RECORD_MAX_CHARS,
};
// 生产构建解锁:见 L59 模块注释。补齐 TrustAllowlistEntry/TrustResolution/
// detect_trust_prompt,让 worker_boot 的 TrustGate 分支可在生产构建接入信任层。
pub use loop_detection::{
    LoopAction, LoopDetector, ABORT_THRESHOLD, MCP_TOOLS_MAX, SAME_OUTPUT_ABORT_THRESHOLD,
    SAME_OUTPUT_WARN_THRESHOLD, SKILLS_MAX, TOOL_ABORT_THRESHOLD, TOOL_WARN_THRESHOLD,
    WARN_THRESHOLD,
};
pub use multi_agent::{
    CoordinationMode, JoinStats, MultiAgentCoordinator, Subagent, SubagentStatus,
};
// v0.2 DAG 多 agent 编排基础设施:petgraph 图封装 + async scheduler + executor trait。
pub use multi_agent::{
    DagError, DagGraph, DagId, DagScheduler, NodeError, NodeResult, RetryPolicy, SubagentExecutor,
    DEFAULT_MAX_PARALLELISM,
};
pub use trace_analyzer::{
    FailureCluster, TraceAnalyzer, TraceRecord, TraceStats, CSV_HEADER as TRACE_CSV_HEADER,
    MAX_SAMPLE_ERRORS_PER_CLUSTER,
};
pub use trust_resolver::{
    detect_trust_prompt, TrustAllowlistEntry, TrustConfig, TrustDecision, TrustEvent, TrustPolicy,
    TrustResolution, TrustResolver,
};
pub use usage::{
    format_cost_localized, format_usd, pricing_for_model, ModelPricing, TokenUsage,
    UsageCostEstimate, UsageTracker, CNY_TO_USD_RATE,
};
pub use verifier::{RuleVerdict, RuleVerifier, VerificationResult, VerifierAgent};
pub use worker_boot::{
    probe_mcp_health, probe_transport_health, StartupHealthSummary, Worker, WorkerEvent,
    WorkerEventKind, WorkerEventPayload, WorkerFailure, WorkerFailureKind, WorkerPromptTarget,
    WorkerReadySnapshot, WorkerRegistry, WorkerStatus, WorkerTrustResolution,
};

// ── Embedding runtime factory ──
// Step 4.x: 将 embedding provider 的创建集中在一个工厂函数中,供 PersistentMemory、
// TraceAnalyzer 等消费者注入。feature `embedding` 开启时优先使用 FastembedProvider
// (BGE-small-en-v1.5,384 维),创建失败则自动降级到 HashEmbeddingProvider。

/// 根据编译 feature 创建 embedding provider。
///
/// - `feature = "embedding"` 开启且 FastembedProvider 初始化成功:返回 BGE-small 384 维。
/// - `feature = "embedding"` 开启但初始化失败(如模型下载失败):自动降级为 HashEmbeddingProvider。
/// - `feature = "embedding"` 未开启:返回 None(调用方应使用 keyword fallback)。
///
/// 返回 `None` 不表示错误,调用方应检测并退化为关键词匹配。
#[must_use]
pub fn build_embedding_provider() -> Option<Box<dyn EmbeddingProvider + Send + Sync>> {
    #[cfg(feature = "embedding")]
    {
        match memory_semantic::fastembed_provider::FastembedProvider::try_new() {
            Ok(provider) => {
                eprintln!(
                    "embedding provider: fastembed ({}-dim BGE-small-en-v1.5)",
                    provider.dim()
                );
                Some(Box::new(provider))
            }
            Err(e) => {
                eprintln!(
                    "fastembed init failed ({}), falling back to hash embedding",
                    e
                );
                Some(Box::new(HashEmbeddingProvider::default_dim()))
            }
        }
    }
    #[cfg(not(feature = "embedding"))]
    {
        // 未编译 embedding feature:不提供 provider,调用方走 keyword fallback。
        None
    }
}

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
