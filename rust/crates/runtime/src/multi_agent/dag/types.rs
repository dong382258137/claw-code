//! DAG (Directed Acyclic Graph) orchestration types (G8.10).
//!
//! Data model for representing work items as a directed acyclic graph,
//! enabling dependency-aware execution ordering.
//!
//! v0.2 升级:在原有 `Dag`/`DagNode`/`DagRun` 数据模型之上,新增 [`DagGraph`]
//! (基于 petgraph 的图封装,提供 SCC 环检测与就绪节点计算),为 async
//! [`DagScheduler`](super::scheduler::DagScheduler) 的分层并行执行提供基础。

use std::collections::{HashMap, HashSet};

use petgraph::algo::kosaraju_scc;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::{Directed, Graph};
use serde::{Deserialize, Serialize};

use crate::multi_agent::CoordinationMode;

/// Unique identifier for a DAG node.
pub type DagNodeId = String;

/// Unique identifier for a DAG run.
pub type DagRunId = String;
/// Unique identifier for a DAG definition.
pub type DagId = String;

/// A single node in the DAG — represents one unit of work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DagNode {
    /// Unique node ID within the DAG.
    pub id: DagNodeId,
    /// Human-readable label.
    pub label: String,
    /// Task description (prompt for the subagent).
    pub task: String,
    /// Node IDs that must complete before this node can start.
    pub depends_on: Vec<DagNodeId>,
    /// Acceptance criteria for verification.
    pub acceptance_criteria: String,
    /// Optional verify command (e.g. `cargo test -p my_crate`).
    pub verify_command: Option<String>,
    /// Maximum retries before marking as failed.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Coordination mode for the subagent dispatched for this node (v0.2).
    /// Controls Fork / Teammate / Worktree dispatch strategy.
    /// Defaults to `Fork` for backward compatibility with v0.1 DAG definitions.
    #[serde(default = "default_coordination_mode")]
    pub mode: CoordinationMode,
    /// Retry policy (v0.2). Defaults to a conservative exponential-backoff policy.
    #[serde(default)]
    pub retry_policy: RetryPolicy,
}

const fn default_max_retries() -> u32 {
    2
}

fn default_coordination_mode() -> CoordinationMode {
    CoordinationMode::Fork
}

/// Retry policy for a DAG node (v0.2).
///
/// Encodes how the scheduler should retry a failed node before marking it as
/// permanently failed. As of v0.2, the async [`DagScheduler`](super::scheduler::DagScheduler)
/// consumes these fields directly: on a node failure, if
/// [`DagNode::max_retries`] has not been reached, the scheduler sleeps for
/// the computed backoff and re-spawns the node.
///
/// # Backoff formula
/// `delay_ms = base_delay_ms * backoff_factor^attempt`, capped at
/// `max_delay_ms`. With the default policy (base=500ms, factor=2.0,
/// max=30s): attempt 0 → 500ms, attempt 1 → 1s, attempt 2 → 2s, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RetryPolicy {
    /// Base delay in milliseconds between retries (exponential backoff seed).
    #[serde(default = "default_retry_base_delay_ms")]
    pub base_delay_ms: u64,
    /// Multiplier applied to the delay after each retry.
    #[serde(default = "default_retry_backoff_factor")]
    pub backoff_factor: f64,
    /// Cap on the per-retry delay in milliseconds.
    #[serde(default = "default_retry_max_delay_ms")]
    pub max_delay_ms: u64,
}

const fn default_retry_base_delay_ms() -> u64 {
    500
}

fn default_retry_backoff_factor() -> f64 {
    2.0
}

const fn default_retry_max_delay_ms() -> u64 {
    30_000
}

