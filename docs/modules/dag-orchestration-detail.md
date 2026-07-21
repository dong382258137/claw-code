# DAG 多 Agent 编排细化方案

- 文档版本: v0.1
- 创建日期: 2026-07-21
- 父文档: [ide-hooks-dag-implementation-plan.md](../ide-hooks-dag-implementation-plan.md)
- 焦点: petgraph 数据结构 + JoinSet 分层调度 + Plan→DAG 转换 + Checkpointer + YAML 声明式
- 适用范围: `rust/crates/runtime/src/dag/` 新模块及其与 `multi_agent` / `planner` / `conversation` / `lane_events` 的集成
- 关联代码:
  - `file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs`
  - `file:///d:/claw-code-src/rust/crates/runtime/src/planner/mod.rs`
  - `file:///d:/claw-code-src/rust/crates/runtime/src/planner/artifact.rs`
  - `file:///d:/claw-code-src/rust/crates/runtime/src/planner/reviewer.rs`
  - `file:///d:/claw-code-src/rust/crates/runtime/src/task_registry.rs`
  - `file:///d:/claw-code-src/rust/crates/runtime/src/lane_events.rs`
  - `file:///d:/claw-code-src/rust/crates/runtime/src/recovery_orchestrator.rs`
  - `file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs`

---

## 一、现状审计

### 1.1 现有 multi_agent 模块能力盘点

`multi_agent/mod.rs` 当前实现了一个最小可用的 `MultiAgentCoordinator`,提供以下能力:

- 三种 `CoordinationMode`:`Fork` / `Teammate` / `Worktree`,但仅 `Worktree` 会分配独立 workdir,`Fork` 与 `Teammate` 在执行层没有真正差异。
- `Subagent` 生命周期状态机:`Created → Running → Completed / Failed / Cancelled`,带终态保护(不可从终态转移)。
- 同步接口:`spawn` / `start` / `complete` / `fail` / `cancel` 均为同步 `Arc<Mutex<HashMap>>` 操作,没有 `async` 等待语义。
- `join_all` 仅返回当前快照统计(`JoinStats`),不做真正的等待;代码注释明确指出"未来扩展:接入 tokio 异步等待"。

`task_registry.rs` 通过 `with_multi_agent_coordinator` 把 task 与 subagent 关联,但只承担"派发登记 + 结果回写",没有依赖图、并行调度、检查点恢复等编排能力。

`conversation.rs` 的 `execute_dispatch_subagent` / `run_subagent_turn` 实现了 "Subagent as Tool" 模式:主 agent 通过 tool call 派发,子 agent 走独立 LLM 请求(单轮、无 tool call 循环),结果写到 `.claw/subagents/{id}.md`,主 agent 收到 `result_ref` 路径后用 `Read` 读取。该路径是为单 agent 单任务设计的,无法支撑多 agent 并行、跨节点依赖、失败重试编排。

### 1.2 Plan/Execute/Review 三段循环的边界

`planner/artifact.rs` 的 `PlanArtifact` 是一个**线性**步骤容器:

- `steps: Vec<PlanStep>`,顺序敏感,无显式依赖图。
- `current_step_mut` 总是选第一个 `Executing` 或第一个 `Pending`,无法表达"两个 step 同层并行"。
- `trigger_replan` 把所有 `Failed` step 重置为 `Pending`,粒度是"全部失败 step",无法对单个失败 step 做 fallback 或局部重试。
- `verify_command` 是 step 级别的,没有节点级别的 `RetryPolicy` / `fallback_agent` 概念。

`planner/reviewer.rs` 的 `PreCompletionChecklistMiddleware` 只在 turn 末尾做一次 review,不参与 step 之间的调度,无法在 step 完成后立刻触发下游 step。

### 1.3 缺失的 DAG 能力清单

| 能力 | 现状 | DAG 模块需补齐 |
|---|---|---|
| 拓扑依赖 | `PlanStep` 仅有顺序 | `DagNode.depends_on` + petgraph 边 |
| 同层并行 | 不支持 | `JoinSet` + `max_parallelism` |
| 条件路由 | 不支持 | `DagNode.condition` 表达式 |
| 节点级重试 | 全局 `trigger_replan` | `RetryPolicy { max_attempts, backoff, fallback_agent }` |
| 检查点持久化 | `PlanArtifact` 整体写盘 | `CheckpointStore` 节点级增量 |
| 跨会话 resume | 不支持 | `resume(dag_id)` 重放已完成节点 |
| 失败传播策略 | 全局 abort / replan | `DagFailurePolicy` (`RetryThenEscalate` / `Abort` / `Fallback`) |
| 取消传播 | 不支持 | `CancellationToken` 层级(DAG 级 → 节点级) |
| 可视化 | `render_for_prompt` 纯文本 | `render_mermaid` 注入 prompt |

### 1.4 设计原则

DAG 是**编排层**,不替代 `Fork` / `Teammate` / `Worktree`。三模式保留为"执行后端",DAG 层负责:

1. 拓扑调度(同层并行 + 跨层 barrier)
2. 条件路由(if-else 分支)
3. 故障恢复(retry / fallback / replan)
4. 检查点持久化(支持 resume)

DAG 节点执行时仍调用 `MultiAgentCoordinator::spawn` 派发子 agent,即" DAG 决定何时跑、跑谁,Fork/Teammate/Worktree 决定怎么跑"。

---

## 二、依赖与 crate 选型

### 2.1 候选 crate 对比

| crate | 维护状态 | API 风格 | 算法覆盖 | 选型结论 |
|---|---|---|---|---|
| `petgraph` | 活跃(社区主流) | `DiGraph<N, E>` 泛型容器 | `toposort` / `kosaraju_scc` / `dijkstra` / `astar` 开箱即用 | **采用** |
| `daggy` | 维护滞后(基于 petgraph) | 强类型 `Dag` 包装 | 仅 DAG 语义,无新增算法 | 不采用(子集,无收益) |
| `petgraph-stable` | 维护滞后 | `StableDiGraph` | 节点删除后 `NodeIndex` 不变 | 暂不需要(DAG 节点不删除) |

### 2.2 版本选择与 Cargo.toml 配置

当前 `rust/Cargo.toml` workspace dependencies 仅有 `serde_json`,**没有** `petgraph` / `tokio-util`。`runtime/Cargo.toml` 也没有这两项。需新增:

```toml
# rust/Cargo.toml(workspace 根)
[workspace.dependencies]
serde_json = "1"
petgraph = "0.6"
tokio-util = { version = "0.7", features = ["rt"] }
```

```toml
# rust/crates/runtime/Cargo.toml
[dependencies]
# ... 现有依赖 ...
petgraph.workspace = true
tokio-util.workspace = true
```

### 2.3 选型理由详细论证

`petgraph` 选型的关键依据:

1. **`algo::kosaraju_scc`** 提供 SCC(强连通分量)分解,DAG 等价于"所有 SCC 大小均为 1",环检测一行代码即可。比起手写 DFS 染色法更可靠,且复杂度 O(V+E)。
2. **`algo::toposort`** 直接给出拓扑序,用于 `render_mermaid` 的线性化展示和 `topological_order` API。失败(存在环)时返回 `Err<NodeIndex>`,可定位环上第一个节点。
3. **`DiGraph<DagNode, ()>`** 用 `()` 作为边权重,因为依赖关系不需要附加属性(条件表达式存在 `DagNode.condition` 上,不在边上)。未来若需要边属性(如 `EdgeCondition`),可改为 `DiGraph<DagNode, EdgeKind>`。
4. **`neighbors_directed(idx, Direction::Incoming)`** 用于 `ready_nodes` 的入度计算,语义清晰。

`tokio-util` 的 `CancellationToken` 选型依据:

1. **协作式取消**:`cancel.cancelled()` 是 `Future`,在 `tokio::select!` 中可优雅退出,不强制 abort。
2. **层级化**:`child_token()` 创建子 token,父 token 取消时所有子 token 自动取消。DAG 级取消 → 节点级取消 → 子 agent LLM 请求取消,一层层传播。
3. **零开销**:未取消时 `cancelled()` 永远 pending,不消耗 CPU。

### 2.4 替代方案与回退路径

若未来 `petgraph` 维护停滞,可回退到自研 `HashMap<String, DagNode>` + 手写 Kahn 算法(入度表 + 队列)。代价:

- 环检测需手写 DFS 染色(O(V+E)),易错。
- 拓扑序需手写 Kahn(O(V+E)),与 `toposort` 重复造轮子。
- 无社区维护的图算法扩展(最短路、流网络等)。

建议保留 `petgraph` 至少 2 个 major 版本窗口,除非出现安全漏洞或 maintenance 停滞超 12 个月。

---

## 三、DagNode 数据结构

### 3.1 完整字段定义

`DagNode` 是 DAG 中最小的可调度单元,对应 `MultiAgentCoordinator::spawn` 的一次派发。设计原则:

- **声明与运行时分离**:`status` / `attempts` / `result` 标记 `#[serde(skip)]`,只持久化声明部分。
- **复用现有类型**:`mode: CoordinationMode` 直接引用 `multi_agent::CoordinationMode`,不重复定义。
- **可扩展**:新字段(如 `priority` / `resource_quota`)通过 `#[serde(default)]` 平滑加入。

```rust
// rust/crates/runtime/src/dag/node.rs

use serde::{Deserialize, Serialize};
use crate::multi_agent::CoordinationMode;

/// DAG 节点的运行时状态机。
///
/// 状态转移图:
///   Pending ──(依赖满足)──▶ Ready ──(spawn)──▶ Running
///                                                │
///                                  ┌─────────────┼─────────────┐
///                                  ▼             ▼             ▼
///                              Succeeded      Failed       Cancelled
///                                  │             │
///                                  │      (retry < max)
///                                  │             ▼
///                                  │           Ready
///                                  │
///                                  │     (retry >= max, 上游失败)
///                                  ▼             ▼
///                              [terminal]    Skipped
///
/// 终态:Succeeded / Failed / Skipped / Cancelled。
/// `is_terminal()` 用于 `DagScheduler::all_terminal()` 判断 DAG 是否跑完。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeStatus {
    /// 初始状态:尚未检查依赖。
    Pending,
    /// 依赖已满足,可被调度器拾取。
    Ready,
    /// 已通过 coordinator.spawn 派发,子 agent 正在运行。
    Running,
    /// 子 agent 成功完成,且 verify_command(如有)通过。
    Succeeded,
    /// 子 agent 失败或 verify_command 失败,且重试次数耗尽。
    Failed,
    /// 上游节点 Failed/Skipped,本节点被级联跳过。
    Skipped,
    /// 收到 CancellationToken 取消信号。
    Cancelled,
}

impl NodeStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled)
    }

    #[must_use]
    pub fn is_success_like(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// 单个 DAG 节点的完整定义(声明 + 运行时状态)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagNode {
    /// 节点全局唯一 ID(用户在 YAML 中指定或 PlanStep.id 透传)。
    pub id: String,
    /// 对应 MultiAgentCoordinator.spawn 的 `name` 参数。
    /// 用于在 LaneEvent 中标识"哪个 agent 跑了这个节点"。
    pub agent: String,
    /// 执行后端模式 — 透传给 coordinator.spawn 的 `mode`。
    pub mode: CoordinationMode,
    /// 任务描述(注入子 agent 的 user message)。
    pub task: String,
    /// 上游节点 ID 列表。空表示根节点(无依赖)。
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 条件边表达式(如 "result.status == 'succeeded'")。
    /// 求值上下文:上游所有 NodeResult 的 JSON 字段。
    /// None 表示无条件边(只要上游全部 Succeeded 即可触发)。
    #[serde(default)]
    pub condition: Option<String>,

    /// 验证命令(如 "cargo test --no-fail-fast")。
    /// 节点执行后运行,exit_code != 0 视为失败。
    #[serde(default)]
    pub verify_command: Option<String>,

    /// 重试策略:最大次数 + 退避 + fallback agent。
    #[serde(default)]
    pub retry: RetryPolicy,

    /// 节点级超时(秒)。超时触发 CancellationToken 取消。
    #[serde(default = "default_node_timeout")]
    pub timeout_secs: u64,

    /// Worktree 模式专用:独立 git worktree 路径。
    /// Fork/Teammate 模式下应为 None。
    #[serde(default)]
    pub workdir: Option<String>,

    /// 记忆访问权限(MIRIX 启发):控制子 agent 读写哪些记忆层。
    #[serde(default)]
    pub memory_access: MemoryAccess,

    // ===== 运行时状态(不序列化) =====
    #[serde(skip)]
    pub status: NodeStatus,
    #[serde(skip)]
    pub attempts: u32,
    #[serde(skip)]
    pub result: Option<NodeResult>,
    #[serde(skip)]
    pub started_at_ms: Option<u64>,
    #[serde(skip)]
    pub completed_at_ms: Option<u64>,
}

fn default_node_timeout() -> u64 { 300 }
```

