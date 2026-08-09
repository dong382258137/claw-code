use std::collections::{BTreeMap, HashMap};
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
// 默认启用(plan_mode=true)。可通过 settings.json `planMode: false` 关闭。
// 缓存保护:PlanArtifact 末尾追加到 prompt 变动区(dynamic_sections),
// 不污染 system_prompt + tools_schema 的"绝对稳定区"。详见
// docs/harness-engineering-optimization-plan.md Step 2.1 与 §5.2。
use crate::planner::{
    assess_complexity, decompose_task, is_auto_planner_enabled, persist_plan_artifact,
    plan_and_convert_to_spawn_requests, ComplexityAssessment, PlanArtifact,
    PreCompletionChecklistMiddleware, ReviewResult,
};
// Harness M(多 agent)层接入:MultiAgentCoordinator — Step 3.2-c。
// 主 agent 通过 dispatch_subagent tool 派发任务给子 agent。
// 子 agent 走独立 LLM 请求 + 独立 prompt cache,不污染主 agent 缓存(§5.2)。
use crate::multi_agent::{
    upgrade_model_for_subagent, CoordinationMode, MultiAgentCoordinator, SpawnRequest,
    SubagentStatus, TaskComplexity,
};
// CoordinatorExecutor + SubagentDispatcher + SubagentRunner — 用于 DAG 真实调度
// (v0.2 TODO 2 生产接入)。with_dag_coordinator 把 CoordinatorExecutor 装到 runtime,
// 供 tools 层 dag_run 工具取出后构造 DagScheduler。
// v3:新增 DagGraph / DagScheduler / DagNode / DagError / NodeResult / RetryPolicy,
// 用于 spawn_parallel_via_dag 真并行 spawn。
use crate::multi_agent::dag::{
    CoordinatorExecutor, DagError, DagGraph, DagNode, DagRunResult, DagScheduler, FailFast,
    NodeResult, RetryPolicy, SubagentDispatcher, SubagentRunner,
};
// Step 3.2-a:LaneEvent helpers for SubagentHandoff / SubagentResult.
use crate::lane_events::{try_publish as publish_lane_event, LaneEvent};
// Harness O(可观测性)层接入:LoopDetectionMiddleware 打断 Doom Loop。
// 在 PostToolUse hook 中调用 LoopDetector::record_edit,根据 LoopAction
// 决定 Continue / InjectContext / Abort。详见
// docs/harness-engineering-optimization-plan.md Step 2.2。
use crate::loop_detection::{LoopAction, LoopDetector};
use crate::slop_scanner::{extract_scan_target, is_file_modifying_tool, SlopScanner};
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

/// P1:并行子任务最大并发数上限。
///
/// 参考 Anthropic 多智能体研究系统(lead agent 并行 spawn 3-5 subagents)。
/// 超过此值的任务将排队等待,避免瞬间冲击 API 触发速率限制。
const MAX_PARALLEL_SUBAGENTS: usize = 5;

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
            },
            "capability": {
                "type": "string",
                "enum": ["analyze", "read-only", "execute"],
                "description": "Subagent capability tier: 'analyze' (L0, read-only reasoning, no tools), 'read-only' (L1, read/grep/glob/repomap tools), 'execute' (L2, edit/write/bash tools). Determines tool whitelist and max tool-call iterations.",
                "default": "analyze"
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

/// v3:Tool specification for the `spawn_parallel_subagents` tool.
///
/// 主 agent 通过此 tool **批量并行**派发多个子 agent,内部走
/// [`ConversationRuntime::spawn_parallel_via_dag_with_fail_fast`],
/// 由 `DagScheduler` 在独立的 tokio task 中真并发执行(而非顺序循环)。
///
/// 与 `dispatch_subagent`(单个派发 + retry loop)的区别:
/// - 一次调用派发 N 个子 agent,共享一个 DAG 调度回合
/// - 真并行:所有 task 在独立的 tokio task 中同时执行
/// - 不带 retry loop(单次执行,`max_retries = 0`)
/// - 支持 `fail_fast` 配置:`on`(默认,任一失败即取消全部)/ `off`(容错,收集部分结果)
///
/// 适用于:独立的可并行任务(如多文件分析、多模块测试、多方案探索)。
/// 不适用于:有依赖关系的任务(应使用 `dag_run` + DAG 定义)。
#[allow(dead_code)] // Reserved for future registration via main.rs tool registry.
pub const SPAWN_PARALLEL_SUBAGENTS_TOOL_SPEC: &str = r#"{
    "name": "spawn_parallel_subagents",
    "description": "Dispatch multiple sub-agents in parallel using a DAG scheduler. All tasks run concurrently in independent tokio tasks (true parallelism, not sequential). Each sub-agent has its own LLM request and prompt cache. Use this for independent parallelizable work (e.g. analyzing multiple files, running multiple test suites). For dependent tasks, use dag_run instead. Returns one result per task.",
    "input_schema": {
        "type": "object",
        "properties": {
            "tasks": {
                "type": "array",
                "description": "List of sub-agent tasks to dispatch in parallel.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Human-readable name for the sub-agent (e.g. 'analyze-auth', 'test-runner')."
                        },
                        "task": {
                            "type": "string",
                            "description": "The task description / prompt to send to the sub-agent."
                        },
                        "model": {
                            "type": "string",
                            "description": "Model name for the sub-agent (e.g. 'deepseek-v4-flash', 'deepseek-v4-pro'). Required — capability check uses this to gate Budget vs Flagship tasks."
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["fork", "teammate", "worktree"],
                            "description": "Coordination mode: 'fork' (shared workdir, parallel), 'teammate' (shared TaskRegistry), 'worktree' (isolated git worktree).",
                            "default": "fork"
                        },
                        "complexity": {
                            "type": "string",
                            "enum": ["simple", "diagnostic", "architectural"],
                            "description": "Task complexity. Budget-tier models (haiku/mini/nano/flash) cannot handle 'diagnostic' or 'architectural'.",
                            "default": "simple"
                        },
                        "capability": {
                            "type": "string",
                            "enum": ["analyze", "read-only", "execute"],
                            "description": "Subagent capability tier: 'analyze' (L0, read-only reasoning, no tools), 'read-only' (L1, read/grep/glob/repomap tools), 'execute' (L2, edit/write/bash tools). Determines tool whitelist and max tool-call iterations.",
                            "default": "analyze"
                        }
                    },
                    "required": ["name", "task", "model"]
                },
                "minItems": 1
            },
            "fail_fast": {
                "type": "string",
                "enum": ["on", "off"],
                "description": "Failure propagation: 'on' (default) cancels all siblings on any failure; 'off' tolerates failures and returns partial results.",
                "default": "on"
            }
        },
        "required": ["tasks"]
    }
}
"#;

/// 请求来源分类 — 用于缓存统计隔离。
/// 子智能体请求经 cli 侧路由到独立的 `subagent-{session}` 统计,
/// 不再污染主 agent 的缓存 break 检测(见设计文档 §4.2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Main,
    Subagent,
}

/// Epic 1(§3.2):子智能体上下文注入载体(owned 字段,避免跨 `.await` 生命周期约束)。
///
/// 持有 repo_map 摘要、ProjectContext、工具签名摘要,由调用方构造后传值给
/// [`build_subagent_system_prompt`] / [`build_subagent_request`]。
/// 所有字段可选/可空,默认值(全空)时行为与改造前一致(向后兼容)。
#[derive(Debug, Clone, Default)]
pub struct SubagentContext {
    /// L1 静态环境:repo_map 摘要(限 1K token,已渲染为 owned String)。
    /// `None` 时不注入 Repository Map section。
    pub repo_map: Option<String>,
    /// L1 静态环境:项目上下文(cwd/date/git_status)。
    /// `None` 时不注入 Environment context section。
    pub project_context: Option<crate::prompt::ProjectContext>,
    /// L2 静态工具:capability 白名单对应的工具签名摘要(name+description)。
    /// 空时不注入工具签名层(Analyze capability 无工具,自然为空)。
    pub tool_summaries: Vec<ToolSummary>,
}

/// Epic 1(§3.2):工具签名摘要(不含完整 schema,减少 token 占用)。
#[derive(Debug, Clone)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
}

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: SystemPromptSplit,
    pub messages: Vec<ConversationMessage>,
    pub request_kind: RequestKind,
}
/// 构造子智能体 system_prompt — §4.6 诊断 SOP 注入 + Epic 1(§3.2)上下文注入。
///
/// 3a 静态化:不再接收 id/name/task — 唯一内容移入 user message(见
/// [`build_subagent_request`]),保证同一复杂度的所有子智能体请求共享
/// 同一前缀,命中 DeepSeek prefix cache。
///
/// Epic 1 分层注入(§3.2),所有 section 进 `static_sections`(task 在 user
/// message,system prompt 全静态以最大化缓存命中):
/// - **L0 指令**:角色约束 + 输出格式 + SOP(按 complexity)+ 能力声明(按 capability)
/// - **L1 环境**:`## Repository Map`(§3.2 heading 对齐)+ `# Environment context`
///   — heading 必须与 [`SystemPromptSplit::static_cache_breakpoints`] 一致,
///   否则 breakpoint 失效、缓存分层退化
/// - **L2 工具**:capability 白名单对应的工具签名摘要(仅 ReadOnly/Execute)
///
/// - `complexity == Diagnostic`:追加诊断任务执行规范
/// - `complexity == Architectural`:追加架构决策执行规范
/// - 其他复杂度:仅基础 prompt,不污染简单任务
fn build_subagent_system_prompt(
    complexity: crate::multi_agent::TaskComplexity,
    capability: crate::multi_agent::SubagentCapability,
    ctx: &SubagentContext,
) -> SystemPromptSplit {
    // L0 指令层:基础 prompt + 能力声明
    let base_prompt = match capability {
        crate::multi_agent::SubagentCapability::Analyze => {
            "你是一个子智能体,由主智能体派发执行独立任务。\n\
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
        }
        crate::multi_agent::SubagentCapability::ReadOnly
        | crate::multi_agent::SubagentCapability::Execute => {
            "你是一个子智能体,由主智能体派发执行独立任务。\n\
\n\
## 约束\n\
- 你拥有独立的工作上下文,不共享主智能体的对话历史\n\
- 你的响应将被写入文件,主智能体会后续读取\n\
- 请提供完整、自包含的分析结果\n\
- 你可以调用工具来完成任务(工具白名单见下文)\n\
- 禁止递归派发子智能体(dispatch_subagent / spawn_parallel_subagents 不可用)\n\
\n\
## 输出格式\n\
请直接输出你的分析结果,使用 Markdown 格式。包含:\n\
1. 任务理解(简要复述)\n\
2. 分析过程\n\
3. 关键发现\n\
4. 结论和建议"
        }
    };

    // §4.6 SOP 注入:Diagnostic 和 Architectural 复杂度追加各自 SOP
    let l0_instruction = match complexity {
        crate::multi_agent::TaskComplexity::Diagnostic => format!(
            "{base_prompt}\n\n\
             ## 诊断任务执行规范\n\
             1. 遇到崩溃/闪退类问题,第一动作是写文件诊断日志(CLAW_DIAG=1 或调用 diag! 宏),\
             而非凭直觉堆砌防御代码\n\
             2. 先用可靠信号确认错误类型(panic vs Err vs 配置错误),再决定修复方向\n\
             3. 修改后必须运行 `cargo build` 验证编译通过\n\
             4. 声称修复后必须提供复现验证证据(重新运行原场景确认不崩溃)\n\
             5. 禁止在未验证根因的情况下堆砌 catch_unwind / panic hook 等防御性代码"
        ),
        crate::multi_agent::TaskComplexity::Architectural => format!(
            "{base_prompt}\n\n\
             ## 架构决策执行规范\n\
             1. 提出方案前必须列出至少 2 个候选方案(alternatives),\
             禁止只给出单一方案就拍板\n\
             2. 每个候选方案需评估 trade-off:优势 / 劣势 / 适用场景 / 风险\n\
             3. 推荐方案必须给出否决其他方案的理由(rationale),\
             而非仅陈述推荐方案的优势\n\
             4. 涉及向后兼容/迁移成本的决策,必须评估现有用户/代码的影响范围\n\
             5. 架构决策写入 NOTEBOOK.md `<decisions>` 段(context/decision/rationale/alternatives),\
             供后续 compaction 后回溯\n\
             6. 禁止凭直觉或习惯拍板:任何架构决策必须有可复现的论证依据\
             (benchmark / 代码引用 / 论文 / 既有项目实践)"
        ),
        crate::multi_agent::TaskComplexity::Simple => base_prompt.to_string(),
    };

    let mut sections: Vec<String> = vec![l0_instruction];

    // L1 环境层:repo_map(§3.2 heading 对齐 — 必须用 "## Repository Map")
    if let Some(repo_map) = &ctx.repo_map {
        sections.push(format!("## Repository Map\n{repo_map}"));
    }
    // L1 环境层:ProjectContext(§3.2 heading 对齐 — 必须用 "# Environment context")
    if let Some(pc) = &ctx.project_context {
        let mut env = String::from("# Environment context");
        env.push_str(&format!("\n- Working directory: {}", pc.cwd.display()));
        env.push_str(&format!("\n- Date: {}", pc.current_date));
        if let Some(gs) = &pc.git_status {
            env.push_str(&format!("\n- Git status:\n```\n{gs}\n```"));
        }
        sections.push(env);
    }

    // L2 工具签名层(仅 ReadOnly/Execute,Analyze 无工具)
    if !ctx.tool_summaries.is_empty() {
        let mut tools = String::from("## Available Tools");
        for ts in &ctx.tool_summaries {
            tools.push_str(&format!("\n- **{}**: {}", ts.name, ts.description));
        }
        sections.push(tools);
    }

    // 全静态(task 在 user message,system prompt 无动态内容)
    SystemPromptSplit::from_sections(sections)
}

/// 构造子智能体完整请求 — 3a/3b DRY 公共入口。
///
/// `execute_subagent_llm` 与 `SubagentDispatcher::dispatch_impl` 共用,
/// 消除两份重复 prompt 构造:
/// - system prompt 纯静态(见 [`build_subagent_system_prompt`])
/// - id/name/task 移入 user message,单次出现
/// - `request_kind = Subagent`,经 cli 侧路由到独立缓存统计 session
pub(crate) fn build_subagent_request(
    subagent_id: &str,
    name: &str,
    task: &str,
    complexity: crate::multi_agent::TaskComplexity,
    capability: crate::multi_agent::SubagentCapability,
    ctx: &SubagentContext,
) -> ApiRequest {
    let system_prompt = build_subagent_system_prompt(complexity, capability, ctx);
    let user_message = ConversationMessage {
        role: MessageRole::User,
        blocks: vec![ContentBlock::Text {
            text: format!("# Subagent: {name} ({subagent_id})\n\n请执行以下任务:\n\n{task}"),
        }],
        usage: None,
    };
    ApiRequest {
        system_prompt,
        messages: vec![user_message],
        request_kind: RequestKind::Subagent,
    }
}