/// DAG 失败传播策略(v3)。
///
/// 控制 [`super::scheduler::DagScheduler`] 在某节点耗尽 retry 后的行为:
/// - `On`(默认):立即取消所有在途节点,返回 `Err(DagError::NodeFailed)`。
///   适用于"任一失败即整体失败"的严格语义。
/// - `Off`:标记失败节点,跳过其下游依赖,继续执行其他独立分支。
///   适用于"收集部分结果"的容错语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FailFast {
    /// FailFast 开启:任一节点失败(耗尽 retry)后立即取消整个 DAG。
    #[default]
    On,
    /// FailFast 关闭:节点失败后标记为 Failed,跳过其下游,继续执行独立分支。
    /// DAG 正常结束(返回 `Ok`),结果 `Vec<NodeResult>` 仅含成功节点。
    Off,
}

/// Result of executing a single DAG node (v0.2).
///
/// Produced by [`SubagentExecutor`](super::executor_trait::SubagentExecutor::execute)
/// and consumed by the async scheduler to update run state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NodeResult {
    /// ID of the node that produced this result.
    pub node_id: DagNodeId,
    /// Human-readable summary of what the subagent accomplished.
    pub summary: String,
    /// Optional artifact path (e.g. written file, git commit) for verification.
    pub artifact_path: Option<String>,
}

/// Execution status of a single DAG node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DagNodeStatus {
    /// Not yet started (dependencies not met).
    Pending,
    /// Ready to execute (all dependencies met).
    Ready,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed (retries exhausted or unrecoverable error).
    Failed,
    /// Skipped because a dependency failed.
    Skipped,
}

/// Overall DAG execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DagStatus {
    /// Not yet started.
    Pending,
    /// Nodes are executing.
    Running,
    /// All nodes succeeded.
    Completed,
    /// At least one node failed and the DAG cannot continue.
    Failed,
    /// Execution was cancelled.
    Cancelled,
    /// v3:FailFast::Off 下 DAG 完成,但有节点失败或跳过。
    /// 区别于 `Completed`(全成功)和 `Failed`(FailFast::On 不可继续)。
    CompletedWithFailures,
}

/// v3:DAG 运行的完整结果(含失败/跳过信息)。
///
/// 由 [`super::scheduler::DagScheduler::run_with_details`] 返回,
/// 供调用方分析部分失败情况并决定是否调用
/// [`retry_failed`](super::scheduler::DagScheduler::retry_failed) /
/// [`recover_skipped`](super::scheduler::DagScheduler::recover_skipped)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DagRunResult {
    /// 成功完成的节点结果(按完成顺序)。
    pub successes: Vec<NodeResult>,
    /// 永久失败的节点(耗尽 retry):`(node_id, 最后一次错误)`。
    pub failures: Vec<(DagNodeId, String)>,
    /// 因依赖失败而被跳过的节点 ID。
    pub skipped: Vec<DagNodeId>,
}

impl DagRunResult {
    /// 返回成功节点数量。
    #[must_use]
    pub fn success_count(&self) -> usize {
        self.successes.len()
    }

    /// 返回失败节点数量。
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// 返回跳过节点数量。
    #[must_use]
    pub fn skip_count(&self) -> usize {
        self.skipped.len()
    }

    /// 是否全部成功(无失败、无跳过)。
    #[must_use]
    pub fn is_all_success(&self) -> bool {
        self.failures.is_empty() && self.skipped.is_empty()
    }

    /// 提取成功节点列表(向后兼容 `run` 方法的 `Vec<NodeResult>` 返回值)。
    #[must_use]
    pub fn into_successes(self) -> Vec<NodeResult> {
        self.successes
    }
}

/// A DAG definition — the graph structure and node metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dag {
    /// Unique DAG identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// All nodes in the DAG.
    pub nodes: Vec<DagNode>,
}