### 3.2 RetryPolicy 与退避策略

```rust
/// 节点级重试策略。
///
/// 失败时的处理顺序:
/// 1. attempts < max_attempts → 退避后重试(同一 agent)
/// 2. fallback_agent 存在 → 切换 agent 重试(重置 attempts)
/// 3. 调用 RecoveryOrchestrator.attempt(WorkerFailureKind::...)
/// 4. 整 DAG replan(replan_count < DEFAULT_MAX_REPLANS)
/// 5. 关键路径检查 → 关键路径失败则 DAG 终止
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_backoff")]
    pub backoff: BackoffStrategy,
    /// 备用 agent 名称(如 "coder-senior")。
    /// 切换后 attempts 重置为 0,允许重新走一轮 max_attempts。
    #[serde(default)]
    pub fallback_agent: Option<String>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff: default_backoff(),
            fallback_agent: None,
        }
    }
}

fn default_max_attempts() -> u32 { 2 }

/// 退避策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackoffStrategy {
    /// 固定间隔重试。
    Fixed { base_secs: u64 },
    /// 指数退避:delay = base_secs * 2^(attempt-1),上限 max_secs。
    Exponential { base_secs: u64, max_secs: u64 },
}

impl Default for BackoffStrategy {
    fn default() -> Self {
        Self::Exponential { base_secs: 5, max_secs: 60 }
    }
}

fn default_backoff() -> BackoffStrategy {
    BackoffStrategy::Exponential { base_secs: 5, max_secs: 60 }
}

impl BackoffStrategy {
    /// 计算第 `attempt` 次重试前的等待时长(attempt 从 1 开始)。
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> std::time::Duration {
        match self {
            Self::Fixed { base_secs } => std::time::Duration::from_secs(*base_secs),
            Self::Exponential { base_secs, max_secs } => {
                let exp = attempt.saturating_sub(1).min(10); // 防溢出,封顶 2^10
                let secs = (*base_secs).saturating_mul(2u64.pow(exp)).min(*max_secs);
                std::time::Duration::from_secs(secs)
            }
        }
    }
}
```

### 3.3 NodeResult 与 MemoryAccess

```rust
/// 节点执行结果(运行时填充,持久化到 CheckpointStore)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub node_id: String,
    pub status: NodeStatus,
    /// 人类可读的执行摘要(注入 prompt 给主 agent)。
    pub summary: String,
    /// 文件引用(Anthropic filesystem pattern)。
    /// 子 agent 结果写到 `.claw/subagents/{id}.md`,refs 存相对路径。
    pub refs: Vec<String>,
    /// 子 agent LLM 请求消耗的 tokens(主 agent 用于 budget 控制)。
    pub tokens_used: u64,
    /// 失败时的错误信息(None 表示成功)。
    #[serde(default)]
    pub error: Option<String>,
    /// 执行耗时(毫秒)。
    pub elapsed_ms: u64,
}

/// 记忆访问权限(MIRIX 启发的四层记忆模型)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryAccess {
    /// 子 agent 可读的记忆层。
    #[serde(default)]
    pub read: Vec<MemoryType>,
    /// 子 agent 可写的记忆层。
    #[serde(default)]
    pub write: Vec<MemoryType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// 过程记忆(NOTEBOOK.md,见 docs/navigation-file-context.md)。
    Notebook,
    /// 语义记忆(SemanticRecaller,基于 fastembed 嵌入)。
    Semantic,
    /// 情节记忆(conversation_search,跨会话历史)。
    Episodic,
    /// 资源记忆(ToolResultArchive,工具调用结果归档)。
    Archive,
}
```

### 3.4 与 PlanStep 的映射关系

`PlanStep` → `DagNode` 的字段映射:

| PlanStep 字段 | DagNode 字段 | 转换规则 |
|---|---|---|
| `id` | `id` | 直接透传 |
| `description` | `task` | 直接透传 |
| `acceptance_criteria` | (丢弃) | DagNode 用 `verify_command` 替代 |
| `verify_command` | `verify_command` | 直接透传 |
| `status` (Pending/Executing/Succeeded/Failed/Skipped) | `status` (Pending/Running/Succeeded/Failed/Skipped) | `Executing` → `Running`,语义对齐 |
| `attempts` | `attempts` | 直接透传 |
| (无) | `agent` | 默认 `"default"`,YAML 可指定 |
| (无) | `mode` | 默认 `Fork`,YAML 可指定 |
| (无) | `depends_on` | 默认线性链(前一个 step);若 `description` 含 "并行"/"parallel",继承前一个的 `depends_on` |
| (无) | `retry` | 默认 `RetryPolicy::default()`,YAML 可指定 |

映射代码见第六章 `from_plan_step`。

---

## 四、DagGraph 数据结构

### 4.1 petgraph DiGraph 封装

`DagGraph` 封装 `petgraph::graph::DiGraph<DagNode, ()>`,对外暴露 ID 索引(`node_map: HashMap<String, NodeIndex>`),隐藏 `NodeIndex` 的内部表示。

```rust
// rust/crates/runtime/src/dag/graph.rs

use std::collections::HashMap;
use petgraph::algo::{kosaraju_scc, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use serde::{Deserialize, Serialize};

use super::node::{DagNode, NodeStatus};

/// DAG 失败策略 — 控制节点失败时 DAG 整体如何反应。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DagFailurePolicy {
    /// 默认:重试 → fallback → RecoveryOrchestrator → replan → 关键路径检查。
    #[default]
    RetryThenEscalate,
    /// 仅重试,不升级到 RecoveryOrchestrator。
    Retry,
    /// 失败立即切换 fallback agent(跳过重试)。
    Fallback,
    /// 任何节点失败立即终止整个 DAG。
    Abort,
    /// 失败升级到 RecoveryOrchestrator(跳过重试)。
    Escalate,
}

/// 检查点策略 — 控制何时持久化节点状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CheckpointPolicy {
    /// 默认:每个节点完成后写检查点(最安全,IO 开销最大)。
    #[default]
    EveryNode,
    /// 仅失败时写检查点(节省 IO,成功节点靠内存状态)。
    OnFailure,
    /// 不持久化(纯内存运行,失败即丢失)。
    None,
}

/// DAG 全局配置(从 YAML `dag:` 段解析)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagConfig {
    pub max_parallelism: usize,
    pub token_budget: u64,
    pub timeout_secs: u64,
    #[serde(default)]
    pub on_failure: DagFailurePolicy,
    #[serde(default)]
    pub checkpoint_policy: CheckpointPolicy,
}

impl Default for DagConfig {
    fn default() -> Self {
        Self {
            max_parallelism: 4,
            token_budget: 200_000,
            timeout_secs: 1800,
            on_failure: DagFailurePolicy::RetryThenEscalate,
            checkpoint_policy: CheckpointPolicy::EveryNode,
        }
    }
}

/// petgraph 封装的 DAG 主结构。
#[derive(Debug, Clone)]
pub struct DagGraph {
    /// 内部图表示。边 `()` 表示"无条件的依赖关系";
    /// 条件表达式存在 `DagNode.condition` 字段,求值时再判断。
    graph: DiGraph<DagNode, ()>,
    /// 节点 ID → petgraph NodeIndex 的索引(避免每次遍历查找)。
    node_map: HashMap<String, NodeIndex>,

    /// DAG 全局元信息。
    pub id: String,
    pub task_summary: String,
    pub config: DagConfig,
    /// 整 DAG 的 replan 次数(用于防止 doom loop)。
    pub replan_count: u32,
}

impl DagGraph {
    /// 构造 DAG 并验证无环。
    ///
    /// # Errors
    /// - `MissingDependency(id)`:`depends_on` 引用了不存在的节点。
    /// - `CycleDetected(nodes)`:检测到环(用 Kosaraju SCC)。
    pub fn new(
        id: String,
        task_summary: String,
        nodes: Vec<DagNode>,
        config: DagConfig,
    ) -> Result<Self, DagError> {
        let mut graph = DiGraph::new();
        let mut node_map = HashMap::new();

        // 第一遍:添加所有节点(初始化 status = Pending)。
        for mut node in nodes {
            node.status = NodeStatus::Pending;
            node.attempts = 0;
            node.result = None;
            let idx = graph.add_node(node.clone());
            node_map.insert(node.id.clone(), idx);
        }

        // 第二遍:添加依赖边。dep → node(箭头方向 = 数据流向)。
        for idx in graph.node_indices() {
            // clone depends_on 避免借用冲突
            let deps: Vec<String> = graph
                .node_weight(idx)
                .map(|n| n.depends_on.clone())
                .unwrap_or_default();
            for dep_id in deps {
                let dep_idx = *node_map
                    .get(&dep_id)
                    .ok_or(DagError::MissingDependency(dep_id.clone()))?;
                graph.add_edge(dep_idx, idx, ());
            }
        }

        let dag = Self { graph, node_map, id, task_summary, config, replan_count: 0 };
        dag.validate_acyclic()?;
        Ok(dag)
    }

    /// 环检测:用 Kosaraju SCC 算法。
    /// DAG 等价于"所有 SCC 大小均为 1"。若任一 SCC 大小 > 1,即存在环。
    fn validate_acyclic(&self) -> Result<(), DagError> {
        let sccs = kosaraju_scc(&self.graph);
        for scc in sccs {
            if scc.len() > 1 {
                let cycle_nodes: Vec<String> = scc
                    .iter()
                    .filter_map(|idx| self.graph.node_weight(*idx).map(|n| n.id.clone()))
                    .collect();
                return Err(DagError::CycleDetected(cycle_nodes));
            }
            // 自环(单节点 SCC 但有 self-loop)也需检测
            if scc.len() == 1 {
                let idx = scc[0];
                if self.graph.contains_edge(idx, idx) {
                    let id = self.graph.node_weight(idx).map(|n| n.id.clone()).unwrap_or_default();
                    return Err(DagError::CycleDetected(vec![id]));
                }
            }
        }
        Ok(())
    }

    /// 获取所有就绪节点(状态 = Pending 且所有上游 Succeeded)。
    /// 用 Kahn 入度法的变体:不走全局拓扑,而是单点检查(适合增量调度)。
    pub fn ready_nodes(&self) -> Vec<&DagNode> {
        self.graph
            .node_indices()
            .filter_map(|idx| {
                let node = self.graph.node_weight(idx)?;
                if node.status != NodeStatus::Pending {
                    return None;
                }
                if self.all_deps_succeeded(idx) {
                    Some(node)
                } else {
                    None
                }
            })
            .collect()
    }

    /// 检查节点所有上游是否已 Succeeded。
    /// 跳过条件边的求值(条件边在 `evaluate_downstream` 中处理)。
    fn all_deps_succeeded(&self, idx: NodeIndex) -> bool {
        self.graph
            .neighbors_directed(idx, Direction::Incoming)
            .all(|dep_idx| {
                self.graph
                    .node_weight(dep_idx)
                    .map(|n| n.status == NodeStatus::Succeeded)
                    .unwrap_or(false)
            })
    }

    /// 标记节点状态(运行时调度器调用)。
    pub fn mark_status(&mut self, node_id: &str, status: NodeStatus) {
        if let Some(idx) = self.node_map.get(node_id) {
            if let Some(node) = self.graph.node_weight_mut(*idx) {
                node.status = status;
            }
        }
    }

    /// 增加节点尝试次数(每次 spawn 子 agent 前调用)。
    pub fn increment_attempts(&mut self, node_id: &str) {
        if let Some(idx) = self.node_map.get(node_id) {
            if let Some(node) = self.graph.node_weight_mut(*idx) {
                node.attempts += 1;
            }
        }
    }

    /// 写回节点结果(成功/失败时调用)。
    pub fn set_result(&mut self, node_id: &str, result: super::node::NodeResult) {
        if let Some(idx) = self.node_map.get(node_id) {
            if let Some(node) = self.graph.node_weight_mut(*idx) {
                node.result = Some(result);
            }
        }
    }

    /// 是否所有节点都已到达终态。
    pub fn all_terminal(&self) -> bool {
        self.graph
            .node_weights()
            .all(|n| n.status.is_terminal())
    }

    /// 获取节点引用(用于调度器读取声明信息)。
    pub fn get_node(&self, node_id: &str) -> Result<&DagNode, DagError> {
        let idx = self
            .node_map
            .get(node_id)
            .ok_or_else(|| DagError::NodeNotFound(node_id.to_string()))?;
        self.graph
            .node_weight(*idx)
            .ok_or_else(|| DagError::NodeNotFound(node_id.to_string()))
    }

    /// 拓扑排序(用于线性化展示和 resume 时按序重放)。
    pub fn topological_order(&self) -> Vec<&DagNode> {
        match toposort(&self.graph, None) {
            Ok(indices) => indices
                .iter()
                .filter_map(|idx| self.graph.node_weight(*idx))
                .collect(),
            Err(_) => Vec::new(), // 不应发生(构造时已验证无环)
        }
    }

    /// 渲染为 Mermaid(注入 prompt 给主 agent 看执行计划)。
    pub fn render_mermaid(&self) -> String {
        let mut out = String::from("graph LR\n");
        for idx in self.graph.node_indices() {
            if let Some(node) = self.graph.node_weight(idx) {
                let label: String = node.task.chars().take(30).collect();
                let shape = match node.status {
                    NodeStatus::Succeeded => "((",
                    NodeStatus::Failed => "[[",
                    NodeStatus::Running => "{{",
                    _ => "[",
                };
                let close = match node.status {
                    NodeStatus::Succeeded => "))",
                    NodeStatus::Failed => "]]",
                    NodeStatus::Running => "}}",
                    _ => "]",
                };
                out.push_str(&format!("    {}{}\"{}\"{}\n", node.id, shape, label, close));
            }
        }
        for edge in self.graph.raw_edges() {
            let src = &self.graph[edge.source()].id;
            let dst = &self.graph[edge.target()].id;
            out.push_str(&format!("    {} --> {}\n", src, dst));
        }
        out
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("missing dependency: {0}")]
    MissingDependency(String),
    #[error("cycle detected involving nodes: {0:?}")]
    CycleDetected(Vec<String>),
    #[error("node not found: {0}")]
    NodeNotFound(String),
    #[error("yaml parse error: {0}")]
    YamlParse(String),
    #[error("condition evaluation failed: {0}")]
    ConditionEval(String),
    #[error("node panic: {0}")]
    NodePanic(String),
    #[error("deadlock: no ready nodes but non-terminal nodes remain")]
    Deadlock,
    #[error("timeout: DAG exceeded {} secs", .0)]
    Timeout(u64),
    #[error("checkpoint io error: {0}")]
    CheckpointIo(String),
}
```

