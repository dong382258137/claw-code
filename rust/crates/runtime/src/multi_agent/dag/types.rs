//! DAG (Directed Acyclic Graph) orchestration types (G8.10).
//!
//! Data model for representing work items as a directed acyclic graph,
//! enabling dependency-aware execution ordering.

use serde::{Deserialize, Serialize};

/// Unique identifier for a DAG node.
pub type DagNodeId = String;

/// Unique identifier for a DAG run.
pub type DagRunId = String;

/// A single node in the DAG — represents one unit of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

const fn default_max_retries() -> u32 {
    2
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
}

/// A DAG definition — the graph structure and node metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