/// A single DAG execution run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DagRun {
    /// Unique run identifier.
    pub id: DagRunId,
    /// Reference to the DAG definition.
    pub dag_id: String,
    /// Overall run status.
    pub status: DagStatus,
    /// Per-node execution status.
    pub node_statuses: Vec<(DagNodeId, DagNodeStatus)>,
    /// Unix epoch second when the run started.
    pub started_at: Option<u64>,
    /// Unix epoch second when the run completed.
    pub completed_at: Option<u64>,
    /// v3:retry 尝试历史(按时间顺序追加)。
    ///
    /// 每次 [`DagScheduler::retry_failed`](super::scheduler::DagScheduler::retry_failed)
    /// 或 [`recover_skipped`](super::scheduler::DagScheduler::recover_skipped)
    /// 调用都会把本次重试的每个节点结果追加到此列表,从而保留完整的尝试轨迹。
    /// 原始 `run` 的首次尝试不写入此字段(其结果直接反映在 `node_statuses` 中),
    /// 仅显式 retry/recover 调用才追加,以便区分"原始执行"与"重试执行"。
    #[serde(default)]
    pub retry_history: Vec<NodeAttempt>,
}

/// v3:单次 retry/recover 尝试的节点结果记录。
///
/// 由 [`DagScheduler::retry_failed`](super::scheduler::DagScheduler::retry_failed)
/// 与 [`recover_skipped`](super::scheduler::DagScheduler::recover_skipped) 在
/// 每次重试完成后追加到 [`DagRun::retry_history`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAttempt {
    /// 被重试的节点 ID。
    pub node_id: DagNodeId,
    /// 重试序号:1 = 第一次 retry,2 = 第二次 retry,以此类推。
    /// 0 保留给原始 run(不写入 `retry_history`)。
    pub attempt: u32,
    /// 本次尝试的结果状态(通常为 `Succeeded` 或 `Failed`)。
    pub status: DagNodeStatus,
    /// 失败时的错误信息;成功时为 `None`。
    pub error: Option<String>,
    /// 本次尝试开始的 Unix epoch second。
    pub started_at: u64,
    /// 本次尝试完成的 Unix epoch second。
    pub completed_at: u64,
}

impl Dag {
    /// Find a node by ID.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&DagNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Get nodes that have no unsatisfied dependencies (ready to execute).
    #[must_use]
    pub fn ready_nodes(&self, completed: &[DagNodeId]) -> Vec<&DagNode> {
        self.nodes
            .iter()
            .filter(|node| node.depends_on.iter().all(|dep| completed.contains(dep)))
            .collect()
    }

    /// Topological sort of node IDs (dependency order).
    #[must_use]
    pub fn topological_order(&self) -> Vec<DagNodeId> {
        let mut order = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut completed = Vec::new();

        loop {
            let ready: Vec<DagNodeId> = self
                .ready_nodes(&completed)
                .into_iter()
                .map(|n| n.id.clone())
                .filter(|id| visited.insert(id.clone()))
                .collect();
            if ready.is_empty() {
                break;
            }
            for id in &ready {
                order.push(id.clone());
                completed.push(id.clone());
            }
        }
        order
    }
}

impl DagRun {
    /// Create a new run for a DAG.
    #[must_use]
    pub fn new(dag: &Dag) -> Self {
        let node_statuses: Vec<_> = dag
            .nodes
            .iter()
            .map(|node| {
                let ready = node.depends_on.is_empty();
                let status = if ready {
                    DagNodeStatus::Ready
                } else {
                    DagNodeStatus::Pending
                };
                (node.id.clone(), status)
            })
            .collect();
        Self {
            id: format!("run-{}", now_secs()),
            dag_id: dag.id.clone(),
            status: DagStatus::Pending,
            node_statuses,
            started_at: None,
            completed_at: None,
            retry_history: Vec::new(),
        }
    }

    /// Get the status of a specific node.
    #[must_use]
    pub fn node_status(&self, node_id: &str) -> Option<DagNodeStatus> {
        self.node_statuses
            .iter()
            .find(|(id, _)| id == node_id)
            .map(|(_, s)| *s)
    }

    /// Update a node's status.
    pub fn set_node_status(&mut self, node_id: &str, status: DagNodeStatus) {
        if let Some((_, existing)) = self.node_statuses.iter_mut().find(|(id, _)| id == node_id) {
            *existing = status;
        }
    }