### 4.2 Kosaraju SCC 算法说明

`petgraph::algo::kosaraju_scc` 返回 `Vec<Vec<NodeIndex>>`,每个内层 Vec 是一个强连通分量。DAG 的定义是"没有环",等价于"所有 SCC 大小均为 1 且无自环"。我们因此:

- 若任一 SCC 大小 > 1 → 存在多节点环,返回 `CycleDetected(节点 ID 列表)`。
- 若 SCC 大小 = 1 但存在 `self_loop` → 单节点自环,也返回 `CycleDetected`。

复杂度:O(V+E),V 是节点数,E 是边数。对于 100 节点 500 边的 DAG,单次环检测 < 1ms。

### 4.3 ready_nodes 的入度法变体

经典 Kahn 算法维护全局入度表,逐个消费入度为 0 的节点。我们的 `ready_nodes` 是**增量**版本:每次调用时,只对状态为 `Pending` 的节点检查"所有上游 Succeeded",而非维护全局入度。

理由:

- DAG 调度是事件驱动的(节点完成 → 触发新一轮 `ready_nodes`),不需要一次性算出所有节点的拓扑序。
- 节点状态会变化(Running → Succeeded),全局入度表需要同步更新,容易出错。
- 增量检查的复杂度 O(剩余 Pending 节点数 × 平均入度),在调度循环中可接受。

---

## 五、DagScheduler 调度引擎

### 5.1 调度算法总览

`DagScheduler` 是 DAG 的执行核心,负责:

1. **分层并行**:每一轮收集所有 `ready_nodes`,用 `JoinSet` 并行 spawn 子 agent。
2. **结果收集**:`join_next().await` 逐个收结果,每收一个就更新节点状态 + 写检查点。
3. **失败传播**:根据 `DagFailurePolicy` 决定重试 / fallback / abort。
4. **取消层级**:DAG 级 `CancellationToken` → 节点级 `child_token()`,父取消传播到所有子。
5. **超时守护**:整个 DAG 有 `timeout_secs` 超时;每个节点也有自己的 `timeout_secs`。

### 5.2 完整代码骨架

```rust
// rust/crates/runtime/src/dag/scheduler.rs

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::multi_agent::MultiAgentCoordinator;
use crate::recovery_orchestrator::RecoveryOrchestrator;
use crate::worker_boot::WorkerFailureKind;

use super::graph::{CheckpointPolicy, DagError, DagGraph, DagFailurePolicy};
use super::node::{NodeResult, NodeStatus};

/// DAG 执行的整体结果。
#[derive(Debug, Clone)]
pub struct DagRunResult {
    pub dag_id: String,
    pub total_tokens: u64,
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub elapsed_ms: u64,
    pub mermaid: String,
}

/// DAG 调度器 — 持有可变 DagGraph + 共享的 coordinator/recovery。
pub struct DagScheduler {
    pub(crate) dag: DagGraph,
    coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    recovery: Arc<Mutex<RecoveryOrchestrator>>,
    /// DAG 级取消 token。cancel() 后所有节点级 child_token 同步取消。
    cancel_token: CancellationToken,
    checkpoint_store: super::checkpoint::CheckpointStore,
}

impl DagScheduler {
    pub fn new(
        dag: DagGraph,
        coordinator: Arc<Mutex<MultiAgentCoordinator>>,
        recovery: Arc<Mutex<RecoveryOrchestrator>>,
        cancel_token: CancellationToken,
        checkpoint_store: super::checkpoint::CheckpointStore,
    ) -> Self {
        Self { dag, coordinator, recovery, cancel_token, checkpoint_store }
    }

    /// 启动调度循环。阻塞直到所有节点到达终态或 DAG 失败/超时。
    pub async fn run(&mut self) -> Result<DagRunResult, DagError> {
        let mut total_tokens = 0u64;
        let start = Instant::now();
        let dag_deadline = start + Duration::from_secs(self.dag.config.timeout_secs);

        loop {
            // 0. DAG 级取消检查
            if self.cancel_token.is_cancelled() {
                self.cascade_cancel_all();
                return Err(DagError::Timeout(self.dag.config.timeout_secs));
            }

            // 1. 收集就绪节点(Pending + 上游全 Succeeded)
            let ready: Vec<String> = self
                .dag
                .ready_nodes()
                .into_iter()
                .map(|n| n.id.clone())
                .collect();

            if ready.is_empty() {
                if self.dag.all_terminal() {
                    return Ok(self.build_result(total_tokens, start));
                }
                // 死锁检测:无就绪节点但仍有非终态节点
                // 可能原因:条件边求值失败 / 上游 Skipped 未级联
                return Err(DagError::Deadlock);
            }

            // 2. 同层并行执行(JoinSet + child_token)
            let mut joinset: JoinSet<Result<NodeResult, DagError>> = JoinSet::new();
            let parallelism = self.dag.config.max_parallelism.min(ready.len());

            for node_id in ready.iter().take(parallelism) {
                let node_cancel = self.cancel_token.child_token();
                let coordinator = self.coordinator.clone();
                let node = self.dag.get_node(node_id)?.clone();

                // 标记 Running + 增加尝试次数
                self.dag.mark_status(node_id, NodeStatus::Running);
                self.dag.increment_attempts(node_id);

                // 发布 DagNodeStarted lane event
                self.emit_dag_node_started(node_id);

                let cancel = node_cancel;
                joinset.spawn(async move {
                    run_node(&node, coordinator, cancel).await
                });
            }

            // 3. 逐个收集结果(不等全部完成再处理,先到先处理)
            while let Some(res) = joinset.join_next().await {
                let node_result = res
                    .map_err(|e| DagError::NodePanic(e.to_string()))??;
                total_tokens += node_result.tokens_used;

                // 4. 检查点持久化
                if self.dag.config.checkpoint_policy != CheckpointPolicy::None {
                    if let Err(e) = self.checkpoint_store.save_node(&self.dag.id, &node_result).await {
                        // 检查点失败不阻断调度(降级到 OnFailure 策略)
                        tracing::warn!("checkpoint save failed for node {}: {e}", node_result.node_id);
                    }
                }

                // 5. 状态更新 + 事件发布
                if node_result.status == NodeStatus::Succeeded {
                    self.dag.mark_status(&node_result.node_id, NodeStatus::Succeeded);
                    self.dag.set_result(&node_result.node_id, node_result.clone());
                    self.emit_dag_node_completed(&node_result);
                } else if node_result.status == NodeStatus::Cancelled {
                    self.dag.mark_status(&node_result.node_id, NodeStatus::Cancelled);
                    self.emit_dag_node_failed(&node_result);
                } else {
                    // Failed — 走失败处理流程
                    let should_continue = self.handle_node_failure(&node_result).await?;
                    if !should_continue {
                        // 关键路径失败 → 级联跳过下游 + 终止 DAG
                        self.cascade_skip_downstream(&node_result.node_id);
                        self.emit_dag_node_failed(&node_result);
                        return Err(DagError::Deadlock);
                    }
                    self.emit_dag_node_failed(&node_result);
                }
            }

            // 6. DAG 级超时检查
            if Instant::now() >= dag_deadline {
                self.cancel_token.cancel();
                self.cascade_cancel_all();
                return Err(DagError::Timeout(self.dag.config.timeout_secs));
            }
        }
    }

    /// 节点失败处理:retry → fallback → RecoveryOrchestrator → replan → 关键路径。
    async fn handle_node_failure(&mut self, result: &NodeResult) -> Result<bool, DagError> {
        let node = self.dag.get_node(&result.node_id)?.clone();
        let policy = self.dag.config.on_failure;

        // 1. 重试(attempts < max_attempts)
        if policy == DagFailurePolicy::RetryThenEscalate
            || policy == DagFailurePolicy::Retry
        {
            if node.attempts < node.retry.max_attempts {
                self.dag.mark_status(&result.node_id, NodeStatus::Ready);
                let delay = node.retry.backoff.delay_for(node.attempts);
                tokio::time::sleep(delay).await;
                return Ok(true);
            }
        }

        // 2. fallback agent(切换 agent,重置 attempts)
        if policy == DagFailurePolicy::RetryThenEscalate
            || policy == DagFailurePolicy::Fallback
        {
            if let Some(fallback) = &node.retry.fallback_agent {
                self.dag.update_node_agent(&result.node_id, fallback);
                self.dag.reset_attempts(&result.node_id);
                self.dag.mark_status(&result.node_id, NodeStatus::Ready);
                return Ok(true);
            }
        }

        // 3. RecoveryOrchestrator
        if policy == DagFailurePolicy::RetryThenEscalate
            || policy == DagFailurePolicy::Escalate
        {
            let failure_kind = WorkerFailureKind::Provider; // 简化:实际应从 result 推断
            let outcome = self.recovery.lock().await.attempt(failure_kind);
            if outcome.recovered() {
                self.dag.mark_status(&result.node_id, NodeStatus::Ready);
                return Ok(true);
            }
        }

        // 4. 整 DAG replan(replan_count < DEFAULT_MAX_REPLANS)
        const DEFAULT_MAX_REPLANS: u32 = 3;
        if self.dag.replan_count < DEFAULT_MAX_REPLANS {
            self.dag.replan_count += 1;
            // 简化:仅重置失败节点为 Pending(完整 replan 应调用 planner 重新生成)
            self.dag.mark_status(&result.node_id, NodeStatus::Pending);
            return Ok(true);
        }

        // 5. 关键路径检查(失败节点是否在任何 Succeeded 节点的下游路径上)
        if self.is_critical_path(&result.node_id) {
            return Ok(false);
        }
        Ok(true)
    }

    /// 级联跳过下游节点(上游 Failed → 下游 Skipped)。
    fn cascade_skip_downstream(&mut self, failed_node_id: &str) {
        let downstream: Vec<String> = self.collect_downstream(failed_node_id);
        for id in downstream {
            self.dag.mark_status(&id, NodeStatus::Skipped);
        }
    }

    /// 收集节点的所有下游节点(传递闭包)。
    fn collect_downstream(&self, node_id: &str) -> Vec<String> {
        let start_idx = match self.dag.node_map.get(node_id) {
            Some(idx) => *idx,
            None => return Vec::new(),
        };
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![start_idx];
        let mut result = Vec::new();
        while let Some(idx) = stack.pop() {
            if !visited.insert(idx) {
                continue;
            }
            for neighbor in self.dag.graph.neighbors_directed(idx, petgraph::Direction::Outgoing) {
                if let Some(n) = self.dag.graph.node_weight(neighbor) {
                    if n.status == NodeStatus::Pending {
                        result.push(n.id.clone());
                    }
                }
                stack.push(neighbor);
            }
        }
        result
    }

    /// 取消所有仍在 Running 的节点。
    fn cascade_cancel_all(&mut self) {
        for idx in self.dag.graph.node_indices() {
            if let Some(node) = self.dag.graph.node_weight_mut(idx) {
                if node.status == NodeStatus::Running {
                    node.status = NodeStatus::Cancelled;
                }
            }
        }
    }

    /// 判断节点是否在关键路径上(简化:任何已 Succeeded 节点的下游 = 关键)。
    fn is_critical_path(&self, node_id: &str) -> bool {
        // 简化实现:若 DAG 中已有 Succeeded 节点,则失败节点在关键路径
        // 完整实现应计算最长路径,这里仅作启发式
        self.dag.graph.node_weights().any(|n| n.status == NodeStatus::Succeeded)
    }

    fn build_result(&self, total_tokens: u64, start: Instant) -> DagRunResult {
        let (succeeded, failed, skipped) = self.dag.graph.node_weights().fold(
            (0usize, 0usize, 0usize),
            |(s, f, sk), n| match n.status {
                NodeStatus::Succeeded => (s + 1, f, sk),
                NodeStatus::Failed => (s, f + 1, sk),
                NodeStatus::Skipped => (s, f, sk + 1),
                _ => (s, f, sk),
            },
        );
        DagRunResult {
            dag_id: self.dag.id.clone(),
            total_tokens,
            succeeded,
            failed,
            skipped,
            elapsed_ms: start.elapsed().as_millis() as u64,
            mermaid: self.dag.render_mermaid(),
        }
    }
}

/// 执行单个 DAG 节点(在 JoinSet task 中运行)。
async fn run_node(
    node: &super::node::DagNode,
    coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    cancel: CancellationToken,
) -> Result<NodeResult, DagError> {
    let start = std::time::Instant::now();

    // 1. 通过 MultiAgentCoordinator spawn 子 agent
    let subagent_id = {
        let mut coord = coordinator.lock().await;
        coord.spawn(&node.agent, &node.task, node.mode)
    };

    // 2. 执行子 agent turn(复用现有 run_subagent_turn 逻辑,
    //    但需要从 ConversationRuntime 借用 api_client — 实际实现需调整签名)
    let task_result = tokio::select! {
        r = run_subagent_turn_isolated(node, &subagent_id) => r,
        _ = cancel.cancelled() => {
            return Ok(NodeResult {
                node_id: node.id.clone(),
                status: NodeStatus::Cancelled,
                summary: "cancelled by DAG".into(),
                refs: vec![],
                tokens_used: 0,
                error: Some("cancelled".into()),
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
        _ = tokio::time::sleep(Duration::from_secs(node.timeout_secs)) => {
            return Ok(NodeResult {
                node_id: node.id.clone(),
                status: NodeStatus::Failed,
                summary: format!("timeout after {}s", node.timeout_secs),
                refs: vec![],
                tokens_used: 0,
                error: Some("node timeout".into()),
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
    };

    let mut result = task_result?;
    result.node_id = node.id.clone();
    result.elapsed_ms = start.elapsed().as_millis() as u64;

    // 3. 验证(若有 verify_command)
    if result.status == NodeStatus::Succeeded {
        if let Some(verify_cmd) = &node.verify_command {
            let exit_code = run_verify_command(verify_cmd).await;
            if exit_code != 0 {
                result.status = NodeStatus::Failed;
                result.error = Some(format!("verify failed: exit {exit_code}"));
            }
        }
    }

    Ok(result)
}

/// 隔离执行子 agent turn(复用 conversation.rs 的 run_subagent_turn 逻辑)。
/// 实际实现需将 run_subagent_turn 抽取为可独立调用的函数,或通过 trait 注入。
async fn run_subagent_turn_isolated(
    _node: &super::node::DagNode,
    _subagent_id: &str,
) -> Result<NodeResult, DagError> {
    // 占位实现 — 实际集成时调用 ConversationRuntime::run_subagent_turn
    // 并把结果转换为 NodeResult(见第七章 dag_run 工具实现)
    Ok(NodeResult {
        node_id: String::new(),
        status: NodeStatus::Succeeded,
        summary: "placeholder".into(),
        refs: vec![],
        tokens_used: 0,
        error: None,
        elapsed_ms: 0,
    })
}

/// 执行验证命令(如 cargo test)。
async fn run_verify_command(cmd: &str) -> i32 {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return -1;
    }
    let output = tokio::process::Command::new(parts[0])
        .args(&parts[1..])
        .output()
        .await;
    match output {
        Ok(o) => o.status.code().unwrap_or(-1),
        Err(_) => -1,
    }
}
```

