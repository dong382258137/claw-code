//! Multi-Agent Coordinator — Step 3.2 多 agent 协调器。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 3.2
//!
//! 架构(参考 Claude Code 源码泄露):
//! - 三种编排模式:
//!   - [`CoordinationMode::Fork`]:主 agent 派生子 agent 并行执行,主 agent 收集结果。
//!   - [`CoordinationMode::Teammate`]:多个 agent 协作,通过共享 TaskRegistry 通信。
//!   - [`CoordinationMode::Worktree`]:每个 agent 独立 git worktree,避免文件冲突。
//! - [`MultiAgentCoordinator`]:统一入口,管理 agent 生命周期 + 任务分派。
//! - 与 [`TaskRegistry`](crate::task_registry::TaskRegistry) 对接。
//! - 与 [`VerifierAgent`](crate::verifier::VerifierAgent) 对接:子 agent 完成后校验。
//!
//! **缓存保护**(详见 §5.2):
//! 每个子 agent 走独立 LLM 请求 + 独立 prompt cache,不污染主 agent 缓存。
//! "Subagent as Tool" 模式 — 主 agent 通过 tool call 接口调用子 agent。

pub mod dag;
// Epic 5:结构化 handoff 协议(SubagentHandoff + write_handoff + parse_handoff)。
pub mod handoff;
// Epic 4:文件操作权限隔离(SubagentFileGuard + LockHandle)。
pub mod file_guard;
// Multi-Agent Hardening §4.4:验证门禁(ValidationGate trait + CommandValidationGate + LlmJudgeGate 预留)。
pub mod validation;
pub use dag::DagStore;
pub use dag::{
    CoordinatorExecutor, DagError, DagGraph, DagId, DagNode, DagRunResult, DagScheduler, FailFast,
    NodeError, NodeResult, ProgressEvent, RetryPolicy, SubagentDispatcher, SubagentExecutor,
    SubagentRunner, DEFAULT_MAX_PARALLELISM,
};
pub use file_guard::{LockHandle, SubagentFileGuard};
pub use handoff::{
    extract_changed_files, parse_handoff, read_handoff, serialize_handoff, write_handoff,
    HandoffStatus, SubagentHandoff,
};
pub use validation::{
    detect_changed_files, rust_compile_gate, CommandValidationGate, JudgeClient, LlmJudgeGate,
    ValidationContext, ValidationError, ValidationGate,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

// ValidationGate 等已通过 `pub use validation::{...}` 导出,无需重复 use。

/// 任务复杂度需求 — Multi-Agent Hardening §4.2。
///
/// 由调用方声明,coordinator 据此匹配模型能力层级。
/// 与 `api::providers::model_tier::TaskComplexity` 保持兼容(避免循环依赖,本地定义)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    /// 简单任务:单文件编辑、已知模式 — Budget 模型可胜任。
    Simple,
    /// 诊断任务:根因定位、复杂调试 — 需要强推理能力,Flagship 模型。
    Diagnostic,
    /// 架构决策:多方案评估、trade-off 分析 — Flagship 模型。
    Architectural,
}

/// 子智能体能力分级 — TRAE 架构对齐(见 docs/2026-08-06-subagent-trae-alignment-design.md §3.1)。
///
/// 三级能力枚举,按能力注入工具白名单与上下文前缀。默认 `Analyze`(向后兼容,
/// 现有调用方零改动)。capability 决定:
/// - `allowed_tools()`:该能力允许调用的工具名白名单
/// - `enables_tools()`:是否启用工具(Analyze 不启用,纯 LLM 推理)
/// - `max_iterations()`:多轮 tool call 循环上限(Epic 3)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentCapability {
    /// L0 分析型:只读 + 推理,无副作用。用于调研、方案设计、代码审查。
    #[default]
    Analyze,
    /// L1 只读型:可调用只读工具(read_file/grep_search/glob_search/repomap/lsp_diagnostics),
    /// 禁止写入。
    ReadOnly,
    /// L2 执行型:可调用写入工具(edit_file/write_file/bash),受白名单约束。
    Execute,
}

impl SubagentCapability {
    /// 返回该能力允许的工具名白名单(按 tools::GlobalToolRegistry 注册名)。
    ///
    /// 使用**规范名**(read_file/grep_search/... 与 `mvp_tool_specs` 注册名一致),
    /// 而非短名(read/grep/...):API 层 `GlobalToolRegistry::definitions()` 按
    /// 规范名过滤工具定义、执行层 `execute_tool` 仅匹配规范名,统一后
    /// guard / API 工具暴露 / 执行 / `## Available Tools` 层全链路一致,
    /// 子 agent 不再"短名通过 guard 却无法执行 / API 只见 bash"。
    ///
    /// 注:`dispatch_subagent` / `spawn_parallel_subagents` 不放入白名单,
    /// 递归派发禁止由 §3.3.1 guard 在 tool_use 提取阶段显式检查实现
    /// (见 execute_subagent_llm 内 `if tu.name == "dispatch_subagent" ...` 分支)。
    #[must_use]
    pub fn allowed_tools(self) -> &'static [&'static str] {
        match self {
            Self::Analyze => &[],
            Self::ReadOnly => &["read_file", "grep_search", "glob_search", "repomap", "lsp_diagnostics"],
            Self::Execute => &[
                "read_file",
                "grep_search",
                "glob_search",
                "repomap",
                "lsp_diagnostics",
                "edit_file",
                "write_file",
                "bash",
            ],
        }
    }

    /// 是否启用工具(Analyze 不启用,纯 LLM 推理)。
    #[must_use]
    pub fn enables_tools(self) -> bool {
        !matches!(self, Self::Analyze)
    }

    /// 多轮 tool call 循环上限(Epic 3)。
    #[must_use]
    pub fn max_iterations(self) -> usize {
        match self {
            Self::Analyze => 1,
            Self::ReadOnly => 5,
            Self::Execute => 10,
        }
    }
}

/// 多 agent 编排模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationMode {
    /// Fork:主 agent 派生子 agent 并行执行,主 agent 收集结果。
    Fork,
    /// Teammate:多个 agent 协作,通过共享 TaskRegistry 通信。
    Teammate,
    /// Worktree:每个 agent 独立 git worktree,避免文件冲突。
    Worktree,
}

/// 子 agent 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    /// 已创建,未启动。
    Created,
    /// 运行中。
    Running,
    /// 已完成(成功)。
    Completed,
    /// 已失败。
    Failed,
    /// 已取消。
    Cancelled,
}

/// 子 agent 描述符。
///
/// Multi-Agent Hardening v3:扩展字段支持模型路由、重试、成本门禁、checkpoint。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subagent {
    /// 全局唯一 ID。
    pub id: String,
    /// 人类可读名称。
    pub name: String,
    /// 编排模式。
    pub mode: CoordinationMode,
    /// 分配的任务描述。
    pub task: String,
    /// 当前状态。
    pub status: SubagentStatus,
    /// 工作目录(Worktree 模式下为独立 git worktree 路径)。
    pub workdir: Option<PathBuf>,
    /// 创建时间(unix epoch 秒)。
    pub created_at: u64,
    /// 完成时间(unix epoch 秒,None 表示未完成)。
    pub completed_at: Option<u64>,
    /// 结果(完成后填充)。
    pub result: Option<String>,

    // === Multi-Agent Hardening v3 扩展字段 ===
    /// 使用的模型名(None 表示使用默认模型)。
    /// §4.2:模型能力分级 + 任务路由依据。
    #[serde(default)]
    pub model: Option<String>,

    /// 任务复杂度分类,用于模型能力匹配。
    /// §4.2:Diagnostic/Architectural 任务拒绝 Budget 模型。
    #[serde(default = "default_complexity")]
    pub complexity: TaskComplexity,

    /// 子智能体能力分级 — TRAE 架构对齐(见 docs/2026-08-06-subagent-trae-alignment-design.md §3.1)。
    /// 决定工具白名单、是否启用工具、多轮循环上限。默认 `Analyze`(向后兼容)。
    #[serde(default)]
    pub capability: SubagentCapability,

    /// 最大尝试次数(默认 1 = 只尝试 1 次不重试;2 = 1 次原始 + 1 次重试)。
    /// §4.5 retry loop 上限,防止无限重试。
    /// 注意:`retry loop` 用 `for attempt in 1..=max_attempts` 作为循环上限,
    /// 即 max_attempts 表示"最大尝试次数"而非"重试次数"。
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// 当前已重置次数(0 = 未重置,1 = 已重置 1 次准备第 2 次尝试,以此类推)。
    /// §4.5:`reset_for_retry` 时 +1,达 `max_attempts - 1` 后拒绝重置
    /// (因为 max_attempts=2 意味着最多 2 次尝试,只能在第 1 次失败后重置 1 次)。
    #[serde(default)]
    pub attempts: u32,

    /// 是否已通过验证门禁。
    /// §4.4:`validate()` 成功后置 true,失败置 false 并触发 retry。
    #[serde(default)]
    pub validated: bool,

    /// 诊断备注(如能力校验警告、retry 原因等)。
    /// §0.7 v2:`spawn` 能力校验失败时不返回 Err,而是把警告写入 notes。
    #[serde(default)]
    pub notes: Vec<String>,

    /// Checkpoint 文件路径(P1 预留)。
    /// §v3 P1:`save_checkpoint` 落盘路径,用于 durable execution。
    /// MVP 仅实现 save,restore 留待 v2。
    #[serde(default)]
    pub checkpoint_path: Option<PathBuf>,

    /// 成本上限(USD)。None 表示无限制。
    /// §v3 P0 成本门禁:retry loop 升级前调用 `check_cost_limit` 校验。
    #[serde(default)]
    pub cost_limit: Option<f64>,

    /// 累计成本(USD)。每次 LLM 调用后累加。
    /// §v3 P0:与 `cost_limit` 配合,达上限后中止 retry。
    #[serde(default)]
    pub cost_accumulated: f64,
}

fn default_complexity() -> TaskComplexity {
    TaskComplexity::Simple
}

fn default_max_attempts() -> u32 {
    1
}