    /// v3:追加一条 retry 尝试记录到 `retry_history`。
    ///
    /// 同时根据本次尝试结果更新 `node_statuses`:
    /// - `Succeeded` → 节点状态更新为 `Succeeded`
    /// - `Failed` → 节点状态保持 `Failed`(不降级)
    /// - 其他状态 → 仅追加 history,不修改 `node_statuses`
    pub fn record_retry_attempt(&mut self, attempt: NodeAttempt) {
        // 同步 node_statuses:retry 成功时把节点从 Failed 提升为 Succeeded
        if attempt.status == DagNodeStatus::Succeeded {
            self.set_node_status(&attempt.node_id, DagNodeStatus::Succeeded);
        }
        self.retry_history.push(attempt);
    }

    /// v3:查询指定节点的 retry 次数(基于 `retry_history`)。
    ///
    /// 返回 0 表示该节点从未被显式 retry(仅有原始 run 的首次尝试)。
    #[must_use]
    pub fn retry_count_for(&self, node_id: &str) -> u32 {
        self.retry_history
            .iter()
            .filter(|a| a.node_id == node_id)
            .count() as u32
    }

    /// v3:查询指定节点的最后一次 retry 尝试。
    #[must_use]
    pub fn last_attempt_for(&self, node_id: &str) -> Option<&NodeAttempt> {
        self.retry_history
            .iter()
            .rev()
            .find(|a| a.node_id == node_id)
    }

    /// Check if all nodes are in a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.node_statuses.iter().all(|(_, status)| {
            matches!(
                status,
                DagNodeStatus::Succeeded | DagNodeStatus::Failed | DagNodeStatus::Skipped
            )
        })
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ============================================================================
// v0.2: petgraph-based DagGraph + DagError
// ============================================================================

/// Errors that can occur when constructing or validating a DAG (v0.2).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DagError {
    /// The graph contains a cycle (DAG invariant violated).
    /// The vector lists the node IDs participating in the cycle.
    #[error("cycle detected in DAG involving nodes: {0:?}")]
    CycleDetected(Vec<DagNodeId>),
    /// A node referenced an edge endpoint that does not exist in the graph.
    #[error("unknown node id referenced in edge: {0}")]
    UnknownNode(DagNodeId),
    /// A node failed during execution (FailFast propagation).
    #[error("node execution failed: {0}")]
    NodeFailed(DagNodeId),
    /// A JoinSet task panicked or was cancelled unexpectedly.
    #[error("scheduler join error: {0}")]
    JoinError(String),
    /// The entire DAG run was cancelled via CancellationToken.
    #[error("dag run cancelled")]
    Cancelled,
}

/// petgraph-backed directed graph wrapping the DAG node model (v0.2).
///
/// This is the structural complement to [`Dag`]: where `Dag` is a plain
/// serializable data model (used by `dag_run` / `dag_status` tools and
/// `DagStore`), `DagGraph` provides graph-algorithm primitives
/// ([`validate_acyclic`](DagGraph::validate_acyclic) via Kosaraju SCC,
/// [`ready_nodes`](DagGraph::ready_nodes) via in-degree computation) needed
/// by the async [`DagScheduler`](super::scheduler::DagScheduler).
///
/// Construction is additive: callers `add_node` + `add_edge` then call
/// `validate_acyclic` before handing the graph to the scheduler.
#[derive(Debug, Clone)]
pub struct DagGraph {
    graph: Graph<DagNode, (), Directed>,
    node_map: HashMap<DagNodeId, NodeIndex>,
    id: DagId,
    name: String,
    max_parallelism: usize,
}