### 5.3 CancellationToken 层级设计

```
DAG 级 cancel_token (DagScheduler 持有)
├── 节点 A child_token
├── 节点 B child_token
└── 节点 C child_token
    └── (子 agent LLM 请求 tokio::select! 监听)
```

调用 `dag_cancel_token.cancel()` 时:

1. 所有 `child_token` 同步触发 `cancelled()` future 就绪。
2. 各 `run_node` 的 `tokio::select!` 分支命中,返回 `NodeStatus::Cancelled`。
3. `JoinSet::join_next()` 收到 Cancelled 结果,标记节点状态。
4. 调度循环退出,返回 `DagError::Timeout` 或 `DagError::Deadlock`。

### 5.4 失败传播策略矩阵

| `DagFailurePolicy` | 重试? | Fallback? | Recovery? | Replan? | 关键路径终止? |
|---|---|---|---|---|---|
| `RetryThenEscalate`(默认) | 是 | 是 | 是 | 是 | 是 |
| `Retry` | 是 | 否 | 否 | 否 | 否(非关键路径继续) |
| `Fallback` | 否 | 是 | 否 | 否 | 否 |
| `Abort` | 否 | 否 | 否 | 否 | 是(任何失败即终止) |
| `Escalate` | 否 | 否 | 是 | 是 | 是 |

---

## 六、Plan → DAG 转换器

### 6.1 转换算法

`PlanArtifact` 是用户意图层(线性 steps),`DagGraph` 是执行计划层(可并行 + 条件)。转换规则:

1. **默认线性链**:`steps[i]` 的 `depends_on = [steps[i-1].id]`,形成单链。
2. **并行检测**:若 `step.description` 含关键词 `"并行"` / `"parallel"`(大小写不敏感),则该 step 继承前一个 step 的 `depends_on`(同层并行)。
3. **字段透传**:`id` / `task` / `verify_command` 直接透传;`agent` 默认 `"default"`,`mode` 默认 `Fork`。
4. **空 steps 兜底**:返回空 DAG(仅含 task_summary),调度器立即返回 `all_terminal = true`。

### 6.2 代码骨架

```rust
// rust/crates/runtime/src/dag/yaml_loader.rs

use crate::planner::{PlanArtifact, PlanStep};
use super::graph::{DagConfig, DagError, DagGraph};
use super::node::{DagNode, MemoryAccess, RetryPolicy};
use crate::multi_agent::CoordinationMode;

/// 并行关键词 — 出现在 step.description 中触发同层并行。
const PARALLEL_KEYWORDS: &[&str] = &["并行", "parallel"];

impl DagGraph {
    /// 从 PlanArtifact 转换为 DagGraph。
    ///
    /// 转换规则见 §6.1。空 steps 返回空 DAG(不报错)。
    pub fn from_plan_artifact(artifact: &PlanArtifact) -> Result<Self, DagError> {
        let mut nodes = Vec::with_capacity(artifact.steps.len());
        let mut prev_id: Option<String> = None;
        let mut prev_deps: Vec<String> = Vec::new();

        for step in &artifact.steps {
            let mut node = DagNode::from_plan_step(step);
            let is_parallel = PARALLEL_KEYWORDS.iter().any(|kw| {
                step.description.to_ascii_lowercase().contains(kw)
            });

            if !is_parallel {
                // 线性:依赖前一个节点
                if let Some(prev) = &prev_id {
                    node.depends_on = vec![prev.clone()];
                }
                prev_deps = node.depends_on.clone();
            } else {
                // 并行:继承前一个节点的依赖(同层)
                node.depends_on = prev_deps.clone();
                // 注意:不更新 prev_id,后续线性节点依赖本"层"最后一个节点
            }

            prev_id = Some(node.id.clone());
            nodes.push(node);
        }

        Self::new(
            artifact.id.clone(),
            artifact.task_summary.clone(),
            nodes,
            DagConfig::default(),
        )
    }
}

impl DagNode {
    /// 从 PlanStep 转换为 DagNode(默认 agent="default", mode=Fork)。
    pub fn from_plan_step(step: &PlanStep) -> Self {
        Self {
            id: step.id.clone(),
            agent: "default".to_string(),
            mode: CoordinationMode::Fork,
            task: step.description.clone(),
            depends_on: Vec::new(),
            condition: None,
            verify_command: step.verify_command.clone(),
            retry: RetryPolicy::default(),
            timeout_secs: 300,
            workdir: None,
            memory_access: MemoryAccess::default(),
            status: super::node::NodeStatus::Pending,
            attempts: 0,
            result: None,
            started_at_ms: None,
            completed_at_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{PlanArtifact, PlanStep};

    fn make_step(id: &str, desc: &str) -> PlanStep {
        PlanStep::new(id, desc, "criteria")
    }

    #[test]
    fn from_plan_artifact_linear_chain() {
        let artifact = PlanArtifact::new(
            "linear task",
            vec![
                make_step("s1", "read file"),
                make_step("s2", "edit file"),
                make_step("s3", "run tests"),
            ],
        );
        let dag = DagGraph::from_plan_artifact(&artifact).expect("convert should succeed");
        // s2 依赖 s1,s3 依赖 s2
        let s2 = dag.get_node("s2").unwrap();
        assert_eq!(s2.depends_on, vec!["s1"]);
        let s3 = dag.get_node("s3").unwrap();
        assert_eq!(s3.depends_on, vec!["s2"]);
    }

    #[test]
    fn from_plan_artifact_parallel_detection() {
        let artifact = PlanArtifact::new(
            "parallel task",
            vec![
                make_step("s1", "analyze"),
                make_step("s2", "并行实现 A"),
                make_step("s3", "并行实现 B"),
                make_step("s4", "集成测试"),
            ],
        );
        let dag = DagGraph::from_plan_artifact(&artifact).expect("convert should succeed");
        // s2 和 s3 都依赖 s1(同层并行),s4 依赖 s3
        let s2 = dag.get_node("s2").unwrap();
        assert_eq!(s2.depends_on, vec!["s1"]);
        let s3 = dag.get_node("s3").unwrap();
        assert_eq!(s3.depends_on, vec!["s1"]);
        let s4 = dag.get_node("s4").unwrap();
        assert_eq!(s4.depends_on, vec!["s3"]);
    }

    #[test]
    fn from_plan_artifact_empty_steps() {
        let artifact = PlanArtifact::new("empty", Vec::new());
        let dag = DagGraph::from_plan_artifact(&artifact).expect("empty DAG should succeed");
        assert!(dag.all_terminal());
    }

    #[test]
    fn from_plan_artifact_preserves_verify_command() {
        let step = PlanStep::with_verify_command("s1", "build", "criteria", "cargo build");
        let artifact = PlanArtifact::new("task", vec![step]);
        let dag = DagGraph::from_plan_artifact(&artifact).unwrap();
        let node = dag.get_node("s1").unwrap();
        assert_eq!(node.verify_command.as_deref(), Some("cargo build"));
    }
}
```