/// v3 新增(P1):`spawn_parallel` 的请求参数。
///
/// 每个 `SpawnRequest` 对应一次 `spawn_with_model` 调用,
/// `spawn_parallel` 按顺序返回每个请求的结果。
///
/// # 示例
/// ```
/// use runtime::multi_agent::{SpawnRequest, CoordinationMode, TaskComplexity};
///
/// let req = SpawnRequest::new(
///     "diag-agent",
///     "定位 wizard 闪退",
///     CoordinationMode::Fork,
///     "deepseek-v4-pro",
///     TaskComplexity::Diagnostic,
/// );
/// assert_eq!(req.name, "diag-agent");
/// ```
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// 子 agent 名称。
    pub name: String,
    /// 任务描述。
    pub task: String,
    /// 编排模式。
    pub mode: CoordinationMode,
    /// 模型名。
    pub model: String,
    /// 任务复杂度。
    pub complexity: TaskComplexity,
    /// 子智能体能力分级 — TRAE 架构对齐(§3.1)。默认 `Analyze`(向后兼容,
    /// `new()` 不接收此参数,调用方通过 `with_capability()` 设置)。
    pub capability: SubagentCapability,
}

impl SpawnRequest {
    /// 创建一个新的 `SpawnRequest`。
    ///
    /// `capability` 默认为 `Analyze`(向后兼容,现有调用方零改动)。
    /// 需要指定能力时链式调用 [`SpawnRequest::with_capability`]。
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
        model: impl Into<String>,
        complexity: TaskComplexity,
    ) -> Self {
        Self {
            name: name.into(),
            task: task.into(),
            mode,
            model: model.into(),
            complexity,
            capability: SubagentCapability::Analyze,
        }
    }

    /// Builder:设置子智能体能力分级(链式调用)。
    #[must_use]
    pub fn with_capability(mut self, capability: SubagentCapability) -> Self {
        self.capability = capability;
        self
    }
}

/// 多 agent 协调器 — 管理 agent 生命周期 + 任务分派。
///
/// Multi-Agent Hardening v3:扩展验证门禁链 + workspace_root 注入。
//
// 注:`Debug` 手动实现,因 `Box<dyn ValidationGate>` 不实现 `Debug`。
// 调试输出仅展示 subagent 数量和 gate 数量,不展开内部状态。
#[derive(Clone, Default)]
pub struct MultiAgentCoordinator {
    /// 已注册的子 agent(按 ID 索引)。
    subagents: Arc<Mutex<HashMap<String, Subagent>>>,
    /// ID 计数器。
    id_counter: Arc<Mutex<u64>>,
    /// 验证门禁链(§4.4:subagent 完成后依次调用)。
    validation_gates: Arc<Mutex<Vec<Box<dyn ValidationGate>>>>,
    /// workspace 根目录(§4.4:用于 detect_changed_files + CommandValidationGate)。
    workspace_root: Arc<Mutex<Option<PathBuf>>>,
}