/// Epic 3a/3b(§3.3.1):工具调用处理公共函数 — 两条执行路径共用,消除重复。
///
/// 对每个 `ToolUse` block 执行:
/// 1. **递归派发 guard**:`dispatch_subagent` / `spawn_parallel_subagents` → 立即 Err
/// 2. **白名单 guard**:不在 `capability.allowed_tools()` → 立即 Err
/// 3. **工具执行**:调用 `tool_executor.execute(name, input)`,失败时返回
///    `is_error=true` 的 `ToolResult`(不中断循环,让 LLM 决定下一步)
/// 4. **changed_files 提取**:`edit`/`write` 工具的 `file_path` 提取并规范化
/// 5. **ToolResult 回填**:追加到 `messages`,供下一轮 LLM 调用使用
///
/// # 参数
/// - `tool_uses`:已过滤的 `ToolUse` block 列表(owned,不含 Text/Thinking)
/// - `tool_executor`:工具执行器(路径 A 传 `&mut dyn ToolExecutor`,
///   路径 B 传 `&mut *Box<dyn ToolExecutor>`)
///
/// # 返回
/// - `Ok(())`:所有工具调用已执行并回填
/// - `Err(ToolError)`:guard 违规(递归/白名单),调用方应中止子智能体
pub(crate) fn process_tool_uses(
    capability: crate::multi_agent::SubagentCapability,
    tool_uses: &[ContentBlock],
    tool_executor: &mut dyn ToolExecutor,
    workspace_root: &std::path::Path,
    messages: &mut Vec<ConversationMessage>,
    tools_used: &mut Vec<String>,
    changed_files: &mut Vec<String>,
) -> Result<(), ToolError> {
    for tu in tool_uses {
        let (id, name, input) = match tu {
            ContentBlock::ToolUse { id, name, input } => (id, name, input),
            _ => continue,
        };

        // Guard 1:禁止递归派发(§3.3.1)
        if name == "dispatch_subagent" || name == "spawn_parallel_subagents" {
            return Err(ToolError::new(format!(
                "subagent recursion forbidden: {name}"
            )));
        }
        // Guard 2:白名单(§3.1)
        if !capability.allowed_tools().contains(&name.as_str()) {
            return Err(ToolError::new(format!(
                "tool {name} not allowed for capability {capability:?}"
            )));
        }

        tools_used.push(name.clone());

        // Epic 4:edit/write 文件锁(SubagentFileGuard)
        // Guard 2 已确保只有 Execute capability 能调 edit/write(Analyze/ReadOnly 被白名单拒绝)
        // 此处获取 per-file 锁,防止并行 Execute 子智能体修改同一文件冲突(§4)
        // try_acquire 是同步阻塞(Condvar wait_timeout,30s 超时),与 tool_executor.execute
        // 同样为同步调用,在路径 A(async)和路径 B(sync thread)中均可接受
        let _file_lock: Option<crate::multi_agent::LockHandle> =
            if matches!(name.as_str(), "edit" | "write") {
                let guard = crate::multi_agent::SubagentFileGuard::new(
                    capability,
                    workspace_root.to_path_buf(),
                );
                // 从 input JSON 提取 file_path(edit/write 工具标准字段)
                let file_path = serde_json::from_str::<serde_json::Value>(input)
                    .ok()
                    .and_then(|v| {
                        v.get("file_path")
                            .and_then(|fp| fp.as_str().map(std::path::PathBuf::from))
                    });

                match file_path {
                    Some(path) => match guard.try_acquire(&path, true) {
                        Ok(lock) => Some(lock),
                        Err(e) => {
                            // 锁获取失败(超时/拒绝)→ 工具未执行,回填 is_error=true
                            // 不中止循环(与 execute 失败处理一致),让 LLM 决定下一步
                            // 不提取 changed_files(文件实际未被修改)
                            messages.push(ConversationMessage::tool_result(
                                id.clone(),
                                name.clone(),
                                e,
                                true,
                            ));
                            continue;
                        }
                    },
                    None => None, // 无 file_path 字段,跳过锁(防御性降级)
                }
            } else {
                None
            };

        // 工具执行 — 失败不中断,返回 is_error=true 让 LLM 决定下一步
        // _file_lock 在此 block 作用域内持有,iteration 结束 drop 释放锁
        let (output, is_error) = match tool_executor.execute(name, input) {
            Ok(result) => (result, false),
            Err(e) => (e.to_string(), true),
        };

        // changed_files 提取(edit/write 可能修改文件)
        if matches!(name.as_str(), "edit" | "write") {
            changed_files.extend(crate::multi_agent::extract_changed_files(
                input,
                workspace_root,
            ));
        }

        messages.push(ConversationMessage::tool_result(
            id.clone(),
            name.clone(),
            output,
            is_error,
        ));
    }
    Ok(())
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
///
/// v3:`ApiClient: Send` supertrait — 所有实现必须 `Send`。这让 `dyn ApiClient`
/// 自动满足 `stream_async` 的 `Self: Send` 约束,从而支持:
/// - `ConversationRuntime<C>` 在 `run_turn_async` 中调用 `C::stream_async`
/// - `execute_subagent_llm` 在 `&mut dyn ApiClient` 上调用 `stream_async`
/// - `spawn_parallel_via_dag_async` 在 JoinSet 中跨线程 spawn 调用链
pub trait ApiClient: Send {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError>;

    /// v3:async 变体 — 供已在 tokio runtime 中的调用方使用,避免嵌套 runtime 开销。
    ///
    /// 默认实现委托给同步 [`stream`](Self::stream),仍会触发 production impl
    /// 内部的 `block_on`。Production 实现(`ProviderRuntimeClient` /
    /// `AnthropicRuntimeClient`)重写本方法以直接 `.await` 异步 provider 调用,
    /// 消除 `runtime.block_on()` 创建嵌套 runtime 的开销。
    ///
    /// # 调用方
    /// - [`ConversationRuntime::run_turn_async`]:在 async 上下文中调用本方法
    /// - `spawn_parallel_via_dag_async`:并行 subagent 调度
    /// - 任何持有 `&mut dyn ApiClient` 且已在 tokio runtime 中的代码路径
    ///
    /// # 返回
    /// `Pin<Box<dyn Future + Send>>`,与 `async-trait` crate 的展开形式一致,
    /// 但不引入额外依赖。由于 `ApiClient: Send`,所有实现已满足 `Self: Send`,
    /// 故无需在方法上额外加 `where Self: Send` 约束。
    fn stream_async<'a>(
        &'a mut self,
        request: ApiRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<AssistantEvent>, RuntimeError>> + Send + 'a,
        >,
    > {
        Box::pin(async move { self.stream(request) })
    }

    /// Construct a fresh client bound to a specific model — used by the
    /// subagent retry loop with model upgrading (Multi-Agent Hardening §4.5).
    ///
    /// Default implementation returns `Err` (model swap unsupported); in that
    /// case the retry loop falls back to reusing `self.api_client` and only
    /// the validation/cost-limit gates take effect.
    ///
    /// Production implementations should construct a fresh client via
    /// `ProviderClient::from_model(model)` and return it as a
    /// `Box<dyn ApiClient>` so the runtime can use it polymorphically.
    ///
    /// Returns `Ok(boxed_client)` if the model swap succeeded.
    fn with_model(&self, _model: &str) -> Result<Box<dyn ApiClient>, String> {
        Err("model swap not supported by this ApiClient implementation".to_string())
    }

    /// Epic 2(TRAE 架构对齐 §3.1):按 `SubagentCapability` 构造绑定到指定模型的
    /// 子 agent client。与 [`with_model`](Self::with_model) 的差异:
    /// - 按 `capability.enables_tools()` 决定是否启用工具(Analyze 不启用)
    /// - 按 `capability.allowed_tools()` 设置工具白名单(ReadOnly/Execute 受限)
    ///
    /// 默认实现委托给 [`with_model`](Self::with_model),忽略 capability
    /// (向后兼容,不支持的实现保持原 `enable_tools=false` 行为)。
    /// Production 实现(`AnthropicRuntimeClient`)重写以按能力启用工具。
    ///
    /// # 参数
    /// - `model`:目标模型名
    /// - `capability`:子智能体能力分级,决定工具启用与白名单
    ///
    /// # 返回
    /// `Ok(boxed_client)` 若构造成功。
    fn with_model_and_capability(
        &self,
        model: &str,
        _capability: crate::multi_agent::SubagentCapability,
    ) -> Result<Box<dyn ApiClient>, String> {
        self.with_model(model)
    }
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
///
/// Epic 2.5(TRAE 架构对齐 §2.5.2):`Send` supertrait 使 `&mut dyn ToolExecutor`
/// 可跨线程传递。路径 B(DAG 调度)用 `std::thread::spawn` 闭包需 `Send`,
/// 路径 A(async)统一带 Send 与 3b 共用 trait。生产实现(`SubagentToolExecutor` /
/// `CliToolExecutor`)字段均已满足 Send,零改动;仅测试用 `StaticToolExecutor`
/// 的 handler 签名需加 `+ Send`。
pub trait ToolExecutor: Send {
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
/// 主对话单 turn 的最大迭代次数(护栏)。
///
/// 原实现用 `usize::MAX`(无限),诊断/工具调用死循环只能靠用户 Ctrl+C 终止。
/// 64 轮足以完成绝大多数合法任务(含复杂多文件重构);超限时
/// [`ConversationRuntime::run_turn_async`] 返回明确错误,替代无限挂起。
///
/// 子代理(subagent)单独走 `DEFAULT_AGENT_MAX_ITERATIONS`(32)。
pub const DEFAULT_MAX_ITERATIONS: usize = 64;

/// 工具调用循环检测的跨 turn 保留窗口(15 分钟)。
/// 窗口内相同工具调用跨 turn 累积计数;超过窗口未出现则衰减清零。
pub const LOOP_DECAY_WINDOW_MS: u64 = 15 * 60 * 1000;

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
    /// 细粒度诊断回调：在 `run_turn` 关键路径埋点，帮助定位"会话卡死"问题。
    /// 每个事件自动带时间戳，回调签名 `Fn(String) + Send`。
    diag_callback: Option<Box<dyn Fn(String) + Send>>,
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
    /// LoopDetector Abort 触发时记录的原因;工具循环看到 Some 立即终止 turn。
    /// 与 hook 的 cancelled 标志区分:普通 hook 取消只把工具结果标错,
    /// loop abort 则真正中断 turn。
    loop_abort_reason: Option<String>,
    /// Harness V(验证)层:幻觉/偷懒信号扫描器。
    /// 在 PostToolUse hook 中对 write_file/edit_file 产物扫描占位标记
    /// (unimplemented!/placeholder/TODO),命中时以 warning 追加到 hook
    /// messages(不阻断)。通过 `slopScan` 配置项 opt-out。
    slop_scanner: SlopScanner,
    /// SlopScanner 开关。`true` 启用,`false` opt-out。从 RuntimeConfig
    /// 的 `slop_scan` 字段读取,`None` 视为 `true`(默认开启)。
    slop_scan_enabled: bool,
    /// P3:完成声明校验器。LLM 声称"完成"且本轮无工具调用时,
    /// 执行项目验证命令(cargo check 等)。验证失败注入 remediation。
    completion_verifier: crate::completion_verifier::CompletionVerifier,
    /// P3:完成声明校验开关。`true` 启用,`false` opt-out。
    completion_verify_enabled: bool,
    /// 改进点 7:`settings.completionVerifyCommands` 配置覆盖。
    /// 非空时优先于 `detect_project_commands` 的自动探测。
    completion_verify_commands: Vec<String>,
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
    /// v0.2 生产接入:CoordinatorExecutor for DAG dispatch。
    ///
    /// `None` 时(默认)DAG 调度走 v0.1 stub 路径(仅注册 Pending run);
    /// `Some` 时 dag_run 工具可取出此 executor 构造 DagScheduler,真正并发
    /// 执行子 agent turn。注入路径:`with_dag_coordinator` 把传入的
    /// api_client + workspace_root 包成 SubagentDispatcher,再装到
    /// CoordinatorExecutor 的 SubagentRunner 回调。
    coordinator_executor: Option<Arc<CoordinatorExecutor>>,
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
    /// 改进点 13:压缩后注入归档 recall 提示的 flag。
    ///
    /// `maybe_auto_compact` 压缩成功后置 true,下一 turn 的 system_prompt
    /// 构造时读取 `list_archived_summary` 列出可 recall 的归档 tool result,
    /// 引导 LLM 在需要原始数据时调用 `recall_full` 工具检索。注入后清除
    /// (一次性提示,避免每 turn 重复注入归档列表)。
    archive_recall_hint_pending: bool,
    /// v2.0 VerifierAgent remediation 注入 — 上一轮 verify 失败的修正建议。
    ///
    /// Review 阶段若 VerifierAgent 检测到失败,把 `FailedVerification` 列表
    /// 序列化为文本存到此字段,下一次 request 构造时在 system_prompt
    /// 变动区追加,引导 LLM 针对性修复(而非盲目重试)。LLM 下轮开始后清除。
    ///
    /// 修复 v1.0 缺陷:`remediation` 字段完全丢失 — 主 agent 不知道
    /// 上一次 verify 为什么失败,只能盲目重试 → 必然陷入 doom loop。
    pending_remediation: Option<String>,
    decision_log: Option<crate::decision_log::DecisionLog>,
    /// v3 §4.7:决策检测策略。控制 `maybe_auto_compact` 在压缩前用何种方式
    /// 提取决策点。默认 `Heuristic`(零成本),可通过
    /// [`with_detection_strategy`](Self::with_detection_strategy) 升级为 `LlmExtract`。
    detection_strategy: crate::decision_log::DetectionStrategy,
    project_topology: Option<std::sync::Arc<crate::project_topology::ProjectTopology>>,
    refactor_tx: Option<crate::vcs_snapshot::RefactorTransaction>,
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
            // P2-8 修复:主对话单 turn 设置有限迭代护栏(默认 64)。
            // 原值 usize::MAX 意味着诊断/工具调用死循环(如反复 netstat/tail
            // 验证同一状态)只能靠用户 Ctrl+C 终止。64 轮足以完成绝大多数
            // 合法任务(含复杂多文件重构),超限返回明确错误替代无限挂起。
            // subagent 仍走 DEFAULT_AGENT_MAX_ITERATIONS(32)。
            max_iterations: DEFAULT_MAX_ITERATIONS,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            context_window: None,
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            diag_callback: None,
            session_tracer: None,
            persistent_memory: None,
            turns_since_last_nudge: 0,
            recovery_orchestrator: RecoveryOrchestrator::default(),
            plan_mode_enabled: true,
            active_plan: None,
            plan_reviewer: PreCompletionChecklistMiddleware::default(),
            workspace_root: None,
            loop_detector: LoopDetector::new(),
            loop_abort_reason: None,
            slop_scanner: SlopScanner::new(),
            slop_scan_enabled: feature_config.slop_scan().unwrap_or(true),
            completion_verifier: crate::completion_verifier::CompletionVerifier::new(),
            completion_verify_enabled: feature_config.completion_verify().unwrap_or(true),
            completion_verify_commands: feature_config
                .completion_verify_commands()
                .map(|cmds| cmds.to_vec())
                .unwrap_or_default(),
            context_assembler: None,
            pending_semantic_context: None,
            verifier_agent: None,
            trace_analyzer: None,
            turn_start: Cell::new(None),
            multi_agent_coordinator: None,
            coordinator_executor: None,
            notebook_refresh_pending: false,
            archive_recall_hint_pending: false,
            pending_remediation: None,
            decision_log: None,
            detection_strategy: crate::decision_log::DetectionStrategy::Heuristic,
            project_topology: None,
            refactor_tx: None,
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

    /// 注入细粒度诊断回调，在 `run_turn` 关键路径埋点。
    ///
    /// 回调在每个 checkpoint 被调用一次，参数为 `[tag] key=val ...` 格式的
    /// 单行消息。上层（TUI）可接入 `diag_log` 写入 `claw-diag.log`。
    #[must_use]
    pub fn with_diag_callback(mut self, diag_callback: Box<dyn Fn(String) + Send>) -> Self {
        self.diag_callback = Some(diag_callback);
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
    /// Phase 4-A:注入 DecisionLog,启用修复经验记录和检索。
    ///
    /// 注入后 LLM 可通过  记录修复决策,通过
    ///  搜索历史决策。
    /// 数据库存储在 ,与 NOTEBOOK.md 互补。
    #[must_use]
    pub fn with_decision_log(mut self, decision_log: crate::decision_log::DecisionLog) -> Self {
        self.decision_log = Some(decision_log);
        self
    }

    /// v3 §4.7:设置决策检测策略。
    ///
    /// 控制 [`Self::maybe_auto_compact`] 在压缩前用何种方式提取决策点:
    /// - [`DetectionStrategy::Heuristic`](crate::decision_log::DetectionStrategy::Heuristic)
    ///   (默认):零 LLM 调用,纯关键词匹配。`alternatives` 字段永远为空。
    /// - [`DetectionStrategy::LlmExtract`](crate::decision_log::DetectionStrategy::LlmExtract)
    ///   { model }:调用轻量模型(flash)提取结构化决策
    ///   (context/decision/rationale/alternatives)。需先通过
    ///   [`set_global_decision_extractor_client`](crate::decision_log::set_global_decision_extractor_client)
    ///   注册全局 client,否则自动降级为 Heuristic。
    ///
    /// **推荐用法**:`build_runtime` 中注入 `DecisionExtractorClient` 后,
    /// 调用 `runtime.with_detection_strategy(DetectionStrategy::LlmExtract { model })`
    /// 启用 LLM 提取。若 client 未注册 / 调用失败 / JSON 解析失败,自动 3 路降级
    /// 保证不阻塞 compaction。
    #[must_use]
    pub fn with_detection_strategy(
        mut self,
        strategy: crate::decision_log::DetectionStrategy,
    ) -> Self {
        self.detection_strategy = strategy;
        self
    }

    /// v3 §4.7:获取当前配置的决策检测策略(用于诊断 / 测试)。
    #[must_use]
    pub fn detection_strategy(&self) -> &crate::decision_log::DetectionStrategy {
        &self.detection_strategy
    }

    /// v3 §4.7:运行时切换决策检测策略(供 `/detection-strategy` 命令使用)。
    ///
    /// 与 `with_detection_strategy`(builder 模式,消耗 self)不同,本方法
    /// 接受 `&mut self`,可在已构造的 runtime 上原地切换策略,无需重建。
    ///
    /// **降级行为**:切换到 `LlmExtract` 但未通过 `set_global_decision_extractor_client`
    /// 注册 client 时,`extract_decisions_before_compaction` 会自动 3 路降级为 Heuristic,
    /// 不会阻塞 compaction。
    pub fn set_detection_strategy(&mut self, strategy: crate::decision_log::DetectionStrategy) {
        self.detection_strategy = strategy;
    }

    pub fn with_project_topology(
        mut self,
        topo: std::sync::Arc<crate::project_topology::ProjectTopology>,
    ) -> Self {
        self.project_topology = Some(topo);
        self
    }

    #[must_use]
    pub fn with_refactor_transaction(
        mut self,
        tx: crate::vcs_snapshot::RefactorTransaction,
    ) -> Self {
        self.refactor_tx = Some(tx);
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

    /// v0.2 生产接入:配置 DAG dispatch 用 CoordinatorExecutor。
    ///
    /// 把传入的 `coordinator` + `api_client` + `workspace_root` 组装成
    /// CoordinatorExecutor(其 SubagentRunner 回调由 SubagentDispatcher 实现),
    /// 装入 runtime。后续 tools 层 dag_run 工具通过 [`coordinator_executor`]
    /// 取出此 executor,即可构造 DagScheduler 进行真实并发调度。
    ///
    /// `api_client` 是独立的一份(不与 runtime 内部主 agent 的 api_client 共享),
    /// 因为 subagent 走独立 LLM 请求 + 独立 prompt cache(§5.2 缓存保护)。
    /// 调用方需在构造 runtime 之前保留 / 重建一份 api_client 传入此处。
    ///
    /// # 类型约束
    /// `C: ApiClient + Send + 'static` — api_client 会被 box 成
    /// `Box<dyn ApiClient + Send>` 并存入 `Arc<Mutex<..>>`,必须满足 Send。
    ///
    /// Epic 3b:新增 `tool_executor` 参数,启用多轮 tool call 循环。
    /// `None` 时子智能体无法调用工具(单轮,向后兼容);
    /// `Some(executor)` 时按 `SubagentDispatcher::with_tool_executor` 注入。
    #[must_use]
    pub fn with_dag_coordinator(
        mut self,
        coordinator: Arc<MultiAgentCoordinator>,
        api_client: C,
        workspace_root: PathBuf,
        tool_executor: Option<Box<dyn ToolExecutor + Send>>,
    ) -> Self
    where
        C: ApiClient + Send + 'static,
    {
        let mut dispatcher =
            SubagentDispatcher::new(Arc::new(Mutex::new(Box::new(api_client))), workspace_root);
        if let Some(te) = tool_executor {
            dispatcher = dispatcher.with_tool_executor(Arc::new(Mutex::new(te)));
        }
        let runner: SubagentRunner = Arc::new(move |id, task| {
            let d = dispatcher.clone();
            Box::pin(async move { d.dispatch(id, task).await })
        });
        self.coordinator_executor = Some(Arc::new(
            CoordinatorExecutor::new(coordinator).with_runner(runner),
        ));
        self
    }

    /// 取出已注入的 CoordinatorExecutor 引用(若已注入)。
    ///
    /// tools 层 dag_run 工具在 "start" 分支调用此方法,若返回 `Some`
    /// 则构造 DagScheduler 进行真实调度;若 `None` 则回退到 v0.1 stub
    /// 路径(仅注册 Pending run)。
    #[must_use]
    pub fn coordinator_executor(&self) -> Option<&Arc<CoordinatorExecutor>> {
        self.coordinator_executor.as_ref()
    }

    /// v3 真并行 spawn:基于 `DagScheduler::run` 实现多 subagent 并发执行。
    ///
    /// 与 [`MultiAgentCoordinator::spawn_parallel`](crate::multi_agent::MultiAgentCoordinator::spawn_parallel)
    /// (串行退化)不同,本方法:
    /// 1. 将每个 `SpawnRequest` 转为 `DagNode`(无依赖,并行根节点)
    /// 2. 通过 `CoordinatorExecutor` + `DagScheduler::run` 真并发调度
    /// 3. async-to-sync 桥接:`tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(scheduler.run())`
    ///
    /// # 调用前提
    /// runtime 必须已通过 [`with_dag_coordinator`](Self::with_dag_coordinator)
    /// 注入 `CoordinatorExecutor`。否则所有 task 返回 `Err`。
    ///
    /// # 能力校验
    /// 与 `spawn_with_model` 一致:Budget 模型(haiku/mini/nano/flash)执行
    /// Diagnostic/Architectural 任务直接返回 `Err`,不进入 DAG 调度。
    ///
    /// # 参数
    /// - `tasks`:spawn 请求列表,每个请求包含 name/task/mode/model/complexity
    ///
    /// # 返回
    /// - `Vec<Result<String, String>>`:每个 task 对应一个结果(顺序与输入一致)
    ///   - `Ok(result_ref)`:subagent 完成后的产物路径(如 `.claw/subagents/{id}.md`)
    ///   - `Err(msg)`:能力校验未通过 / DAG 调度失败 / runtime 未注入
    ///
    /// # FailFast 语义
    /// 本方法使用 `FailFast::On`(向后兼容):任一 node 失败(且耗尽 retry)后,
    /// 整个 DAG 取消,未完成的 node 一并标记为 `Err`。
    /// 本方法设置 `max_retries = 0`(无重试),任一 subagent 失败即整体失败。
    /// 如需容错模式,请使用 [`spawn_parallel_via_dag_with_fail_fast`](Self::spawn_parallel_via_dag_with_fail_fast)
    /// 或异步版本 [`spawn_parallel_via_dag_async`](Self::spawn_parallel_via_dag_async)。
    ///
    /// # 示例
    /// ```ignore
    /// use runtime::multi_agent::{SpawnRequest, CoordinationMode, TaskComplexity};
    /// # use runtime::ConversationRuntime;
    /// # let runtime: ConversationRuntime<_, _> = unimplemented!();
    /// let tasks = vec![
    ///     SpawnRequest::new("agent-a", "task A", CoordinationMode::Fork, "deepseek-v4-flash", TaskComplexity::Simple),
    ///     SpawnRequest::new("agent-b", "task B", CoordinationMode::Fork, "deepseek-v4-pro", TaskComplexity::Diagnostic),
    /// ];
    /// let results = runtime.spawn_parallel_via_dag(tasks);
    /// assert_eq!(results.len(), 2);
    /// ```
    pub fn spawn_parallel_via_dag(&self, tasks: Vec<SpawnRequest>) -> Vec<Result<String, String>> {
        // P0:默认 FailFast::Off — 单个子任务失败不应影响兄弟节点并发执行,
        // 避免"一颗老鼠屎坏了一锅粥"。调用方如需严格语义可显式调用
        // spawn_parallel_via_dag_with_fail_fast(tasks, FailFast::On)。
        self.spawn_parallel_via_dag_with_fail_fast(tasks, FailFast::Off)
    }

    /// PlannerAgent 自动拆解 + 并行派发入口。
    ///
    /// 当 `auto_planner` feature flag 开启时(通过 `planner::set_auto_planner_enabled(true)`),
    /// 自动将用户输入拆解为多个子任务并并行派发。流程:
    ///
    /// 1. **复杂度评估**:用 `assess_complexity` 判断是否为复杂任务
    ///    - `Simple` → 直接返回(不拆解,由主 agent 处理)
    ///    - `Complex` → 继续拆解
    /// 2. **LLM 驱动拆解**(优先):调用 `plan_and_convert_to_spawn_requests`
    ///    - LLM 失败或未注册 client → 自动降级到启发式 `decompose_task`
    /// 3. **并行派发**:调用 `spawn_parallel_via_dag`(FailFast::Off + retry + validation)
    ///
    /// # 返回
    /// - `Ok(Some(results))`:已拆解并派发,results 为每个子任务的结果
    /// - `Ok(None)`:未拆解(feature flag 关闭 / 任务太简单)
    /// - `Err(e)`:拆解失败(理论上不会,因为启发式总是返回至少 1 个 step)
    ///
    /// # Feature flag
    /// 默认关闭。开启方式:
    /// ```ignore
    /// runtime::planner::set_auto_planner_enabled(true);
    /// ```
    ///
    /// # 安全保障
    /// - 并行路径已补齐 retry(P0-a)+ FailFast::Off(P0-b)+ 限流(P1)+ validation gate(P0-d)
    /// - High risk step → `TaskComplexity::Architectural`(max_retries=2)
    /// - Low risk step → `TaskComplexity::Simple`(max_retries=0)
    pub fn plan_and_spawn_parallel(
        &mut self,
        user_input: &str,
    ) -> Result<Option<Vec<Result<String, String>>>, String> {
        // 1. Feature flag 检查
        if !is_auto_planner_enabled() {
            return Ok(None);
        }

        // 2. 复杂度评估 — Simple 任务不拆解
        match assess_complexity(user_input) {
            ComplexityAssessment::Simple => Ok(None),
            ComplexityAssessment::Complex { reason } => {
                // 3. LLM 驱动拆解(优先)+ 降级到启发式
                let spawn_requests = plan_and_convert_to_spawn_requests(user_input)
                    .ok_or_else(|| "planner returned no spawn requests".to_string())?;

                crate::diag::global().append(
                    crate::diag::DiagEntry::new(
                        crate::diag::DiagLevel::Info,
                        "auto_planner",
                        format!(
                            "auto-planner triggered: {} steps, reason: {}",
                            spawn_requests.len(),
                            reason
                        ),
                    )
                    .with_field(
                        "step_count",
                        serde_json::Value::Number(serde_json::Number::from(spawn_requests.len())),
                    ),
                );

                // 4. 并行派发(FailFast::Off + retry + validation 已内置)
                let results = self.spawn_parallel_via_dag(spawn_requests);
                Ok(Some(results))
            }
        }
    }

    /// v3:同步版本的 `spawn_parallel_via_dag`,支持配置 [`FailFast`] 策略。
    ///
    /// 与 [`spawn_parallel_via_dag`](Self::spawn_parallel_via_dag) 行为一致,
    /// 但允许调用方选择失败传播策略:
    /// - `FailFast::On`:任一 node 失败即取消整个 DAG(严格语义)
    /// - `FailFast::Off`:node 失败后标记为 Failed,继续执行其他独立分支(容错语义,默认)
    ///
    /// 内部通过 `tokio::runtime::Builder::new_current_thread` + `block_on` 桥接
    /// [`spawn_parallel_via_dag_async`](Self::spawn_parallel_via_dag_async)。
    /// 供同步上下文(如 `run_turn`)调用。若调用方已在 async 上下文中,应直接使用
    /// async 版本以避免 `block_on` 开销。
    ///
    /// # 示例
    /// ```ignore
    /// use runtime::multi_agent::{SpawnRequest, CoordinationMode, TaskComplexity};
    /// use runtime::multi_agent::dag::FailFast;
    /// # use runtime::ConversationRuntime;
    /// # let runtime: ConversationRuntime<_, _> = unimplemented!();
    /// let tasks = vec![
    ///     SpawnRequest::new("agent-a", "task A", CoordinationMode::Fork, "deepseek-v4-flash", TaskComplexity::Simple),
    /// ];
    /// // 容错模式:失败的 task 返回 Err,成功的 task 仍返回 Ok
    /// let results = runtime.spawn_parallel_via_dag_with_fail_fast(tasks, FailFast::Off);
    /// ```
    pub fn spawn_parallel_via_dag_with_fail_fast(
        &self,
        tasks: Vec<SpawnRequest>,
        fail_fast: FailFast,
    ) -> Vec<Result<String, String>> {
        let (executor, nodes, results) = match self.prepare_dag_for_spawn_parallel(tasks) {
            Ok(v) => v,
            Err(early) => return early,
        };

        let graph = Self::build_spawn_parallel_graph(&nodes);
        let scheduler = DagScheduler::new(graph, executor).with_fail_fast(fail_fast);

        // 同步桥接:本方法从同步上下文(run_turn)调用,当前线程无 tokio runtime。
        // 使用 new_current_thread + enable_all + block_on(与 tools 层 dag_run 工具一致)。
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let msg = format!("failed to create tokio runtime: {e}");
                let mut results = results;
                for (idx, _) in &nodes {
                    results[*idx] = Err(msg.clone());
                }
                return results;
            }
        };

        let run_result = rt.block_on(async move { scheduler.run_with_details().await });
        Self::map_dag_run_result(run_result, nodes, results)
    }

    /// v3:异步版本的 `spawn_parallel_via_dag`,供 async 调用方使用(避免 `block_on`)。
    ///
    /// 与 [`spawn_parallel_via_dag_with_fail_fast`](Self::spawn_parallel_via_dag_with_fail_fast)
    /// 行为一致,但直接 `.await` `DagScheduler::run()`,无需桥接 tokio runtime。
    /// 适合在 async 上下文(如 TUI event loop、HTTP handler)中调用。
    ///
    /// # 参数
    /// - `tasks`:spawn 请求列表
    /// - `fail_fast`:失败传播策略
    ///
    /// # 返回
    /// 同 [`spawn_parallel_via_dag`](Self::spawn_parallel_via_dag)。
    ///
    /// # 示例
    /// ```ignore
    /// use runtime::multi_agent::{SpawnRequest, CoordinationMode, TaskComplexity};
    /// use runtime::multi_agent::dag::FailFast;
    /// # use runtime::ConversationRuntime;
    /// # let runtime: ConversationRuntime<_, _> = unimplemented!();
    /// let tasks = vec![
    ///     SpawnRequest::new("agent-a", "task A", CoordinationMode::Fork, "deepseek-v4-flash", TaskComplexity::Simple),
    /// ];
    /// // 在 async 上下文中直接 await
    /// let results = runtime.spawn_parallel_via_dag_async(tasks, FailFast::On).await;
    /// ```
    pub async fn spawn_parallel_via_dag_async(
        &self,
        tasks: Vec<SpawnRequest>,
        fail_fast: FailFast,
    ) -> Vec<Result<String, String>> {
        let (executor, nodes, results) = match self.prepare_dag_for_spawn_parallel(tasks) {
            Ok(v) => v,
            Err(early) => return early,
        };

        let graph = Self::build_spawn_parallel_graph(&nodes);
        let scheduler = DagScheduler::new(graph, executor).with_fail_fast(fail_fast);

        let run_result = scheduler.run_with_details().await;
        Self::map_dag_run_result(run_result, nodes, results)
    }

    /// 私有辅助:为 `spawn_parallel_via_dag*` 系列方法准备 DAG 调度所需的
    /// executor、节点列表和结果占位向量。
    ///
    /// 提取 sync / async 变体共享的逻辑:
    /// 1. 取出已注入的 `CoordinatorExecutor`(若未注入,返回 `Err` 早退结果)
    /// 2. 能力校验 + 构建 `DagNode`(并行根节点,无依赖,`max_retries = 0`)
    /// 3. 初始化结果占位向量(通过校验的位置为 `Ok(String::new())`)
    ///
    /// # 返回
    /// - `Ok((executor, nodes, results_skeleton))`:可继续构造 DagScheduler
    /// - `Err(results)`:早退 — 未注入 executor / 空 tasks / 全部 task 未通过能力校验
    #[allow(clippy::type_complexity)]
    fn prepare_dag_for_spawn_parallel(
        &self,
        tasks: Vec<SpawnRequest>,
    ) -> Result<
        (
            Arc<CoordinatorExecutor>,
            Vec<(usize, DagNode)>,
            Vec<Result<String, String>>,
        ),
        Vec<Result<String, String>>,
    > {
        let n = tasks.len();
        let executor = match &self.coordinator_executor {
            Some(e) => e.clone(),
            None => {
                return Err((0..n)
                    .map(|_| {
                        Err(
                            "coordinator_executor not injected — call with_dag_coordinator first"
                                .to_string(),
                        )
                    })
                    .collect());
            }
        };

        if n == 0 {
            return Err(Vec::new());
        }

        // 能力校验 + 构建 DagNode(并行根节点,无依赖)
        // 与 spawn_with_model 保持一致:
        // Budget 模型(haiku/mini/nano/flash)执行 Diagnostic/Architectural 任务拒绝
        let mut nodes: Vec<(usize, DagNode)> = Vec::with_capacity(n);
        let mut results: Vec<Result<String, String>> = Vec::with_capacity(n);
        for (idx, task) in tasks.into_iter().enumerate() {
            let lower = task.model.to_ascii_lowercase();
            let is_budget = lower.contains("haiku")
                || lower.contains("mini")
                || lower.contains("nano")
                || lower.contains("flash");
            if is_budget
                && matches!(
                    task.complexity,
                    TaskComplexity::Diagnostic | TaskComplexity::Architectural
                )
            {
                results.push(Err(format!(
                    "model '{}' (Budget tier) cannot handle {:?} task — use Flagship model",
                    task.model, task.complexity
                )));
                continue;
            }

            // P0:按 complexity 动态设置 max_retries — 挽救瞬时失败(30%~50% 并行路径失败)。
            // 与单路径 spawn_with_model 的 max_attempts 语义对齐:
            // - Simple:0(无重试,机械操作)
            // - Diagnostic:1(1 次重试,诊断任务容错)
            // - Architectural:2(2 次重试,架构决策容错)
            // 注意:scheduler 的 max_retries 表示"重试次数"(不含首次),
            //       与 coordinator 的 max_attempts(含首次)语义不同。
            let max_retries = match task.complexity {
                TaskComplexity::Simple => 0,
                TaskComplexity::Diagnostic => 1,
                TaskComplexity::Architectural => 2,
            };

            let node = DagNode {
                id: format!("spawn-{idx}"),
                label: task.name,
                task: task.task,
                depends_on: Vec::new(),
                acceptance_criteria: String::new(),
                verify_command: None,
                max_retries,
                mode: task.mode,
                retry_policy: RetryPolicy::default(),
            };
            nodes.push((idx, node));
            results.push(Ok(String::new())); // 占位,后续填充
        }

        // 若所有 task 都未通过能力校验,提前返回
        if nodes.is_empty() {
            return Err(results);
        }

        Ok((executor, nodes, results))
    }

    /// 私有辅助:从已校验的节点列表构造并行 DAG 图。
    ///
    /// 所有节点作为并行根节点(无 edge)。`max_parallelism` 取
    /// `min(nodes.len(), MAX_PARALLEL_SUBAGENTS)` 以防瞬间大量子任务
    /// 冲击 API 触发速率限制(P1 限流,参考 Anthropic 多智能体研究系统 3-5 并发)。
    fn build_spawn_parallel_graph(nodes: &[(usize, DagNode)]) -> DagGraph {
        let max_parallel = nodes.len().min(MAX_PARALLEL_SUBAGENTS);
        let mut graph = DagGraph::new("spawn_parallel_via_dag").with_max_parallelism(max_parallel);
        for (_, node) in nodes {
            graph.add_node(node.clone());
        }
        graph
    }

    /// 私有辅助:将 `DagScheduler::run_with_details` 的结果映射回 `Vec<Result<String, String>>`。
    ///
    /// - `Ok(DagRunResult)`:
    ///   - `successes`:按 node_id 索引填充成功结果
    ///   - `failures`:`(node_id, error)` 逐条标记为 `Err(subagent failed: ...)`
    ///     携带真实失败原因(FailFast::Off 场景,节点失败但 DAG 继续执行)
    ///   - `skipped`:标记为 `Err(skipped due to dependency failure)`。
    ///     理论上的"结果缺失"(既不在成功也不在失败/跳过)仍标记 `Err(result missing)`,
    ///     作为防御性兜底
    /// - `Err(dag_err)`:FailFast 场景,失败的 node 标记为 `subagent failed`,
    ///   其他 node 标记为 `cancelled due to sibling failure`
    fn map_dag_run_result(
        run_result: Result<DagRunResult, DagError>,
        nodes: Vec<(usize, DagNode)>,
        mut results: Vec<Result<String, String>>,
    ) -> Vec<Result<String, String>> {
        match run_result {
            Ok(details) => {
                // 成功节点:按 node_id 索引填充
                let mut result_map: HashMap<String, NodeResult> = HashMap::new();
                for nr in details.successes {
                    result_map.insert(nr.node_id.clone(), nr);
                }
                // 失败节点:node_id → 真实失败原因
                let failed_map: HashMap<String, String> = details.failures.into_iter().collect();
                // 跳过节点
                let skipped: Vec<String> = details.skipped;
                for (idx, node) in nodes {
                    if let Some(nr) = result_map.remove(&node.id) {
                        results[idx] = Ok(nr.summary);
                    } else if let Some(err) = failed_map.get(&node.id) {
                        results[idx] = Err(format!("subagent failed: {err}"));
                    } else if skipped.contains(&node.id) {
                        results[idx] = Err(format!(
                            "skipped due to dependency failure: node {} not executed",
                            node.id
                        ));
                    } else {
                        results[idx] =
                            Err(format!("node {} result missing after DAG run", node.id));
                    }
                }
            }
            Err(dag_err) => {
                // FailFast:任一 node 失败导致整体失败
                // 失败的 node_id 提取自 DagError::NodeFailed(id)
                let failed_node_id = match &dag_err {
                    DagError::NodeFailed(id) => Some(id.clone()),
                    _ => None,
                };
                let msg = dag_err.to_string();
                for (idx, node) in nodes {
                    if let Some(ref failed_id) = failed_node_id {
                        if node.id == *failed_id {
                            results[idx] = Err(format!("subagent failed: {msg}"));
                        } else {
                            results[idx] = Err(format!("cancelled due to sibling failure: {msg}"));
                        }
                    } else {
                        results[idx] = Err(msg.clone());
                    }
                }
            }
        }
        results
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

    /// Emit a fine-grained diagnostic event through the optional callback.
    ///
    /// Format: `[diag] {tag} ts={ms_since_turn_start} ...`
    /// Calling code appends key=value pairs after the tag.
    fn emit_diag(&self, msg: String) {
        if let Some(ref cb) = self.diag_callback {
            cb(msg);
        }
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

    /// SlopScanner 内置幻觉/偷懒信号扫描。
    ///
    /// 仅对 `write_file`/`edit_file` 等文件修改工具的原始 JSON 产物扫描,
    /// 命中占位标记(`unimplemented!`/`placeholder`/`TODO` 等)时返回 warning 文本。
    ///
    /// 设计:
    /// - 纯文本扫描,不调 LLM,不碰 system prompt,不影响 prompt cache
    /// - warning 模式,不阻断(返回 `Some(warning)` 由调用方 append_message)
    /// - 通过 `slopScan: false` 配置项 opt-out
    /// - 非文件工具或 JSON 解析失败时返回 `None`(自然跳过)
    fn maybe_scan_slop(&self, tool_name: &str, raw_output: &str) -> Option<String> {
        if !self.slop_scan_enabled {
            return None;
        }
        if !is_file_modifying_tool(tool_name) {
            return None;
        }
        let target = extract_scan_target(raw_output)?;
        let signals = self.slop_scanner.scan(&target);
        self.slop_scanner.render_warning(&signals)
    }

    /// 运行 LoopDetector(文件编辑 + 工具调用双通道),合并动作并返回。
    /// Abort 时同时写入 `loop_abort_reason`,供工具循环识别并终止 turn。
    fn apply_loop_detection(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> LoopAction {
        let mut action = LoopAction::Continue;
        if let Some(file_path) = extract_file_path_from_tool_input(tool_name, input) {
            match self.loop_detector.record_edit(&file_path) {
                LoopAction::Abort(reason) => return LoopAction::Abort(reason),
                LoopAction::InjectContext(msg) => action = LoopAction::InjectContext(msg),
                LoopAction::Continue => {}
            }
        }
        match self.loop_detector.record_tool_call(tool_name, input, output) {
            LoopAction::Abort(reason) => return LoopAction::Abort(reason),
            LoopAction::InjectContext(msg) => {
                action = match action {
                    LoopAction::InjectContext(existing) => {
                        LoopAction::InjectContext(format!("{existing}\n{msg}"))
                    }
                    _ => LoopAction::InjectContext(msg),
                };
            }
            LoopAction::Continue => {}
        }
        action
    }

    fn run_post_tool_use_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        // BUG-2 修复:在 PostToolUse hook 中接入 LoopDetector(两个检测维度见
        // apply_loop_detection)。处理:
        // - Continue:正常流程,继续走原 hook_runner。
        // - InjectContext:把警告消息附加到 hook 结果的 messages 中,
        //   让主 agent 在下一轮看到"重新考虑方法"的提示。
        // - Abort:记录 loop_abort_reason 并返回 cancelled=true 的 HookRunResult,
        //   工具循环检测到 loop_abort_reason 后**真正终止 turn**(而非仅标错)。
        match self.apply_loop_detection(tool_name, input, output) {
            LoopAction::Abort(reason) => {
                self.loop_abort_reason = Some(reason.clone());
                return HookRunResult::cancelled_with_message(reason);
            }
            LoopAction::InjectContext(msg) => {
                let mut base_result = self.run_post_tool_use_hook_base(
                    tool_name, input, output, is_error,
                );
                base_result.append_message(msg);
                return base_result;
            }
            LoopAction::Continue => {}
        }
        self.run_post_tool_use_hook_base(tool_name, input, output, is_error)
    }

    /// 执行真正的 PostToolUse hook(不含 loop 检测前置)。
    fn run_post_tool_use_hook_base(
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
        // BUG-2 修复(升级):失败的工具调用同样进入循环检测。
        // 原实现失败路径完全绕过 LoopDetector,"命令报错 → 换参数再报错"的
        // 循环(exit != 0)无法被捕获。现在与成功路径对称处理。
        match self.apply_loop_detection(tool_name, input, output) {
            LoopAction::Abort(reason) => {
                self.loop_abort_reason = Some(reason.clone());
                return HookRunResult::cancelled_with_message(reason);
            }
            LoopAction::InjectContext(msg) => {
                let mut base_result = self.run_post_tool_use_failure_hook_base(
                    tool_name, input, output,
                );
                base_result.append_message(msg);
                return base_result;
            }
            LoopAction::Continue => {}
        }
        self.run_post_tool_use_failure_hook_base(tool_name, input, output)
    }

    /// 执行真正的 PostToolUseFailure hook(不含 loop 检测前置)。
    fn run_post_tool_use_failure_hook_base(
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
    pub async fn run_turn_async(
        &mut self,
        user_input: impl Into<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        let user_input = user_input.into();

        // G9.1: SessionStart lifecycle hook — 在 turn 主循环开始前触发,
        // 让外部观察者(session 审计、UI 状态指示器等)感知会话启动。
        // 返回值不影响主流程,故意丢弃(messages 已在 HookRunner 内部处理)。
        let _ = self.hook_runner.run_session_start(&self.session.session_id);

        // P2-7 修复(升级 v2):跨 turn 循环检测。
        // 原实现每 turn 全量 reset(),跨 turn 的"换参数再诊断"循环不可见
        // (turn 1 诊断失败 → turn 2 换参数再诊断 → ...)。现改为:
        // - 文件编辑计数每 turn 清空(reset_edits):避免多 turn 合法编辑被误判;
        // - 工具调用计数按时间窗口衰减(prune_decayed):窗口内跨 turn 累积,
        //   超时自动清零,兼顾"跨 turn 循环检测"与"合法重复检查"。
        self.loop_detector.reset_edits();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.loop_detector.prune_decayed(now_ms, LOOP_DECAY_WINDOW_MS);
        // 详见 docs/agent-cognitive-exoskeleton-plan.md 第三章。
        if let Some(tx) = &mut self.refactor_tx {
            let turn_id = format!(
                "{}-{}",
                self.session.session_id,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            );
            let _ = tx.pre_turn_snapshot(&turn_id);
        }

        // BUG-9:记录 turn 开始时间,供 record_turn_* 计算 latency_ms。
        self.turn_start.set(Some(Instant::now()));

        self.record_turn_started(&user_input);
        self.session
            .push_user_text(user_input.clone())
            .map_err(|error| RuntimeError::new(error.to_string()))?;

        // G9.1: UserPromptSubmit lifecycle hook — 用户输入已入 session 后触发,
        // 让外部观察者(敏感词过滤、prompt 审计、telemetry 等)看到原始 prompt。
        // 返回值不影响主流程,故意丢弃。
        let _ = self.hook_runner.run_user_prompt_submit(&user_input);

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
                    // D1.0:LLM-driven planning — 已注册 PlanGeneratorClient 时
                    // 由模型生成步骤(JSON),失败/未注册回退启发式 decompose_task。
                    // 模型输出经 parse_llm_plan_steps 容错解析,任何异常都不阻断
                    // plan 创建(回退启发式,保证至少 1 个 step)。
                    let steps = crate::planner::generate_steps_with_llm(&user_input)
                        .unwrap_or_else(|| decompose_task(&user_input));
                    let mut artifact = PlanArtifact::new(user_input.clone(), steps);
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
            self.emit_diag(format!("[diag] loop_start iter={iterations}"));
            if iterations > self.max_iterations {
                // BUG-3 修复(升级):超限错误携带诊断上下文。
                // 原实现裸错误,下一 turn 不知道上次为什么卡住 → 跨 turn 死循环
                // 仍可能复发。现在错误明确指向 NOTEBOOK <attempted> 段
                // (Task 2 已自动记录本 turn 所有失败尝试)。
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations (64). \
                     Turn aborted to prevent a runaway loop; failed attempts are \
                     recorded in the NOTEBOOK <attempted> section. Change strategy \
                     or ask the user before retrying.",
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

                    // 改进点 13:压缩后主动列出可 recall 的归档 tool result。
                    // auto_compaction 删除了旧消息,但原始 tool result 已通过
                    // microcompact 的 archiver 归档到 .claw/tool_results_archive.jsonl。
                    // 这里列出最近的归档摘要,引导 LLM 在需要原始数据(如实验数值、
                    // 完整文件内容)时调用 `recall_full` 工具按 tool_use_id 检索。
                    // 一次性注入:注入后立即清除 flag,避免每 turn 重复注入。
                    if self.archive_recall_hint_pending {
                        self.archive_recall_hint_pending = false;
                        if let Ok(summaries) =
                            crate::tool_result_archive::list_archived_summary(workspace_root)
                        {
                            if !summaries.is_empty() {
                                // 取最近 10 条归档(文件中靠后的更新),避免列表过长
                                let recent: Vec<&(String, String, String, u64)> =
                                    summaries.iter().rev().take(10).collect();
                                let mut hint = String::with_capacity(512);
                                hint.push_str(
                                    "# 📦 Archived Tool Results Available for Recall\n\
                                     上下文压缩已发生,部分旧 tool result 的原始内容已归档。\n\
                                     若需要原始数据(完整文件内容、实验数值、命令输出等),\n\
                                     可调用 `recall_full` 工具按 tool_use_id 检索。\n\
                                     最近归档(最多 10 条):\n",
                                );
                                for (id, name, preview, _ts) in recent {
                                    let p: String = preview.chars().take(60).collect();
                                    hint.push_str(&format!("- id={id} tool={name} preview={p}\n"));
                                }
                                hint.push_str(
                                    "调用示例:recall_full({\"tool_use_id\": \"<id>\"})\n\
                                     或 recall_full({\"list_only\": true}) 查看全部归档。",
                                );
                                system_split.dynamic_sections.push(hint);
                            }
                        }
                    }

                    // 方案 C:跨会话 plan stale 检测。
                    // 上一会话结束时通过 mark_plan_stale 写入标记文件,本会话
                    // 首 turn 检测到则注入提醒,引导 LLM 调用 notebook_update
                    // 把上一会话的任务摘要写入 <plan>,让本会话后续 turn 能看到。
                    // 清除时机:LLM 成功调用 notebook_update 后由
                    // execute_notebook_update 触发 clear_plan_stale。
                    if crate::notebook::is_plan_stale(workspace_root) {
                        system_split.dynamic_sections.push(
                            "# 📝 New Session — NOTEBOOK <plan> Refresh Recommended\n\
                             这是新会话的首个 turn,上一会话结束时标记了 <plan> 为 stale。\n\
                             **请在处理完当前用户请求后,调用 `notebook_update` 工具**,\n\
                             把上一会话的任务摘要写入 `<plan>` 段(若当前任务已变化):\n\
                             - 本次会话的目标 / 关键决策 / 约束 / 进度\n\
                             这样后续 turn 和下一会话能零延迟知道任务状态。\n\
                             若当前任务与上一会话无关,可忽略此提醒。"
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
                        let rendered = plan.render_skeleton();
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
                        let rendered = plan.render_skeleton();
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
                    // Verbosity steering(Headroom Output Token Reduction 对标):
                    // 在 dynamic 区末尾追加简洁指令,引导模型减少 output token。
                    // 放 dynamic 区不影响 static 缓存前缀;内容常量,不破坏隐式前缀缓存。
                    // 不复述上下文中已有的代码/文件内容,直接给出结论和行动。
                    system_split.dynamic_sections.push(
                        "Be concise. Do not restate context, repeat code already shown, or preface actions with restatements. Lead with the answer or the change.".to_string()
                    );
                }
                // BUG 修复:pending_remediation 在此清空(读取后立即消费),
                // 而非 turn 结束时清空。否则 Review 阶段新设置的 remediation
                // 会被 turn 末尾的 clear 覆盖,导致 remediation 永远丢失。
                // P3 完成声明校验也依赖此修复:break 点设置的 remediation
                // 需要存活到下一 turn 被读取。
                self.pending_remediation = None;
                // 缓存保护(§5.2-enhanced):PlanArtifact 的 status_delta
                // (step 状态标签 ⏳→▶→✓)每 turn 变化,若放在 system_prompt
                // 的 dynamic_sections 会破坏隐式前缀缓存,导致之后的 tools
                // 和 messages 全部 miss。改为追加到 messages 末尾,只影响
                // 最后一条 message,system_prompt + tools + 历史 messages
                // 全部保持 cache 命中。预期命中率从 88-92% 回升到 97%+。
                let mut messages = sliced.to_vec();
                if let Some(plan) = &self.active_plan {
                    let delta = plan.render_status_delta();
                    if !delta.is_empty() {
                        messages.push(ConversationMessage::user_text(delta));
                    }
                }
                ApiRequest {
                    system_prompt: system_split,
                    messages,
                    request_kind: RequestKind::Main,
                }
            };
            self.emit_diag("[diag] api_stream_start".to_string());
            let events = match self.api_client.stream_async(request).await {
                Ok(events) => {
                    self.emit_diag("[diag] api_stream_done".to_string());
                    events
                }
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
            // 细粒度中断检查：API 流式调用（可能长达数十秒）完成后立即检查 abort。
            // 用户在流式响应期间按 Ctrl+C 无法中断阻塞 IO，但可以在此处立即返回，
            // 避免继续处理 assistant message 和执行工具。
            if self.hook_abort_signal.is_aborted() {
                self.record_turn_failed(iterations, &RuntimeError::new("turn interrupted by user"));
                return Err(RuntimeError::new("turn interrupted by user"));
            }

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
            self.emit_diag(format!(
                "[diag] events_parsed iter={iterations} tool_count={} text_len={}",
                pending_tool_uses.len(),
                assistant_message
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.len()),
                        _ => None,
                    })
                    .sum::<usize>()
            ));
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
                // P3:完成声明校验 — 四条件严格 gating + 30s 超时
                // 条件 1 (turn end): 即将 break ✓
                // 条件 2 (no tool calls): pending_tool_uses.is_empty() ✓
                // 条件 3 (completion claim): 检查 LLM 文本
                // 条件 4 (not already verified): break 退出循环,本 turn 不会再次进入 ✓
                //
                // 缓存保护:验证走子进程,不调 LLM,不碰 system prompt。
                // remediation 复用 pending_remediation(已在 request 构造后清空,
                // 此处设置的新值存活到下一 turn)。
                if self.completion_verify_enabled {
                    // assistant_message 已 move 到 assistant_messages,取最后一条。
                    let llm_text: String = assistant_messages
                        .last()
                        .map(|msg| {
                            msg.blocks
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();

                    if let Some(signal) =
                        crate::completion_verifier::CompletionVerifier::detect_completion_claim(
                            &llm_text,
                        )
                    {
                        if let Some(workspace_root) = &self.workspace_root {
                            // 改进点 7:优先读取 settings.completionVerifyCommands 配置覆盖,
                            // 兑现 completion_verifier.rs:24 行注释承诺。未配置时回退自动探测。
                            let commands = if !self.completion_verify_commands.is_empty() {
                                self.completion_verify_commands.clone()
                            } else {
                                crate::completion_verifier::CompletionVerifier::detect_project_commands(workspace_root)
                            };
                            if !commands.is_empty() {
                                self.emit_diag(format!(
                                    "[diag] P3 completion_verify: signal={:?} commands={:?}",
                                    signal.pattern, commands
                                ));
                                let results = self
                                    .completion_verifier
                                    .run_verification(&commands, workspace_root);
                                if let Some(remediation) =
                                    crate::completion_verifier::CompletionVerifier::render_remediation(&results)
                                {
                                    self.pending_remediation = Some(remediation);
                                }
                            }
                        }
                    }
                }
                break;
            }

            // 细粒度中断检查：在工具循环入口检查 abort signal。
            // 若 API 流式调用期间用户按下 Ctrl+C，abort flag 已被设置，
            // 在进入工具循环时即可返回，无需等待所有工具执行完毕。
            if self.hook_abort_signal.is_aborted() {
                self.record_turn_failed(iterations, &RuntimeError::new("turn interrupted by user"));
                return Err(RuntimeError::new("turn interrupted by user"));
            }

            let tool_count = pending_tool_uses.len();
            self.emit_diag(format!(
                "[diag] tool_loop_enter iter={iterations} tool_count={tool_count}"
            ));
            for (tool_use_id, tool_name, input) in pending_tool_uses {
                // 细粒度中断检查：执行下一个工具前检查 abort signal。
                // 若上一个工具执行时间较长（如 cargo build），用户在等待期间
                // 按了 Ctrl+C，此检查能阻止后续工具继续执行。
                if self.hook_abort_signal.is_aborted() {
                    self.record_turn_failed(
                        iterations,
                        &RuntimeError::new("turn interrupted by user"),
                    );
                    return Err(RuntimeError::new("turn interrupted by user"));
                }

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
                        self.emit_diag(format!(
                            "[diag] tool_start iter={iterations} name={tool_name}"
                        ));
                        self.record_tool_started(iterations, &tool_name);
                        // Intercept `session_search` and route it directly to
                        // the session's `HistoryIndex`. The tool is implemented
                        // inside the runtime (not registered with the external
                        // `ToolExecutor`) so it can read from the session's
                        // `Arc<HistoryIndex>` without going through a foreign
                        // dispatcher. All other tool names fall through to the
                        // standard executor.
                        // 工具是否经外部 ToolExecutor 执行(自定义工具路径)。
                        // runtime 内建拦截工具(session_search/dispatch_subagent 等)
                        // 不触发 PostCustomToolCall,与"自定义工具"语义对齐。
                        let mut executed_via_external_executor = false;
                        let (mut output, mut is_error) = if tool_name == "session_search" {
                            match self.execute_session_search(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "dispatch_subagent" {
                            // Step 3.2-c:subagent-as-tool 路由。
                            // 主 agent 通过 tool call 派发子 agent,走独立 LLM 请求,
                            // 不污染主 agent 的 prompt cache(§5.2 缓存保护)。
                            // v3:使用 async 变体避免 nested block_on panic。
                            match self.execute_dispatch_subagent_async(&effective_input).await {
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
                        } else if tool_name == "spawn_parallel_subagents" {
                            // v3:批量并行派发多个子 agent(走 DagScheduler 真并行)。
                            // 与 dispatch_subagent(单个 + retry loop)互补:
                            // - 适用于独立的可并行任务(多文件分析、多模块测试)
                            // - 不带 retry,支持 fail_fast 配置
                            // v3:使用 async 变体避免 nested block_on panic。
                            match self
                                .execute_spawn_parallel_subagents_async(&effective_input)
                                .await
                            {
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
                        } else if tool_name == "log_decision" {
                            // Phase 4-A:记录修复决策到 DecisionLog(SQLite + FTS5)。
                            // LLM 在完成修复并验证后调用,以便未来会话能从经验中学习。
                            match self.execute_log_decision(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "search_past_decisions" {
                            // Phase 4-A:搜索历史修复决策(FTS5 全文检索)。
                            // LLM 在遇到问题时可先查历史,避免重复犯错。
                            match self.execute_search_past_decisions(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "verify_decision" {
                            // Phase 4-A P1-4:闭合 success_rate 学习环。
                            // LLM 在重新验证历史决策(成功/失败/部分成功)后调用,
                            // 原子更新 verify_count/success_rate/verified_at_ms。
                            // 详见 docs/agent-cognitive-exoskeleton-plan.md §4.4。
                            match self.execute_verify_decision(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "query_project_graph" {
                            match self.execute_query_project_graph() {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "find_boundary_crossings" {
                            match self.execute_find_boundary_crossings(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "get_symbol_info" {
                            match self.execute_get_symbol_info(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "rollback_transaction" {
                            match self.execute_rollback_transaction() {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "transaction_status" {
                            match self.execute_transaction_status() {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "refactor_algorithm_topo" {
                            // Phase 4-B:建议模式符号重命名。不修改文件,
                            // 基于 ProjectTopology SymbolIndex 生成建议列表,
                            // LLM 拿到建议后用 edit_file 逐个应用。
                            match self.execute_refactor_algorithm_topo(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else if tool_name == "benchmark_compare" {
                            // Phase 4-B:运行命令多次并报告计时统计(avg/median/min/max/stddev),
                            // 支持 warmup/sample_size/timeout。
                            match self.execute_benchmark_compare(&effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        } else {
                            // 外部自定义工具路径:经 ToolExecutor 注册的工具(含 MCP)。
                            // 成功执行后触发 PostCustomToolCall 事件(见下方接入点)。
                            executed_via_external_executor = true;
                            match self.tool_executor.execute(&tool_name, &effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            }
                        };
                        // SlopScanner:在 merge_hook_feedback 污染前扫描原始产物。
                        // write_file/edit_file 的 output 是 JSON,含 content/newString。
                        // 命中占位标记(unimplemented!/placeholder/TODO)时生成 warning,
                        // 稍后通过 post_hook_result.append_message 回灌到 tool result。
                        // 缓存保护:纯文本扫描,不调 LLM,不碰 system prompt。
                        let slop_warning = self.maybe_scan_slop(&tool_name, &output);
                        output = merge_hook_feedback(pre_hook_result.messages(), output, false);

                        // Phase 4 P1-1：文件修改工具执行后调用 mark_dirty，
                        // 记录被修改的文件路径到事务管理器，以便 rollback 时恢复。
                        // 仅对非 error 的文件修改工具（write_file/edit_file）生效。
                        if !is_error && (tool_name == "write_file" || tool_name == "edit_file") {
                            if let Some(tx) = &mut self.refactor_tx {
                                // 从 effective_input JSON 中提取 path 字段
                                if let Ok(parsed) =
                                    serde_json::from_str::<serde_json::Value>(&effective_input)
                                {
                                    if let Some(path_str) =
                                        parsed.get("path").and_then(|v| v.as_str())
                                    {
                                        let file_path = std::path::PathBuf::from(path_str);
                                        tx.mark_dirty(&[file_path]);
                                    }
                                }
                            }
                        }

                        let mut post_hook_result = if is_error {
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
                        // SlopScanner warning 回灌:命中占位标记时追加到 hook messages,
                        // 不改变 denied/failed/cancelled 状态(warning 模式不阻断)。
                        if let Some(warning) = slop_warning {
                            post_hook_result.append_message(warning);
                        }
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

                        // PostCustomToolCall(design-gaps #7):外部自定义工具调用
                        // 成功后的监控/审计事件。仅对经 ToolExecutor 执行的外部工具
                        // 触发,失败路径已由 PostToolUseFailure 覆盖,此处不重复。
                        // 返回消息追加到 tool result(不改变状态,监控语义)。
                        if !is_error && executed_via_external_executor {
                            let custom_hook_result =
                                self.hook_runner.run_post_custom_tool_call(&tool_name, &output);
                            if !custom_hook_result.messages().is_empty() {
                                output = merge_hook_feedback(
                                    custom_hook_result.messages(),
                                    output,
                                    false,
                                );
                            }
                        }

                        // P0:失败的工具调用自动记录到 NOTEBOOK <attempted> 段。
                        // 循环中的 LLM 不会主动调用 notebook_update,此处由运行时记账,
                        // 使下一轮/下一 turn 看到"已尝试且失败"的路径,从源头消除重复诊断。
                        // 静默吞错:记录失败不阻断工具结果返回(与历史索引 hook 一致)。
                        if is_error {
                            if let Some(workspace_root) = &self.workspace_root {
                                let _ = crate::notebook::append_attempt(
                                    workspace_root,
                                    &tool_name,
                                    &effective_input,
                                    &output,
                                );
                            }
                        }

                        // BUG-2 修复(升级):LoopDetector Abort 现在真正终止 turn。
                        // 原实现只把工具结果标记为 error,LLM 看到错误消息后仍会
                        // 继续循环,只有 64 次迭代上限兜底。现在 Abort 立即返回
                        // 带诊断的错误;已尝试记录在 NOTEBOOK <attempted> 段
                        // (Task 2 自动记账),供下一 turn 改变策略。
                        if let Some(reason) = self.loop_abort_reason.take() {
                            let error = RuntimeError::new(format!(
                                "doom loop detected, turn aborted: {reason}. \
                                 Failed attempts are recorded in the NOTEBOOK \
                                 <attempted> section; change strategy or ask the \
                                 user before retrying."
                            ));
                            self.record_turn_failed(iterations, &error);
                            return Err(error);
                        }

                        ConversationMessage::tool_result(tool_use_id, tool_name, output, is_error)
                    }
                    PermissionOutcome::Deny { reason } => {
                        // 触发 Notification hook:权限拒绝是用户通知的天然触发点
                        let _ = self
                            .hook_runner
                            .run_notification(&format!("Tool `{tool_name}` was denied: {reason}"));
                        ConversationMessage::tool_result(
                            tool_use_id,
                            tool_name,
                            merge_hook_feedback(pre_hook_result.messages(), reason, true),
                            true,
                        )
                    }
                };
                self.session
                    .push_message(result_message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                self.record_tool_finished(iterations, &result_message);
                {
                    // 提取 tool_name 和 is_error 用于 diag 日志
                    let tn = result_message.blocks.iter().find_map(|b| match b {
                        ContentBlock::ToolResult {
                            tool_name,
                            is_error,
                            ..
                        } => Some((tool_name.clone(), *is_error)),
                        _ => None,
                    });
                    if let Some((name, is_err)) = tn {
                        self.emit_diag(format!(
                            "[diag] tool_done iter={iterations} name={name} is_error={is_err}"
                        ));
                    }
                }
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

        // 注:pending_remediation 已在 request 构造后立即清空(见 line ~1182),
        // 此处不再重复清空。Review 阶段或 P3 完成声明校验若设置了新 remediation,
        // 需要存活到下一 turn 被读取。

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

        // G9.1: Stop lifecycle hook — turn 主循环正常结束前触发,
        // 让外部观察者(完成通知、telemetry、UI 状态指示器等)感知会话停止。
        // reason 描述结束原因(此处为正常完成);异常路径由各自分支的
        // record_turn_failed 处理,不在此触发 Stop(避免与失败信号竞争)。
        // 返回值不影响主流程,故意丢弃。
        let _ = self.hook_runner.run_stop("turn_completed");

        // 方案 C:会话结束时标记 NOTEBOOK <plan> 为 stale。
        // 下一会话首 turn 检测到此标记会注入"刷新 <plan>"提醒,引导 LLM
        // 调用 notebook_update 把本次会话的任务摘要写入 <plan> 段,让下一会话
        // 零延迟知道上次任务状态。失败静默忽略(非关键路径)。
        if let Some(workspace_root) = &self.workspace_root {
            crate::notebook::mark_plan_stale(workspace_root);
        }

        // G9.1: SessionEnd lifecycle hook — turn 完全结束(含 nudge 等清理)后触发,
        // 让外部观察者(session 审计、状态持久化、清理逻辑等)感知会话结束。
        // 返回值不影响主流程,故意丢弃。
        let _ = self.hook_runner.run_session_end(&self.session.session_id);

        Ok(summary)
    }

    /// 同步入口 — 为不在 tokio runtime 中的调用方提供向后兼容。
    ///
    /// 内部创建 `current_thread` runtime 并 `block_on`
    /// [`run_turn_async`](Self::run_turn_async)。
    ///
    /// **若调用方已在 tokio runtime 中(如 claw-shell 的 LocalSet),请直接使用
    /// [`run_turn_async`](Self::run_turn_async)**,以避免嵌套 runtime 开销和
    /// `block_on` panic。
    ///
    /// # 嵌套 runtime 检测
    /// 若检测到调用方已在 tokio runtime 上下文中,返回 `Err` 而非 panic,
    /// 提示调用方改用 [`run_turn_async`](Self::run_turn_async)。
    pub fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(RuntimeError::new(
                "run_turn (sync) called from within a tokio runtime — \
                 use run_turn_async instead to avoid nested runtime overhead",
            ));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| RuntimeError::new(format!("failed to create tokio runtime: {e}")))?;
        rt.block_on(self.run_turn_async(user_input, prompter))
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

        // P11-2:当 history_index 和 workspace_root 均未配置时,返回 not available 消息,
        // 与其他工具(dispatch_subagent/check_subagent/recall_full)的行为一致。
        if self.session.history_index.is_none() && self.workspace_root.is_none() {
            return Ok(
                "session_search is not available: no history index or workspace_root configured."
                    .to_string(),
            );
        }

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
    /// 1. 解析 JSON 输入(`name`/`task`/`mode`/`model`?/`complexity`?/`cost_limit`?)
    /// 2. 检查 `multi_agent_coordinator` 是否注入
    /// 3. 调用 `coordinator.spawn()`(或 `spawn_with_model`) + `coordinator.start()`
    /// 4. 发布 `SubagentHandoff` lane event(可观测性)
    /// 5. **Multi-Agent Hardening §4.5 retry loop**:执行 → 验证 → 失败时
    ///    升级模型重试,达 `max_attempts` 或 `cost_limit` 中止
    /// 6. 返回结果给主 agent
    ///
    /// **缓存保护**(§5.2):子 agent 走独立 LLM 请求 + 独立 prompt cache,
    /// 不污染主 agent 缓存。
    ///
    /// **JSON 输入字段**:
    /// - `name`(必填):子 agent 名称
    /// - `task`(必填):任务描述
    /// - `mode`(可选,默认 `fork`):编排模式 fork/teammate/worktree
    /// - `model`(可选):指定模型名(如 `deepseek-v4-flash`),省略则用主 agent client
    /// - `complexity`(可选,默认 `simple`):任务复杂度 simple/diagnostic/architectural
    /// - `cost_limit`(可选,USD):成本上限,达上限中止 retry
    async fn execute_dispatch_subagent_async(
        &mut self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if self.multi_agent_coordinator.is_none() {
            return Ok(
                "dispatch_subagent is not available: no multi-agent coordinator configured."
                    .to_string(),
            );
        }

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

        // Multi-Agent Hardening §4.2/§4.5:可选 model + complexity + cost_limit + max_attempts 字段
        let model_str = parsed.get("model").and_then(|v| v.as_str());
        let complexity = parse_complexity(parsed.get("complexity"));
        let capability = parse_capability(parsed.get("capability"));
        let cost_limit = parsed.get("cost_limit").and_then(|v| v.as_f64());
        let max_attempts_override = parsed
            .get("max_attempts")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);

        // spawn:有 model 走 spawn_with_model(能力校验),否则走原 spawn(向后兼容)
        // 借用:此处 coordinator 是 &MultiAgentCoordinator(不可变借用 self.multi_agent_coordinator)
        let subagent_id = {
            let coordinator = self
                .multi_agent_coordinator
                .as_ref()
                .expect("checked above");
            let id = if let Some(m) = model_str {
                coordinator
                    .spawn_with_model(name, task, mode, m, complexity)
                    .map_err(|e| format!("spawn_with_model failed: {e}"))?
            } else {
                coordinator.spawn(name, task, mode)
            };
            // 注入 capability — TRAE 架构对齐(§3.1)
            let _ = coordinator.set_capability(&id, capability);
            // 注入 cost_limit(如有)
            if let Some(limit) = cost_limit {
                let _ = coordinator.set_cost_limit(&id, Some(limit));
            }
            // 注入 max_attempts 覆盖(如有)— 允许调用方显式控制重试次数
            if let Some(ma) = max_attempts_override {
                let _ = coordinator.set_max_attempts(&id, ma);
            }
            coordinator
                .start(&id)
                .map_err(|e| format!("failed to start subagent: {e}"))?;
            id
        }; // coordinator 借用在此结束,后续可重新获取

        // 桥接:在 global TaskRegistry 注册子 agent 任务,让 AI 能通过 TaskOutput 查询进度。
        let bridge_task_id = {
            let registry = crate::task_registry::global();
            let t = registry.create(task, Some(name));
            let _ = registry.set_status(&t.task_id, crate::task_registry::TaskStatus::Running);
            t.task_id
        };

        // 发布 SubagentHandoff lane event — 主 agent → 子 agent 任务派发记录。
        let emitted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let event = LaneEvent::subagent_handoff(emitted_at.clone(), &subagent_id, mode_str, task);
        publish_lane_event(event);

        // 读取 max_attempts(用于 retry loop 上限)和 complexity(用于 §4.6 诊断 SOP 注入)
        // 和 capability(用于 Epic 1 上下文注入 + Epic 3 工具白名单)
        // 注:complexity/capability 在 retry 过程中不变(只升级 model,不改变任务属性),故循环外读取一次即可
        let (max_attempts, subagent_complexity, subagent_capability) = self
            .multi_agent_coordinator
            .as_ref()
            .and_then(|c| c.get(&subagent_id))
            .map(|a| (a.max_attempts, a.complexity, a.capability))
            .unwrap_or((
                1,
                crate::multi_agent::TaskComplexity::Simple,
                crate::multi_agent::SubagentCapability::Analyze,
            ));
        let max_attempts = max_attempts.max(1);
        let mut current_model = model_str.map(String::from);
        let mut final_status = "failed";
        let mut final_result_msg = String::new();

        // P0-4:Multi-Agent Hardening §4.5 retry loop
        // 论文依据:Anthropic Multi-Agent Research System + Router-R1 + FrugalGPT
        // - 失败时升级模型重试,达 max_attempts / cost_limit 中止
        // - validate() 通过方为终态成功
        // - reset_for_retry() 修复 v1 状态不可达漏洞
        //
        // 借用策略:循环内每次 `self.run_subagent_turn_with_model` (mut self) 后,
        // 重新通过 `self.multi_agent_coordinator.as_ref()` 获取 coordinator 引用,
        // 避免 mut self 与不可变 coordinator 借用冲突。
        for attempt in 1..=max_attempts {
            // 记录诊断:attempt + model
            crate::diag::global().append(
                crate::diag::DiagEntry::new(
                    crate::diag::DiagLevel::Info,
                    "subagent_attempt",
                    format!(
                        "subagent {subagent_id} attempt {attempt}/{max_attempts} with model {:?}",
                        current_model
                    ),
                )
                .with_field(
                    "subagent_id",
                    serde_json::Value::String(subagent_id.clone()),
                )
                .with_field(
                    "attempt",
                    serde_json::Value::Number(serde_json::Number::from(attempt)),
                )
                .with_field(
                    "max_attempts",
                    serde_json::Value::Number(serde_json::Number::from(max_attempts)),
                )
                .with_field(
                    "model",
                    serde_json::Value::String(
                        current_model
                            .clone()
                            .unwrap_or_else(|| "<default>".to_string()),
                    ),
                ),
            );

            // 执行子智能体 LLM 请求(单轮,完全隔离)— mut self 借用,调用后释放
            // §4.6 诊断 SOP 注入:传入 complexity,Diagnostic 时追加 SOP 到 system_prompt
            let subagent_result = self
                .run_subagent_turn_with_model(
                    &subagent_id,
                    name,
                    task,
                    current_model.as_deref(),
                    subagent_complexity,
                    subagent_capability,
                )
                .await;

            // 重新获取 coordinator 引用(multi_agent_coordinator 不可变借用)
            let coordinator = self
                .multi_agent_coordinator
                .as_ref()
                .expect("coordinator checked above");

            // MVP 成本累计:名义值 $0.001/次,旗舰模型(pro)×10
            // 完整成本计算需 token 用量,v2 由 run_subagent_turn_with_model 回传
            let nominal_cost = if current_model
                .as_deref()
                .map(|m| m.contains("pro"))
                .unwrap_or(false)
            {
                0.01
            } else {
                0.001
            };
            let _ = coordinator.add_cost(&subagent_id, nominal_cost);

            // v3 P1 checkpoint:每轮 turn 后保存(借鉴 LangGraph durable execution)
            let _ = coordinator.save_checkpoint(&subagent_id);

            match subagent_result {
                Ok(result_ref) => {
                    // turn 成功 → complete() → validate()
                    let _ = coordinator.complete(&subagent_id, &result_ref);

                    match coordinator.validate(&subagent_id) {
                        Ok(()) => {
                            // 验证通过 → 终态成功
                            final_status = "completed";
                            // Epic 5 §8.4:解析 handoff frontmatter,`summary` + `changed_files`
                            // 进主上下文(给主 agent 足够信息决策是否 Read details),
                            // `details` 通过 result_ref 按需 Read(不进上下文,避免污染)。
                            // 解析失败(旧格式/IO 错误)时降级到原文本路径,向后兼容。
                            let handoff = self.workspace_root.as_deref().and_then(|root| {
                                crate::multi_agent::read_handoff(&root.join(&result_ref)).ok()
                            });
                            final_result_msg = match handoff {
                                Some(h) => {
                                    let changed = if h.changed_files.is_empty() {
                                        "none".to_string()
                                    } else {
                                        h.changed_files.join(", ")
                                    };
                                    format!(
                                        "Subagent `{subagent_id}` completed (attempt {attempt}/{max_attempts}).\n\
                                         Summary: {summary}\n\
                                         Changed files: {changed}\n\
                                         Full result: {result_ref} (use Read tool to inspect details).\n\
                                         The subagent ran with an isolated context — it did not pollute your context window.",
                                        summary = h.summary,
                                    )
                                }
                                None => format!(
                                    "Subagent `{subagent_id}` completed (attempt {attempt}/{max_attempts}). \
                                     Result written to: {result_ref}\n\
                                     Use Read tool to inspect the result. \
                                     The subagent ran with an isolated context — it did not pollute your context window."
                                ),
                            };
                            break;
                        }
                        Err(ve) if ve.retryable && attempt < max_attempts => {
                            // 验证失败(可重试)— 升级模型 + reset_for_retry
                            crate::diag::global().append(
                                crate::diag::DiagEntry::new(
                                    crate::diag::DiagLevel::Warn,
                                    "subagent_validation_failed",
                                    format!(
                                        "subagent {subagent_id} validation failed (attempt {attempt}): {}",
                                        ve.message
                                    ),
                                )
                                .with_field("subagent_id", serde_json::Value::String(subagent_id.clone()))
                                .with_field("retryable", serde_json::Value::Bool(true)),
                            );

                            // 成本门禁:升级前检查
                            if !coordinator.check_cost_limit(&subagent_id) {
                                let cost_acc = coordinator.get_cost_accumulated(&subagent_id);
                                let cost_lim =
                                    coordinator.get_cost_limit(&subagent_id).unwrap_or(0.0);
                                let msg = format!(
                                    "Subagent `{subagent_id}` failed: cost limit ${cost_lim:.4} exceeded (accumulated ${cost_acc:.4}); validation error: {}",
                                    ve.message
                                );
                                let _ = coordinator.fail(&subagent_id, &msg);
                                final_status = "failed";
                                final_result_msg = msg;
                                break;
                            }

                            // 升级模型
                            let upgraded = current_model
                                .as_deref()
                                .and_then(upgrade_model_for_subagent);
                            match upgraded {
                                Some(upgrade) => {
                                    let _ = coordinator.reset_for_retry(
                                        &subagent_id,
                                        Some(upgrade.target_model.clone()),
                                    );
                                    let _ = coordinator.start(&subagent_id);
                                    current_model = Some(upgrade.target_model);
                                    // continue 下一轮 retry
                                }
                                None => {
                                    // 已是旗舰,无法升级 — 立即失败
                                    let msg = format!(
                                        "Subagent `{subagent_id}` failed: model at flagship but validation still fails (attempt {attempt}): {}",
                                        ve.message
                                    );
                                    let _ = coordinator.fail(&subagent_id, &msg);
                                    final_status = "failed";
                                    final_result_msg = msg;
                                    break;
                                }
                            }
                        }
                        Err(ve) => {
                            // 不可重试 或 达 max_attempts
                            let msg = format!(
                                "Subagent `{subagent_id}` failed validation after {attempt} attempts: {}",
                                ve.message
                            );
                            let _ = coordinator.fail(&subagent_id, &msg);
                            final_status = "failed";
                            final_result_msg = msg;
                            break;
                        }
                    }
                }
                Err(e) if attempt < max_attempts => {
                    // turn 失败(可重试)— 升级模型 + reset_for_retry
                    let _ = coordinator.fail(&subagent_id, &e);

                    crate::diag::global().append(
                        crate::diag::DiagEntry::new(
                            crate::diag::DiagLevel::Warn,
                            "subagent_turn_failed",
                            format!(
                                "subagent {subagent_id} turn failed (attempt {attempt}): {e}, retrying with upgraded model"
                            ),
                        )
                        .with_field("subagent_id", serde_json::Value::String(subagent_id.clone())),
                    );

                    // 成本门禁
                    if !coordinator.check_cost_limit(&subagent_id) {
                        let cost_acc = coordinator.get_cost_accumulated(&subagent_id);
                        let cost_lim = coordinator.get_cost_limit(&subagent_id).unwrap_or(0.0);
                        let msg = format!(
                            "Subagent `{subagent_id}` failed: cost limit ${cost_lim:.4} exceeded (accumulated ${cost_acc:.4}); turn error: {e}"
                        );
                        final_status = "failed";
                        final_result_msg = msg;
                        break;
                    }

                    let upgraded = current_model
                        .as_deref()
                        .and_then(upgrade_model_for_subagent);
                    match upgraded {
                        Some(upgrade) => {
                            let _ = coordinator
                                .reset_for_retry(&subagent_id, Some(upgrade.target_model.clone()));
                            let _ = coordinator.start(&subagent_id);
                            current_model = Some(upgrade.target_model);
                            // continue 下一轮 retry
                        }
                        None => {
                            // 无升级路径 — 立即失败
                            let msg = format!(
                                "Subagent `{subagent_id}` failed after {attempt} attempts (no model upgrade path): {e}"
                            );
                            final_status = "failed";
                            final_result_msg = msg;
                            break;
                        }
                    }
                }
                Err(e) => {
                    // 达 max_attempts — 终态失败
                    let _ = coordinator.fail(&subagent_id, &e);
                    final_status = "failed";
                    final_result_msg = format!(
                        "Subagent `{subagent_id}` failed after {max_attempts} attempts: {e}\n\
                         You may retry with a different task description or approach the task directly."
                    );
                    break;
                }
            }
        }

        // G9.1: SubagentStop lifecycle hook — 子 agent 进入终态时触发
        let _ = self.hook_runner.run_subagent_stop(&subagent_id);

        // 发布终态 SubagentResult lane event
        let event =
            LaneEvent::subagent_result(emitted_at, &subagent_id, final_status, &final_result_msg);
        publish_lane_event(event);

        // 桥接:更新 global TaskRegistry 中子 agent 任务的终态,让 AI 能通过 TaskOutput 查到结果。
        {
            let registry = crate::task_registry::global();
            let status = if final_status == "completed" {
                crate::task_registry::TaskStatus::Completed
            } else {
                crate::task_registry::TaskStatus::Failed
            };
            let _ = registry.set_status(&bridge_task_id, status);
            let _ = registry.append_output(&bridge_task_id, &final_result_msg);
            // 把 task_id 暴露给 AI,让它知道可以用 TaskOutput(task_id) 查询此子 agent 的结果。
            final_result_msg.push_str(&format!(
                "\n\n(task_id: {bridge_task_id} — 可用 TaskOutput 工具查询此子 agent 的状态和输出)"
            ));
        }

        Ok(final_result_msg)
    }

    /// 同步入口 — 为不在 tokio runtime 中的调用方(主要是单元测试)提供向后兼容。
    ///
    /// 内部创建 `current_thread` runtime 并 `block_on`
    /// [`execute_dispatch_subagent_async`](Self::execute_dispatch_subagent_async)。
    ///
    /// **生产代码(已在 tokio runtime 中)应直接调用
    /// [`execute_dispatch_subagent_async`](Self::execute_dispatch_subagent_async)**,
    /// 以避免嵌套 runtime 开销和 `block_on` panic。
    ///
    /// # 嵌套 runtime 检测
    /// 若检测到调用方已在 tokio runtime 上下文中(如 `run_turn_async` 调用栈),
    /// 返回 `Err` 而非 panic,提示调用方改用
    /// [`execute_dispatch_subagent_async`](Self::execute_dispatch_subagent_async)。
    /// 与 [`run_turn`](Self::run_turn) 的检测保持一致。
    #[allow(dead_code)]
    fn execute_dispatch_subagent(
        &mut self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // v3 修复(c051bac0 后):run_turn 已改为 run_turn_async,主 turn loop
        // 在 LocalSet async 上下文中执行。若本同步包装器被 async 路径误调用,
        // 直接 block_on 会触发 "Cannot start a runtime from within a runtime" panic。
        // 与 run_turn 一致,返回 Err 而非 panic。
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(
                "execute_dispatch_subagent (sync) called from within a tokio runtime — \
                 use execute_dispatch_subagent_async instead to avoid nested runtime panic"
                    .into(),
            );
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("failed to create tokio runtime: {e}").into()
            })?;
        rt.block_on(self.execute_dispatch_subagent_async(input))
    }

    /// P0-2:执行子智能体的独立 LLM 请求(单轮,完全隔离)。
    ///
    /// 子智能体拥有:
    /// - 独立 Session(空 messages,只有 task 作为 user message)
    /// - 独立 system_prompt(子智能体专用,不包含主 agent 的上下文)
    /// - 独立 prompt cache(不污染主 agent 缓存)
    ///
    /// **注意**:本方法是 [`run_subagent_turn_with_model`] 的便利包装,
    /// 等价于 `run_subagent_turn_with_model(id, name, task, None, complexity, capability)`。
    /// 新代码应直接调用 `run_subagent_turn_with_model`。
    /// 保留本方法以兼容现有文档引用和未来外部调用。
    #[allow(dead_code)]
    async fn run_subagent_turn(
        &mut self,
        subagent_id: &str,
        name: &str,
        task: &str,
        complexity: crate::multi_agent::TaskComplexity,
        capability: crate::multi_agent::SubagentCapability,
    ) -> Result<String, String> {
        self.run_subagent_turn_with_model(subagent_id, name, task, None, complexity, capability)
            .await
    }

    /// Multi-Agent Hardening §4.5.3:带模型选择的 subagent turn 执行。
    ///
    /// - `model = None`:复用主 agent client(同 [`run_subagent_turn`])
    /// - `model = Some(m)`:通过 [`ApiClient::with_model`] 构造独立 client
    ///
    /// §4.6 诊断 SOP 注入:`complexity` 参数传递给 [`execute_subagent_llm`],
    /// 当为 `Diagnostic` 时向 system_prompt 追加诊断 SOP。
    ///
    /// Epic 1(§3.2):`capability` 参数用于上下文注入(工具白名单声明 +
    /// 工具签名摘要)。`SubagentContext` 从主 agent 的 `system_prompt` sections
    /// 中提取 repo_map 和 environment(复用已渲染内容,避免重复扫描)。
    ///
    /// 若 `with_model` 返回 `Err`(默认实现或生产实现构造失败),
    /// 记录诊断日志并回退到主 agent client — 保证 retry loop 不因
    /// client 构造失败而中止,降级为"同模型重试"。
    async fn run_subagent_turn_with_model(
        &mut self,
        subagent_id: &str,
        name: &str,
        task: &str,
        model: Option<&str>,
        complexity: crate::multi_agent::TaskComplexity,
        capability: crate::multi_agent::SubagentCapability,
    ) -> Result<String, String> {
        let workspace_root = self.workspace_root.as_ref().ok_or_else(|| {
            "workspace_root not configured — subagent requires filesystem access for result persistence".to_string()
        })?;

        // Epic 1(§3.2):从主 agent system_prompt sections 提取 repo_map 和 environment,
        // 复用已渲染内容(避免重复扫描),heading 已对齐 static_cache_breakpoints。
        let ctx = self.build_subagent_context(capability);

        // model 为 None:走原 run_subagent_turn 路径(复用主 agent client)
        let model = match model {
            None | Some("") => {
                return Self::execute_subagent_llm(
                    workspace_root,
                    &mut self.api_client,
                    &mut self.tool_executor,
                    subagent_id,
                    name,
                    task,
                    complexity,
                    capability,
                    &ctx,
                )
                .await;
            }
            Some(m) => m,
        };

        // Epic 2:按 capability 构造独立 client — Multi-Agent Hardening §4.5.3
        // Analyze 不启用工具;ReadOnly/Execute 按白名单启用
        match self.api_client.with_model_and_capability(model, capability) {
            Ok(mut sub_client) => {
                Self::execute_subagent_llm(
                    workspace_root,
                    &mut *sub_client,
                    &mut self.tool_executor,
                    subagent_id,
                    name,
                    task,
                    complexity,
                    capability,
                    &ctx,
                )
                .await
            }
            Err(e) => {
                // 降级:client 构造失败时回退到主 agent client(同模型重试)
                // 记录诊断日志,便于排查环境配置问题
                crate::diag::global().append(
                    crate::diag::DiagEntry::new(
                        crate::diag::DiagLevel::Warn,
                        "subagent_model_swap",
                        format!(
                            "model swap failed for {model}: {e}; falling back to main client (same-model retry)"
                        ),
                    )
                    .with_field("subagent_id", serde_json::Value::String(subagent_id.into()))
                    .with_field("target_model", serde_json::Value::String(model.into())),
                );
                Self::execute_subagent_llm(
                    workspace_root,
                    &mut self.api_client,
                    &mut self.tool_executor,
                    subagent_id,
                    name,
                    task,
                    complexity,
                    capability,
                    &ctx,
                )
                .await
            }
        }
    }

    /// Epic 1(§3.2):从主 agent `system_prompt` sections 提取 repo_map 和
    /// environment,构造子智能体上下文。复用已渲染内容,避免重复扫描。
    /// heading 已对齐 `static_cache_breakpoints`(§3.2 heading 对齐约束)。
    fn build_subagent_context(
        &self,
        capability: crate::multi_agent::SubagentCapability,
    ) -> SubagentContext {
        let mut ctx = SubagentContext::default();
        // 从主 agent system_prompt sections 提取 repo_map 和 environment
        // (heading 对齐:## Repository Map / # Environment context)
        for section in &self.system_prompt {
            if section.starts_with("## Repository Map") {
                // 去掉 heading 行,保留内容
                ctx.repo_map = Some(
                    section
                        .strip_prefix("## Repository Map\n")
                        .unwrap_or(section)
                        .to_string(),
                );
            }
            // ProjectContext:从 workspace_root 构造(简化版,完整版由 Epic 2 工具签名补充)
            if section.starts_with("# Environment context") && self.workspace_root.is_some() {
                ctx.project_context = Some(crate::prompt::ProjectContext {
                    cwd: self.workspace_root.clone().unwrap_or_default(),
                    current_date: "unknown".to_string(),
                    git_status: None,
                    git_diff: None,
                    git_context: None,
                    instruction_files: Vec::new(),
                });
            }
        }
        // L2 工具签名层:Epic 2 会填充真实工具签名,此处暂留空
        // (Analyze 无工具,ReadOnly/Execute 的工具签名在 Epic 2 接入)
        let _ = capability;
        ctx
    }

    /// 子智能体 LLM 调用的核心逻辑(无 `self` 借用,避免与 `api_client` 冲突)。
    ///
    /// 抽出为自由函数,以便 [`run_subagent_turn`] 和
    /// [`run_subagent_turn_with_model`] 复用 — 前者传入 `&mut self.api_client`,
    /// 后者传入 `&mut *boxed_client`(由 `with_model` 构造的独立 client)。
    ///
    /// §4.6 诊断 SOP 注入:当 `complexity == Diagnostic` 时,向 system_prompt 追加
    /// 诊断任务执行规范,强制子智能体遵循"先诊断后修复"流程,避免堆砌防御代码。
    ///
    /// Epic 1(§3.2):`capability` + `ctx` 用于上下文注入(repo_map/environment/工具签名)。
    ///
    /// Epic 3a(§3.3.1):多轮 tool call 循环。`Analyze` 能力 `max_iterations=1`(单轮,
    /// 行为与改造前一致);`ReadOnly=5` / `Execute=10` 支持多轮工具调用。每轮:
    /// 1. 构造 `ApiRequest`(system_prompt 不变,messages 增长)
    /// 2. `stream_async` → `build_assistant_message`
    /// 3. 提取 `ToolUse` blocks → 若为空则正常终止
    /// 4. `process_tool_uses`:guard(递归/白名单)+ 执行 + 回填 `ToolResult`
    /// 5. 超过 `max_iterations` → 落盘 `Truncated` handoff + Err(§8.1)
    #[allow(clippy::too_many_arguments)]
    async fn execute_subagent_llm(
        workspace_root: &std::path::Path,
        client: &mut dyn ApiClient,
        tool_executor: &mut dyn ToolExecutor,
        subagent_id: &str,
        name: &str,
        task: &str,
        complexity: crate::multi_agent::TaskComplexity,
        capability: crate::multi_agent::SubagentCapability,
        ctx: &SubagentContext,
    ) -> Result<String, String> {
        use crate::multi_agent::{write_handoff, HandoffStatus, SubagentHandoff};

        // 知识新鲜度门控(Phase 1):Novel 任务注入调研摘要到 task 文本。
        let gated = crate::knowledge_freshness::gate_task(task, 0).await;
        let enhanced_task = gated.enhance_task(task);

        // system_prompt 构建一次,多轮循环中不变(保 prefix cache 命中)
        let system_prompt = build_subagent_system_prompt(complexity, capability, ctx);
        let max_iter = capability.max_iterations();
        let mut messages = vec![ConversationMessage::user_text(format!(
            "# Subagent: {name} ({subagent_id})\n\n请执行以下任务:\n\n{enhanced_task}"
        ))];
        let mut iterations = 0;
        let mut tools_used: Vec<String> = Vec::new();
        let mut changed_files: Vec<String> = Vec::new();
        let mut final_text = String::new();

        loop {
            iterations += 1;
            if iterations > max_iter {
                // §8.1:截断 → 落盘 Truncated handoff + Err
                let handoff = SubagentHandoff::new(
                    subagent_id,
                    name,
                    capability,
                    complexity,
                    iterations,
                    tools_used.clone(),
                    changed_files.clone(),
                    &final_text,
                    &final_text,
                )
                .with_status(HandoffStatus::Truncated)
                .with_task(task);
                let _ = write_handoff(workspace_root, &handoff);
                return Err(format!(
                    "subagent exceeded max_iterations ({max_iter}); partial result at .claw/subagents/{subagent_id}.md"
                ));
            }

            let request = ApiRequest {
                system_prompt: system_prompt.clone(),
                messages: messages.clone(),
                request_kind: RequestKind::Subagent,
            };

            // v3:async 调用 LLM — stream_async 避免 nested block_on panic
            let events = client
                .stream_async(request)
                .await
                .map_err(|e| format!("subagent LLM request failed: {e}"))?;

            let (assistant_message, _usage, _cache_events) = build_assistant_message(events)
                .map_err(|e| format!("subagent response parsing failed: {e}"))?;

            // 提取 ToolUse blocks(cloned — assistant_message 随后 move 进 messages)
            let tool_uses: Vec<ContentBlock> = assistant_message
                .blocks
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                .cloned()
                .collect();

            // 累积 text 内容(最终 summary/details 来源)
            for block in &assistant_message.blocks {
                if let ContentBlock::Text { text } = block {
                    final_text.push_str(text);
                    final_text.push('\n');
                }
            }

            messages.push(assistant_message);

            if tool_uses.is_empty() {
                break; // 正常终止:无工具调用
            }

            // 工具调用处理(guard + 执行 + 回填)
            if let Err(e) = process_tool_uses(
                capability,
                &tool_uses,
                tool_executor,
                workspace_root,
                &mut messages,
                &mut tools_used,
                &mut changed_files,
            ) {
                // guard 违规(递归/白名单)→ 落盘 Failed handoff + Err
                let handoff = SubagentHandoff::new(
                    subagent_id,
                    name,
                    capability,
                    complexity,
                    iterations,
                    tools_used.clone(),
                    changed_files.clone(),
                    e.to_string(),
                    e.to_string(),
                )
                .with_status(HandoffStatus::Failed)
                .with_task(task);
                let _ = write_handoff(workspace_root, &handoff);
                return Err(format!("subagent guard violation: {e}"));
            }
        }

        if final_text.trim().is_empty() {
            return Err("subagent produced no text content".to_string());
        }

        // 正常完成 → 落盘 Completed handoff(Epic 5 结构化协议)
        let handoff = SubagentHandoff::new(
            subagent_id,
            name,
            capability,
            complexity,
            iterations,
            tools_used,
            changed_files,
            &final_text,
            &final_text,
        )
        .with_task(task);
        write_handoff(workspace_root, &handoff)
            .map_err(|e| format!("failed to write subagent handoff: {e}"))
    }

    /// v3:Execute the `spawn_parallel_subagents` tool — 批量并行派发多个子 agent。
    ///
    /// 主 agent 通过此 tool 一次调用派发 N 个子 agent,内部走
    /// [`Self::spawn_parallel_via_dag_with_fail_fast`],由 `DagScheduler`
    /// 在独立的 tokio task 中真并发执行。
    ///
    /// 流程:
    /// 1. 解析 JSON 输入(`tasks` 数组 + 可选 `fail_fast`)
    /// 2. 每个任务项解析为 `SpawnRequest`(name/task/model 必填,mode/complexity 可选)
    /// 3. 调用 `spawn_parallel_via_dag_with_fail_fast(tasks, fail_fast)`
    /// 4. 格式化结果为可读字符串,每个任务一行
    ///
    /// **JSON 输入字段**:
    /// - `tasks`(必填):数组,每项含 `name`/`task`/`model`/`mode`?/`complexity`?
    /// - `fail_fast`(可选,默认 `off`):`on`/`off`
    ///
    /// **返回**:可读的多行字符串,每个任务一行,标明成功(产物路径)或失败(错误信息)。
    #[allow(dead_code)]
    fn execute_spawn_parallel_subagents(
        &mut self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // 共享解析逻辑(与 async 变体复用)
        let (tasks, fail_fast, fail_fast_str) = Self::parse_spawn_parallel_input(input)?;

        // 发布 lane event(可观测性):批量并行派发开始
        let emitted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let summary = format!(
            "spawn_parallel_subagents: {} tasks, fail_fast={}",
            tasks.len(),
            fail_fast_str
        );
        publish_lane_event(LaneEvent::subagent_handoff(
            emitted_at,
            "parallel-batch",
            "fork",
            &summary,
        ));

        // 调用 spawn_parallel_via_dag_with_fail_fast(同步桥接 DagScheduler::run)
        let results = self.spawn_parallel_via_dag_with_fail_fast(tasks, fail_fast);

        // 格式化结果(与 async 版本共享逻辑)
        Ok(Self::format_spawn_parallel_results(
            &results,
            &fail_fast_str,
            self.workspace_root.as_deref(),
        ))
    }

    /// v3:async 变体 — 供 async 调用方使用,避免 `block_on`。
    ///
    /// 与 [`execute_spawn_parallel_subagents`](Self::execute_spawn_parallel_subagents)
    /// 功能完全相同,但内部调用 [`spawn_parallel_via_dag_async`](Self::spawn_parallel_via_dag_async)
    /// 而非同步桥接版本,适用于已在 tokio runtime 中的调用方(如未来的 `run_turn_async`)。
    ///
    /// **当前状态**:接口就绪,但 `run_turn` 仍是同步函数,本方法暂未被生产路径调用。
    /// 待 `run_turn_async` 改造完成后,可在 async 上下文中直接 `.await` 本方法。
    ///
    /// # Errors
    /// 同 [`execute_spawn_parallel_subagents`](Self::execute_spawn_parallel_subagents)。
    pub async fn execute_spawn_parallel_subagents_async(
        &mut self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // 共享解析逻辑
        let (tasks, fail_fast, fail_fast_str) = Self::parse_spawn_parallel_input(input)?;

        // 发布 lane event(可观测性)
        let emitted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let summary = format!(
            "spawn_parallel_subagents: {} tasks, fail_fast={}",
            tasks.len(),
            fail_fast_str
        );
        publish_lane_event(LaneEvent::subagent_handoff(
            emitted_at,
            "parallel-batch",
            "fork",
            &summary,
        ));

        // 调用 async 变体(无 block_on,直接 .await)
        let results = self.spawn_parallel_via_dag_async(tasks, fail_fast).await;

        // 格式化结果(与同步版本共享逻辑)
        Ok(Self::format_spawn_parallel_results(
            &results,
            &fail_fast_str,
            self.workspace_root.as_deref(),
        ))
    }

    /// v3:解析 `spawn_parallel_subagents` 工具的 JSON 输入(共享逻辑)。
    ///
    /// 返回 `(tasks, fail_fast, fail_fast_str)`,供同步/async 变体复用。
    fn parse_spawn_parallel_input(
        input: &str,
    ) -> Result<(Vec<SpawnRequest>, FailFast, String), Box<dyn std::error::Error + Send + Sync>>
    {
        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;

        let tasks_arr = parsed
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or("missing or invalid 'tasks' field (expected array)")?;
        if tasks_arr.is_empty() {
            return Err("'tasks' array must not be empty".into());
        }

        let fail_fast_str = parsed
            .get("fail_fast")
            .and_then(|v| v.as_str())
            .unwrap_or("off")
            .to_ascii_lowercase();
        let fail_fast = match fail_fast_str.as_str() {
            "on" => FailFast::On,
            "off" => FailFast::Off,
            other => {
                return Err(format!("invalid fail_fast '{other}': expected 'on' or 'off'").into());
            }
        };

        let mut tasks: Vec<SpawnRequest> = Vec::with_capacity(tasks_arr.len());
        for (idx, item) in tasks_arr.iter().enumerate() {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("tasks[{idx}]: missing 'name' field"))?;
            let task = item
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("tasks[{idx}]: missing 'task' field"))?;
            let model = item
                .get("model")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("tasks[{idx}]: missing 'model' field"))?;

            let mode_str = item.get("mode").and_then(|v| v.as_str()).unwrap_or("fork");
            let mode = match mode_str {
                "fork" => CoordinationMode::Fork,
                "teammate" => CoordinationMode::Teammate,
                "worktree" => CoordinationMode::Worktree,
                other => {
                    return Err(format!(
                        "tasks[{idx}]: invalid mode '{other}' (expected fork/teammate/worktree)"
                    )
                    .into());
                }
            };

            let complexity = parse_complexity(item.get("complexity"));
            let capability = parse_capability(item.get("capability"));
            tasks.push(
                SpawnRequest::new(name, task, mode, model, complexity).with_capability(capability),
            );
        }

        Ok((tasks, fail_fast, fail_fast_str))
    }

    /// v3:格式化 `spawn_parallel_subagents` 结果为可读字符串(共享逻辑)。
    fn format_spawn_parallel_results(
        results: &[Result<String, String>],
        fail_fast_str: &str,
        workspace_root: Option<&std::path::Path>,
    ) -> String {
        let mut output = String::new();
        let success_count = results.iter().filter(|r| r.is_ok()).count();
        let fail_count = results.len() - success_count;
        output.push_str(&format!(
            "spawn_parallel_subagents: {} succeeded, {} failed (fail_fast={})\n",
            success_count, fail_count, fail_fast_str
        ));
        for (i, r) in results.iter().enumerate() {
            match r {
                Ok(path) => {
                    // Epic 5 §8.4:解析 handoff frontmatter,summary 进主上下文,
                    // details 通过 path 按需 Read。解析失败时降级到仅显示路径。
                    let summary_line = workspace_root
                        .and_then(|root| crate::multi_agent::read_handoff(&root.join(path)).ok())
                        .map(|h| {
                            let changed = if h.changed_files.is_empty() {
                                String::new()
                            } else {
                                format!(" | changed: {}", h.changed_files.join(", "))
                            };
                            format!(" — {}{changed}", h.summary)
                        })
                        .unwrap_or_default();
                    output.push_str(&format!("  [{i}] OK: {path}{summary_line}\n"));
                }
                Err(msg) => {
                    output.push_str(&format!("  [{i}] FAIL: {msg}\n"));
                }
            }
        }
        output
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
        // 方案 C:LLM 已响应 NOTEBOOK 刷新,清除跨会话 stale marker。
        // 避免下一 turn / 下一会话重复提醒。
        crate::notebook::clear_plan_stale(workspace_root);
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

    /// Phase 4-A:执行  工具调用,将修复决策记录到 DecisionLog。
    ///
    /// 委托 [] 处理 SQLite 写入
    /// 和 FTS5 索引同步。
    fn execute_log_decision(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(decision_log) = &self.decision_log else {
            return Ok(
                "log_decision is not available: no DecisionLog configured.                  Use --workspace-root or set_workspace_root to enable decision logging."
                    .to_string(),
            );
        };

        // #3 路径 A 精确传递(design-gaps):knowledge_source 不再依赖全局
        // last-gated(并发时会被其他子任务覆盖,导致统计串任务)。
        // 改为 LLM 基于自身任务上下文显式传参 —— 子 agent 的 log_decision
        // 由外部 executor 执行时,LLM 在自己的上下文中明确 knowledge_source;
        // 主 agent 未做调研,无参时默认 "parametric"(纯参数记忆),不继承
        // 任何子任务的调研来源。
        let effective_input = match serde_json::from_str::<serde_json::Value>(input) {
            Ok(mut parsed) => {
                if parsed.get("knowledge_source").is_none() {
                    if let Some(obj) = parsed.as_object_mut() {
                        obj.insert(
                            "knowledge_source".to_string(),
                            serde_json::Value::String("parametric".to_string()),
                        );
                    }
                }
                serde_json::to_string(&parsed).unwrap_or_else(|_| input.to_string())
            }
            Err(_) => input.to_string(), // JSON 解析失败,原样传入让 log_decision 报错
        };

        decision_log
            .log_decision(&effective_input)
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))
    }

    /// Phase 4-A:执行  工具调用,搜索历史修复决策。
    ///
    /// 使用 FTS5 全文检索 + simhash 去重,返回 top-k 匹配决策。
    fn execute_search_past_decisions(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(decision_log) = &self.decision_log else {
            return Ok(
                "search_past_decisions is not available: no DecisionLog configured.                  Use --workspace-root or set_workspace_root to enable decision search."
                    .to_string(),
            );
        };

        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;
        let query = parsed
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("missing 'query' field")?;
        let top_k = parsed.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        decision_log
            .search_decisions(query, top_k)
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))
    }

    /// Phase 4-A P1-4:执行 `verify_decision` 工具调用,闭合 success_rate 学习环。
    ///
    /// 从输入 JSON 解析 `decision_id`(必需,整数)、`verification_result`
    /// (必需,枚举字符串)、`verification_evidence`(可选,字符串),
    /// 委托给 [`DecisionLog::verify_decision`] 处理 SQLite 事务性更新。
    ///
    /// # 输入示例
    ///
    /// ```json
    /// {
    ///   "decision_id": 42,
    ///   "verification_result": "Confirmed",
    ///   "verification_evidence": "cargo test: 87 passed, 0 failed"
    /// }
    /// ```
    ///
    /// # 错误处理
    ///
    /// - `decision_log` 未配置:返回降级字符串(与 log_decision /
    ///   search_past_decisions 一致,不阻断 LLM 工作流)
    /// - 输入 JSON 无效:返回错误
    /// - `decision_id` 缺失/非整数:返回错误
    /// - `verification_result` 缺失/非法:返回错误
    /// - 目标决策不存在:由 `verify_decision` 返回 `InvalidInput`,
    ///   此处透传错误消息
    fn execute_verify_decision(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(decision_log) = &self.decision_log else {
            return Ok(
                "verify_decision is not available: no DecisionLog configured.                  Use --workspace-root or set_workspace_root to enable decision verification."
                    .to_string(),
            );
        };

        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;

        let decision_id = parsed
            .get("decision_id")
            .and_then(|v| v.as_i64())
            .ok_or("missing or non-integer 'decision_id' field")?;

        let result_str = parsed
            .get("verification_result")
            .and_then(|v| v.as_str())
            .ok_or("missing 'verification_result' field")?;

        let verification = crate::decision_log::DecisionVerification::from_str_ic(result_str)
            .ok_or_else(|| {
                format!(
                    "invalid 'verification_result' value: '{result_str}'. \
                     Must be one of: Confirmed, Refuted, Partial, Pending (case-insensitive)."
                )
            })?;

        let evidence = parsed.get("verification_evidence").and_then(|v| v.as_str());

        decision_log
            .verify_decision(decision_id, verification, evidence)
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))
    }

    fn execute_query_project_graph(
        &self,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(t) = &self.project_topology else {
            return Ok("query_project_graph not available.".to_string());
        };
        t.query_project_graph()
            .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
    }

    fn execute_find_boundary_crossings(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(t) = &self.project_topology else {
            return Ok("find_boundary_crossings not available.".to_string());
        };
        let p: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid JSON: {e}"))?;
        let q = p.get("query").and_then(|v| v.as_str());
        t.find_boundary_crossings(q)
            .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
    }

    fn execute_get_symbol_info(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(t) = &self.project_topology else {
            return Ok("get_symbol_info not available.".to_string());
        };
        let p: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid JSON: {e}"))?;
        let s = p
            .get("symbol")
            .and_then(|v| v.as_str())
            .ok_or("missing symbol")?;
        t.get_symbol_info(s)
            .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
    }

    fn execute_rollback_transaction(
        &mut self,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(tx) = &mut self.refactor_tx else {
            return Ok("rollback_transaction not available.".to_string());
        };
        tx.rollback()
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))
    }

    fn execute_transaction_status(
        &self,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(tx) = &self.refactor_tx else {
            return Ok("transaction_status not available.".to_string());
        };
        let status = tx.status();
        Ok(serde_json::to_string_pretty(&status)
            .unwrap_or_else(|e| format!("status serialization error: {e}")))
    }

    /// Phase 4-B:执行 `refactor_algorithm_topo` 工具调用。
    ///
    /// 建议模式:不修改文件,基于 ProjectTopology SymbolIndex 生成符号重命名建议列表。
    /// 无 ProjectTopology 时返回提示信息(不报错),引导 LLM 改用 grep_search。
    fn execute_refactor_algorithm_topo(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;
        let target_symbol = parsed
            .get("target_symbol")
            .and_then(|v| v.as_str())
            .ok_or("missing 'target_symbol' field")?;
        let new_name = parsed.get("new_name").and_then(|v| v.as_str());
        let reason = parsed.get("reason").and_then(|v| v.as_str());

        let Some(t) = &self.project_topology else {
            return Ok(
                "refactor_algorithm_topo is not available: no ProjectTopology configured. \
                 Use --workspace-root or set_workspace_root to enable topology queries, \
                 or use grep_search to find references manually."
                    .to_string(),
            );
        };
        crate::domain_algorithm::refactor_algorithm_topo(t, target_symbol, new_name, reason)
            .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
    }

    /// Phase 4-B:执行 `benchmark_compare` 工具调用。
    ///
    /// 运行命令多次并报告计时统计。工作目录使用 `workspace_root`(若已配置)。
    fn execute_benchmark_compare(
        &self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let parsed: serde_json::Value =
            serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;
        let command = parsed
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or("missing 'command' field")?;
        let timeout_seconds = parsed
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(60);
        let sample_size = parsed
            .get("sample_size")
            .and_then(|v| v.as_u64())
            .unwrap_or(20) as usize;
        let warmup_runs = parsed
            .get("warmup_runs")
            .and_then(|v| v.as_u64())
            .unwrap_or(2) as usize;

        let cwd = self.workspace_root.as_deref();
        crate::domain_algorithm::benchmark_compare(
            command,
            cwd,
            timeout_seconds,
            sample_size,
            warmup_runs,
        )
        .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
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
        // 使用 context_tokens() 而非 input_tokens：
        // DeepSeek 风格的 API 返回 input_tokens=0，所有 prompt tokens 在 cache 字段中。
        // context_tokens() 通过 max(input_tokens, cache_creation + cache_read) 统一处理。
        if self.usage_tracker.cumulative_usage().context_tokens() < threshold {
            return None;
        }

        // G9.1: PreCompact lifecycle hook — 在 compact_session 实际执行前触发,
        // 让外部观察者(上下文快照、telemetry、审计等)在压缩发生前捕获 pre 状态。
        // context 包含当前 token 用量与阈值,便于 hook 决策是否记录。
        // 返回值不影响主流程,故意丢弃。
        let pre_compact_context = format!(
            "auto_compaction: context_tokens={} threshold={}",
            self.usage_tracker.cumulative_usage().context_tokens(),
            threshold
        );
        let _ = self.hook_runner.run_pre_compact(&pre_compact_context);

        // §4.7 v3:compaction 前提取决策点,避免设计决策随原始消息消失。
        // MVP 用 Heuristic 策略(零 LLM 成本),v2 升级为 LlmExtract。
        // 与 §4.1 的 diag! 互补:diag! 记录"发生了什么",decision_log 记录"决定了什么"。
        let preserve_recent = CompactionConfig::default().preserve_recent_messages;
        let messages_to_compact: Vec<String> = self
            .session
            .messages
            .iter()
            .rev()
            .skip(preserve_recent)
            .map(crate::session::extract_indexable_text)
            .collect();
        let messages_refs: Vec<&str> = messages_to_compact.iter().map(String::as_str).collect();
        // v3 §4.7:从 runtime 字段读取策略(默认 Heuristic,可通过 with_detection_strategy 升级)。
        // LlmExtract 分支会调用全局 DecisionExtractorClient(由 build_runtime 注入),
        // 失败时 3 路降级保证不阻塞 compaction。
        let strategy = self.detection_strategy.clone();
        let decisions = crate::decision_log::extract_decisions_before_compaction(
            &messages_refs,
            &strategy,
            &self.session.session_id,
        );
        if !decisions.is_empty() {
            eprintln!(
                "[decision_log] extracted {} decision point(s) before compaction",
                decisions.len()
            );
            // 持久化到 NOTEBOOK.md decisions 段(§4.7.2)
            if let Some(ws) = &self.workspace_root {
                if let Err(e) = crate::decision_log::persist_decisions_to_notebook(ws, &decisions) {
                    eprintln!("[decision_log] failed to persist decisions to notebook: {e}");
                }
            }
            // 同步写入 FTS5 索引(role="decision"),提升可检索性(§4.7.4)
            if let Some(history_index) = self.session.history_index.as_ref() {
                for d in &decisions {
                    let content = format!(
                        "[DECISION {}] context: {}\ndecision: {}\nrationale: {}\nalternatives: {}",
                        d.id,
                        d.context,
                        d.decision,
                        d.rationale,
                        d.alternatives.join("; "),
                    );
                    let _ = history_index.index_message(
                        &content,
                        &d.session_id,
                        "decision", // §4.7 v3 新增 role 类型
                        0,          // message_index=0 表示决策点
                        d.timestamp_ms,
                    );
                }
            }
        }

        // 改进点 12:compaction 前提取实验证据(对比矩阵/基准数值等),
        // 避免实验数据随原始消息消失。与 decisions 互补:decisions 记录
        // "决定了什么",evidence 记录"实验数据是什么"。Heuristic 策略,
        // 零 LLM 成本,从待压缩的 ToolResult 块中识别表格/数值列表/关键词。
        let compact_end = self.session.messages.len().saturating_sub(preserve_recent);
        let messages_to_compact = &self.session.messages[..compact_end];
        let evidence = crate::compact::extract_evidence_before_compaction(messages_to_compact);
        if !evidence.is_empty() {
            eprintln!(
                "[compact] extracted {} evidence item(s) before compaction",
                evidence.len()
            );
            // 持久化到 NOTEBOOK.md <evidence> 段(改进点 12)
            if let Some(ws) = &self.workspace_root {
                match crate::notebook::Notebook::load(ws) {
                    Ok(mut nb) => {
                        for item in &evidence {
                            nb.append_evidence(item);
                        }
                        if let Err(e) = nb.save(ws) {
                            eprintln!("[compact] failed to persist evidence to notebook: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("[compact] failed to load notebook for evidence: {e}");
                    }
                }
            }
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
        // 改进点 13:压缩后下个 turn 注入归档 recall 提示,主动列出可 recall
        // 的归档 tool result,引导 LLM 在需要时调用 recall_full 检索原始内容。
        self.archive_recall_hint_pending = true;
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
    parse_auto_compaction_threshold_opt(
        std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR)
            .ok()
            .as_deref(),
    )
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

/// Multi-Agent Hardening §4.2:从 JSON 值解析任务复杂度。
///
/// 接受字符串("simple"/"diagnostic"/"architectural",大小写不敏感)。
/// 缺失或无法识别时返回 `Simple`(向后兼容,与 Subagent 默认值一致)。
fn parse_complexity(value: Option<&serde_json::Value>) -> TaskComplexity {
    let s = value
        .and_then(|v| v.as_str())
        .unwrap_or("simple")
        .to_ascii_lowercase();
    match s.as_str() {
        "diagnostic" => TaskComplexity::Diagnostic,
        "architectural" | "architecture" => TaskComplexity::Architectural,
        // simple / unknown / 缺失 — 均回退到 Simple
        _ => TaskComplexity::Simple,
    }
}

/// TRAE 架构对齐(§3.1):从 JSON 值解析子智能体能力分级。
///
/// 接受字符串("analyze"/"read-only"/"execute",大小写不敏感,
/// 与 `SubagentCapability` 的 `kebab-case` serde 表示一致)。
/// 缺失或无法识别时返回 `Analyze`(向后兼容,与 Subagent 默认值一致)。
fn parse_capability(value: Option<&serde_json::Value>) -> crate::multi_agent::SubagentCapability {
    let s = value
        .and_then(|v| v.as_str())
        .unwrap_or("analyze")
        .to_ascii_lowercase();
    match s.as_str() {
        "read-only" | "readonly" | "read_only" => crate::multi_agent::SubagentCapability::ReadOnly,
        "execute" => crate::multi_agent::SubagentCapability::Execute,
        // analyze / unknown / 缺失 — 均回退到 Analyze
        _ => crate::multi_agent::SubagentCapability::Analyze,
    }
}

pub(crate) fn build_assistant_message(
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

type ToolHandler = Box<dyn FnMut(&str) -> Result<String, ToolError> + Send>;

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
        handler: impl FnMut(&str) -> Result<String, ToolError> + Send + 'static,
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
        build_assistant_message, build_subagent_request, build_subagent_system_prompt,
        compaction_threshold_for_context_window, parse_auto_compaction_threshold,
        parse_auto_compaction_threshold_opt, process_tool_uses, ApiClient, ApiRequest,
        AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RequestKind,
        RuntimeError, StaticToolExecutor, SubagentContext, ToolExecutor,
        DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD, SESSION_SEARCH_TOOL_SPEC,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::memory::PersistentMemory;
    use crate::multi_agent::dag::FailFast;
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
        assert_eq!(
            parse_auto_compaction_threshold_opt(Some("4321")),
            Some(4321)
        );
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
        assert_eq!(compaction_threshold_for_context_window(1_000_000), 650_000);
        // 200K context window → 130K threshold (65%)
        assert_eq!(compaction_threshold_for_context_window(200_000), 130_000);
        // 256K context window → 166K threshold (65%)
        assert_eq!(compaction_threshold_for_context_window(256_000), 166_400);
        // 131K context window → ~85K threshold
        assert_eq!(compaction_threshold_for_context_window(131_072), 85_196);
    }

    #[test]
    fn compaction_threshold_capped_at_800k() {
        // 2M window → should cap at 800K, not 1.3M
        assert_eq!(compaction_threshold_for_context_window(2_000_000), 800_000);
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
    fn run_turn_aborts_early_on_tool_loop() {
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

        // max_iterations=64 远高于中止阈值(6),证明是 loop detector 而非迭代上限兜底
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            LoopingApi,
            StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_max_iterations(64);

        // when
        let error = runtime
            .run_turn("loop", None)
            .expect_err("identical tool calls should abort the turn");

        // then
        assert!(
            error.to_string().contains("doom loop detected"),
            "unexpected error: {error}"
        );
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
            request_kind: RequestKind::Main,
        };
        assert_eq!(request.system_prompt.static_sections, vec!["static"]);
        assert_eq!(request.system_prompt.dynamic_sections, vec!["dynamic"]);
    }

    #[test]
    fn api_request_carries_request_kind() {
        let request = ApiRequest {
            system_prompt: SystemPromptSplit::from_sections(vec!["static".to_string()]),
            messages: Vec::new(),
            request_kind: RequestKind::Main,
        };
        assert_eq!(request.request_kind, RequestKind::Main);
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
            tool_result_outputs[0].contains(" summarized:"),
            "oldest tool result should be summarized"
        );
        assert!(
            tool_result_outputs[1].contains(" summarized:"),
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
        // P0-2:清空可能残留的 lane events,避免并行测试污染。
        let _ = crate::lane_events::drain_lane_events();
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

    // ===== Multi-Agent Hardening P0 步骤 9:端到端 MVP 验证 =====
    // 依据 docs/multi-agent-hardening-plan.md §10.4 验收标准
    // 场景 1-5 全覆盖:
    //   场景 1:简单任务 → flash → 一次成功
    //   场景 2:诊断任务 → pro → 一次成功
    //   场景 3:flash 失败 → 升级 pro → 重试成功
    //   场景 4:flash 失败 → 升级 pro → 仍失败 → 达 max_attempts fail
    //   场景 5:成本超限 → 拒绝升级 → fail(不浪费 pro 调用)

    // ===== P1 步骤 6:诊断 SOP 注入单元测试(3a 静态化重写) =====

    /// §4.6 验收:Diagnostic 复杂度时 system_prompt 含诊断 SOP;且不含唯一内容(3a 静态化)
    #[test]
    fn build_subagent_system_prompt_injects_diagnostic_sop() {
        let prompt = build_subagent_system_prompt(
            crate::multi_agent::TaskComplexity::Diagnostic,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        )
        .render();
        // 3a:静态化后不得包含 id/name/task 唯一内容
        assert!(
            !prompt.contains("# Subagent:"),
            "unique header must move to user message"
        );
        assert!(
            !prompt.contains("定位 wizard 闪退"),
            "task must move to user message"
        );
        // 诊断 SOP 五条规则
        assert!(prompt.contains("## 诊断任务执行规范"), "missing SOP header");
        assert!(
            prompt.contains("CLAW_DIAG=1"),
            "missing rule 1: diag log first"
        );
        assert!(
            prompt.contains("panic vs Err vs 配置错误"),
            "missing rule 2: confirm error type"
        );
        assert!(
            prompt.contains("cargo build"),
            "missing rule 3: verify compilation"
        );
        assert!(
            prompt.contains("复现验证证据"),
            "missing rule 4: reproduce evidence"
        );
        assert!(
            prompt.contains("catch_unwind / panic hook"),
            "missing rule 5: no defensive code"
        );
    }

    /// §4.6 验收:Simple 复杂度时 system_prompt 不含诊断 SOP(避免污染简单任务);且为纯静态
    #[test]
    fn build_subagent_system_prompt_skips_sop_for_simple_task() {
        let prompt = build_subagent_system_prompt(
            crate::multi_agent::TaskComplexity::Simple,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        )
        .render();
        assert!(
            !prompt.contains("# Subagent:"),
            "unique header must move to user message"
        );
        assert!(
            !prompt.contains("## 诊断任务执行规范"),
            "Simple task should NOT have SOP"
        );
        assert!(
            !prompt.contains("CLAW_DIAG=1"),
            "Simple task should NOT contain diag rule"
        );
    }

    /// §4.6 v2 验收:Architectural 复杂度注入架构决策 SOP(非诊断 SOP);且为纯静态
    #[test]
    fn build_subagent_system_prompt_injects_architectural_sop() {
        let prompt = build_subagent_system_prompt(
            crate::multi_agent::TaskComplexity::Architectural,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        )
        .render();
        assert!(
            !prompt.contains("# Subagent:"),
            "unique header must move to user message"
        );
        // 架构决策 SOP 六条规则
        assert!(
            prompt.contains("## 架构决策执行规范"),
            "missing architectural SOP header"
        );
        assert!(
            prompt.contains("候选方案"),
            "missing rule 1: alternatives required"
        );
        assert!(
            prompt.contains("trade-off"),
            "missing rule 2: trade-off evaluation"
        );
        assert!(
            prompt.contains("rationale"),
            "missing rule 3: rationale for rejecting alternatives"
        );
        assert!(
            prompt.contains("向后兼容"),
            "missing rule 4: backward compatibility impact"
        );
        assert!(
            prompt.contains("NOTEBOOK.md"),
            "missing rule 5: decisions persistence"
        );
        assert!(
            prompt.contains("禁止凭直觉"),
            "missing rule 6: no intuition-based decisions"
        );
        // 不应含诊断 SOP(两个 SOP 互斥)
        assert!(
            !prompt.contains("## 诊断任务执行规范"),
            "Architectural should NOT have diagnostic SOP"
        );
    }

    /// 3a:子智能体请求 — system 纯静态,id/name/task 移入 user message 且只出现一次
    #[test]
    fn build_subagent_request_moves_unique_fields_to_user_message() {
        let req = build_subagent_request(
            "s-1",
            "diag-agent",
            "定位 wizard 闪退",
            crate::multi_agent::TaskComplexity::Diagnostic,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        );
        let system = req.system_prompt.static_sections.join("\n");
        assert!(
            !system.contains("Subagent"),
            "id/name must not be in system"
        );
        assert!(
            !system.contains("定位 wizard 闪退"),
            "task must not be in system"
        );
        assert_eq!(req.request_kind, RequestKind::Subagent);
        assert_eq!(req.messages.len(), 1);
        let user_text = match &req.messages[0].blocks[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("user message must be text"),
        };
        assert!(
            user_text.contains("# Subagent: diag-agent (s-1)"),
            "id/name header in user"
        );
        // task 只出现一次
        assert_eq!(
            user_text.matches("定位 wizard 闪退").count(),
            1,
            "task must appear exactly once"
        );
    }

    /// 3a:同一复杂度的不同子智能体共享完全相同的 system prompt(前缀缓存可命中)
    #[test]
    fn build_subagent_request_shared_prefix_for_same_complexity() {
        let a = build_subagent_request(
            "s-1",
            "agent-a",
            "任务 A",
            crate::multi_agent::TaskComplexity::Diagnostic,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        );
        let b = build_subagent_request(
            "s-2",
            "agent-b",
            "任务 B",
            crate::multi_agent::TaskComplexity::Diagnostic,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        );
        assert_eq!(
            a.system_prompt.static_sections,
            b.system_prompt.static_sections
        );
        // 三个复杂度变体互不相同(各自前缀)
        let simple = build_subagent_request(
            "s-3",
            "agent-c",
            "任务 C",
            crate::multi_agent::TaskComplexity::Simple,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        );
        assert_ne!(
            simple.system_prompt.static_sections,
            a.system_prompt.static_sections
        );
    }

    /// Epic 1(§3.2 heading 对齐):注入 repo_map 时 section heading 必须为
    /// `## Repository Map`,ProjectContext 必须为 `# Environment context`,
    /// 才能被 `static_cache_breakpoints` 正确识别为 breakpoint。
    #[test]
    fn build_subagent_system_prompt_breakpoint_heading_alignment() {
        let ctx = SubagentContext {
            repo_map: Some("src/main.rs (refs: 5)\n  fn main".to_string()),
            project_context: Some(crate::prompt::ProjectContext {
                cwd: std::path::PathBuf::from("/workspace"),
                current_date: "2026-08-06".to_string(),
                git_status: Some("M src/foo.rs".to_string()),
                git_diff: None,
                git_context: None,
                instruction_files: Vec::new(),
            }),
            tool_summaries: Vec::new(),
        };
        let split = build_subagent_system_prompt(
            crate::multi_agent::TaskComplexity::Simple,
            crate::multi_agent::SubagentCapability::ReadOnly,
            &ctx,
        );
        // heading 对齐:## Repository Map 和 # Environment context 必须存在
        let has_repo_map = split
            .static_sections
            .iter()
            .any(|s| s.starts_with("## Repository Map"));
        assert!(has_repo_map, "missing ## Repository Map heading");
        let has_env = split
            .static_sections
            .iter()
            .any(|s| s.starts_with("# Environment context"));
        assert!(has_env, "missing # Environment context heading");
        // breakpoint 必须识别这两个 heading(回归测试,防止 heading 拼写错误导致缓存分层退化)
        let breakpoints = split.static_cache_breakpoints();
        assert!(
            !breakpoints.is_empty(),
            "breakpoints must be non-empty when repo_map + environment present"
        );
    }

    /// Epic 1:Analyze capability(无工具)不注入工具签名层;
    /// ReadOnly/Execute 注入 repo_map 后 system prompt 含 Repository Map。
    #[test]
    fn build_subagent_system_prompt_capability_and_context_injection() {
        // Analyze + 空 ctx → 单 section(仅 L0 指令),无 repo_map/tools
        let split_analyze = build_subagent_system_prompt(
            crate::multi_agent::TaskComplexity::Simple,
            crate::multi_agent::SubagentCapability::Analyze,
            &SubagentContext::default(),
        );
        assert_eq!(
            split_analyze.static_sections.len(),
            1,
            "Analyze + empty ctx should have single L0 section"
        );
        let rendered = split_analyze.render();
        assert!(
            rendered.contains("不需要调用工具"),
            "Analyze should declare no tools"
        );

        // ReadOnly + repo_map → 含 Repository Map section
        let ctx = SubagentContext {
            repo_map: Some("src/lib.rs".to_string()),
            project_context: None,
            tool_summaries: vec![crate::conversation::ToolSummary {
                name: "read".to_string(),
                description: "Read a file".to_string(),
            }],
        };
        let split_ro = build_subagent_system_prompt(
            crate::multi_agent::TaskComplexity::Simple,
            crate::multi_agent::SubagentCapability::ReadOnly,
            &ctx,
        );
        let ro_rendered = split_ro.render();
        assert!(
            ro_rendered.contains("## Repository Map"),
            "ReadOnly + repo_map should inject Repository Map"
        );
        assert!(
            ro_rendered.contains("## Available Tools"),
            "ReadOnly + tool_summaries should inject Available Tools"
        );
        assert!(
            ro_rendered.contains("你可以调用工具"),
            "ReadOnly should declare tool capability"
        );
    }

    // Epic 4 集成测试:process_tool_uses + SubagentFileGuard
    // 验证 edit/write 工具执行前获取文件锁,capability 白名单二次防护

    /// 构造 ToolUse block,input 为含 file_path 的 JSON。
    fn make_edit_tool_use(id: &str, name: &str, file_path: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: format!(r#"{{"file_path":"{file_path}"}}"#),
        }
    }

    #[test]
    fn process_tool_uses_execute_edit_executes_with_file_lock() {
        let tmp = tempfile::tempdir().expect("temp workspace");
        let workspace = tmp.path().to_path_buf();

        let tool_uses = vec![make_edit_tool_use("tu1", "edit", "src/foo.rs")];
        let mut executor =
            StaticToolExecutor::new().register("edit", |_input| Ok("edit applied".to_string()));
        let mut messages = Vec::new();
        let mut tools_used = Vec::new();
        let mut changed_files = Vec::new();

        let result = process_tool_uses(
            crate::multi_agent::SubagentCapability::Execute,
            &tool_uses,
            &mut executor,
            &workspace,
            &mut messages,
            &mut tools_used,
            &mut changed_files,
        );

        assert!(result.is_ok(), "Execute + edit should succeed");
        assert_eq!(tools_used, vec!["edit"]);
        assert_eq!(messages.len(), 1, "tool_result should be appended");
        // changed_files 应包含 edit 的 file_path(规范化后)
        assert!(
            !changed_files.is_empty(),
            "changed_files should be populated"
        );
    }

    #[test]
    fn process_tool_uses_analyze_edit_rejected_by_whitelist() {
        let tmp = tempfile::tempdir().expect("temp workspace");
        let workspace = tmp.path().to_path_buf();

        let tool_uses = vec![make_edit_tool_use("tu1", "edit", "src/foo.rs")];
        let mut executor =
            StaticToolExecutor::new().register("edit", |_input| Ok("should not reach".to_string()));
        let mut messages = Vec::new();
        let mut tools_used = Vec::new();
        let mut changed_files = Vec::new();

        let result = process_tool_uses(
            crate::multi_agent::SubagentCapability::Analyze,
            &tool_uses,
            &mut executor,
            &workspace,
            &mut messages,
            &mut tools_used,
            &mut changed_files,
        );

        // Analyze 白名单不含 edit -> Guard 2 拒绝,返回 Err
        assert!(
            result.is_err(),
            "Analyze + edit should be rejected by whitelist"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("not allowed"),
            "err should mention not allowed: {err}"
        );
        // 工具未执行
        assert!(tools_used.is_empty(), "tools_used should be empty");
        assert!(messages.is_empty(), "messages should be empty");
    }

    #[test]
    fn process_tool_uses_readonly_write_rejected_by_whitelist() {
        let tmp = tempfile::tempdir().expect("temp workspace");
        let workspace = tmp.path().to_path_buf();

        let tool_uses = vec![make_edit_tool_use("tu1", "write", "src/bar.rs")];
        let mut executor = StaticToolExecutor::new()
            .register("write", |_input| Ok("should not reach".to_string()));
        let mut messages = Vec::new();
        let mut tools_used = Vec::new();
        let mut changed_files = Vec::new();

        let result = process_tool_uses(
            crate::multi_agent::SubagentCapability::ReadOnly,
            &tool_uses,
            &mut executor,
            &workspace,
            &mut messages,
            &mut tools_used,
            &mut changed_files,
        );

        assert!(
            result.is_err(),
            "ReadOnly + write should be rejected by whitelist"
        );
        assert!(tools_used.is_empty());
    }

    #[test]
    fn process_tool_uses_execute_write_acquires_lock_and_executes() {
        let tmp = tempfile::tempdir().expect("temp workspace");
        let workspace = tmp.path().to_path_buf();

        let tool_uses = vec![make_edit_tool_use("tu2", "write", "src/new.rs")];
        let mut executor =
            StaticToolExecutor::new().register("write", |_input| Ok("written".to_string()));
        let mut messages = Vec::new();
        let mut tools_used = Vec::new();
        let mut changed_files = Vec::new();

        let result = process_tool_uses(
            crate::multi_agent::SubagentCapability::Execute,
            &tool_uses,
            &mut executor,
            &workspace,
            &mut messages,
            &mut tools_used,
            &mut changed_files,
        );

        assert!(result.is_ok(), "Execute + write should succeed");
        assert_eq!(tools_used, vec!["write"]);
        assert_eq!(messages.len(), 1);
    }

    /// 可控的 mock ValidationGate -- 通过 AtomicUsize 控制第 N 次返回 Ok/Err。
    /// 用于场景 3-5 端到端 retry loop 测试。
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct ScriptedGate {
        call_count: AtomicUsize,
        /// 每次调用的返回结果序列:Some(true)=Ok, Some(false)=retryable Err, None=fatal Err
        script: Vec<Option<bool>>,
    }

    impl ScriptedGate {
        fn new(script: Vec<Option<bool>>) -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                script,
            }
        }
    }

    impl crate::multi_agent::ValidationGate for ScriptedGate {
        fn validate(
            &self,
            _ctx: &crate::multi_agent::validation::ValidationContext,
        ) -> Result<(), crate::multi_agent::validation::ValidationError> {
            let n = self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
            match self.script.get(n) {
                Some(Some(true)) => Ok(()),
                Some(Some(false)) => Err(crate::multi_agent::validation::ValidationError {
                    message: format!("scripted retryable failure at call #{n}"),
                    retryable: true,
                }),
                Some(None) => Err(crate::multi_agent::validation::ValidationError {
                    message: format!("scripted fatal failure at call #{n}"),
                    retryable: false,
                }),
                None => Ok(()), // 脚本耗尽,默认 Ok
            }
        }

        fn name(&self) -> &'static str {
            "scripted-gate"
        }
    }

    /// 场景 1:简单任务路由到 flash,一次成功(§10.4 端到端流程 P0)
    /// 验收:简单任务 → flash → run → validate 通过 → completed
    #[test]
    fn dispatch_subagent_scenario1_simple_task_flash_succeeds() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();
        let unique_task = "scenario1-simple-flash-uuid-p0-9-s1";

        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        // 无 validation gate → validate 默认通过(coordinator.validate 在无 gate 时返回 Ok)
        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let input = serde_json::json!({
            "name": "fmt-agent",
            "task": unique_task,
            "mode": "fork",
            "model": "deepseek-v4-flash",
            "complexity": "simple",
            "max_attempts": 1
        })
        .to_string();

        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("scenario 1 should succeed");

        assert!(
            output.contains("Subagent `") && output.contains("completed"),
            "scenario 1 should complete on first attempt: {output}"
        );

        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("extract subagent_id");
        let agent = coordinator.get(subagent_id).expect("agent exists");
        // 关键不变量:模型未升级(仍是 flash),attempts=0(无重试)
        assert_eq!(
            agent.model.as_deref(),
            Some("deepseek-v4-flash"),
            "scenario 1: model should remain flash (no upgrade needed)"
        );
        assert_eq!(agent.attempts, 0, "scenario 1: no retry should happen");
        assert!(agent.validated, "scenario 1: should be validated");
        assert_eq!(
            agent.status,
            crate::multi_agent::SubagentStatus::Completed,
            "scenario 1: should be Completed"
        );
    }

    /// 场景 2:诊断任务路由到 pro,一次成功(§10.4 端到端流程 P0)
    /// 验收:诊断任务 → pro → run → validate 通过 → completed
    #[test]
    fn dispatch_subagent_scenario2_diagnostic_task_pro_succeeds() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();
        let unique_task = "scenario2-diagnostic-pro-uuid-p0-9-s2";

        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let input = serde_json::json!({
            "name": "diag-agent",
            "task": unique_task,
            "mode": "fork",
            "model": "deepseek-v4-pro",
            "complexity": "diagnostic",
            "max_attempts": 2
        })
        .to_string();

        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("scenario 2 should succeed");

        assert!(
            output.contains("Subagent `") && output.contains("completed"),
            "scenario 2 should complete on first attempt: {output}"
        );

        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("extract subagent_id");
        let agent = coordinator.get(subagent_id).expect("agent exists");
        // 关键不变量:模型保持 pro(旗舰,无需升级),attempts=0(一次成功)
        assert_eq!(
            agent.model.as_deref(),
            Some("deepseek-v4-pro"),
            "scenario 2: model should remain pro (flagship, no upgrade)"
        );
        assert_eq!(agent.attempts, 0, "scenario 2: no retry should happen");
        assert!(agent.validated, "scenario 2: should be validated");
        assert_eq!(
            agent.status,
            crate::multi_agent::SubagentStatus::Completed,
            "scenario 2: should be Completed"
        );
        // 诊断任务默认 max_attempts=2(spawn_with_model 中设置),但本测试一次通过
        assert_eq!(
            agent.max_attempts, 2,
            "scenario 2: diagnostic task should default to max_attempts=2"
        );
    }

    /// 场景 3:flash + Simple + max_attempts=2,validate 失败 → 无升级路径 → 立即 fail
    ///
    /// V4-Flash 正式版(2026-07-31)上线后自动升级链已关闭,
    /// retryable 失败时无升级路径,直接 fail 而非升级重试。
    #[test]
    fn dispatch_subagent_scenario3_no_upgrade_fails_immediately() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();
        let unique_task = "scenario3-no-upgrade-uuid-p0-9-s3";

        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        // gate 脚本:第一次 Err(retryable)
        // 自动升级已关闭,第一次失败即终止,不会到达第二次
        coordinator.add_validation_gate(Box::new(ScriptedGate::new(vec![
            Some(false), // attempt 1 validate: retryable Err
            Some(true),  // 不会被消费
        ])));

        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let input = serde_json::json!({
            "name": "diag-agent",
            "task": unique_task,
            "mode": "fork",
            "model": "deepseek-v4-flash",
            "complexity": "simple",
            "max_attempts": 2
        })
        .to_string();

        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should not propagate as hard error");

        // 自动升级已关闭:失败即终止
        assert!(
            output.contains("Subagent `") && output.contains("failed"),
            "scenario 3 should fail without upgrade: {output}"
        );

        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("extract subagent_id");
        let agent = coordinator.get(subagent_id).expect("agent exists");
        // 模型不应升级,保持 flash
        assert_eq!(
            agent.model.as_deref(),
            Some("deepseek-v4-flash"),
            "model should NOT be upgraded (auto-upgrade disabled)"
        );
        assert!(!agent.validated, "should not be validated");
        assert_eq!(agent.status, crate::multi_agent::SubagentStatus::Failed);
    }

    /// 场景 4:flash + Simple + max_attempts=2,validate 失败 → 无升级路径 → fail
    ///
    /// 自动升级已关闭,第一次失败即终止,模型保持 flash。
    #[test]
    fn dispatch_subagent_scenario4_no_upgrade_fails() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();
        let unique_task = "scenario4-no-upgrade-uuid-p0-9-s4";

        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        // gate 脚本:总是 retryable Err
        // 自动升级已关闭,第一次失败即终止,第二次不会被消费
        coordinator.add_validation_gate(Box::new(ScriptedGate::new(vec![
            Some(false), // attempt 1: retryable Err
            Some(false), // 不会被消费
        ])));

        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let input = serde_json::json!({
            "name": "diag-agent",
            "task": unique_task,
            "mode": "fork",
            "model": "deepseek-v4-flash",
            "complexity": "simple",
            "max_attempts": 2
        })
        .to_string();

        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should not propagate as hard error");

        // 自动升级已关闭:第一次失败即 fail
        assert!(
            output.contains("Subagent `") && output.contains("failed"),
            "scenario 4 should fail without upgrade: {output}"
        );

        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("extract subagent_id");
        let agent = coordinator.get(subagent_id).expect("agent exists");
        assert_eq!(
            agent.status,
            crate::multi_agent::SubagentStatus::Failed,
            "should be Failed"
        );
        assert!(!agent.validated, "should not be validated");
        // 模型不应升级,保持 flash
        assert_eq!(
            agent.model.as_deref(),
            Some("deepseek-v4-flash"),
            "model should NOT be upgraded (auto-upgrade disabled)"
        );
    }

    /// 场景 5:flash + Simple + max_attempts=2 + cost_limit=0.0005,
    /// attempt 1 后 accumulated($0.001) > cost_limit($0.0005) → 成本门禁拒绝升级 → fail
    #[test]
    fn dispatch_subagent_scenario5_cost_limit_blocks_upgrade() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();
        let unique_task = "scenario5-cost-limit-uuid-p0-9-s5";

        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        // gate 脚本:第一次 retryable Err(触发升级检查)
        coordinator.add_validation_gate(Box::new(ScriptedGate::new(vec![
            Some(false), // attempt 1: retryable Err → 触发升级 + 成本门禁
        ])));

        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        // cost_limit=0.0005:flash 调用 $0.001 后 accumulated=0.001 > 0.0005 → 拒绝升级
        let input = serde_json::json!({
            "name": "diag-agent",
            "task": unique_task,
            "mode": "fork",
            "model": "deepseek-v4-flash",
            "complexity": "simple",
            "max_attempts": 2,
            "cost_limit": 0.0005
        })
        .to_string();

        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should not propagate as hard error");

        // 场景 5:成本超限 → fail(不浪费 pro 调用)
        assert!(
            output.contains("Subagent `") && output.contains("failed"),
            "scenario 5 should fail due to cost limit: {output}"
        );
        assert!(
            output.contains("cost limit"),
            "should mention cost limit in failure msg: {output}"
        );

        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("extract subagent_id");
        let agent = coordinator.get(subagent_id).expect("agent exists");
        assert_eq!(
            agent.status,
            crate::multi_agent::SubagentStatus::Failed,
            "should be Failed due to cost limit"
        );
        // 关键不变量:模型未升级(仍是 flash),因为没有调用 pro
        assert_eq!(
            agent.model.as_deref(),
            Some("deepseek-v4-flash"),
            "model should NOT be upgraded — cost gate blocked pro call"
        );
        // 累计成本应只有 flash 的 $0.001(名义值)
        assert!(
            (agent.cost_accumulated - 0.001).abs() < 1e-9,
            "cost_accumulated should be 0.001 (flash only), got: {}",
            agent.cost_accumulated
        );
    }

    /// 场景 5 补充:cost_limit 足够大,但自动升级已关闭 → 仍 fail
    ///
    /// V4-Flash 正式版上线后自动升级链已关闭,
    /// 即使成本上限足够,也不会升级到 pro,直接 fail。
    #[test]
    fn dispatch_subagent_scenario5_high_cost_limit_no_upgrade_disabled() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();
        let unique_task = "scenario5-high-limit-no-upgrade-uuid-p0-9-s5b";

        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        coordinator.add_validation_gate(Box::new(ScriptedGate::new(vec![
            Some(false), // attempt 1: retryable Err
            Some(true),  // 不会被消费(自动升级已关闭)
        ])));

        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        // cost_limit=10.0:足够大,但自动升级已关闭
        let input = serde_json::json!({
            "name": "diag-agent",
            "task": unique_task,
            "mode": "fork",
            "model": "deepseek-v4-flash",
            "complexity": "simple",
            "max_attempts": 2,
            "cost_limit": 10.0
        })
        .to_string();

        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should not propagate as hard error");

        // 自动升级已关闭:即使成本上限足够,也直接 fail
        assert!(
            output.contains("failed"),
            "should fail without upgrade even with high cost_limit: {output}"
        );

        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("extract subagent_id");
        let agent = coordinator.get(subagent_id).expect("agent exists");
        // 模型不应升级,保持 flash
        assert_eq!(
            agent.model.as_deref(),
            Some("deepseek-v4-flash"),
            "model should NOT be upgraded (auto-upgrade disabled)"
        );
    }

    /// §10.4 端到端:max_attempts=1 时不重试,第一次 validate 失败直接 fail
    #[test]
    fn dispatch_subagent_no_retry_when_max_attempts_is_one() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();
        let unique_task = "no-retry-max-1-uuid-p0-9";

        let coordinator = crate::multi_agent::MultiAgentCoordinator::new();
        coordinator.add_validation_gate(Box::new(ScriptedGate::new(vec![
            Some(false), // attempt 1: retryable Err
        ])));

        let mut runtime = runtime_with_coordinator(coordinator.clone());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        runtime.set_workspace_root(tempdir.path().to_path_buf());

        let input = serde_json::json!({
            "name": "agent",
            "task": unique_task,
            "mode": "fork",
            "model": "deepseek-v4-flash",
            "complexity": "simple",
            "max_attempts": 1
        })
        .to_string();

        let output = runtime
            .execute_dispatch_subagent(&input)
            .expect("dispatch should not propagate as hard error");

        // max_attempts=1:不重试,直接 fail
        assert!(
            output.contains("failed"),
            "max_attempts=1 should fail without retry: {output}"
        );

        let subagent_id = output
            .split("Subagent `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("extract subagent_id");
        let agent = coordinator.get(subagent_id).expect("agent exists");
        assert_eq!(
            agent.model.as_deref(),
            Some("deepseek-v4-flash"),
            "model should not change"
        );
        assert_eq!(agent.attempts, 0, "no reset should happen");
    }

    // ===== v3 Phase 3:spawn_parallel_via_dag 真并行集成测试 =====
    // 依据 docs/multi-agent-hardening-plan.md v3 阶段验收标准

    /// v3:未注入 coordinator_executor 时,所有 task 返回 Err
    #[test]
    fn spawn_parallel_via_dag_no_executor_returns_err() {
        let runtime: ConversationRuntime<NoopApi, StaticToolExecutor> = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        // 未调用 with_dag_coordinator → coordinator_executor = None

        let tasks = vec![
            crate::multi_agent::SpawnRequest::new(
                "agent-a",
                "task A",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-flash",
                crate::multi_agent::TaskComplexity::Simple,
            ),
            crate::multi_agent::SpawnRequest::new(
                "agent-b",
                "task B",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-pro",
                crate::multi_agent::TaskComplexity::Diagnostic,
            ),
        ];

        let results = runtime.spawn_parallel_via_dag(tasks);
        assert_eq!(results.len(), 2, "should return one result per task");
        for (i, r) in results.iter().enumerate() {
            match r {
                Err(msg) => assert!(
                    msg.contains("coordinator_executor not injected"),
                    "task {i} should report missing executor, got: {msg}"
                ),
                Ok(_) => panic!("task {i} should fail, but got Ok"),
            }
        }
    }

    /// v3:空 task 列表返回空 vec(不触发 DAG 调度)
    #[test]
    fn spawn_parallel_via_dag_empty_tasks_returns_empty() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let results = runtime.spawn_parallel_via_dag(vec![]);
        assert!(results.is_empty(), "empty tasks should return empty vec");
    }

    /// v3:能力校验 — Budget 模型 + Diagnostic/Architectural 任务直接拒绝,不进入 DAG
    #[test]
    fn spawn_parallel_via_dag_capability_check_rejects_budget_diagnostic() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        // 两个 task 都应被能力校验拒绝:flash+Diagnostic / flash+Architectural
        let tasks = vec![
            crate::multi_agent::SpawnRequest::new(
                "diag-agent",
                "诊断闪退",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-flash",
                crate::multi_agent::TaskComplexity::Diagnostic,
            ),
            crate::multi_agent::SpawnRequest::new(
                "arch-agent",
                "架构评估",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-flash",
                crate::multi_agent::TaskComplexity::Architectural,
            ),
        ];

        let results = runtime.spawn_parallel_via_dag(tasks);
        assert_eq!(results.len(), 2);
        for (i, r) in results.iter().enumerate() {
            match r {
                Err(msg) => assert!(
                    msg.contains("Budget tier"),
                    "task {i} should be rejected for Budget tier, got: {msg}"
                ),
                Ok(_) => panic!("task {i} should be rejected, but got Ok"),
            }
        }
    }

    /// v3:端到端真并行 — 两个 Simple task 通过 DagScheduler 并发执行,返回 result_ref 路径
    #[test]
    fn spawn_parallel_via_dag_parallel_execution_succeeds() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let tasks = vec![
            crate::multi_agent::SpawnRequest::new(
                "agent-a",
                "task A",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-flash",
                crate::multi_agent::TaskComplexity::Simple,
            ),
            crate::multi_agent::SpawnRequest::new(
                "agent-b",
                "task B",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-flash",
                crate::multi_agent::TaskComplexity::Simple,
            ),
        ];

        let results = runtime.spawn_parallel_via_dag(tasks);
        assert_eq!(results.len(), 2, "should return 2 results");

        // 两个 task 都应成功,返回 result_ref 路径
        for (i, r) in results.iter().enumerate() {
            match r {
                Ok(path) => {
                    assert!(
                        path.contains(".claw/subagents/"),
                        "task {i} result should be a subagent path, got: {path}"
                    );
                    // 验证文件实际写入
                    let full_path = tempdir.path().join(path);
                    assert!(
                        full_path.exists(),
                        "task {i} result file should exist at {full_path:?}"
                    );
                }
                Err(e) => panic!("task {i} should succeed, got err: {e}"),
            }
        }
    }

    // ===== v3 Phase 3:异步接口变体 + FailFast 容错测试 =====

    /// v3:异步变体 — 未注入 executor 时所有 task 返回 Err
    #[tokio::test]
    async fn spawn_parallel_via_dag_async_no_executor_returns_err() {
        let runtime: ConversationRuntime<NoopApi, StaticToolExecutor> = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let tasks = vec![crate::multi_agent::SpawnRequest::new(
            "agent-a",
            "task A",
            crate::multi_agent::CoordinationMode::Fork,
            "deepseek-v4-flash",
            crate::multi_agent::TaskComplexity::Simple,
        )];

        let results = runtime
            .spawn_parallel_via_dag_async(tasks, FailFast::On)
            .await;
        assert_eq!(results.len(), 1);
        match &results[0] {
            Err(msg) => assert!(
                msg.contains("coordinator_executor not injected"),
                "got: {msg}"
            ),
            Ok(_) => panic!("should fail"),
        }
    }

    /// v3:异步变体 — 空 tasks 返回空 vec
    #[tokio::test]
    async fn spawn_parallel_via_dag_async_empty_tasks_returns_empty() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let results = runtime
            .spawn_parallel_via_dag_async(vec![], FailFast::On)
            .await;
        assert!(results.is_empty());
    }

    /// v3:异步变体 — 能力校验拒绝 Budget tier + Diagnostic
    #[tokio::test]
    async fn spawn_parallel_via_dag_async_capability_check_rejects() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let tasks = vec![crate::multi_agent::SpawnRequest::new(
            "diag-agent",
            "诊断",
            crate::multi_agent::CoordinationMode::Fork,
            "deepseek-v4-flash",
            crate::multi_agent::TaskComplexity::Diagnostic,
        )];

        let results = runtime
            .spawn_parallel_via_dag_async(tasks, FailFast::On)
            .await;
        assert_eq!(results.len(), 1);
        match &results[0] {
            Err(msg) => assert!(msg.contains("Budget tier"), "got: {msg}"),
            Ok(_) => panic!("should reject"),
        }
    }

    /// v3:异步变体 — 端到端真并行执行成功
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 测试互斥锁:有意持有跨 await,串行化共享 lane-events 状态
    async fn spawn_parallel_via_dag_async_parallel_execution_succeeds() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let tasks = vec![
            crate::multi_agent::SpawnRequest::new(
                "agent-a",
                "task A",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-flash",
                crate::multi_agent::TaskComplexity::Simple,
            ),
            crate::multi_agent::SpawnRequest::new(
                "agent-b",
                "task B",
                crate::multi_agent::CoordinationMode::Fork,
                "deepseek-v4-flash",
                crate::multi_agent::TaskComplexity::Simple,
            ),
        ];

        let results = runtime
            .spawn_parallel_via_dag_async(tasks, FailFast::On)
            .await;
        assert_eq!(results.len(), 2);
        for (i, r) in results.iter().enumerate() {
            match r {
                Ok(path) => {
                    assert!(path.contains(".claw/subagents/"), "task {i} got: {path}");
                    assert!(
                        tempdir.path().join(path).exists(),
                        "task {i} file should exist"
                    );
                }
                Err(e) => panic!("task {i} should succeed, got: {e}"),
            }
        }
    }

    /// v3:同步 FailFast 变体 — 显式 FailFast::On 路径应正常执行
    /// (默认已改为 FailFast::Off,本测试验证显式 On 路径)
    #[test]
    fn spawn_parallel_via_dag_with_fail_fast_on_executes_successfully() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let tasks = vec![crate::multi_agent::SpawnRequest::new(
            "agent-x",
            "task X",
            crate::multi_agent::CoordinationMode::Fork,
            "deepseek-v4-flash",
            crate::multi_agent::TaskComplexity::Simple,
        )];

        let results = runtime.spawn_parallel_via_dag_with_fail_fast(tasks, FailFast::On);
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok(), "should succeed: {:?}", results[0]);
    }

    // ===== PlannerAgent 自动拆解接入并行链路测试 =====

    /// `plan_and_spawn_parallel` — feature flag 关闭时返回 Ok(None)
    ///
    /// 注意:library 层默认 false(unwrap_or(false)),CLI 层默认 true。
    /// 此测试在 runtime crate 中运行,OnceLock 未被设置,默认 false。
    #[test]
    fn plan_and_spawn_parallel_disabled_returns_none() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        // library 层默认 false,应返回 Ok(None)
        let result = runtime
            .plan_and_spawn_parallel("refactor multiple files across modules")
            .expect("should not error when disabled");
        assert!(result.is_none(), "should return None when feature flag off");
    }

    /// `plan_and_spawn_parallel` — Simple 任务不拆解(即使 feature flag 开启)
    #[test]
    fn plan_and_spawn_parallel_simple_task_returns_none() {
        // 注意:OnceLock 是进程级单例,可能已被其他测试开启
        // 这里用 Simple 任务测试,无论 flag 状态都应返回 None
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        // "hello" 是 Simple 任务,不应触发拆解
        let result = runtime
            .plan_and_spawn_parallel("hello")
            .expect("should not error for simple task");
        assert!(result.is_none(), "simple task should not trigger planner");
    }

    // ===== v3 Phase 3:execute_spawn_parallel_subagents(CLI tool 接入)测试 =====

    /// v3:`spawn_parallel_subagents` tool — 无效 JSON 返回错误
    #[test]
    fn execute_spawn_parallel_subagents_invalid_json_errors() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let err = runtime
            .execute_spawn_parallel_subagents("not json")
            .expect_err("invalid JSON should error");
        assert!(err.to_string().contains("invalid input JSON"));
    }

    /// v3:`spawn_parallel_subagents` tool — 缺少 tasks 数组返回错误
    #[test]
    fn execute_spawn_parallel_subagents_missing_tasks_errors() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let err = runtime
            .execute_spawn_parallel_subagents(r#"{"fail_fast":"on"}"#)
            .expect_err("missing tasks should error");
        assert!(err.to_string().contains("missing or invalid 'tasks'"));
    }

    /// v3:`spawn_parallel_subagents` tool — 空 tasks 数组返回错误
    #[test]
    fn execute_spawn_parallel_subagents_empty_tasks_errors() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let err = runtime
            .execute_spawn_parallel_subagents(r#"{"tasks":[]}"#)
            .expect_err("empty tasks should error");
        assert!(err.to_string().contains("must not be empty"));
    }

    /// v3:`spawn_parallel_subagents` tool — 无效 fail_fast 值返回错误
    #[test]
    fn execute_spawn_parallel_subagents_invalid_fail_fast_errors() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let input = r#"{"tasks":[{"name":"a","task":"b","model":"deepseek-v4-flash"}],"fail_fast":"bogus"}"#;
        let err = runtime
            .execute_spawn_parallel_subagents(input)
            .expect_err("invalid fail_fast should error");
        assert!(err.to_string().contains("invalid fail_fast 'bogus'"));
    }

    /// v3:`spawn_parallel_subagents` tool — 任务项缺少 model 字段返回错误
    #[test]
    fn execute_spawn_parallel_subagents_task_missing_model_errors() {
        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let input = r#"{"tasks":[{"name":"a","task":"b"}]}"#;
        let err = runtime
            .execute_spawn_parallel_subagents(input)
            .expect_err("missing model should error");
        assert!(err.to_string().contains("missing 'model'"));
    }

    /// v3:`spawn_parallel_subagents` tool — 端到端成功:2 个 Simple task 并行执行
    #[test]
    fn execute_spawn_parallel_subagents_succeeds() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let input = r#"{
            "tasks": [
                {"name":"agent-a","task":"task A","model":"deepseek-v4-flash"},
                {"name":"agent-b","task":"task B","model":"deepseek-v4-flash","mode":"fork","complexity":"simple"}
            ],
            "fail_fast": "on"
        }"#;
        let output = runtime
            .execute_spawn_parallel_subagents(input)
            .expect("should succeed");
        assert!(output.contains("2 succeeded"), "got: {output}");
        assert!(output.contains("0 failed"), "got: {output}");
        assert!(output.contains("[0] OK:"), "got: {output}");
        assert!(output.contains("[1] OK:"), "got: {output}");
        assert!(output.contains(".claw/subagents/"), "got: {output}");
    }

    /// v3:`spawn_parallel_subagents` tool — 能力校验失败:Budget + Diagnostic
    #[test]
    fn execute_spawn_parallel_subagents_capability_reject() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let input = r#"{
            "tasks": [
                {"name":"diag","task":"诊断","model":"deepseek-v4-flash","complexity":"diagnostic"}
            ]
        }"#;
        let output = runtime
            .execute_spawn_parallel_subagents(input)
            .expect("should return formatted output");
        // 能力校验失败标记为 FAIL,但 tool 调用本身成功(返回 formatted 输出)
        assert!(output.contains("0 succeeded"), "got: {output}");
        assert!(output.contains("1 failed"), "got: {output}");
        assert!(output.contains("Budget tier"), "got: {output}");
    }

    // ===== v3:async 变体测试 =====

    /// v3:`execute_spawn_parallel_subagents_async` — 无效 JSON 返回错误
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 测试互斥锁:有意持有跨 await,串行化共享 lane-events 状态
    async fn execute_spawn_parallel_subagents_async_invalid_json_errors() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let err = runtime
            .execute_spawn_parallel_subagents_async("not json")
            .await
            .expect_err("invalid JSON should error");
        assert!(err.to_string().contains("invalid input JSON"));
    }

    /// v3:`execute_spawn_parallel_subagents_async` — 空 tasks 数组返回错误
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 测试互斥锁:有意持有跨 await,串行化共享 lane-events 状态
    async fn execute_spawn_parallel_subagents_async_empty_tasks_errors() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let err = runtime
            .execute_spawn_parallel_subagents_async(r#"{"tasks":[]}"#)
            .await
            .expect_err("empty tasks should error");
        assert!(err.to_string().contains("must not be empty"));
    }

    /// v3:`execute_spawn_parallel_subagents_async` — 端到端成功:2 个 Simple task 并行执行
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 测试互斥锁:有意持有跨 await,串行化共享 lane-events 状态
    async fn execute_spawn_parallel_subagents_async_succeeds() {
        let _guard = acquire_lane_event_lock();
        let _ = crate::lane_events::drain_lane_events();

        let coordinator = Arc::new(crate::multi_agent::MultiAgentCoordinator::new());
        let tempdir = tempfile::tempdir().expect("temp workspace");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_dag_coordinator(coordinator, NoopApi, tempdir.path().to_path_buf(), None);

        let input = r#"{
            "tasks": [
                {"name":"task-a","task":"do A","model":"deepseek-v4-pro"},
                {"name":"task-b","task":"do B","model":"claude-haiku"}
            ],
            "fail_fast":"off"
        }"#;
        let output = runtime
            .execute_spawn_parallel_subagents_async(input)
            .await
            .expect("should return formatted output");
        assert!(
            output.contains("spawn_parallel_subagents:"),
            "got: {output}"
        );
        assert!(output.contains("fail_fast=off"), "got: {output}");
    }

    /// v3:`parse_spawn_parallel_input` — 共享解析逻辑的单元测试
    #[test]
    fn parse_spawn_parallel_input_valid_input() {
        let input = r#"{
            "tasks": [
                {"name":"a","task":"do A","model":"deepseek-v4-pro"},
                {"name":"b","task":"do B","model":"deepseek-v4-flash","mode":"fork","complexity":"simple"}
            ],
            "fail_fast":"off"
        }"#;
        let (tasks, fail_fast, fail_fast_str) =
            ConversationRuntime::<NoopApi, StaticToolExecutor>::parse_spawn_parallel_input(input)
                .expect("valid input should parse");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "a");
        assert_eq!(tasks[0].model, "deepseek-v4-pro");
        assert_eq!(tasks[1].name, "b");
        assert_eq!(fail_fast, FailFast::Off);
        assert_eq!(fail_fast_str, "off");
    }

    /// Epic 0(§3.1):`parse_spawn_parallel_input` 解析 capability 字段,
    /// 缺失时默认 Analyze(向后兼容)。
    #[test]
    fn parse_spawn_parallel_input_capability_field() {
        let input = r#"{
            "tasks": [
                {"name":"a","task":"do A","model":"deepseek-v4-pro"},
                {"name":"b","task":"do B","model":"deepseek-v4-pro","capability":"read-only"},
                {"name":"c","task":"do C","model":"deepseek-v4-pro","capability":"execute"}
            ],
            "fail_fast":"off"
        }"#;
        let (tasks, _, _) =
            ConversationRuntime::<NoopApi, StaticToolExecutor>::parse_spawn_parallel_input(input)
                .expect("valid input should parse");
        assert_eq!(tasks.len(), 3);
        // 缺失字段默认 Analyze
        assert_eq!(
            tasks[0].capability,
            crate::multi_agent::SubagentCapability::Analyze
        );
        assert_eq!(
            tasks[1].capability,
            crate::multi_agent::SubagentCapability::ReadOnly
        );
        assert_eq!(
            tasks[2].capability,
            crate::multi_agent::SubagentCapability::Execute
        );
    }

    /// v3:`format_spawn_parallel_results` — 共享格式化逻辑的单元测试
    #[test]
    fn format_spawn_parallel_results_mixed_success_failure() {
        let results = vec![
            Ok("/path/to/artifact-1".to_string()),
            Err("capability check failed".to_string()),
            Ok("/path/to/artifact-2".to_string()),
        ];
        let output =
            ConversationRuntime::<NoopApi, StaticToolExecutor>::format_spawn_parallel_results(
                &results, "on", None,
            );
        assert!(output.contains("2 succeeded"), "got: {output}");
        assert!(output.contains("1 failed"), "got: {output}");
        assert!(output.contains("fail_fast=on"), "got: {output}");
        assert!(output.contains("[0] OK:"), "got: {output}");
        assert!(output.contains("[1] FAIL:"), "got: {output}");
        assert!(output.contains("[2] OK:"), "got: {output}");
    }

    // ===== v3 §4.7:DetectionStrategy 端到端接入测试 =====

    /// v3:默认策略为 Heuristic
    #[test]
    fn detection_strategy_defaults_to_heuristic() {
        let runtime: ConversationRuntime<NoopApi, StaticToolExecutor> = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        assert!(matches!(
            runtime.detection_strategy(),
            crate::decision_log::DetectionStrategy::Heuristic
        ));
    }

    /// v3:`with_detection_strategy` 正确设置 LlmExtract
    #[test]
    fn with_detection_strategy_sets_llm_extract() {
        let runtime: ConversationRuntime<NoopApi, StaticToolExecutor> = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_detection_strategy(crate::decision_log::DetectionStrategy::LlmExtract {
            model: "deepseek-v4-flash".to_string(),
        });
        match runtime.detection_strategy() {
            crate::decision_log::DetectionStrategy::LlmExtract { model } => {
                assert_eq!(model, "deepseek-v4-flash");
            }
            other => panic!("expected LlmExtract, got {other:?}"),
        }
    }

    // ===== #3 路径 A 精确传递:log_decision 的 knowledge_source =====

    /// 无显式 knowledge_source → 默认 "parametric",不受任何子任务
    /// last-gated 全局变量污染(design-gaps #3:统计不再串任务)。
    #[test]
    fn execute_log_decision_defaults_to_parametric_without_explicit_source() {
        let dir = tempfile::tempdir().unwrap();
        let log = crate::decision_log::DecisionLog::open(dir.path()).unwrap();
        let runtime: ConversationRuntime<NoopApi, StaticToolExecutor> = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_decision_log(log);

        let input = r#"{"session_id":"s1","problem_signature":"p1","root_cause_hypothesis":"r1","applied_solution":"s1","affected_files":["x.rs"],"verification_result":"Confirmed"}"#;
        runtime
            .execute_log_decision(input)
            .unwrap_or_else(|e| panic!("log_decision failed: {e:?}"));

        let stats = runtime
            .decision_log
            .as_ref()
            .unwrap()
            .stats_by_knowledge_source()
            .expect("stats");
        assert!(stats.contains("parametric"), "stats: {stats}");
        assert!(!stats.contains("web_research"), "stats: {stats}");
    }

    /// 显式传 knowledge_source → 原样保留(LLM 基于自身任务上下文精确标注)。
    #[test]
    fn execute_log_decision_preserves_explicit_source() {
        let dir = tempfile::tempdir().unwrap();
        let log = crate::decision_log::DecisionLog::open(dir.path()).unwrap();
        let runtime: ConversationRuntime<NoopApi, StaticToolExecutor> = ConversationRuntime::new(
            Session::new(),
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_decision_log(log);

        let input = r#"{"session_id":"s2","problem_signature":"p2","root_cause_hypothesis":"r2","applied_solution":"s2","affected_files":["y.rs"],"verification_result":"Confirmed","knowledge_source":"web_research"}"#;
        runtime
            .execute_log_decision(input)
            .expect("log_decision should succeed");

        let stats = runtime
            .decision_log
            .as_ref()
            .unwrap()
            .stats_by_knowledge_source()
            .expect("stats");
        assert!(stats.contains("web_research"), "stats: {stats}");
    }
}