### 6.3 复杂度分析

- **时间复杂度**:O(N) 遍历 steps + O(N + E) 构造图 + O(N + E) 环检测 = O(N + E)。
- **空间复杂度**:O(N + E),N 是 step 数,E 是依赖边数(线性链时 E = N-1)。
- **典型规模**:100 steps 线性链 → E = 99,转换 + 验证 < 5ms。

### 6.4 测试用例清单

| 测试名 | 输入 | 预期 |
|---|---|---|
| `linear_chain` | 3 个 step,无并行关键词 | s2.depends_on = [s1], s3.depends_on = [s2] |
| `parallel_detection` | 4 个 step,中间 2 个含"并行" | s2/s3 都依赖 s1,s4 依赖 s3 |
| `empty_steps` | 0 个 step | 空 DAG,`all_terminal() == true` |
| `verify_command_preserved` | step 含 verify_command | node.verify_command 透传 |
| `cycle_in_plan` | (异常)step 互相依赖 | `Err(CycleDetected)` |
| `missing_dependency` | (异常)depends_on 引用不存在的 id | `Err(MissingDependency)` |

---

## 七、YAML 声明式 DAG

### 7.1 Schema 定义

```yaml
# .claw/dags/dag-1778000000-ab12.yaml
dag:
  id: dag-1778000000-ab12            # 必填,全局唯一
  task_summary: "重构 multi_agent 模块为 DAG 编排"  # 必填
  max_parallelism: 4                  # 默认 4
  token_budget: 200000                # 默认 200000
  timeout_secs: 1800                  # 默认 1800(30 分钟)
  on_failure: retry_then_escalate     # 默认 retry_then_escalate
  checkpoint_policy: every_node       # 默认 every_node

  nodes:                              # 必填,节点列表
    - id: analyze                     # 必填,节点 ID
      agent: "code-analyst"           # 必填,对应 coordinator.spawn 的 name
      mode: Fork                      # 默认 Fork,可选 Fork/Teammate/Worktree
      task: "分析 multi_agent/mod.rs 现状"  # 必填
      # depends_on: []                # 默认空(根节点)
      # condition: null               # 默认无条件
      verify_command: "cargo test multi_agent -- --nocapture"
      memory_access:
        read: [notebook, semantic]
        write: [notebook]
      timeout_secs: 300
      retry:
        max_attempts: 2
        backoff:
          exponential:
            base_secs: 5
            max_secs: 60
        # fallback_agent: "coder-senior"  # 可选

    - id: design
      agent: "architect"
      task: "设计 DAG 数据结构"
      depends_on: [analyze]
      condition: "result.status == 'succeeded'"

    - id: impl_v1
      agent: "coder"
      mode: Worktree
      workdir: ".claw/worktrees/impl-v1"
      task: "实现 DagGraph + Scheduler"
      depends_on: [design]
      retry:
        max_attempts: 3
        fallback_agent: "coder-senior"

    - id: impl_v2
      agent: "coder"
      mode: Worktree
      workdir: ".claw/worktrees/impl-v2"
      task: "实现 Checkpointer"
      depends_on: [design]

    - id: test
      agent: "qa"
      task: "集成测试"
      depends_on: [impl_v1, impl_v2]
      verify_command: "cargo test --all"
```

### 7.2 解析器代码骨架

```rust
// rust/crates/runtime/src/dag/yaml_loader.rs(续)

use serde::{Deserialize, Serialize};
use std::path::Path;

/// YAML 文件顶层结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagYamlFile {
    pub dag: DagYamlSpec,
}

/// `dag:` 段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagYamlSpec {
    pub id: String,
    pub task_summary: String,
    #[serde(default = "default_parallelism")]
    pub max_parallelism: usize,
    #[serde(default = "default_token_budget")]
    pub token_budget: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub on_failure: super::graph::DagFailurePolicy,
    #[serde(default)]
    pub checkpoint_policy: super::graph::CheckpointPolicy,
    pub nodes: Vec<DagYamlNode>,
}

/// 单个节点的 YAML 表示(允许省略可选字段)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagYamlNode {
    pub id: String,
    pub agent: String,
    #[serde(default = "default_mode")]
    pub mode: CoordinationMode,
    pub task: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub verify_command: Option<String>,
    #[serde(default)]
    pub retry: super::node::RetryPolicy,
    #[serde(default = "super::node::default_node_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub memory_access: super::node::MemoryAccess,
}

fn default_parallelism() -> usize { 4 }
fn default_token_budget() -> u64 { 200_000 }
fn default_timeout() -> u64 { 1800 }
fn default_mode() -> CoordinationMode { CoordinationMode::Fork }

impl DagYamlSpec {
    /// 从 YAML 文本解析。
    pub fn parse(yaml: &str) -> Result<Self, DagError> {
        serde_yaml::from_str::<DagYamlFile>(yaml)
            .map(|f| f.dag)
            .map_err(|e| DagError::YamlParse(e.to_string()))
    }

    /// 从文件加载。
    pub async fn load_file(path: &Path) -> Result<Self, DagError> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| DagError::YamlParse(format!("read {}: {e}", path.display())))?;
        Self::parse(&content)
    }

    /// 转换为 DagGraph(触发环检测)。
    pub fn into_dag_graph(self) -> Result<DagGraph, DagError> {
        let nodes: Vec<super::node::DagNode> = self
            .nodes
            .into_iter()
            .map(|n| super::node::DagNode {
                id: n.id,
                agent: n.agent,
                mode: n.mode,
                task: n.task,
                depends_on: n.depends_on,
                condition: n.condition,
                verify_command: n.verify_command,
                retry: n.retry,
                timeout_secs: n.timeout_secs,
                workdir: n.workdir,
                memory_access: n.memory_access,
                status: super::node::NodeStatus::Pending,
                attempts: 0,
                result: None,
                started_at_ms: None,
                completed_at_ms: None,
            })
            .collect();
        DagGraph::new(
            self.id,
            self.task_summary,
            nodes,
            super::graph::DagConfig {
                max_parallelism: self.max_parallelism,
                token_budget: self.token_budget,
                timeout_secs: self.timeout_secs,
                on_failure: self.on_failure,
                checkpoint_policy: self.checkpoint_policy,
            },
        )
    }
}

#[cfg(test)]
mod yaml_tests {
    use super::*;

    const SAMPLE_YAML: &str = r#"
dag:
  id: test-dag-001
  task_summary: "测试 YAML 解析"
  max_parallelism: 2
  nodes:
    - id: a
      agent: "agent-a"
      task: "任务 A"
    - id: b
      agent: "agent-b"
      task: "任务 B"
      depends_on: [a]
"#;

    #[test]
    fn parse_sample_yaml() {
        let spec = DagYamlSpec::parse(SAMPLE_YAML).expect("parse should succeed");
        assert_eq!(spec.id, "test-dag-001");
        assert_eq!(spec.max_parallelism, 2);
        assert_eq!(spec.nodes.len(), 2);
        assert_eq!(spec.nodes[1].depends_on, vec!["a"]);
    }

    #[test]
    fn yaml_to_dag_graph() {
        let spec = DagYamlSpec::parse(SAMPLE_YAML).unwrap();
        let dag = spec.into_dag_graph().expect("convert should succeed");
        assert_eq!(dag.get_node("a").unwrap().agent, "agent-a");
    }

    #[test]
    fn yaml_with_cycle_fails() {
        let cyclic_yaml = r#"
dag:
  id: cyclic
  task_summary: "环测试"
  nodes:
    - id: x
      agent: "a"
      task: "X"
      depends_on: [y]
    - id: y
      agent: "a"
      task: "Y"
      depends_on: [x]
"#;
        let spec = DagYamlSpec::parse(cyclic_yaml).unwrap();
        let err = spec.into_dag_graph().unwrap_err();
        assert!(matches!(err, DagError::CycleDetected(_)));
    }
}
```

### 7.3 序列化依赖

YAML 解析依赖 `serde_yaml`。当前 `runtime/Cargo.toml` **未引入** `serde_yaml`,需新增:

```toml
# rust/crates/runtime/Cargo.toml
[dependencies]
serde_yaml = "0.9"
```

注意:`serde_yaml` 0.9+ 的 API 与 0.8 不同,使用 `serde_yaml::from_str` 而非 `serde_yaml::from_str::<T>` 的旧形式。我们的代码已按 0.9 风格编写。

---

## 八、Checkpointer 持久化

### 8.1 SAGA 补偿模式

DAG 调度遵循 SAGA 模式:每个节点是 SAGA 的一个事务,失败时执行**补偿动作**(compensation)。补偿策略:

| 节点类型 | 正向动作 | 补偿动作 |
|---|---|---|
| 只读(分析) | spawn 子 agent | 无(无需补偿) |
| 写文件(Fork) | spawn 子 agent + 子 agent 写文件 | `git checkout -- <files>` 还原 |
| Worktree 写 | spawn 子 agent + worktree 提交 | `git worktree remove --force` |
| 验证 | 运行 verify_command | 无(验证无副作用) |

补偿触发时机:节点失败且 `DagFailurePolicy = Abort` 时,对所有已 Succeeded 节点按拓扑逆序执行补偿。

### 8.2 检查点格式

检查点存储在 `<workspace>/.claw/dags/<dag_id>/`:

```
.claw/dags/dag-1778000000-ab12/
├── dag.yaml              # DagGraph 声明(含节点状态快照)
├── meta.json             # DAG 元信息(replan_count, started_at, ...)
└── nodes/
    ├── analyze.json      # NodeResult(成功)
    ├── design.json       # NodeResult(成功)
    └── impl_v1.json      # NodeResult(失败,带 error)
```

`dag.yaml` 是 `DagGraph` 的完整序列化(含 `status` / `attempts` / `result` 的运行时快照)。`nodes/*.json` 是每个节点的 `NodeResult`,用于快速 resume(不重新解析整个 dag.yaml)。

### 8.3 CheckpointStore 代码