impl std::fmt::Debug for MultiAgentCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let subagent_count = self.subagents.lock().map_or(0, |s| s.len());
        let gate_count = self.validation_gates.lock().map_or(0, |g| g.len());
        f.debug_struct("MultiAgentCoordinator")
            .field("subagents_count", &subagent_count)
            .field("validation_gates_count", &gate_count)
            .finish_non_exhaustive()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl MultiAgentCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 派生子 agent。
    ///
    /// 根据 `mode` 创建子 agent:
    /// - `Fork` → 创建子 agent,workdir=None(共享主 agent 工作目录)
    /// - `Teammate` → 创建子 agent,workdir=None(通过 TaskRegistry 通信)
    /// - `Worktree` → 创建子 agent,workdir=Some(worktree_path)(独立 git worktree)
    pub fn spawn(
        &self,
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
    ) -> String {
        let name = name.into();
        let task = task.into();
        let mut counter = self.id_counter.lock().expect("id counter lock poisoned");
        *counter += 1;
        let id = format!("subagent-{}", *counter);
        drop(counter);

        let workdir = match mode {
            CoordinationMode::Worktree => Some(PathBuf::from(format!(".claw/worktrees/{id}"))),
            _ => None,
        };

        // P2-2:Worktree 模式下检测 branch lock 碰撞(宽松模式)。
        // 碰撞时记录警告到 stderr(不阻止 spawn,向后兼容)。
        if mode == CoordinationMode::Worktree {
            let intent = crate::branch_lock::BranchLockIntent {
                lane_id: id.clone(),
                branch: format!("worktree-{}", id),
                worktree: workdir.as_ref().map(|p| p.to_string_lossy().to_string()),
                modules: Vec::new(),
            };
            let collisions = crate::branch_lock::detect_branch_lock_collisions(&[intent]);
            if !collisions.is_empty() {
                eprintln!(
                    "[branch_lock] {} collision(s) detected for worktree spawn {}, proceeding anyway",
                    collisions.len(),
                    id
                );
            }
        }

        let subagent = Subagent {
            id: id.clone(),
            name,
            mode,
            task,
            status: SubagentStatus::Created,
            workdir,
            created_at: now_secs(),
            completed_at: None,
            result: None,
            // v3 扩展字段默认值
            model: None,
            complexity: TaskComplexity::Simple,
            capability: SubagentCapability::Analyze,
            max_attempts: 1,
            attempts: 0,
            validated: false,
            notes: Vec::new(),
            checkpoint_path: None,
            cost_limit: None,
            cost_accumulated: 0.0,
        };

        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        agents.insert(id.clone(), subagent);
        id
    }

    /// 派生子 agent(带模型 + 复杂度) — Multi-Agent Hardening §4.2/§0.7 v2。
    ///
    /// 与 [`spawn`](Self::spawn) 不同,此方法:
    /// - 显式指定模型名和任务复杂度
    /// - 返回 `Result<String, String>`,能力校验失败时返回 Err
    /// - §0.7 v2:保留 `spawn` 原签名 `-> String`(避免破坏 12 处单测),
    ///   新增本方法作为扩展入口
    ///
    /// # 参数
    /// - `name`: 子 agent 名称
    /// - `task`: 任务描述
    /// - `mode`: 编排模式
    /// - `model`: 模型名(如 "deepseek-v4-flash")
    /// - `complexity`: 任务复杂度(Simple/Diagnostic/Architectural)
    ///
    /// # 返回
    /// - `Ok(id)`: 子 agent 创建成功
    /// - `Err(msg)`: 能力校验失败(如 Budget 模型执行 Diagnostic 任务)
    pub fn spawn_with_model(
        &self,
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
        model: impl Into<String>,
        complexity: TaskComplexity,
    ) -> Result<String, String> {
        let model = model.into();
        let name = name.into();
        let task = task.into();

        // §4.2:能力校验 — Diagnostic/Architectural 任务拒绝 Budget 模型
        // 注意:这里只做粗粒度前缀检查,不依赖 api crate(避免循环依赖)。
        // 完整的 tier_for_model 校验在 conversation.rs 调用方执行(那里能访问 api)。
        let lower = model.to_ascii_lowercase();
        let is_budget = lower.contains("haiku")
            || lower.contains("mini")
            || lower.contains("nano")
            || lower.contains("flash");
        if is_budget
            && matches!(
                complexity,
                TaskComplexity::Diagnostic | TaskComplexity::Architectural
            )
        {
            return Err(format!(
                "model '{model}' (Budget tier) cannot handle {complexity:?} task — use Flagship model"
            ));
        }

        // 复用 spawn 创建基础 subagent,然后填充 v3 扩展字段
        let id = self.spawn(name, task, mode);
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        if let Some(agent) = agents.get_mut(&id) {
            agent.model = Some(model);
            agent.complexity = complexity;
            // Diagnostic/Architectural 任务默认允许 1 次重试(升级到 Flagship)
            // max_attempts=2 意味着"最多 2 次尝试"(1 次原始 + 1 次升级重试)
            if matches!(
                complexity,
                TaskComplexity::Diagnostic | TaskComplexity::Architectural
            ) {
                agent.max_attempts = 2;
            }
        }
        Ok(id)
    }

    /// v3 新增(P1):并行 spawn 多个 subagent(预留接口)。
    ///
    /// 借鉴 Anthropic Multi-Agent Research System:lead agent 并行 spawn 3-5 subagents,
    /// 复杂查询研究时间减少 90%。
    ///
    /// **MVP 阶段退化为串行调用 `spawn_with_model`**,v2 接入 tokio 实现真并行。
    /// 串行退化保证接口就位,调用方可直接使用,v2 升级时只需替换实现。
    ///
    /// # 参数
    /// - `tasks`:spawn 请求列表,每个请求包含 name/task/mode/model/complexity
    ///
    /// # 返回
    /// - `Vec<Result<String, String>>`:每个请求对应一个结果(Ok=id, Err=原因)
    /// - 顺序与输入 `tasks` 一致
    ///
    /// # 示例
    /// ```ignore
    /// let coordinator = MultiAgentCoordinator::new();
    /// let tasks = vec![
    ///     SpawnRequest::new("agent-a", "task A", CoordinationMode::Fork, "deepseek-v4-flash", TaskComplexity::Simple),
    ///     SpawnRequest::new("agent-b", "task B", CoordinationMode::Fork, "deepseek-v4-pro", TaskComplexity::Diagnostic),
    /// ];
    /// let results = coordinator.spawn_parallel(tasks);
    /// assert_eq!(results.len(), 2);
    /// ```
    pub fn spawn_parallel(&self, tasks: Vec<SpawnRequest>) -> Vec<Result<String, String>> {
        // v2:真并行 spawn — 用 std::thread::scope 在多线程中并行注册 subagent。
        // 因为 spawn_with_model 只在内部做 capability check + Mutex-guarded HashMap 操作,
        // OS 线程并行化可消除串行瓶颈:capability check / 字符串格式化在各自线程中执行,
        // 仅 HashMap 插入时短暂争抢 Mutex(持锁时间极短)。
        //
        // 注意:本方法仅"注册 subagent 到 registry",不执行 turn。
        // 真并行执行请使用 DagScheduler(via spawn_parallel_via_dag_with_fail_fast)。
        if tasks.is_empty() {
            return Vec::new();
        }

        let len = tasks.len();
        let results = std::sync::Mutex::new(vec![None; len]);
        // Clone self once:MultiAgentCoordinator 内部全是 Arc<Mutex<...>>,clone 极轻。
        let this = self.clone();

        std::thread::scope(|s| {
            for (i, task) in tasks.into_iter().enumerate() {
                let results = &results;
                let coord = this.clone();
                s.spawn(move || {
                    let capability = task.capability;
                    let result = coord
                        .spawn_with_model(
                            task.name,
                            task.task,
                            task.mode,
                            task.model,
                            task.complexity,
                        )
                        .and_then(|id| {
                            // 传播 SpawnRequest.capability 到 Subagent(§3.1)
                            coord.set_capability(&id, capability).map(|()| id)
                        });
                    results.lock().expect("results lock poisoned")[i] = Some(result);
                });
            }
        });

        // std::thread::scope 保证所有线程在此处已 join,安全 unwrap。
        results
            .into_inner()
            .expect("results lock poisoned")
            .into_iter()
            .map(|opt| opt.expect("all slots filled by threads"))
            .collect()
    }
    /// 重置子 agent 以进行重试 — Multi-Agent Hardening §4.5。
    ///
    /// 在 retry loop 中调用,将子 agent 状态从 `Failed` 或 `Completed`
    /// (验证失败后)重置为 `Created`,允许重新执行。同时:
    /// - `attempts` +1
    /// - `validated` 重置为 false
    /// - `status` 重置为 Created
    /// - `completed_at` 清空
    /// - `result` 清空
    /// - 可选:更新 `model`(升级模型重试)
    ///
    /// # 状态转换图(§4.5.1)
    /// - `Failed --retryable--> reset_for_retry() --> Created --> start() --> Running`
    /// - `Completed --validate retryable fail--> reset_for_retry() --> Created --> start() --> Running`
    ///
    /// v2 修正:同时接受 `Failed` 和 `Completed` 状态 — 修复 v1 中
    /// `Completed` 状态不可重置的漏洞(验证失败后无法 retry)。
    ///
    /// # 返回
    /// - `Ok(())`: 重置成功
    /// - `Err(msg)`: 子 agent 不存在 / 已达 max_attempts / 状态不允许重试
    pub fn reset_for_retry(
        &self,
        subagent_id: &str,
        upgraded_model: Option<String>,
    ) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;

        // §4.5 状态转换图:Failed 和 Completed 都可重置
        // - Failed:run_subagent_turn 返回 Err 后调用 fail() 进入 Failed
        // - Completed:run_subagent_turn 成功后 complete(),但 validate() 失败
        if agent.status != SubagentStatus::Failed && agent.status != SubagentStatus::Completed {
            return Err(format!(
                "subagent {subagent_id} cannot reset_for_retry from status {:?} (only Failed/Completed allowed)",
                agent.status
            ));
        }

        // §4.5:达 reset 次数上限后停止重试
        // max_attempts 语义 = "最大尝试次数"(retry loop 用 `for attempt in 1..=max_attempts`)
        // reset 次数上限 = max_attempts - 1(因为 max_attempts=2 意味着 2 次尝试,只能 reset 1 次)
        // saturating_sub 防止 max_attempts=0 时下溢(0-1=0,即不允许任何 reset)
        let max_resets = agent.max_attempts.saturating_sub(1);
        if agent.attempts >= max_resets {
            return Err(format!(
                "subagent {subagent_id} reached max_attempts {} (resets used: {}, max resets: {})",
                agent.max_attempts, agent.attempts, max_resets
            ));
        }

        agent.attempts += 1;
        agent.validated = false;
        agent.status = SubagentStatus::Created;
        agent.completed_at = None;
        agent.result = None;
        if let Some(model) = upgraded_model {
            agent.notes.push(format!(
                "retry attempt {}: upgraded model to '{}'",
                agent.attempts, model
            ));
            agent.model = Some(model);
        } else {
            agent
                .notes
                .push(format!("retry attempt {}", agent.attempts));
        }
        Ok(())
    }

    /// 保存 checkpoint — Multi-Agent Hardening v3 P1。
    ///
    /// MVP 仅实现 save(落盘到 `checkpoint_path`),restore 留待 v2。
    /// 借鉴 LangGraph/Temporal durable execution + Anthropic "resume from where the agent was"。
    ///
    /// # 行为
    /// 1. 序列化 Subagent 为 JSON
    /// 2. 落盘到 `{workspace_root}/.claw/checkpoints/{id}.json`
    /// 3. 更新 `checkpoint_path` 字段
    ///
    /// # 返回
    /// - `Ok(path)`: checkpoint 保存路径
    /// - `Err(msg)`: 子 agent 不存在 / 序列化失败 / 落盘失败
    pub fn save_checkpoint(&self, subagent_id: &str) -> Result<PathBuf, String> {
        let agent = self
            .get(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;

        let workspace_root = self
            .workspace_root
            .lock()
            .expect("workspace_root lock")
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));

        let checkpoint_dir = workspace_root.join(".claw").join("checkpoints");
        std::fs::create_dir_all(&checkpoint_dir)
            .map_err(|e| format!("create checkpoint dir failed: {e}"))?;

        let path = checkpoint_dir.join(format!("{subagent_id}.json"));
        let json = serde_json::to_string_pretty(&agent)
            .map_err(|e| format!("serialize subagent failed: {e}"))?;
        std::fs::write(&path, json).map_err(|e| format!("write checkpoint failed: {e}"))?;

        // 更新 checkpoint_path 字段
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        if let Some(a) = agents.get_mut(subagent_id) {
            a.checkpoint_path = Some(path.clone());
        }

        crate::diag::global().append(
            crate::diag::DiagEntry::new(
                crate::diag::DiagLevel::Info,
                "checkpoint",
                format!("checkpoint saved for subagent {subagent_id}"),
            )
            .with_field(
                "path",
                serde_json::Value::String(path.to_string_lossy().into_owned()),
            ),
        );

        Ok(path)
    }

    /// 从 checkpoint 恢复 subagent — Multi-Agent Hardening v2 §10.5 Epic 4。
    ///
    /// `save_checkpoint` 的逆操作:读取 JSON 文件 → 反序列化为 `Subagent` →
    /// 插入 registry,返回恢复的 subagent_id。
    ///
    /// # 语义边界(v2 设计决策)
    /// **恢复 = 恢复 subagent 注册表 + 元状态,不恢复 LLM 对话历史。**
    /// - 恢复后 subagent 可被 retry loop 重新调度(`get`/`reset_for_retry`/`start`)
    /// - 恢复后 subagent 可被 `validate`(若状态为 Completed)
    /// - **不恢复** LLM 上下文:下一次 turn 会用全新 system prompt + task 重新构造请求
    ///
    /// 这与 LangGraph/Temporal 的 durable execution 语义一致:"resume from where
    /// the agent was" 指恢复到最近的 checkpoint 状态,而非完整的执行历史。
    ///
    /// # 状态机一致性
    /// 持久化时若状态为 `Running`(崩溃前正在执行),恢复后降级为 `Created`:
    /// - `Running` 意味着有活跃的 tokio task,但崩溃后该 task 已不存在
    /// - 降级为 `Created` 允许 retry loop 重新 `start()` 调度
    /// - 其他状态(`Created`/`Completed`/`Failed`/`Cancelled`)原样保留
    ///
    /// # 返回
    /// - `Ok(subagent_id)`: 恢复成功,返回 subagent id(可用于后续 `start`/`get`)
    /// - `Err(msg)`: 文件不存在 / 反序列化失败 / registry 已有同 id subagent
    ///
    /// # 使用场景
    /// 1. **崩溃恢复**:进程重启后扫描 `.claw/checkpoints/` 目录,对每个 checkpoint
    ///    调用 restore,让 retry loop 接管未完成的 subagent
    /// 2. **跨进程恢复**:主 CLI 崩溃后,headless 模式或新 CLI 进程可恢复 subagent
    /// 3. **调试**:从 checkpoint 文件恢复特定 subagent 状态用于复现
    pub fn restore_from_checkpoint(&self, path: &Path) -> Result<String, String> {
        let json =
            std::fs::read_to_string(path).map_err(|e| format!("read checkpoint failed: {e}"))?;
        let mut agent: Subagent =
            serde_json::from_str(&json).map_err(|e| format!("deserialize subagent failed: {e}"))?;

        // 状态机一致性:Running 降级为 Created
        // 崩溃前 Running 的 subagent 没有活跃的 tokio task,需重新 start()
        if agent.status == SubagentStatus::Running {
            agent.status = SubagentStatus::Created;
        }

        let id = agent.id.clone();

        // 检查 id 冲突:registry 已有同 id subagent 时拒绝覆盖
        {
            let agents = self.subagents.lock().expect("subagents lock poisoned");
            if agents.contains_key(&id) {
                return Err(format!(
                    "subagent {id} already exists in registry — restore would overwrite"
                ));
            }
        }

        // 插入 registry
        {
            let mut agents = self.subagents.lock().expect("subagents lock poisoned");
            agents.insert(id.clone(), agent);
        }

        crate::diag::global().append(
            crate::diag::DiagEntry::new(
                crate::diag::DiagLevel::Info,
                "checkpoint",
                format!("checkpoint restored for subagent {id}"),
            )
            .with_field(
                "path",
                serde_json::Value::String(path.to_string_lossy().into_owned()),
            ),
        );

        Ok(id)
    }

    /// 注册验证门禁 — Multi-Agent Hardening §4.4。
    pub fn add_validation_gate(&self, gate: Box<dyn ValidationGate>) {
        self.validation_gates.lock().expect("gates lock").push(gate);
    }

    /// 设置 workspace_root — Multi-Agent Hardening §4.4。
    ///
    /// 从 ConversationRuntime 注入,用于 `detect_changed_files` + `CommandValidationGate`。
    pub fn set_workspace_root(&self, root: PathBuf) {
        *self.workspace_root.lock().expect("workspace_root lock") = Some(root);
    }

    /// 验证 subagent 结果 — Multi-Agent Hardening §4.4。
    ///
    /// 调用所有注册的 gate,首个失败即返回。
    /// 成功后标记 `validated = true`。
    pub fn validate(&self, subagent_id: &str) -> Result<(), validation::ValidationError> {
        let agents = self.subagents.lock().expect("subagents lock");
        let agent = agents
            .get(subagent_id)
            .ok_or_else(|| validation::ValidationError {
                message: format!("subagent not found: {subagent_id}"),
                retryable: false,
            })?;
        if agent.status != SubagentStatus::Completed {
            return Err(validation::ValidationError {
                message: format!(
                    "subagent {subagent_id} cannot validate from status {:?} (only Completed allowed)",
                    agent.status
                ),
                retryable: false,
            });
        }
        let task = agent.task.clone();
        let result = agent.result.clone().unwrap_or_default();
        let model = agent.model.clone().unwrap_or_default();
        drop(agents); // 释放锁

        let result_path = PathBuf::from(&result);
        let workspace_root = self
            .workspace_root
            .lock()
            .expect("workspace_root lock")
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let changed_files = validation::detect_changed_files(&workspace_root);

        // Epic 5 §8.4:从 handoff frontmatter 提取子智能体声称的变更集。
        // 与 git diff 全局列表交叉比对(双列表检查),解析失败时为空(向后兼容)。
        let subagent_changed_files: Vec<PathBuf> =
            validation::read_handoff_changed_files(&workspace_root.join(&result_path));

        let ctx = validation::ValidationContext {
            subagent_id,
            task: &task,
            result_path: &result_path,
            workspace_root: &workspace_root,
            changed_files: &changed_files,
            subagent_changed_files: &subagent_changed_files,
            model: &model,
        };

        // §8.4 双列表交叉检查:仅诊断(不触发 retry),surface 异常供主 agent 排查。
        validation::diagnose_changed_files_mismatch(&ctx);

        let gates = self.validation_gates.lock().expect("gates lock");
        for gate in gates.iter() {
            gate.validate(&ctx)?;
        }
        drop(gates);

        // 标记 validated = true
        let mut agents = self.subagents.lock().expect("subagents lock");
        if let Some(a) = agents.get_mut(subagent_id) {
            a.validated = true;
        }

        Ok(())
    }

    /// 检查成本上限 — Multi-Agent Hardening v3 P0。
    ///
    /// 在 retry loop 升级模型前调用。达上限返回 false,中止 retry。
    ///
    /// # 返回
    /// - `true`: 可继续 retry(未达 cost_limit 或无 cost_limit)
    /// - `false`: 已达 cost_limit,应中止 retry
    pub fn check_cost_limit(&self, subagent_id: &str) -> bool {
        let agents = self.subagents.lock().expect("subagents lock");
        if let Some(agent) = agents.get(subagent_id) {
            if let Some(limit) = agent.cost_limit {
                return agent.cost_accumulated < limit;
            }
        }
        true // 无 cost_limit 表示无限制
    }

    /// 累加成本 — Multi-Agent Hardening v3 P0。
    ///
    /// 每次 LLM 调用后调用,更新 `cost_accumulated`。
    pub fn add_cost(&self, subagent_id: &str, cost_usd: f64) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        agent.cost_accumulated += cost_usd;
        Ok(())
    }

    /// 设置子 agent 的成本上限 — Multi-Agent Hardening v3 P0。
    ///
    /// 由 `execute_dispatch_subagent` 在 spawn 后调用,根据 JSON 输入的
    /// `cost_limit` 字段注入。`None` 表示无限制。
    pub fn set_cost_limit(&self, subagent_id: &str, limit: Option<f64>) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        agent.cost_limit = limit;
        Ok(())
    }

    /// 设置子 agent 的最大尝试次数 — Multi-Agent Hardening §4.5。
    ///
    /// 由 `execute_dispatch_subagent` 在 spawn 后调用,根据 JSON 输入的
    /// 可选 `max_attempts` 字段注入,覆盖 spawn_with_model 的默认值。
    /// 这允许调用方显式控制重试次数(如测试场景或高级用户配置)。
    pub fn set_max_attempts(&self, subagent_id: &str, max_attempts: u32) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        agent.max_attempts = max_attempts;
        Ok(())
    }

    /// 设置子 agent 的能力分级 — TRAE 架构对齐(§3.1)。
    ///
    /// 由 `execute_dispatch_subagent` / `spawn_parallel` 在 spawn 后调用,
    /// 根据 JSON 输入或 `SpawnRequest.capability` 字段注入。决定工具白名单、
    /// 是否启用工具、多轮循环上限。
    pub fn set_capability(
        &self,
        subagent_id: &str,
        capability: SubagentCapability,
    ) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        agent.capability = capability;
        Ok(())
    }

    /// 读取子 agent 的成本上限 — 用于 retry loop 失败消息中显示。
    #[must_use]
    pub fn get_cost_limit(&self, subagent_id: &str) -> Option<f64> {
        self.subagents
            .lock()
            .expect("subagents lock")
            .get(subagent_id)
            .and_then(|a| a.cost_limit)
    }

    /// 读取子 agent 的累计成本 — 用于诊断与失败消息。
    #[must_use]
    pub fn get_cost_accumulated(&self, subagent_id: &str) -> f64 {
        self.subagents
            .lock()
            .expect("subagents lock")
            .get(subagent_id)
            .map_or(0.0, |a| a.cost_accumulated)
    }

    /// 启动子 agent(标记为 Running)。
    pub fn start(&self, subagent_id: &str) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status != SubagentStatus::Created {
            return Err(format!(
                "subagent {subagent_id} cannot start from status {:?}",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Running;
        Ok(())
    }

    /// 异步执行子 agent(G10.6:tokio::spawn runtime)。
    ///
    /// 与同步 [`start`](Self::start) 不同,此方法在后台 tokio task 中
    /// 执行用户提供的闭包,自动管理状态转换:
    /// 1. 调用 `start()` 转换 Created → Running
    /// 2. 在 tokio::spawn 中执行 `executor` 闭包
    /// 3. 成功时自动标记 Completed,失败时自动标记 Failed
    ///
    /// 返回后子 agent 立即进入 Running 状态,实际结果在后台异步填充。
    /// 调用方可通过 [`get`](Self::get) 或 [`join_all`](Self::join_all) 轮询结果。
    ///
    /// # 参数
    /// - `subagent_id`:已 spawn 的子 agent ID
    /// - `executor`:实际执行逻辑(接收 id + task,返回 Ok(result) 或 Err(error))
    ///
    /// # 返回
    /// - `Ok(join_handle)`:异步任务已 spawn,可 await 获取最终结果
    /// - `Err(msg)`:子 agent 不存在或状态不允许启动
    pub fn execute_async<F, Fut>(
        &self,
        subagent_id: &str,
        executor: F,
    ) -> Result<JoinHandle<Result<String, String>>, String>
    where
        F: FnOnce(String, String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String, String>> + Send,
    {
        let agent = self
            .get(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        let task = agent.task.clone();
        let id = subagent_id.to_string();

        self.start(&id)?;

        let coord = self.clone();
        let handle = tokio::spawn(async move {
            match executor(id.clone(), task).await {
                Ok(result) => {
                    let mut agents = coord.subagents.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(agent) = agents.get_mut(&id) {
                        if agent.status == SubagentStatus::Running {
                            agent.status = SubagentStatus::Completed;
                            agent.completed_at = Some(now_secs());
                            agent.result = Some(result.clone());
                        }
                    }
                    Ok(result)
                }
                Err(error) => {
                    let mut agents = coord.subagents.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(agent) = agents.get_mut(&id) {
                        if agent.status == SubagentStatus::Running {
                            agent.status = SubagentStatus::Failed;
                            agent.completed_at = Some(now_secs());
                            agent.result = Some(format!("error: {}", &error));
                        }
                    }
                    Err(error)
                }
            }
        });

        Ok(handle)
    }

    /// 标记子 agent 完成(成功)。
    pub fn complete(&self, subagent_id: &str, result: impl Into<String>) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status != SubagentStatus::Running {
            return Err(format!(
                "subagent {subagent_id} cannot complete from status {:?}",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Completed;
        agent.completed_at = Some(now_secs());
        agent.result = Some(result.into());
        Ok(())
    }

    /// 标记子 agent 失败。
    ///
    /// 接受两种起始状态:
    /// - `Running`:turn 执行失败(LLM 错误、panic 等)
    /// - `Completed`:turn 成功但 validation 失败(§4.4 验证门禁失败后转 Failed)
    ///
    /// §4.5 retry loop 路径:`Running --turn Ok--> Completed --validate fail--> Failed`
    /// 这是合法转换,因 validate() 在 complete() 之后调用,验证失败需回退终态。
    pub fn fail(&self, subagent_id: &str, error: impl Into<String>) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status != SubagentStatus::Running && agent.status != SubagentStatus::Completed {
            return Err(format!(
                "subagent {subagent_id} cannot fail from status {:?} (only Running/Completed allowed)",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Failed;
        agent.completed_at = Some(now_secs());
        agent.result = Some(format!("error: {}", error.into()));
        Ok(())
    }

    /// 取消子 agent。
    pub fn cancel(&self, subagent_id: &str) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status == SubagentStatus::Completed
            || agent.status == SubagentStatus::Failed
            || agent.status == SubagentStatus::Cancelled
        {
            return Err(format!(
                "subagent {subagent_id} cannot cancel from terminal status {:?}",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Cancelled;
        agent.completed_at = Some(now_secs());
        Ok(())
    }

    /// 获取子 agent 引用。
    #[must_use]
    pub fn get(&self, subagent_id: &str) -> Option<Subagent> {
        self.subagents
            .lock()
            .expect("subagents lock poisoned")
            .get(subagent_id)
            .cloned()
    }

    /// 获取所有子 agent。
    #[must_use]
    pub fn list(&self) -> Vec<Subagent> {
        self.subagents
            .lock()
            .expect("subagents lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// 获取按状态过滤的子 agent。
    #[must_use]
    pub fn list_by_status(&self, status: SubagentStatus) -> Vec<Subagent> {
        self.subagents
            .lock()
            .expect("subagents lock poisoned")
            .values()
            .filter(|agent| agent.status == status)
            .cloned()
            .collect()
    }

    /// 等待所有子 agent 完成(轮询,返回最终状态统计)。
    ///
    /// 当前为同步实现(无实际异步等待),返回当前快照。
    /// 未来扩展:接入 tokio 异步等待。
    #[must_use]
    pub fn join_all(&self) -> JoinStats {
        let agents = self
            .subagents
            .lock()
            .expect("subagents lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let total = agents.len() as u64;
        let completed = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Completed)
            .count() as u64;
        let failed = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Failed)
            .count() as u64;
        let running = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Running)
            .count() as u64;
        let cancelled = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Cancelled)
            .count() as u64;
        JoinStats {
            total,
            completed,
            failed,
            running,
            cancelled,
        }
    }
}

/// 子 agent 协调器(G8.1:dispatch 逻辑)。
///
/// 在 [`MultiAgentCoordinator`] 之上提供高层 dispatch 逻辑:
/// - 根据 [`CoordinationMode`] 选择执行策略
/// - Fork/Teammate 模式:共享工作目录,主 agent 收集结果
/// - Worktree 模式:独立 git worktree,避免文件冲突
/// - 批量 spawn + dispatch + join 工作流
#[derive(Clone, Default)]
pub struct SubagentCoordinator {
    inner: MultiAgentCoordinator,
}

impl SubagentCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: MultiAgentCoordinator::new(),
        }
    }

    /// 获取内部 [`MultiAgentCoordinator`] 引用。
    #[must_use]
    pub fn inner(&self) -> &MultiAgentCoordinator {
        &self.inner
    }

    /// 派生子 agent(委托到 inner.spawn)。
    pub fn spawn(
        &self,
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
    ) -> String {
        self.inner.spawn(name, task, mode)
    }

    /// 派生子 agent 并立即异步执行。
    ///
    /// 这是最常见的 dispatch 模式:spawn + execute_async 组合,
    /// 子 agent 在后台异步执行,调用方通过 `get`/`join_all` 轮询结果。
    ///
    /// # 参数
    /// - `name`:人类可读名称
    /// - `task`:任务描述
    /// - `mode`:编排模式
    /// - `executor`:异步执行闭包
    ///
    /// # 返回
    /// - `Ok((subagent_id, join_handle))`:spawn + dispatch 成功
    /// - `Err(msg)`:状态不允许启动
    pub fn dispatch<F, Fut>(
        &self,
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
        executor: F,
    ) -> Result<(String, JoinHandle<Result<String, String>>), String>
    where
        F: FnOnce(String, String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String, String>> + Send,
    {
        let id = self.inner.spawn(name, task, mode);
        let handle = self.inner.execute_async(&id, executor)?;
        Ok((id, handle))
    }

    /// 获取子 agent 状态。
    #[must_use]
    pub fn get(&self, subagent_id: &str) -> Option<Subagent> {
        self.inner.get(subagent_id)
    }

    /// 获取所有子 agent。
    #[must_use]
    pub fn list(&self) -> Vec<Subagent> {
        self.inner.list()
    }

    /// 取消子 agent。
    pub fn cancel(&self, subagent_id: &str) -> Result<(), String> {
        self.inner.cancel(subagent_id)
    }

    /// 等待所有子 agent 完成。
    #[must_use]
    pub fn join_all(&self) -> JoinStats {
        self.inner.join_all()
    }
}

/// join_all 返回的统计信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinStats {
    pub total: u64,
    pub completed: u64,
    pub failed: u64,
    pub running: u64,
    pub cancelled: u64,
}

impl JoinStats {
    /// 是否所有子 agent 都已到达终态(completed/failed/cancelled)。
    #[must_use]
    pub fn all_done(&self) -> bool {
        self.running == 0 && self.completed + self.failed + self.cancelled == self.total
    }
}

/// 模型升级目标 — Multi-Agent Hardening §4.2/§4.5。
///
/// 与 `api::providers::model_tier::UpgradeEntry` 对应,但因 `runtime`
/// 不能依赖 `api` crate(循环依赖),本地定义。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUpgrade {
    /// 升级目标模型名(如 "deepseek-v4-pro")
    pub target_model: String,
    /// 成本倍数:升级后单次调用成本 ≈ 原成本 × cost_multiplier。
    /// deepseek-v4-pro 相对 flash 约 10 倍(输入)+ 30 倍(输出),MVP 取 10.0。
    pub cost_multiplier: f64,
}

/// 模型升级路径查询 — Multi-Agent Hardening §4.5 retry loop 调用。
///
/// DeepSeek 升级链:`deepseek-v4-flash → deepseek-v4-pro`
///
/// 完整的 `upgrade_map` 配置化在 `api::providers::model_tier`(运行时可访问),
/// 本函数仅作为 runtime 内部的最小可用回退,避免循环依赖。
/// 配置文件 `~/.claw/model-upgrades.json` 存在时由 api crate 优先读取。
///
/// # 返回
/// - `Some(ModelUpgrade)`:存在升级路径
/// - `None`:模型已是最高层级 / 无升级路径
///
/// # 升级表
/// | 当前模型 | 目标模型 | cost_multiplier | 链 |
/// |---|---|---|---|
/// | deepseek-v4-flash | deepseek-v4-pro | 10.0 | DeepSeek |
#[must_use]
pub fn upgrade_model_for_subagent(current_model: &str) -> Option<ModelUpgrade> {
    let lower = current_model.to_ascii_lowercase();

    // 已是旗舰:不再升级
    // 覆盖所有 Flagship 模式(与 api::model_tier::tier_for_model 保持一致):
    // - `*-pro` 后缀(deepseek-v4-pro 等)
    if is_flagship_model(&lower) {
        return None;
    }

    // DeepSeek 升级链
    upgrade_lookup(&lower)
}

/// 判断模型是否为 Flagship 层级(与 `api::providers::model_tier::tier_for_model` 保持一致)。
///
/// runtime crate 不能依赖 api crate,因此本地复制判断逻辑。
/// 若 api crate 的 `tier_for_model` 修改,本函数需同步更新。
fn is_flagship_model(lower: &str) -> bool {
    // 旗舰:*-pro 后缀(DeepSeek 系列旗舰)
    lower.ends_with("-pro")
}

/// 升级表查询 — DeepSeek 系列升级路径。
///
/// 2026-07-31 V4-Flash 正式版上线后,Agent 能力全面超越 Pro 预览版且价格更低,
/// 自动升级链已关闭。Pro 正式版发布后再评估是否重新启用。
///
/// 返回 `Some(ModelUpgrade)` 若命中升级路径,`None` 若无升级路径或已是旗舰。
fn upgrade_lookup(_lower: &str) -> Option<ModelUpgrade> {
    // 自动升级已关闭 — 如需恢复,取消下方注释:
    // // DeepSeek 链:flash → pro
    // if _lower.contains("deepseek") && _lower.contains("flash") {
    //     return Some(ModelUpgrade {
    //         target_model: "deepseek-v4-pro".to_string(),
    //         cost_multiplier: 10.0,
    //     });
    // }
    // // 通用 flash → pro 升级(兜底:其他含 flash 的模型替换为 pro)
    // if _lower.contains("flash") {
    //     let target = _lower.replace("flash", "pro");
    //     return Some(ModelUpgrade {
    //         target_model: target,
    //         cost_multiplier: 10.0,
    //     });
    // }

    // 无升级路径
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_fork_mode_creates_subagent_without_workdir() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("test-agent", "do something", CoordinationMode::Fork);
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.mode, CoordinationMode::Fork);
        assert!(agent.workdir.is_none());
        assert_eq!(agent.status, SubagentStatus::Created);
    }

    #[test]
    fn spawn_worktree_mode_creates_subagent_with_workdir() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("worktree-agent", "refactor", CoordinationMode::Worktree);
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.mode, CoordinationMode::Worktree);
        assert!(agent.workdir.is_some());
        assert!(agent
            .workdir
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .contains(&id));
    }

    #[test]
    fn start_transitions_created_to_running() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).expect("start should succeed");
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Running);
    }

    #[test]
    fn start_fails_from_terminal_status() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "done").unwrap();
        // Cannot start from Completed
        let err = coord.start(&id).unwrap_err();
        assert!(err.contains("cannot start"));
    }

    #[test]
    fn complete_transitions_running_to_completed() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "all done").unwrap();
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Completed);
        assert_eq!(agent.result.as_deref(), Some("all done"));
        assert!(agent.completed_at.is_some());
    }

    #[test]
    fn fail_transitions_running_to_failed() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.fail(&id, "compilation error").unwrap();
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Failed);
        assert!(agent.result.as_ref().unwrap().contains("compilation error"));
    }

    #[test]
    fn cancel_transitions_non_terminal_to_cancelled() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.cancel(&id).unwrap();
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Cancelled);
    }

    #[test]
    fn cancel_fails_from_terminal_status() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "done").unwrap();
        let err = coord.cancel(&id).unwrap_err();
        assert!(err.contains("cannot cancel"));
    }

    #[test]
    fn list_returns_all_subagents() {
        let coord = MultiAgentCoordinator::new();
        coord.spawn("a1", "t1", CoordinationMode::Fork);
        coord.spawn("a2", "t2", CoordinationMode::Teammate);
        coord.spawn("a3", "t3", CoordinationMode::Worktree);
        assert_eq!(coord.list().len(), 3);
    }

    #[test]
    fn list_by_status_filters_correctly() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        coord.start(&id1).unwrap();
        coord.start(&id2).unwrap();
        coord.complete(&id1, "done").unwrap();
        assert_eq!(coord.list_by_status(SubagentStatus::Completed).len(), 1);
        assert_eq!(coord.list_by_status(SubagentStatus::Running).len(), 1);
        assert_eq!(coord.list_by_status(SubagentStatus::Created).len(), 0);
    }

    #[test]
    fn join_all_returns_correct_stats() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        let id3 = coord.spawn("a3", "t3", CoordinationMode::Fork);
        coord.start(&id1).unwrap();
        coord.start(&id2).unwrap();
        coord.start(&id3).unwrap();
        coord.complete(&id1, "done").unwrap();
        coord.fail(&id2, "error").unwrap();

        let stats = coord.join_all();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.running, 1);
        assert_eq!(stats.cancelled, 0);
        assert!(!stats.all_done());
    }

    #[test]
    fn join_all_all_done_when_no_running() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        coord.start(&id1).unwrap();
        coord.start(&id2).unwrap();
        coord.complete(&id1, "done").unwrap();
        coord.cancel(&id2).unwrap();

        let stats = coord.join_all();
        assert!(stats.all_done());
    }

    #[test]
    fn teammate_mode_creates_subagent_without_workdir() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("teammate", "collab task", CoordinationMode::Teammate);
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.mode, CoordinationMode::Teammate);
        assert!(agent.workdir.is_none());
    }

    #[test]
    fn subagent_ids_are_unique() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        assert_ne!(id1, id2);
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let coord = MultiAgentCoordinator::new();
        assert!(coord.get("nonexistent").is_none());
    }

    #[test]
    fn start_returns_error_for_unknown_id() {
        let coord = MultiAgentCoordinator::new();
        assert!(coord.start("nonexistent").is_err());
    }

    // ===== Multi-Agent Hardening P0 步骤 9 端到端 MVP 验证 =====
    // 依据 docs/multi-agent-hardening-plan.md §10.4 验收标准

    /// §10.4 spawn_with_model:Budget 模型拒绝 Diagnostic 任务
    #[test]
    fn spawn_with_model_rejects_budget_for_diagnostic() {
        let coord = MultiAgentCoordinator::new();
        let err = coord
            .spawn_with_model(
                "diag-agent",
                "root cause analysis",
                CoordinationMode::Fork,
                "deepseek-v4-flash",
                TaskComplexity::Diagnostic,
            )
            .expect_err("flash should reject Diagnostic");
        assert!(
            err.contains("Budget tier") && err.contains("Diagnostic"),
            "expected Budget rejection msg, got: {err}"
        );
    }

    /// §10.4 spawn_with_model:Budget 模型拒绝 Architectural 任务
    #[test]
    fn spawn_with_model_rejects_budget_for_architectural() {
        let coord = MultiAgentCoordinator::new();
        let err = coord
            .spawn_with_model(
                "arch-agent",
                "design review",
                CoordinationMode::Fork,
                "claude-haiku-4-5",
                TaskComplexity::Architectural,
            )
            .expect_err("haiku should reject Architectural");
        assert!(err.contains("Budget tier"));
    }

    /// §10.4 spawn_with_model:Flagship 模型接受 Diagnostic 任务,且 max_attempts 默认 2(1 次原始 + 1 次重试)
    #[test]
    fn spawn_with_model_accepts_flagship_for_diagnostic() {
        let coord = MultiAgentCoordinator::new();
        let id = coord
            .spawn_with_model(
                "diag-agent",
                "root cause analysis",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Diagnostic,
            )
            .expect("pro should accept Diagnostic");
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(agent.complexity, TaskComplexity::Diagnostic);
        assert_eq!(
            agent.max_attempts, 2,
            "Diagnostic 默认 2 次尝试(1 次原始 + 1 次重试)"
        );
        assert_eq!(agent.attempts, 0);
        assert!(!agent.validated);
    }

    /// §10.4 spawn_with_model:Simple 任务 + Budget 模型,max_attempts 保持默认 1
    #[test]
    fn spawn_with_model_simple_task_keeps_default_max_attempts() {
        let coord = MultiAgentCoordinator::new();
        let id = coord
            .spawn_with_model(
                "simple-agent",
                "format code",
                CoordinationMode::Fork,
                "deepseek-v4-flash",
                TaskComplexity::Simple,
            )
            .expect("flash should accept Simple");
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.complexity, TaskComplexity::Simple);
        assert_eq!(agent.max_attempts, 1);
    }

    /// §10.3 P1 步骤 7:spawn_parallel 接口预留 — 多任务串行 spawn
    #[test]
    fn spawn_parallel_spawns_multiple_subagents_serially() {
        let coord = MultiAgentCoordinator::new();
        let tasks = vec![
            SpawnRequest::new(
                "agent-a",
                "task A",
                CoordinationMode::Fork,
                "deepseek-v4-flash",
                TaskComplexity::Simple,
            ),
            SpawnRequest::new(
                "agent-b",
                "task B",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Diagnostic,
            ),
            SpawnRequest::new(
                "agent-c",
                "task C",
                CoordinationMode::Teammate,
                "deepseek-v4-flash",
                TaskComplexity::Simple,
            ),
        ];

        let results = coord.spawn_parallel(tasks);
        assert_eq!(results.len(), 3, "should return 3 results");
        assert!(
            results.iter().all(|r| r.is_ok()),
            "all spawns should succeed"
        );

        // 验证三个 subagent 都已注册且字段正确
        let id_a = results[0].as_ref().unwrap();
        let id_b = results[1].as_ref().unwrap();
        let id_c = results[2].as_ref().unwrap();
        assert_ne!(id_a, id_b, "ids should be unique");
        assert_ne!(id_b, id_c, "ids should be unique");

        let agent_a = coord.get(id_a).expect("agent-a exists");
        assert_eq!(agent_a.name, "agent-a");
        assert_eq!(agent_a.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(agent_a.complexity, TaskComplexity::Simple);

        let agent_b = coord.get(id_b).expect("agent-b exists");
        assert_eq!(agent_b.name, "agent-b");
        assert_eq!(agent_b.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(agent_b.complexity, TaskComplexity::Diagnostic);
        assert_eq!(
            agent_b.max_attempts, 2,
            "Diagnostic should default to max_attempts=2"
        );

        let agent_c = coord.get(id_c).expect("agent-c exists");
        assert_eq!(agent_c.mode, CoordinationMode::Teammate);
    }

    /// §10.3 P1 步骤 7:spawn_parallel 空列表返回空结果
    #[test]
    fn spawn_parallel_empty_list_returns_empty() {
        let coord = MultiAgentCoordinator::new();
        let results = coord.spawn_parallel(vec![]);
        assert!(results.is_empty(), "empty input should return empty");
    }

    /// §10.3 P1 步骤 7:spawn_parallel 能力校验失败时返回对应 Err
    #[test]
    fn spawn_parallel_propagates_capability_errors() {
        let coord = MultiAgentCoordinator::new();
        // flash + Diagnostic 应失败(Budget 模型不能处理 Diagnostic 任务)
        let tasks = vec![
            SpawnRequest::new(
                "ok-agent",
                "simple task",
                CoordinationMode::Fork,
                "deepseek-v4-flash",
                TaskComplexity::Simple,
            ),
            SpawnRequest::new(
                "bad-agent",
                "diagnostic task",
                CoordinationMode::Fork,
                "deepseek-v4-flash",
                TaskComplexity::Diagnostic,
            ),
        ];

        let results = coord.spawn_parallel(tasks);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok(), "first task should succeed");
        assert!(
            results[1].is_err(),
            "second task should fail (Budget + Diagnostic)"
        );

        // 验证错误消息含能力校验信息
        let err = results[1].as_ref().unwrap_err();
        assert!(
            err.contains("Budget"),
            "error should mention Budget tier: {err}"
        );
    }

    /// §10.3 P1 步骤 7:SpawnRequest::new 构造正确字段
    #[test]
    fn spawn_request_new_constructs_correctly() {
        let req = SpawnRequest::new(
            "test-agent",
            "test task",
            CoordinationMode::Worktree,
            "deepseek-v4-pro",
            TaskComplexity::Architectural,
        );
        assert_eq!(req.name, "test-agent");
        assert_eq!(req.task, "test task");
        assert_eq!(req.mode, CoordinationMode::Worktree);
        assert_eq!(req.model, "deepseek-v4-pro");
        assert_eq!(req.complexity, TaskComplexity::Architectural);
    }

    /// Epic 0(§3.1):`SubagentCapability::allowed_tools()` 三个变体返回值正确。
    #[test]
    fn subagent_capability_allowed_tools_correct() {
        assert!(SubagentCapability::Analyze.allowed_tools().is_empty());
        let ro = SubagentCapability::ReadOnly.allowed_tools();
        assert_eq!(
            ro,
            &["read_file", "grep_search", "glob_search", "repomap", "lsp_diagnostics"]
        );
        let ex = SubagentCapability::Execute.allowed_tools();
        assert!(ex.contains(&"edit_file"));
        assert!(ex.contains(&"write_file"));
        assert!(ex.contains(&"bash"));
        // Execute 是 ReadOnly 的超集
        for t in ro {
            assert!(ex.contains(t), "Execute missing read-only tool {t}");
        }
        // 白名单不含递归派发工具
        assert!(!ex.contains(&"dispatch_subagent"));
        assert!(!ex.contains(&"spawn_parallel_subagents"));
    }

    /// Epic 0(§3.1):`enables_tools()` / `max_iterations()` 行为正确。
    #[test]
    fn subagent_capability_enables_tools_and_max_iterations() {
        assert!(!SubagentCapability::Analyze.enables_tools());
        assert!(SubagentCapability::ReadOnly.enables_tools());
        assert!(SubagentCapability::Execute.enables_tools());

        assert_eq!(SubagentCapability::Analyze.max_iterations(), 1);
        assert_eq!(SubagentCapability::ReadOnly.max_iterations(), 5);
        assert_eq!(SubagentCapability::Execute.max_iterations(), 10);
    }

    /// Epic 0:`SpawnRequest::new` 默认 capability = Analyze(向后兼容)。
    /// `with_capability` builder 正确设置。
    #[test]
    fn spawn_request_default_capability_and_builder() {
        let req = SpawnRequest::new(
            "a",
            "t",
            CoordinationMode::Fork,
            "m",
            TaskComplexity::Simple,
        );
        assert_eq!(req.capability, SubagentCapability::Analyze);

        let req = req.with_capability(SubagentCapability::Execute);
        assert_eq!(req.capability, SubagentCapability::Execute);
    }

    /// Epic 0:`Subagent` 缺 `capability` 字段反序列化默认 Analyze(向后兼容)。
    #[test]
    fn subagent_deserialize_missing_capability_defaults_analyze() {
        // 旧格式 JSON(无 capability 字段)应能正确反序列化,capability 默认 Analyze
        let json = r#"{
            "id": "test-id",
            "name": "test",
            "mode": "fork",
            "task": "do something",
            "status": "created",
            "workdir": null,
            "created_at": 0,
            "completed_at": null,
            "result": null,
            "model": null,
            "complexity": "simple",
            "max_attempts": 1,
            "attempts": 0,
            "validated": false,
            "notes": [],
            "checkpoint_path": null,
            "cost_limit": null,
            "cost_accumulated": 0.0
        }"#;
        let agent: Subagent = serde_json::from_str(json).expect("deserialize old format");
        assert_eq!(agent.capability, SubagentCapability::Analyze);
    }

    /// Epic 0:`Subagent` 含 `capability` 字段反序列化(kebab-case)。
    #[test]
    fn subagent_deserialize_with_capability() {
        let json = r#"{
            "id": "x",
            "name": "x",
            "mode": "fork",
            "task": "x",
            "status": "created",
            "workdir": null,
            "created_at": 0,
            "completed_at": null,
            "result": null,
            "complexity": "simple",
            "capability": "read-only",
            "max_attempts": 1,
            "attempts": 0,
            "validated": false,
            "notes": [],
            "checkpoint_path": null,
            "cost_limit": null,
            "cost_accumulated": 0.0
        }"#;
        let agent: Subagent = serde_json::from_str(json).expect("deserialize with capability");
        assert_eq!(agent.capability, SubagentCapability::ReadOnly);
    }

    /// Epic 0:`set_capability` 正确更新 subagent capability。
    #[test]
    fn set_capability_updates_subagent() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        assert_eq!(
            coord.get(&id).unwrap().capability,
            SubagentCapability::Analyze
        );
        coord
            .set_capability(&id, SubagentCapability::Execute)
            .unwrap();
        assert_eq!(
            coord.get(&id).unwrap().capability,
            SubagentCapability::Execute
        );
    }

    /// §10.5 v2:spawn_parallel 串行退化仅注册 subagent,不执行 turn。
    /// 真并行执行应使用 DAG 模块(DagScheduler + CoordinatorExecutor)。
    /// 本测试验证串行退化的语义正确性:返回的 id 都能在 coordinator 中查到,
    /// 但 status 仍是 Created(未执行 turn)。
    #[test]
    fn spawn_parallel_serial_degradation_registers_without_executing() {
        let coord = MultiAgentCoordinator::new();
        let tasks = vec![
            SpawnRequest::new(
                "agent-a",
                "task A",
                CoordinationMode::Fork,
                "deepseek-v4-flash",
                TaskComplexity::Simple,
            ),
            SpawnRequest::new(
                "agent-b",
                "task B",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Simple,
            ),
        ];
        let results = coord.spawn_parallel(tasks);
        assert_eq!(results.len(), 2, "should return 2 results");
        // 两个 id 都应成功注册
        let id_a = results[0].as_ref().expect("agent-a spawn should succeed");
        let id_b = results[1].as_ref().expect("agent-b spawn should succeed");
        // 注册后 status 应为 Created(串行退化不执行 turn)
        let agent_a = coord.get(id_a).expect("agent-a should be in registry");
        assert_eq!(
            agent_a.status,
            SubagentStatus::Created,
            "serial degradation should NOT execute turn — status must remain Created"
        );
        let agent_b = coord.get(id_b).expect("agent-b should be in registry");
        assert_eq!(
            agent_b.status,
            SubagentStatus::Created,
            "serial degradation should NOT execute turn — status must remain Created"
        );
        // 真并行执行路径:用 DagScheduler + CoordinatorExecutor,
        // 详见 dag::coordinator_executor::tests 和 dag::scheduler::tests
    }

    /// §10.4 reset_for_retry:从 Failed 状态重置,attempts+1,model 升级
    #[test]
    fn reset_for_retry_from_failed_upgrades_model() {
        let coord = MultiAgentCoordinator::new();
        // 用 pro + Diagnostic 获得 max_attempts=2(允许 1 次 reset)
        let id = coord
            .spawn_with_model(
                "diag",
                "task",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Diagnostic,
            )
            .expect("spawn ok");
        coord.start(&id).unwrap();
        coord.fail(&id, "turn error").unwrap();

        coord
            .reset_for_retry(&id, Some("deepseek-v4-pro".to_string()))
            .expect("reset should succeed");

        let agent = coord.get(&id).expect("agent exists");
        assert_eq!(agent.status, SubagentStatus::Created);
        assert_eq!(agent.attempts, 1);
        assert!(!agent.validated);
        assert_eq!(agent.model.as_deref(), Some("deepseek-v4-pro"));
        assert!(agent.result.is_none());
        assert!(agent.completed_at.is_none());
        assert!(agent.notes.iter().any(|n| n.contains("upgraded model")));
    }

    /// §10.4 reset_for_retry:从 Completed 状态(验证失败)重置
    #[test]
    fn reset_for_retry_from_completed_after_validation_fail() {
        let coord = MultiAgentCoordinator::new();
        // 用 pro + Diagnostic 获得 max_attempts=2(允许 1 次 reset)
        let id = coord
            .spawn_with_model(
                "agent",
                "task",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Diagnostic,
            )
            .expect("spawn ok");
        coord.start(&id).unwrap();
        coord.complete(&id, "result").unwrap();

        coord
            .reset_for_retry(&id, None)
            .expect("reset from Completed should succeed");

        let agent = coord.get(&id).expect("agent exists");
        assert_eq!(agent.status, SubagentStatus::Created);
        assert_eq!(agent.attempts, 1);
        assert!(agent.result.is_none());
    }

    /// §10.4 reset_for_retry:从 Running 状态不可重置
    #[test]
    fn reset_for_retry_rejects_running_state() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        let err = coord
            .reset_for_retry(&id, None)
            .expect_err("Running should reject");
        assert!(err.contains("cannot reset_for_retry"));
    }

    /// §10.4 reset_for_retry:达 reset 次数上限(max_attempts - 1)后不可重置
    #[test]
    fn reset_for_retry_rejects_when_max_attempts_reached() {
        let coord = MultiAgentCoordinator::new();
        let id = coord
            .spawn_with_model(
                "diag",
                "task",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Diagnostic,
            )
            .expect("spawn ok");
        // Diagnostic 默认 max_attempts=2(2 次尝试),reset 上限 = 2-1 = 1
        coord.start(&id).unwrap();
        coord.fail(&id, "err").unwrap();
        // 第一次 reset:attempts 0→1,允许(1 <= max_resets=1)
        coord.reset_for_retry(&id, None).expect("first reset ok");
        // 第二次 reset:attempts=1 已达 max_resets=1,拒绝
        coord.start(&id).unwrap();
        coord.fail(&id, "err2").unwrap();
        let err = coord
            .reset_for_retry(&id, None)
            .expect_err("should reject at max_attempts");
        assert!(err.contains("max_attempts"));
    }

    /// §10.4 reset_for_retry:max_attempts=1(Simple 任务)时不允许任何 reset
    #[test]
    fn reset_for_retry_rejects_when_max_attempts_is_one() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("simple", "task", CoordinationMode::Fork);
        // 默认 max_attempts=1,max_resets=0,不允许任何 reset
        coord.start(&id).unwrap();
        coord.fail(&id, "err").unwrap();
        let err = coord
            .reset_for_retry(&id, None)
            .expect_err("max_attempts=1 should reject all resets");
        assert!(err.contains("max_attempts"));
        assert!(err.contains("max resets: 0"));
    }

    /// §10.4 成本门禁:check_cost_limit 无 limit 时返回 true
    #[test]
    fn check_cost_limit_returns_true_when_no_limit() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        assert!(coord.check_cost_limit(&id));
    }

    /// §10.4 成本门禁:check_cost_limit 在 accumulated < limit 时返回 true
    #[test]
    fn check_cost_limit_returns_true_when_under_limit() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        coord.set_cost_limit(&id, Some(0.50)).unwrap();
        coord.add_cost(&id, 0.10).unwrap();
        assert!(coord.check_cost_limit(&id), "0.10 < 0.50 应允许");
    }

    /// §10.4 场景 5:check_cost_limit 在 accumulated >= limit 时返回 false
    #[test]
    fn check_cost_limit_returns_false_when_limit_exceeded() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        // 场景 5:cost_limit=0.30,flash 调用 $0.001 后 accumulated=0.001,
        // 但升级 pro 预估 $0.42,0.001+0.42 > 0.30 应拒绝
        coord.set_cost_limit(&id, Some(0.30)).unwrap();
        coord.add_cost(&id, 0.001).unwrap();
        // check_cost_limit 仅检查 accumulated < limit
        // 0.001 < 0.30 → true(实际场景 5 需升级前预估,见 conversation 端到端测试)
        assert!(coord.check_cost_limit(&id), "0.001 < 0.30 仍允许");
        // 累加到超限
        coord.add_cost(&id, 0.50).unwrap();
        assert!(!coord.check_cost_limit(&id), "0.501 > 0.30 应拒绝");
    }

    /// §10.4 成本门禁:add_cost 正确累加
    #[test]
    fn add_cost_accumulates_correctly() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        assert_eq!(coord.get_cost_accumulated(&id), 0.0);
        coord.add_cost(&id, 0.001).unwrap();
        assert_eq!(coord.get_cost_accumulated(&id), 0.001);
        coord.add_cost(&id, 0.01).unwrap();
        assert!((coord.get_cost_accumulated(&id) - 0.011).abs() < 1e-9);
    }

    /// §10.4 成本门禁:add_cost 对未知 id 返回 Err
    #[test]
    fn add_cost_errors_for_unknown_id() {
        let coord = MultiAgentCoordinator::new();
        assert!(coord.add_cost("nonexistent", 0.1).is_err());
    }

    /// §10.4 成本门禁:set_cost_limit + get_cost_limit 往返
    #[test]
    fn set_and_get_cost_limit_roundtrip() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        assert_eq!(coord.get_cost_limit(&id), None);
        coord.set_cost_limit(&id, Some(0.42)).unwrap();
        assert_eq!(coord.get_cost_limit(&id), Some(0.42));
        coord.set_cost_limit(&id, None).unwrap();
        assert_eq!(coord.get_cost_limit(&id), None);
    }

    /// §10.4 checkpoint:save_checkpoint 落盘到 {workspace_root}/.claw/checkpoints/{id}.json
    #[test]
    fn save_checkpoint_writes_json_file_with_required_fields() {
        let coord = MultiAgentCoordinator::new();
        let tempdir = tempfile::tempdir().expect("create temp dir");
        coord.set_workspace_root(tempdir.path().to_path_buf());

        let id = coord
            .spawn_with_model(
                "diag-agent",
                "root cause analysis",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Diagnostic,
            )
            .expect("spawn ok");
        coord.start(&id).unwrap();
        coord.add_cost(&id, 0.022).unwrap();

        let path = coord.save_checkpoint(&id).expect("save should succeed");
        assert!(path.exists(), "checkpoint file should exist at {path:?}");
        // 跨平台路径检查:Windows 用 `\`,Unix 用 `/`
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("checkpoints") && path_str.contains(".claw"),
            "checkpoint path should contain .claw/checkpoints/: {path_str}"
        );

        // 验证文件包含 §10.4 要求的字段:id/task/model/attempts/cost_accumulated
        let content = std::fs::read_to_string(&path).expect("read checkpoint");
        let json: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert_eq!(json["id"], id);
        assert_eq!(json["task"], "root cause analysis");
        assert_eq!(json["model"], "deepseek-v4-pro");
        assert_eq!(json["attempts"], 0);
        assert!((json["cost_accumulated"].as_f64().unwrap() - 0.022).abs() < 1e-9);

        // checkpoint_path 字段应更新
        let agent = coord.get(&id).expect("agent exists");
        assert_eq!(agent.checkpoint_path.as_ref(), Some(&path));
    }

    /// §10.4 checkpoint:save_checkpoint 对未知 id 返回 Err
    #[test]
    fn save_checkpoint_errors_for_unknown_id() {
        let coord = MultiAgentCoordinator::new();
        assert!(coord.save_checkpoint("nonexistent").is_err());
    }

    /// §10.4 checkpoint:save_checkpoint 失败不影响主流程(由调用方 `let _ =` 容错)
    #[test]
    fn save_checkpoint_failure_is_non_fatal() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        // 故意不设置 workspace_root,使用默认 "."(可能无写权限或不存在 .claw/checkpoints)
        // 实际场景中调用方用 `let _ = coordinator.save_checkpoint(&id)` 容错
        let _ = coord.save_checkpoint(&id);
        // 不 panic 即视为通过
    }

    /// §10.5 v2 Epic 4:restore_from_checkpoint roundtrip 恢复 subagent 元状态
    #[test]
    fn restore_from_checkpoint_rebuilds_subagent_state() {
        let coord = MultiAgentCoordinator::new();
        let tempdir = tempfile::tempdir().expect("create temp dir");
        coord.set_workspace_root(tempdir.path().to_path_buf());

        // 创建 subagent 并推进到 Completed 状态
        let id = coord
            .spawn_with_model(
                "diag-agent",
                "root cause analysis",
                CoordinationMode::Fork,
                "deepseek-v4-pro",
                TaskComplexity::Diagnostic,
            )
            .expect("spawn ok");
        coord.start(&id).unwrap();
        coord.add_cost(&id, 0.022).unwrap();
        coord.complete(&id, "fixed").unwrap();

        // 保存 checkpoint
        let path = coord.save_checkpoint(&id).expect("save should succeed");

        // 用新的 coordinator 模拟"进程重启"
        let coord2 = MultiAgentCoordinator::new();
        coord2.set_workspace_root(tempdir.path().to_path_buf());

        // 恢复
        let restored_id = coord2
            .restore_from_checkpoint(&path)
            .expect("restore should succeed");
        assert_eq!(restored_id, id, "restored id should match original");

        // 验证元状态字段完整恢复
        let agent = coord2
            .get(&restored_id)
            .expect("restored agent should exist");
        assert_eq!(agent.id, id);
        assert_eq!(agent.name, "diag-agent");
        assert_eq!(agent.task, "root cause analysis");
        assert_eq!(agent.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(agent.complexity, TaskComplexity::Diagnostic);
        assert_eq!(
            agent.status,
            SubagentStatus::Completed,
            "Completed 状态应原样保留"
        );
        assert!(
            (agent.cost_accumulated - 0.022).abs() < 1e-9,
            "cost_accumulated 应恢复"
        );
        assert_eq!(agent.result.as_deref(), Some("fixed"));
    }

    /// §10.5 v2 Epic 4:restore_from_checkpoint 将 Running 状态降级为 Created
    /// (崩溃前 Running 的 subagent 没有活跃 tokio task,需重新 start)
    #[test]
    fn restore_from_checkpoint_demotes_running_to_created() {
        let coord = MultiAgentCoordinator::new();
        let tempdir = tempfile::tempdir().expect("create temp dir");
        coord.set_workspace_root(tempdir.path().to_path_buf());

        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        coord.start(&id).unwrap(); // 进入 Running
        assert_eq!(coord.get(&id).unwrap().status, SubagentStatus::Running);

        let path = coord.save_checkpoint(&id).expect("save");

        // 新 coordinator 恢复
        let coord2 = MultiAgentCoordinator::new();
        let restored_id = coord2.restore_from_checkpoint(&path).expect("restore");
        let agent = coord2.get(&restored_id).expect("exists");
        assert_eq!(
            agent.status,
            SubagentStatus::Created,
            "Running 应降级为 Created(无活跃 tokio task)"
        );
    }

    /// §10.5 v2 Epic 4:restore_from_checkpoint 对不存在文件返回 Err
    #[test]
    fn restore_from_checkpoint_errors_for_missing_file() {
        let coord = MultiAgentCoordinator::new();
        let path = std::path::Path::new("/nonexistent/checkpoint.json");
        let err = coord
            .restore_from_checkpoint(path)
            .expect_err("should fail");
        assert!(
            err.contains("read checkpoint failed"),
            "unexpected error: {err}"
        );
    }

    /// §10.5 v2 Epic 4:restore_from_checkpoint 对损坏 JSON 返回 Err
    #[test]
    fn restore_from_checkpoint_errors_for_corrupt_json() {
        let coord = MultiAgentCoordinator::new();
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let corrupt_path = tempdir.path().join("corrupt.json");
        std::fs::write(&corrupt_path, "not valid json {").unwrap();

        let err = coord
            .restore_from_checkpoint(&corrupt_path)
            .expect_err("should fail");
        assert!(err.contains("deserialize"), "unexpected error: {err}");
    }

    /// §10.5 v2 Epic 4:restore_from_checkpoint 拒绝覆盖已有同 id subagent
    #[test]
    fn restore_from_checkpoint_rejects_id_collision() {
        let coord = MultiAgentCoordinator::new();
        let tempdir = tempfile::tempdir().expect("create temp dir");
        coord.set_workspace_root(tempdir.path().to_path_buf());

        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        let path = coord.save_checkpoint(&id).expect("save");

        // 同一 coordinator 再 restore 应失败(id 已存在)
        let err = coord
            .restore_from_checkpoint(&path)
            .expect_err("should reject");
        assert!(err.contains("already exists"), "unexpected error: {err}");
    }

    /// §10.4 validate:无 gate 注册时,Completed 状态始终通过
    #[test]
    fn validate_passes_when_no_gates_registered() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "result").unwrap();
        coord.validate(&id).expect("no gates = always Ok");
        let agent = coord.get(&id).expect("agent exists");
        assert!(agent.validated, "validated flag should be set");
    }

    /// §10.4 validate:非 Completed 状态不可验证
    #[test]
    fn validate_rejects_non_completed_state() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        let err = coord.validate(&id).expect_err("Running should reject");
        assert!(!err.retryable, "状态错误不可重试");
        assert!(err.message.contains("only Completed allowed"));
    }

    /// §10.4 validate:注册自定义 gate,验证 gate 失败时返回 retryable 错误
    #[test]
    fn validate_runs_registered_gate_and_returns_retryable_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FailGate {
            call_count: AtomicUsize,
        }
        impl ValidationGate for FailGate {
            fn validate(
                &self,
                _ctx: &validation::ValidationContext,
            ) -> Result<(), validation::ValidationError> {
                let n = self.call_count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(validation::ValidationError {
                        message: "cargo build failed".into(),
                        retryable: true,
                    })
                } else {
                    Ok(())
                }
            }
            fn name(&self) -> &'static str {
                "fail-gate"
            }
        }

        let coord = MultiAgentCoordinator::new();
        let tempdir = tempfile::tempdir().expect("temp dir");
        coord.set_workspace_root(tempdir.path().to_path_buf());
        coord.add_validation_gate(Box::new(FailGate {
            call_count: AtomicUsize::new(0),
        }));

        let id = coord.spawn("a", "t", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "result").unwrap();
        // 第一次验证:gate 返回 retryable Err
        let err = coord.validate(&id).expect_err("first validate should fail");
        assert!(err.retryable);
        assert_eq!(err.message, "cargo build failed");
        // validated 不应被标记
        let agent = coord.get(&id).expect("agent exists");
        assert!(!agent.validated);
    }

    /// §10.4 upgrade_model_for_subagent:V4-Flash 正式版上线后自动升级已关闭,
    /// flash 返回 None
    #[test]
    fn upgrade_model_for_subagent_flash_returns_none_when_disabled() {
        assert!(upgrade_model_for_subagent("deepseek-v4-flash").is_none());
    }

    /// §10.4 upgrade_model_for_subagent:pro 已顶级,返回 None
    #[test]
    fn upgrade_model_for_subagent_pro_returns_none() {
        assert!(upgrade_model_for_subagent("deepseek-v4-pro").is_none());
    }

    /// §10.4 upgrade_model_for_subagent:未知模型返回 None
    #[test]
    fn upgrade_model_for_subagent_unknown_returns_none() {
        assert!(upgrade_model_for_subagent("unknown-model").is_none());
    }
}