impl DagGraph {
    /// Create a new empty graph with the given DAG id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            graph: Graph::new(),
            node_map: HashMap::new(),
            id: id.into(),
            name: String::new(),
            max_parallelism: DEFAULT_MAX_PARALLELISM,
        }
    }

    /// Set the human-readable name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set the max parallelism hint consumed by the scheduler.
    #[must_use]
    pub fn with_max_parallelism(mut self, limit: usize) -> Self {
        self.max_parallelism = limit.max(1);
        self
    }

    /// DAG identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Human-readable name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Configured max parallelism.
    #[must_use]
    pub fn max_parallelism(&self) -> usize {
        self.max_parallelism
    }

    /// Number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of edges in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// Add a node to the graph. Returns the petgraph NodeIndex.
    /// If a node with the same id already exists it is replaced (last-wins).
    pub fn add_node(&mut self, node: DagNode) -> NodeIndex {
        let id = node.id.clone();
        if let Some(&existing) = self.node_map.get(&id) {
            // Replace the node weight, keep edges.
            self.graph[existing] = node;
            return existing;
        }
        let idx = self.graph.add_node(node);
        self.node_map.insert(id, idx);
        idx
    }

    /// Add a directed edge `from -> to` (meaning `from` must complete before `to`).
    /// Returns an error if either endpoint is unknown.
    pub fn add_edge(&mut self, from: &DagNodeId, to: &DagNodeId) -> Result<(), DagError> {
        let from_idx = self
            .node_map
            .get(from)
            .copied()
            .ok_or_else(|| DagError::UnknownNode(from.clone()))?;
        let to_idx = self
            .node_map
            .get(to)
            .copied()
            .ok_or_else(|| DagError::UnknownNode(to.clone()))?;
        self.graph.add_edge(from_idx, to_idx, ());
        Ok(())
    }

    /// Build a `DagGraph` from an existing [`Dag`] (the v0.1 data model).
    ///
    /// Edges are derived from each node's `depends_on` list
    /// (`dep -> node` direction, matching [`DagGraph::add_edge`] semantics).
    #[must_use]
    pub fn from_dag(dag: &Dag) -> Self {
        let mut graph = Self::new(dag.id.clone()).with_name(dag.name.clone());
        for node in &dag.nodes {
            graph.add_node(node.clone());
        }
        for node in &dag.nodes {
            for dep in &node.depends_on {
                // Best-effort edge addition: unknown deps are silently skipped
                // (they will surface as UnknownNode on explicit add_edge, but
                // from_dag favours forgiving construction for v0.1 compat).
                let _ = graph.add_edge(dep, &node.id);
            }
        }
        graph
    }

    /// Get a node by id.
    #[must_use]
    pub fn get_node(&self, id: &DagNodeId) -> Option<&DagNode> {
        self.node_map.get(id).map(|&idx| &self.graph[idx])
    }

    /// Iterate over all nodes.
    pub fn nodes(&self) -> impl Iterator<Item = &DagNode> {
        self.graph.node_weights()
    }

    /// Iterate over node indices (internal petgraph handles).
    pub fn node_indices(&self) -> impl Iterator<Item = NodeIndex> + '_ {
        self.graph.node_indices()
    }

    /// Detect cycles using Kosaraju's strongly-connected-components algorithm.
    ///
    /// A DAG is acyclic iff every SCC has size <= 1. Self-loops are reported
    /// as a single-node SCC of size 1 with a self edge — we additionally
    /// check `graph.edges(idx)` for self-loops to catch that case.
    pub fn validate_acyclic(&self) -> Result<(), DagError> {
        let sccs = kosaraju_scc(&self.graph);
        for scc in sccs {
            // Self-loop: single node with an edge to itself.
            if scc.len() == 1 {
                let idx = scc[0];
                if self.graph.edges(idx).any(|e| e.target() == idx) {
                    let node_id = self.graph[idx].id.clone();
                    return Err(DagError::CycleDetected(vec![node_id]));
                }
                continue;
            }
            // Multi-node SCC = cycle.
            let cycle: Vec<DagNodeId> = scc
                .iter()
                .map(|&idx| self.graph[idx].id.clone())
                .collect();
            return Err(DagError::CycleDetected(cycle));
        }
        Ok(())
    }

    /// Return node IDs that are ready to execute (v0.2).
    ///
    /// A node is "ready" when:
    /// 1. It is not in the `completed` set (idempotent across scheduler ticks).
    /// 2. All of its predecessors (incoming edges) are in `completed`.
    ///
    /// This is the Kahn in-degree approach: count incoming edges whose source
    /// is NOT in `completed`; a node is ready when that count is 0.
    #[must_use]
    pub fn ready_nodes(&self, completed: &HashSet<DagNodeId>) -> Vec<DagNodeId> {
        let mut ready = Vec::new();
        for idx in self.graph.node_indices() {
            let node = &self.graph[idx];
            if completed.contains(&node.id) {
                continue;
            }
            // A node is ready when every predecessor is completed.
            let blocked = self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .any(|pred| {
                    let pred_id = &self.graph[pred].id;
                    !completed.contains(pred_id)
                });
            if !blocked {
                ready.push(node.id.clone());
            }
        }
        ready
    }

    /// Topological order via petgraph's `toposort` (returns Err on cycle).
    ///
    /// Convenience wrapper for callers that want a linear execution order
    /// (e.g. the sync [`DagExecutor`](super::executor::DagExecutor)).
    pub fn topological_order(&self) -> Result<Vec<DagNodeId>, DagError> {
        match petgraph::algo::toposort(&self.graph, None) {
            Ok(order) => Ok(order.into_iter().map(|idx| self.graph[idx].id.clone()).collect()),
            Err(_) => {
                // Cycle: surface via the same SCC-based reporting for consistency.
                self.validate_acyclic()?;
                // Should be unreachable (toposort err means cycle), but guard anyway.
                Err(DagError::CycleDetected(Vec::new()))
            }
        }
    }
}