```rust
// rust/crates/runtime/src/dag/checkpoint.rs

use std::path::{Path, PathBuf};
use tokio::fs;
use serde::{Deserialize, Serialize};

use super::graph::DagGraph;
use super::node::NodeResult;

/// 检查点存储 — 负责持久化 DagGraph 与节点结果。
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    workspace_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagMeta {
    pub dag_id: String,
    pub started_at_ms: u64,
    pub last_checkpoint_ms: u64,
    pub replan_count: u32,
    pub completed: bool,
}

impl CheckpointStore {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self { workspace_root: workspace_root.into() }
    }

    fn dag_dir(&self, dag_id: &str) -> PathBuf {
        self.workspace_root.join(".claw").join("dags").join(dag_id)
    }

    fn nodes_dir(&self, dag_id: &str) -> PathBuf {
        self.dag_dir(dag_id).join("nodes")
    }

    fn meta_path(&self, dag_id: &str) -> PathBuf {
        self.dag_dir(dag_id).join("meta.json")
    }

    /// 持久化整个 DagGraph(声明 + 当前状态快照)。
    pub async fn save_dag(&self, dag: &DagGraph) -> Result<(), DagError> {
        let dir = self.dag_dir(&dag.id);
        fs::create_dir_all(&dir).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        let path = dir.join("dag.yaml");
        let yaml = serde_yaml::to_string(dag)
            .map_err(|e| DagError::YamlParse(e.to_string()))?;
        // 原子写:先写 .tmp,再 rename
        let tmp = dir.join("dag.yaml.tmp");
        fs::write(&tmp, yaml).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        fs::rename(&tmp, &path).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        Ok(())
    }

    /// 持久化单个节点结果(增量,不重写整个 dag.yaml)。
    pub async fn save_node(&self, dag_id: &str, result: &NodeResult) -> Result<(), DagError> {
        let dir = self.nodes_dir(dag_id);
        fs::create_dir_all(&dir).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        let path = dir.join(format!("{}.json", result.node_id));
        let tmp = dir.join(format!("{}.json.tmp", result.node_id));
        let json = serde_json::to_string_pretty(result)
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        fs::write(&tmp, json).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        fs::rename(&tmp, &path).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        Ok(())
    }

    /// 加载 DagGraph(仅声明 + 序列化的运行时状态)。
    pub async fn load_dag(&self, dag_id: &str) -> Result<Option<DagGraph>, DagError> {
        let path = self.dag_dir(dag_id).join("dag.yaml");
        if !path.exists() {
            return Ok(None);
        }
        let yaml = fs::read_to_string(&path).await
            .map_err(|e| DagError::YamlParse(e.to_string()))?;
        // DagGraph 需要实现 Deserialize — 见 §8.4
        let dag: DagGraphSnapshot = serde_yaml::from_str(&yaml)
            .map_err(|e| DagError::YamlParse(e.to_string()))?;
        Ok(Some(dag.into_dag_graph()?))
    }

    /// 加载 DagMeta。
    pub async fn load_meta(&self, dag_id: &str) -> Result<Option<DagMeta>, DagError> {
        let path = self.meta_path(dag_id);
        if !path.exists() {
            return Ok(None);
        }
        let json = fs::read_to_string(&path).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        let meta: DagMeta = serde_json::from_str(&json)
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        Ok(Some(meta))
    }

    /// Resume:加载 DAG + 重放所有已完成节点的结果。
    ///
    /// 流程:
    /// 1. load_dag(dag_id) → DagGraph(含序列化的 status 快照)
    /// 2. 遍历 nodes/ 目录,加载每个 NodeResult
    /// 3. 对每个 result,调用 dag.mark_status + dag.set_result
    /// 4. 返回恢复后的 DagGraph,调度器可继续 run()
    pub async fn resume(&self, dag_id: &str) -> Result<Option<DagGraph>, DagError> {
        let mut dag = match self.load_dag(dag_id).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        let nodes_dir = self.nodes_dir(dag_id);
        if !nodes_dir.exists() {
            return Ok(Some(dag));
        }
        let mut reader = fs::read_dir(&nodes_dir).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        while let Some(entry) = reader.next_entry().await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let json = fs::read_to_string(&path).await
                .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
            let result: NodeResult = serde_json::from_str(&json)
                .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
            dag.mark_status(&result.node_id, result.status);
            dag.set_result(&result.node_id, result);
        }
        Ok(Some(dag))
    }

    /// 列出所有未完成的 DAG(用于 `dag_status --list`)。
    pub async fn list_incomplete(&self) -> Result<Vec<String>, DagError> {
        let dags_root = self.workspace_root.join(".claw").join("dags");
        if !dags_root.exists() {
            return Ok(Vec::new());
        }
        let mut reader = fs::read_dir(&dags_root).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        let mut result = Vec::new();
        while let Some(entry) = reader.next_entry().await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?
        {
            if !entry.path().is_dir() {
                continue;
            }
            let dag_id = entry.file_name().to_string_lossy().to_string();
            if let Some(meta) = self.load_meta(&dag_id).await? {
                if !meta.completed {
                    result.push(dag_id);
                }
            }
        }
        Ok(result)
    }
}

/// DagGraph 的可序列化快照(分离声明与运行时状态)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DagGraphSnapshot {
    id: String,
    task_summary: String,
    config: super::graph::DagConfig,
    replan_count: u32,
    nodes: Vec<super::node::DagNode>,
}

impl DagGraphSnapshot {
    fn into_dag_graph(self) -> Result<DagGraph, DagError> {
        DagGraph::new(self.id, self.task_summary, self.nodes, self.config)
    }
}
```

### 8.4 与 NOTEBOOK.md 协同

`NOTEBOOK.md` 是过程记忆(见 `docs/navigation-file-context.md`),记录 agent 的工作过程。DAG 与 NOTEBOOK.md 的协同点:

1. **节点完成时追加**:每个节点 Succeeded 后,把 `NodeResult.summary` 追加到 NOTEBOOK.md 的"DAG Progress"段。
2. **DAG 完成时总结**:`DagRunResult` 生成后,把 mermaid 图 + 节点统计写入 NOTEBOOK.md 的"Session Summary"段。
3. **Resume 时读取**:Resume DAG 时,先读 NOTEBOOK.md 的"DAG Progress"段,把已完成节点的 summary 注入子 agent 的 prompt(让子 agent 知道历史上下文)。

实现接口(在 `DagScheduler` 中):

```rust
impl DagScheduler {
    /// 节点完成后追加到 NOTEBOOK.md。
    async fn append_to_notebook(&self, result: &NodeResult) -> Result<(), DagError> {
        let notebook_path = self.checkpoint_store.workspace_root.join("NOTEBOOK.md");
        let entry = format!(
            "\n## DAG Node: {} ({})\n- Status: {:?}\n- Summary: {}\n- Refs: {:?}\n",
            result.node_id,
            self.dag.id,
            result.status,
            result.summary,
            result.refs
        );
        let mut content = tokio::fs::read_to_string(&notebook_path).await
            .unwrap_or_default();
        content.push_str(&entry);
        tokio::fs::write(&notebook_path, content).await
            .map_err(|e| DagError::CheckpointIo(e.to_string()))?;
        Ok(())
    }
}
```

---

## 九、LaneEvent 扩展

### 9.1 新增事件类型

当前 `LaneEventName` 有 23 个变体(见 `lane_events.rs` line 5-57)。DAG 模块需新增 6 个事件:

| 事件名 | wire 值 | 触发时机 | 数据字段 |
|---|---|---|---|
| `DagStarted` | `dag.started` | `DagScheduler::run` 开始 | `dag_id`, `node_count`, `task_summary` |
| `DagNodeStarted` | `dag.node.started` | 单个节点 spawn 子 agent 前 | `dag_id`, `node_id`, `agent`, `mode` |
| `DagNodeCompleted` | `dag.node.completed` | 节点到达 Succeeded 终态 | `dag_id`, `node_id`, `status`, `tokens_used` |
| `DagNodeFailed` | `dag.node.failed` | 节点到达 Failed/Cancelled 终态 | `dag_id`, `node_id`, `status`, `error` |
| `DagCompleted` | `dag.completed` | 所有节点到达终态 | `dag_id`, `succeeded`, `failed`, `skipped` |
| `DagFailed` | `dag.failed` | DAG 整体失败(关键路径失败/超时) | `dag_id`, `error`, `failed_nodes` |

### 9.2 LaneEventName 扩展代码

```rust
// rust/crates/runtime/src/lane_events.rs(扩展)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaneEventName {
    // ... 现有 23 个变体保持不变 ...

    /// ★ 新增:DAG 事件(§9.1)
    #[serde(rename = "dag.started")]
    DagStarted,
    #[serde(rename = "dag.node.started")]
    DagNodeStarted,
    #[serde(rename = "dag.node.completed")]
    DagNodeCompleted,
    #[serde(rename = "dag.node.failed")]
    DagNodeFailed,
    #[serde(rename = "dag.completed")]
    DagCompleted,
    #[serde(rename = "dag.failed")]
    DagFailed,
}

impl LaneEvent {
    /// DAG 启动事件。
    #[must_use]
    pub fn dag_started(
        emitted_at: impl Into<String>,
        dag_id: impl Into<String>,
        node_count: usize,
        task_summary: impl Into<String>,
    ) -> Self {
        let data = serde_json::json!({
            "dag_id": dag_id.into(),
            "node_count": node_count,
            "task_summary": task_summary.into(),
        });
        Self::new(LaneEventName::DagStarted, LaneEventStatus::Running, emitted_at)
            .with_data(data)
    }

    /// DAG 节点启动事件。
    #[must_use]
    pub fn dag_node_started(
        emitted_at: impl Into<String>,
        dag_id: impl Into<String>,
        node_id: impl Into<String>,
        agent: impl Into<String>,
        mode: &str,
    ) -> Self {
        let data = serde_json::json!({
            "dag_id": dag_id.into(),
            "node_id": node_id.into(),
            "agent": agent.into(),
            "mode": mode,
        });
        Self::new(LaneEventName::DagNodeStarted, LaneEventStatus::Running, emitted_at)
            .with_data(data)
    }

    /// DAG 节点完成事件(成功)。
    #[must_use]
    pub fn dag_node_completed(
        emitted_at: impl Into<String>,
        dag_id: impl Into<String>,
        node_id: impl Into<String>,
        tokens_used: u64,
        summary: impl Into<String>,
    ) -> Self {
        let data = serde_json::json!({
            "dag_id": dag_id.into(),
            "node_id": node_id.into(),
            "status": "succeeded",
            "tokens_used": tokens_used,
            "summary": summary.into(),
        });
        Self::new(LaneEventName::DagNodeCompleted, LaneEventStatus::Completed, emitted_at)
            .with_data(data)
    }

    /// DAG 节点失败事件。
    #[must_use]
    pub fn dag_node_failed(
        emitted_at: impl Into<String>,
        dag_id: impl Into<String>,
        node_id: impl Into<String>,
        status: &str,
        error: impl Into<String>,
    ) -> Self {
        let data = serde_json::json!({
            "dag_id": dag_id.into(),
            "node_id": node_id.into(),
            "status": status,
            "error": error.into(),
        });
        let mut event = Self::new(
            LaneEventName::DagNodeFailed,
            LaneEventStatus::Failed,
            emitted_at,
        )
        .with_data(data)
        .with_failure_class(LaneFailureClass::SubagentFailure);
        // 设置 fingerprint 用于去重(类比 SubagentResult 的处理)
        let fp = compute_event_fingerprint(&event.event, &event.status, event.data.as_ref());
        event.metadata.event_fingerprint = Some(fp);
        event
    }

    /// DAG 整体完成事件。
    #[must_use]
    pub fn dag_completed(
        emitted_at: impl Into<String>,
        dag_id: impl Into<String>,
        succeeded: usize,
        failed: usize,
        skipped: usize,
    ) -> Self {
        let data = serde_json::json!({
            "dag_id": dag_id.into(),
            "succeeded": succeeded,
            "failed": failed,
            "skipped": skipped,
        });
        Self::new(LaneEventName::DagCompleted, LaneEventStatus::Completed, emitted_at)
            .with_data(data)
            .with_terminal_fingerprint()
    }

    /// DAG 整体失败事件。
    #[must_use]
    pub fn dag_failed(
        emitted_at: impl Into<String>,
        dag_id: impl Into<String>,
        error: impl Into<String>,
        failed_nodes: Vec<String>,
    ) -> Self {
        let data = serde_json::json!({
            "dag_id": dag_id.into(),
            "error": error.into(),
            "failed_nodes": failed_nodes,
        });
        Self::new(LaneEventName::DagFailed, LaneEventStatus::Failed, emitted_at)
            .with_data(data)
            .with_failure_class(LaneFailureClass::SubagentFailure)
            .with_terminal_fingerprint()
    }
}
```

### 9.3 与现有 SubagentHandoff/SubagentResult 的关系

DAG 节点执行时**仍会**触发 `SubagentHandoff` / `SubagentResult`(因为底层调用 `MultiAgentCoordinator::spawn`)。两层事件的关系:

- `SubagentHandoff` / `SubagentResult`:子 agent 维度(单次 spawn 的生命周期)。
- `DagNodeStarted` / `DagNodeCompleted`:DAG 节点维度(可能包含多次 spawn — retry / fallback)。

即一个 `DagNodeCompleted` 可能对应多个 `SubagentResult`(重试场景)。下游消费者可通过 `dag_id` + `node_id` 关联两层事件。

---

## 十、dag_run / dag_status 工具

### 10.1 Tool Spec 定义

```rust
// rust/crates/rusty-claude-cli/src/plugin_state.rs(扩展 build_runtime_tools)

fn build_dag_tools() -> Vec<RuntimeToolDefinition> {
    vec![
        RuntimeToolDefinition {
            name: "dag_run",
            description: "启动一个 DAG 多 agent 编排任务。DAG 节点可并行执行、有依赖关系、支持重试与检查点。适用于多文件重构、跨模块分析等复杂任务。",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_summary": {
                        "type": "string",
                        "description": "DAG 任务的整体描述,注入到每个子 agent 的 prompt 中作为上下文"
                    },
                    "nodes": {
                        "type": "array",
                        "description": "DAG 节点列表,顺序不限(依赖关系由 depends_on 决定)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "节点 ID,在 DAG 内唯一" },
                                "agent": { "type": "string", "description": "子 agent 名称" },
                                "task": { "type": "string", "description": "节点任务描述" },
                                "depends_on": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "上游节点 ID 列表,空表示根节点"
                                },
                                "mode": {
                                    "type": "string",
                                    "enum": ["fork", "teammate", "worktree"],
                                    "default": "fork"
                                },
                                "verify_command": { "type": "string" },
                                "timeout_secs": { "type": "integer", "default": 300 },
                                "retry": {
                                    "type": "object",
                                    "properties": {
                                        "max_attempts": { "type": "integer", "default": 2 },
                                        "fallback_agent": { "type": "string" }
                                    }
                                }
                            },
                            "required": ["id", "agent", "task"]
                        }
                    },
                    "max_parallelism": { "type": "integer", "default": 4 },
                    "timeout_secs": { "type": "integer", "default": 1800 }
                },
                "required": ["task_summary", "nodes"]
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        RuntimeToolDefinition {
            name: "dag_status",
            description: "查询 DAG 执行状态。可查询单个 DAG(dag_id)或列出所有未完成 DAG(无 dag_id 时)。",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "dag_id": {
                        "type": "string",
                        "description": "DAG ID。省略时列出所有未完成 DAG。"
                    }
                }
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}
```

### 10.2 主 agent 调用接口

主 agent 调用 `dag_run` 后,执行流程:

1. `conversation.rs` 的 tool call 路由命中 `"dag_run"` 分支。
2. 解析 JSON 输入,构造 `DagGraph`。
3. 持久化到 `CheckpointStore`(写 `dag.yaml`)。
4. 启动 `DagScheduler::run()`(异步,不阻塞主 turn)。
5. 注入 Mermaid 渲染到 prompt(让主 agent 看到执行计划)。
6. 返回 `dag_id` + 节点统计给主 agent。

```rust
// rust/crates/runtime/src/conversation.rs(扩展 tool call 路由)

// 在现有路由分支中(line ~1232)添加:
match tool_name.as_str() {
    "dispatch_subagent" => self.execute_dispatch_subagent(input),
    "check_subagent" => self.execute_check_subagent(input),
    // ... 现有工具 ...

    // ★ P0 新增:DAG 工具
    "dag_run" => self.execute_dag_run(input).await,
    "dag_status" => self.execute_dag_status(input).await,
    // ...
}

/// 执行 dag_run 工具 — 构造 DAG + 启动调度器。
async fn execute_dag_run(
    &mut self,
    input: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| format!("invalid input JSON: {e}"))?;

    let task_summary = parsed
        .get("task_summary")
        .and_then(|v| v.as_str())
        .ok_or("missing 'task_summary'")?;

    // 构造 DagConfig
    let max_parallelism = parsed
        .get("max_parallelism")
        .and_then(|v| v.as_u64())
        .unwrap_or(4) as usize;
    let timeout_secs = parsed
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(1800);

    // 解析节点列表
    let nodes: Vec<DagNode> = serde_json::from_value(parsed["nodes"].clone())
        .map_err(|e| format!("parse nodes failed: {e}"))?;

    // 生成 dag_id(时间戳 + 短随机后缀)
    let dag_id = format!(
        "dag-{}-{:04x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        rand::random::<u32>() & 0xFFFF
    );

    let config = DagConfig {
        max_parallelism,
        timeout_secs,
        ..Default::default()
    };

    let dag = DagGraph::new(dag_id.clone(), task_summary.to_string(), nodes, config)
        .map_err(|e| format!("DAG construction failed: {e}"))?;

    // 持久化(先存盘再跑,确保 resume 可用)
    if let Some(store) = &self.checkpoint_store {
        store.save_dag(&dag).await
            .map_err(|e| format!("checkpoint save failed: {e}"))?;
    }

    // 注入 Mermaid 到 prompt(让主 agent 看到执行计划)
    let mermaid = dag.render_mermaid();
    self.inject_context(&format!(
        "# DAG Execution Plan (dag_id={dag_id})\n\n```mermaid\n{mermaid}\n```\n\n\
         DAG 已启动。使用 `dag_status` 工具查询进度。"
    ));

    // 发布 DagStarted lane event
    self.publish_lane_event(LaneEvent::dag_started(
        emitted_at_str(),
        &dag_id,
        dag.node_count(),
        task_summary,
    ));

    // 构造调度器并启动
    let coordinator = self.multi_agent_coordinator.clone()
        .ok_or("multi_agent_coordinator not configured")?;
    let recovery = self.recovery_orchestrator.clone()
        .unwrap_or_else(|| Arc::new(Mutex::new(RecoveryOrchestrator::new())));
    let cancel_token = self.cancel_token.child_token();
    let checkpoint_store = self.checkpoint_store.clone()
        .ok_or("checkpoint_store not configured")?;

    let mut scheduler = DagScheduler::new(
        dag,
        coordinator,
        recovery,
        cancel_token,
        checkpoint_store,
    );

    // 同步等待 DAG 完成(主 agent 阻塞)
    // 未来可改为后台执行 + dag_status 轮询
    match scheduler.run().await {
        Ok(result) => {
            self.publish_lane_event(LaneEvent::dag_completed(
                emitted_at_str(),
                &result.dag_id,
                result.succeeded,
                result.failed,
                result.skipped,
            ));
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "status": "completed",
                "dag_id": result.dag_id,
                "succeeded": result.succeeded,
                "failed": result.failed,
                "skipped": result.skipped,
                "total_tokens": result.total_tokens,
                "elapsed_ms": result.elapsed_ms,
            ))?)
        }
        Err(e) => {
            self.publish_lane_event(LaneEvent::dag_failed(
                emitted_at_str(),
                &dag_id,
                e.to_string(),
                vec![],
            ));
            Ok(format!("DAG failed: {e}"))
        }
    }
}

/// 执行 dag_status 工具 — 查询 DAG 状态。
async fn execute_dag_status(
    &self,
    input: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| format!("invalid input JSON: {e}"))?;
    let store = self.checkpoint_store.as_ref()
        .ok_or("checkpoint_store not configured")?;

    if let Some(dag_id) = parsed.get("dag_id").and_then(|v| v.as_str()) {
        // 查询单个 DAG
        let dag = store.load_dag(dag_id).await
            .map_err(|e| format!("load failed: {e}"))?
            .ok_or("DAG not found")?;
        let stats = dag.node_stats();
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "dag_id": dag.id,
            "task_summary": dag.task_summary,
            "nodes": stats,
            "mermaid": dag.render_mermaid(),
        }))?)
    } else {
        // 列出所有未完成 DAG
        let incomplete = store.list_incomplete().await
            .map_err(|e| format!("list failed: {e}"))?;
        Ok(serde_json::to_string_pretty(&incomplete)?)
    }
}
```

---

## 十一、与 MultiAgentCoordinator 协同

### 11.1 适配层设计

DAG 节点通过 `MultiAgentCoordinator::spawn` 派发子 agent,但 `spawn` 当前是**同步**接口(`Arc<Mutex<HashMap>>`)。DAG 调度器在 `tokio::spawn` 中调用 spawn 时,需要把 `coordinator: Arc<Mutex<MultiAgentCoordinator>>` 传给每个 task。

适配层的关键问题:

1. **`spawn` 同步性**:`coordinator.lock().await` 在 tokio 任务中可接受(不阻塞 runtime)。
2. **`run_subagent_turn` 复用**:现有 `ConversationRuntime::run_subagent_turn` 是 `&mut self` 方法,无法在 DAG task 中直接调用。需要抽取为独立函数或 trait。
3. **缓存隔离**:每个 DAG 节点的子 agent 走独立 LLM 请求(已有 `run_subagent_turn` 的隔离逻辑),DAG 层不需要额外处理。

### 11.2 适配层代码骨架

```rust
// rust/crates/runtime/src/dag/coordinator_adapter.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use crate::multi_agent::{CoordinationMode, MultiAgentCoordinator, SubagentStatus};
use super::node::{DagNode, NodeResult, NodeStatus};

/// 子 agent 执行器 trait — 解耦 DagScheduler 与 ConversationRuntime。
///
/// 实现者:
/// - `ConversationRuntime`:通过 `run_subagent_turn` 实际调用 LLM。
/// - `MockExecutor`(测试):返回固定结果,不调用 LLM。
#[async_trait::async_trait]
pub trait SubagentExecutor: Send + Sync {
    /// 执行子 agent turn,返回 NodeResult。
    ///
    /// 实现应:
    /// 1. 调用 coordinator.spawn + start
    /// 2. 构造隔离的 LLM 请求(独立 Session + 独立 prompt cache)
    /// 3. 写结果到 .claw/subagents/{id}.md
    /// 4. 返回 NodeResult(含 refs 路径)
    async fn execute(
        &self,
        node: &DagNode,
        coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    ) -> NodeResult;
}

/// 生产实现 — 通过 ConversationRuntime 调用 LLM。
pub struct ConversationExecutor {
    pub workspace_root: std::path::PathBuf,
    // 实际实现需持有 api_client 引用
}

#[async_trait::async_trait]
impl SubagentExecutor for ConversationExecutor {
    async fn execute(
        &self,
        node: &DagNode,
        coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    ) -> NodeResult {
        let start = std::time::Instant::now();

        // 1. spawn + start
        let subagent_id = {
            let coord = coordinator.lock().await;
            coord.spawn(&node.agent, &node.task, node.mode)
            // coord 自动 drop 释放锁
        };
        {
            let coord = coordinator.lock().await;
            let _ = coord.start(&subagent_id);
        }

        // 2. 执行子 agent turn(实际实现需调用 ConversationRuntime::run_subagent_turn)
        //    这里用占位逻辑,真实集成时替换
        let result_text = format!("子 agent {} 执行任务: {}", node.agent, node.task);
        let result_ref = format!(".claw/subagents/{subagent_id}.md");

        // 3. 写结果文件
        let result_path = self.workspace_root.join(&result_ref);
        if let Some(parent) = result_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&result_path, &result_text).await;

        // 4. 标记完成
        {
            let coord = coordinator.lock().await;
            let _ = coord.complete(&subagent_id, &result_ref);
        }

        NodeResult {
            node_id: node.id.clone(),
            status: NodeStatus::Succeeded,
            summary: result_text.chars().take(200).collect(),
            refs: vec![result_ref],
            tokens_used: 1000, // 占位:实际从 LLM response 提取
            error: None,
            elapsed_ms: start.elapsed().as_millis() as u64,
        }
    }
}

/// 测试用 mock 执行器 — 不调用 LLM,返回固定结果。
pub struct MockExecutor {
    pub result_status: NodeStatus,
    pub delay_ms: u64,
}

#[async_trait::async_trait]
impl SubagentExecutor for MockExecutor {
    async fn execute(
        &self,
        node: &DagNode,
        _coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    ) -> NodeResult {
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        NodeResult {
            node_id: node.id.clone(),
            status: self.result_status,
            summary: format!("mock result for {}", node.id),
            refs: vec![],
            tokens_used: 100,
            error: if self.result_status == NodeStatus::Failed {
                Some("mock failure".into())
            } else {
                None
            },
            elapsed_ms: self.delay_ms,
        }
    }
}
```

### 11.3 DagScheduler 持有 executor

```rust
// scheduler.rs 扩展

pub struct DagScheduler {
    pub(crate) dag: DagGraph,
    coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    recovery: Arc<Mutex<RecoveryOrchestrator>>,
    cancel_token: CancellationToken,
    checkpoint_store: super::checkpoint::CheckpointStore,
    /// ★ 新增:子 agent 执行器(替代直接调用 run_node)
    executor: Arc<dyn SubagentExecutor>,
}

impl DagScheduler {
    pub fn with_executor(
        mut self,
        executor: Arc<dyn SubagentExecutor>,
    ) -> Self {
        self.executor = executor;
        self
    }
}
```

---

## 十二、实施步骤分解

### 12.1 P0 阶段周维度任务