impl PartialEq for DagGraph {
    fn eq(&self, other: &Self) -> bool {
        // Structural equality on node ids + edge set (ignores petgraph NodeIndex ordering).
        if self.id != other.id || self.name != other.name {
            return false;
        }
        if self.node_count() != other.node_count()
            || self.edge_count() != other.edge_count()
        {
            return false;
        }
        // Compare nodes by id (order-independent).
        let mut self_nodes: Vec<&DagNode> = self.nodes().collect();
        let mut other_nodes: Vec<&DagNode> = other.nodes().collect();
        self_nodes.sort_by_key(|n| n.id.as_str());
        other_nodes.sort_by_key(|n| n.id.as_str());
        if self_nodes != other_nodes {
            return false;
        }
        // Compare edges as (from_id, to_id) pairs.
        let self_edges: HashSet<(DagNodeId, DagNodeId)> = self
            .graph
            .edge_indices()
            .map(|e| {
                let (f, t) = self.graph.edge_endpoints(e).expect("edge endpoints");
                (self.graph[f].id.clone(), self.graph[t].id.clone())
            })
            .collect();
        let other_edges: HashSet<(DagNodeId, DagNodeId)> = other
            .graph
            .edge_indices()
            .map(|e| {
                let (f, t) = other.graph.edge_endpoints(e).expect("edge endpoints");
                (other.graph[f].id.clone(), other.graph[t].id.clone())
            })
            .collect();
        self_edges == other_edges
    }
}

impl Eq for DagGraph {}

/// Default parallelism consumed by [`DagGraph`] and the async scheduler.
pub const DEFAULT_MAX_PARALLELISM: usize = 4;

#[cfg(test)]
mod graph_tests {
    use super::*;
    use crate::multi_agent::dag::types::DagNode;