| 周次 | 任务 | 交付物 | 验收标准 |
|---|---|---|---|
| W1 | 依赖引入 + 模块骨架 | `Cargo.toml` 加 petgraph/tokio-util;`dag/mod.rs` + 9 个子文件空壳 | `cargo build` 通过 |
| W1 | DagNode + NodeStatus | `dag/node.rs` 完整实现 + 单测 | 单测覆盖状态转移、RetryPolicy 退避 |
| W2 | DagGraph + 环检测 | `dag/graph.rs` 完整实现 + 单测 | Kosaraju SCC 检测单测(含自环) |
| W2 | Plan → DAG 转换器 | `from_plan_artifact` + 单测 | 线性链 + 并行检测测试通过 |
| W3 | DagScheduler(不含 retry) | `dag/scheduler.rs` 主循环 + JoinSet | Mock executor 跑通 3 节点线性 DAG |
| W3 | CancellationToken 层级 | DAG 级 → 节点级取消 | 超时测试:5s 超时 DAG 在 5s 后退出 |
| W4 | 失败处理 + retry + fallback | `handle_node_failure` 完整逻辑 | 测试:节点失败 → retry → fallback → 恢复 |
| W4 | CheckpointStore | `dag/checkpoint.rs` 持久化 + resume | 测试:DAG 中断后 resume 跳过已完成节点 |
| W5 | YAML 解析器 | `dag/yaml_loader.rs` + schema | 测试:示例 YAML 解析 + 环检测拒绝 |
| W5 | LaneEvent 扩展 | 6 个新事件 + 构造函数 | 单测覆盖每个事件的 wire 值与 data |
| W6 | dag_run/dag_status 工具 | `plugin_state.rs` + `conversation.rs` 路由 | 集成测试:主 agent 调用 dag_run 跑通 2 节点 |
| W6 | Mermaid 渲染 + prompt 注入 | `render_mermaid` + `inject_context` | 人工验收:prompt 中看到 mermaid 图 |
| W7 | 端到端测试 + 文档 | `dag/tests.rs` + 本文档 | 5 个端到端场景全部通过 |

### 12.2 关键里程碑

- **M1(Week 2 末)**:DagGraph + Plan 转换器可用,单测全绿。可独立 review 数据结构。
- **M2(Week 4 末)**:DagScheduler + Checkpointer 可用,Mock executor 跑通完整 DAG 生命周期。
- **M3(Week 6 末)**:dag_run/dag_status 工具上线,主 agent 可通过 tool call 触发 DAG。
- **M4(Week 7 末)**:端到端测试通过,文档定稿,可合并到 main。

---

## 十三、测试矩阵

### 13.1 单元测试

| 模块 | 测试名 | 覆盖点 |
|---|---|---|
| `node.rs` | `node_status_is_terminal` | 终态判定 |
| `node.rs` | `backoff_delay_exponential_caps_at_max` | 指数退避上限 |
| `node.rs` | `retry_policy_default_values` | 默认值 |
| `graph.rs` | `new_dag_validates_acyclic` | Kosaraju 环检测 |
| `graph.rs` | `new_dag_detects_self_loop` | 自环检测 |
| `graph.rs` | `ready_nodes_returns_pending_with_succeeded_deps` | 就绪节点计算 |
| `graph.rs` | `ready_nodes_skips_running` | Running 节点不重复拾取 |
| `graph.rs` | `mark_status_transitions` | 状态标记 |
| `graph.rs` | `all_terminal_detects_completion` | 全终态判定 |
| `graph.rs` | `render_mermaid_includes_all_nodes` | Mermaid 渲染 |
| `yaml_loader.rs` | `from_plan_linear_chain` | 线性转换 |
| `yaml_loader.rs` | `from_plan_parallel_detection` | 并行检测 |
| `yaml_loader.rs` | `yaml_parse_sample` | YAML 解析 |
| `yaml_loader.rs` | `yaml_rejects_cycle` | YAML 环检测 |
| `checkpoint.rs` | `save_and_load_dag_roundtrip` | 持久化往返 |
| `checkpoint.rs` | `resume_skips_completed_nodes` | Resume 跳过已完成 |
| `lane_events.rs` | `dag_started_serializes_correctly` | 事件序列化 |
| `lane_events.rs` | `dag_failed_carries_failure_class` | 失败事件分类 |

### 13.2 集成测试

| 场景 | 输入 | 预期 |
|---|---|---|
| 3 节点线性 DAG | A → B → C,全部成功 | succeeded=3, failed=0 |
| 2 节点并行 DAG | A → (B ∥ C) → D,全部成功 | succeeded=4, B/C 并行执行 |
| 节点失败 + retry | A 成功,B 第 1 次失败第 2 次成功 | succeeded=2, B.attempts=2 |
| 节点失败 + fallback | A 失败,fallback agent 成功 | succeeded=1, agent 切换 |
| DAG 超时 | timeout_secs=1,节点 sleep 10s | Err(Timeout), 节点 Cancelled |
| 关键路径失败 | A → B → C,B 失败(replan 上限达) | C 级联 Skipped, DAG 终止 |
| Checkpoint resume | DAG 跑到一半进程崩溃,重启 resume | 跳过已完成节点,继续未完成节点 |

### 13.3 端到端测试

| 场景 | 主 agent 调用 | 预期 |
|---|---|---|
| `dag_run` 基本流程 | `dag_run({task_summary, nodes: [...]})` | 返回 `dag_id` + 统计 |
| `dag_status` 查询 | `dag_status({dag_id})` | 返回 mermaid + 节点状态 |
| `dag_status` 列表 | `dag_status({})` | 返回未完成 DAG 列表 |
| YAML 加载 | 读取 `.claw/dags/x.yaml` 并跑 | 与 inline nodes 等价 |
| 复杂 DAG(5 节点) | 重构 multi_agent 模块 | 全部 Succeeded,verify 通过 |

### 13.4 性能测试

| 指标 | 目标 | 测试方法 |
|---|---|---|
| 100 节点 DAG 构造 | < 10ms | `criterion` benchmark |
| Kosaraju SCC(500 节点) | < 5ms | `criterion` benchmark |
| 10 节点并行调度开销 | < 50ms(不含 LLM) | 集成测试计时 |
| Checkpoint 写盘(单节点) | < 20ms | 集成测试计时 |

---

## 十四、风险与缓解

### 14.1 petgraph 大图性能

**风险**:DAG 节点数 > 1000 时,`kosaraju_scc` 与 `ready_nodes` 的 O(V+E) 遍历可能成为瓶颈。

**缓解**:

1. **缓存 ready_nodes**:每次节点状态变化后,只重新计算受影响节点的就绪状态,而非全图扫描。
2. **增量入度表**:维护 `HashMap<NodeIndex, usize>` 记录每个节点的未完成上游数,节点 Succeeded 时递减下游的入度,入度为 0 即就绪。
3. **限制 DAG 规模**:YAML schema 添加 `max_nodes: 200` 默认限制,超过则拒绝构造。

### 14.2 死锁检测

**风险**:`ready_nodes` 返回空但 `all_terminal` 返回 false,可能原因:

1. 条件边求值失败(表达式语法错误)。
2. 上游节点 Skipped 但下游未级联。
3. 节点状态不一致(Running 但子 agent 已退出未回调)。

**缓解**:

1. **死锁诊断**:`DagError::Deadlock` 携带当前所有非终态节点的 ID + 状态,便于定位。
2. **超时兜底**:每个节点有 `timeout_secs`,即使子 agent 不回调,超时也会触发 Cancelled。
3. **状态一致性检查**:调度循环每轮校验"Running 节点数 == JoinSet 任务数",不一致则 panic(开发期)或 log warn(生产期)。

### 14.3 跨节点状态污染

**风险**:Worktree 模式下,多个节点共享同一 worktree 路径,或 Fork 模式下子 agent 写同一文件,导致状态污染。

**缓解**:

1. **Worktree 隔离**:`MultiAgentCoordinator::spawn` 已为 Worktree 模式生成独立路径 `.claw/worktrees/{subagent_id}`。DAG 层透传 `node.workdir` 时,校验"同 DAG 内 workdir 唯一"。
2. **文件锁**:对 verify_command 涉及的文件(如 `Cargo.lock`),用 `tokio::fs::File::try_lock` 串行化。
3. **结果隔离**:每个子 agent 结果写到 `.claw/subagents/{subagent_id}.md`,DAG 节点间不共享 result 文件。

### 14.4 Token budget 失控

**风险**:DAG 节点数量大,总 token 消耗超出预算,导致 API 配额耗尽。

**缓解**:

1. **预算检查**:每轮 `ready_nodes` 前检查 `total_tokens + 预估本轮消耗 > token_budget`,超出则拒绝 spawn 新节点。
2. **节点级 token 限制**:`DagNode` 增加 `max_tokens` 字段,子 agent LLM 请求带 `max_tokens` 参数。
3. **降级策略**:预算耗尽时,未完成节点标记为 Skipped,DAG 返回部分结果。

### 14.5 Replan doom loop

**风险**:节点反复失败 → replan → 再失败,形成死循环。

**缓解**:

1. **硬上限**:`DEFAULT_MAX_REPLANS = 3`,超过即返回 `DagError`。
2. **attempts 不重置**:replan 时保留 `attempts` 计数,让 RecoveryOrchestrator 看到 escalating attempts,选择不同策略。
3. **降级到 abort**:replan_count >= 2 时,自动切换 `DagFailurePolicy::Abort`,避免无谓重试。

### 14.6 Checkpoint 损坏

**风险**:`dag.yaml` 或 `nodes/*.json` 文件损坏(磁盘错误、并发写),resume 失败。

**缓解**:

1. **原子写**:所有检查点文件先写 `.tmp` 再 `rename`,保证原子性(已在 `CheckpointStore` 实现)。
2. **schema 校验**:load 时用 `serde_yaml::from_str` 严格校验,解析失败返回 `DagError::YamlParse` 而非 panic。
3. **降级到空 DAG**:resume 失败时,log warn 并返回 `None`,调度器从头开始(不阻塞主流程)。

---

## 附录 A:模块文件清单

```
rust/crates/runtime/src/dag/
├── mod.rs                  # 模块入口,导出公共 API
├── graph.rs                # DagGraph + DagConfig + DagError + 环检测
├── node.rs                 # DagNode + NodeStatus + RetryPolicy + NodeResult
├── scheduler.rs            # DagScheduler + DagRunResult + run_node
├── checkpoint.rs           # CheckpointStore + DagMeta
├── yaml_loader.rs          # DagYamlSpec + from_plan_artifact
├── coordinator_adapter.rs  # SubagentExecutor trait + ConversationExecutor + MockExecutor
├── mermaid_render.rs       # render_mermaid(可合并到 graph.rs)
├── recovery.rs             # 失败处理策略(可合并到 scheduler.rs)
└── tests.rs                # 集成测试
```

## 附录 B:依赖清单

| 依赖 | 版本 | 用途 | 现状 |
|---|---|---|---|
| `petgraph` | 0.6 | DAG 数据结构 + SCC 算法 | **需新增** |
| `tokio-util` | 0.7 | CancellationToken | **需新增** |
| `serde_yaml` | 0.9 | YAML 解析 | **需新增** |
| `async-trait` | 0.1 | SubagentExecutor trait | **需新增**(若用 trait) |
| `tokio` | 1(现有) | JoinSet + 异步 IO | 已有 |
| `serde` | 1(现有) | 序列化 | 已有 |
| `thiserror` | (现有) | DagError 派生 | 已有 |
| `tracing` | (现有) | 日志 | 已有 |

## 附录 C:与主文档章节对应关系

| 本文档章节 | 主文档(ide-hooks-dag-implementation-plan.md)章节 |
|---|---|
| §1 现状审计 | §4.1 核心结论(展开) |
| §2 依赖选型 | §4.2 推荐技术栈(展开) |
| §3 DagNode | §4.4.1(完整重写 + 注释) |
| §4 DagGraph | §4.4.2(完整重写 + 注释) |
| §5 DagScheduler | §4.4.3(完整重写 + 注释) |
| §6 Plan→DAG 转换 | §4.4.4(展开 + 测试用例) |
| §7 YAML 声明式 | §4.4.5(展开 + 解析器代码) |
| §8 Checkpointer | §4.4.6(展开 + SAGA 模式) |
| §9 LaneEvent 扩展 | §4.4.7(展开 + 6 个事件构造函数) |
| §10 dag_run/dag_status | §4.4.8 + §4.4.9(展开 + 完整路由代码) |
| §11 与 Coordinator 协同 | (新增,主文档未展开) |
| §12 实施步骤 | §4.5 P0 交付物(周维度分解) |
| §13 测试矩阵 | (新增,主文档未展开) |
| §14 风险与缓解 | (新增,主文档未展开) |

---

文档结束。