    fn node(id: &str, deps: &[&str]) -> DagNode {
        DagNode {
            id: id.to_string(),
            label: id.to_string(),
            task: format!("task-{id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            acceptance_criteria: "ok".to_string(),
            verify_command: None,
            max_retries: 1,
            mode: CoordinationMode::Fork,
            retry_policy: RetryPolicy::default(),
        }
    }

    fn linear_graph() -> DagGraph {
        // n1 -> n2 -> n3
        let mut g = DagGraph::new("linear");
        g.add_node(node("n1", &[]));
        g.add_node(node("n2", &["n1"]));
        g.add_node(node("n3", &["n2"]));
        g.add_edge(&"n1".to_string(), &"n2".to_string()).unwrap();
        g.add_edge(&"n2".to_string(), &"n3".to_string()).unwrap();
        g
    }

    #[test]
    fn validate_acyclic_accepts_linear_dag() {
        let g = linear_graph();
        assert!(g.validate_acyclic().is_ok());
    }

    #[test]
    fn validate_acyclic_rejects_cycle() {
        let mut g = DagGraph::new("cyclic");
        g.add_node(node("a", &["b"]));
        g.add_node(node("b", &["a"]));
        g.add_edge(&"a".to_string(), &"b".to_string()).unwrap();
        g.add_edge(&"b".to_string(), &"a".to_string()).unwrap();
        let err = g.validate_acyclic().unwrap_err();
        match err {
            DagError::CycleDetected(cycle) => {
                assert!(cycle.contains(&"a".to_string()));
                assert!(cycle.contains(&"b".to_string()));
            }
            other => panic!("expected CycleDetected, got {other:?}"),
        }
    }

    #[test]
    fn validate_acyclic_rejects_self_loop() {
        let mut g = DagGraph::new("self-loop");
        g.add_node(node("a", &["a"]));
        g.add_edge(&"a".to_string(), &"a".to_string()).unwrap();
        assert!(matches!(
            g.validate_acyclic(),
            Err(DagError::CycleDetected(_))
        ));
    }

    #[test]
    fn ready_nodes_returns_roots_initially() {
        let g = linear_graph();
        let completed: HashSet<DagNodeId> = HashSet::new();
        let ready = g.ready_nodes(&completed);
        assert_eq!(ready, vec!["n1".to_string()]);
    }

    #[test]
    fn ready_nodes_advances_after_completion() {
        let g = linear_graph();
        let completed: HashSet<DagNodeId> = ["n1".to_string()].into_iter().collect();
        let ready = g.ready_nodes(&completed);
        assert_eq!(ready, vec!["n2".to_string()]);
    }

    #[test]
    fn ready_nodes_handles_parallel_roots() {
        // Two independent roots n1, n2 then n3 depends on both.
        let mut g = DagGraph::new("diamond-ish");
        g.add_node(node("n1", &[]));
        g.add_node(node("n2", &[]));
        g.add_node(node("n3", &["n1", "n2"]));
        g.add_edge(&"n1".to_string(), &"n3".to_string()).unwrap();
        g.add_edge(&"n2".to_string(), &"n3".to_string()).unwrap();
        let ready = g.ready_nodes(&HashSet::new());
        assert!(ready.contains(&"n1".to_string()));
        assert!(ready.contains(&"n2".to_string()));
        assert!(!ready.contains(&"n3".to_string()));
    }

    #[test]
    fn ready_nodes_empty_when_all_completed() {
        let g = linear_graph();
        let completed: HashSet<DagNodeId> =
            ["n1".to_string(), "n2".to_string(), "n3".to_string()]
                .into_iter()
                .collect();
        assert!(g.ready_nodes(&completed).is_empty());
    }

    #[test]
    fn add_edge_unknown_node_returns_error() {
        let mut g = DagGraph::new("g");
        g.add_node(node("a", &[]));
        let err = g.add_edge(&"a".to_string(), &"ghost".to_string()).unwrap_err();
        assert!(matches!(err, DagError::UnknownNode(_)));
    }

    #[test]
    fn topological_order_matches_dependencies() {
        let g = linear_graph();
        let order = g.topological_order().unwrap();
        assert_eq!(order, vec!["n1".to_string(), "n2".to_string(), "n3".to_string()]);
    }

    #[test]
    fn from_dag_preserves_structure() {
        let dag = Dag {
            id: "d".to_string(),
            name: "D".to_string(),
            nodes: vec![node("a", &[]), node("b", &["a"])],
        };
        let graph = DagGraph::from_dag(&dag);
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert!(graph.validate_acyclic().is_ok());
    }
}
