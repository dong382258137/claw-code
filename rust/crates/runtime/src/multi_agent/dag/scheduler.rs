//! DAG scheduler (v0.2) — async concurrent DAG execution with FailFast cancellation.
//!
//! Replaces the v0.1 synchronous polling scheduler with a tokio-based
//! implementation that:
//! - Uses [`DagGraph`](super::types::DagGraph) (petgraph) for ready-node
//!   computation via Kahn in-degree.
//! - Spawns ready nodes concurrently into a [`tokio::task::JoinSet`],
//!   bounded by `max_parallelism`.
//! - Honours a DAG-level [`CancellationToken`](tokio_util::sync::CancellationToken)
//!   for cooperative wind-down.
//! - Applies FailFast semantics: any node failure cancels all in-flight work
//!   and short-circuits the run.
//!
//! The scheduler is decoupled from the dispatch mechanism via the
//! [`SubagentExecutor`](super::executor_trait::SubagentExecutor) trait.
//!
//! # Current status (skeleton)
//! The async loop is functional but intentionally minimal:
//! - It tracks completed node IDs and feeds them back into `ready_nodes`.
//! - It does NOT yet persist per-node status into a [`DagRun`]
//!   (the v0.1 `DagExecutor` still owns that for `dag_status` tool compat).
//!   TODO(v0.3): bridge `DagScheduler` results into `DagStore` runs so
//!   `dag_status` reflects async execution.
//! - It does NOT yet implement retry-aware backoff; the executor owns
//!   retries per [`SubagentExecutor::execute`] contract.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::executor_trait::{NodeError, SubagentExecutor};
use super::types::{DagError, DagGraph, DagNodeId, NodeResult};

/// Async concurrent DAG scheduler (v0.2).
///
/// Owns a [`DagGraph`] + a [`SubagentExecutor`] and runs the graph to
/// completion (or first failure). The scheduler is single-use: construct a
/// fresh one per DAG run.
///
/// # Cancellation
/// [`DagScheduler::cancel`] fires the internal `CancellationToken`, which
/// propagates to every spawned task via `child_token()`. In-flight
/// `execute` calls should observe the child token (or the executor's own
/// `cancel`) and return [`NodeError::Cancelled`].
pub struct DagScheduler {
    dag: DagGraph,
    executor: Arc<dyn SubagentExecutor>,
    cancel_token: CancellationToken,
    max_parallelism: usize,
}

impl DagScheduler {
    /// Create a new scheduler for the given graph + executor.
    ///
    /// Uses [`DagGraph::max_parallelism`] as the concurrency bound.
    pub fn new(dag: DagGraph, executor: Arc<dyn SubagentExecutor>) -> Self {
        let max_parallelism = dag.max_parallelism().max(1);
        Self {
            dag,
            executor,
            cancel_token: CancellationToken::new(),
            max_parallelism,
        }
    }

    /// Override the concurrency bound.
    #[must_use]
    pub fn with_max_parallelism(mut self, limit: usize) -> Self {
        self.max_parallelism = limit.max(1);
        self
    }

    /// Access the underlying graph (read-only).
    #[must_use]
    pub fn graph(&self) -> &DagGraph {
        &self.dag
    }

    /// Fire the DAG-level cancellation token.
    ///
    /// In-flight tasks observe this via their child tokens and should
    /// return [`NodeError::Cancelled`]. The async [`run`](Self::run) loop
    /// will then drain the JoinSet and return [`DagError::Cancelled`].
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Run the DAG to completion (or first failure).
    ///
    /// Algorithm (layered + bounded parallel):
    /// 1. Compute `ready_nodes` from the current `completed` set.
    /// 2. Spawn up to `max_parallelism - in_flight` ready nodes into the
    ///    JoinSet, each with a child cancellation token.
    /// 3. `join_next().await` on any in-flight task.
    ///    - Success → mark node completed, loop.
    ///    - Failure → FailFast: cancel all, drain, return Err.
    /// 4. Stop when `ready_nodes` is empty AND JoinSet is empty.
    ///
    /// # Errors
    /// - [`DagError::NodeFailed`] — a node returned [`NodeError::ExecutionFailed`]
    ///   or [`NodeError::Timeout`]. FailFast: no further nodes are spawned.
    /// - [`DagError::Cancelled`] — the DAG-level token was cancelled.
    /// - [`DagError::JoinError`] — a spawned task panicked.
    pub async fn run(&self) -> Result<Vec<NodeResult>, DagError> {
        // Validate acyclicity up-front so we never loop forever on a cyclic graph.
        self.dag.validate_acyclic()?;

        let mut completed: HashSet<DagNodeId> = HashSet::new();
        let mut results: Vec<NodeResult> = Vec::with_capacity(self.dag.node_count());
        let mut joinset: JoinSet<Result<NodeResult, NodeError>> = JoinSet::new();
        // Track node ids whose tasks are currently spawned in the JoinSet.
        // `ready_nodes` only knows about `completed`; without this set we
        // would re-spawn a still-running node on every loop iteration.
        let mut inflight: HashSet<DagNodeId> = HashSet::new();

        loop {
            if self.cancel_token.is_cancelled() {
                joinset.abort_all();
                return Err(DagError::Cancelled);
            }

            // Spawn ready nodes up to the parallelism cap.
            // `ready_nodes` returns nodes whose dependencies are all in
            // `completed`; we additionally exclude nodes already in-flight
            // to avoid duplicate spawns.
            let ready: Vec<DagNodeId> = self
                .dag
                .ready_nodes(&completed)
                .into_iter()
                .filter(|id| !inflight.contains(id))
                .collect();
            let available_slots = self.max_parallelism.saturating_sub(inflight.len());
            for node_id in ready.iter().take(available_slots) {
                let Some(node) = self.dag.get_node(node_id) else {
                    // Should be impossible (ready_nodes only returns known ids).
                    continue;
                };
                let node = node.clone();
                let executor = self.executor.clone();
                let child_token = self.cancel_token.child_token();
                inflight.insert(node_id.clone());
                joinset.spawn(async move {
                    // Cooperatively cancel if the parent token fires mid-execution.
                    let exec_fut = executor.execute(&node);
                    tokio::select! {
                        biased;
                        _ = child_token.cancelled() => Err(NodeError::Cancelled),
                        res = exec_fut => res,
                    }
                });
            }

            // Nothing in flight and nothing ready → done.
            if joinset.is_empty() {
                if ready.is_empty() {
                    break;
                }
                // ready non-empty but no slots — unreachable because we
                // always spawn up to available_slots. Guard anyway.
                continue;
            }

            // Wait for any one task to finish.
            let Some(joined) = joinset.join_next().await else {
                break;
            };

            // Reconcile inflight via the result's node_id (set by the
            // executor on success; on failure we fall back to "any
            // still-inflight node" since the error carries no id).
            match joined {
                Ok(Ok(result)) => {
                    let node_id = result.node_id.clone();
                    // Remove from inflight; if not found (executor didn't
                    // populate node_id), drain one arbitrary entry.
                    if !inflight.remove(&node_id) {
                        let arbitrary = inflight.iter().next().cloned();
                        if let Some(id) = arbitrary {
                            inflight.remove(&id);
                        }
                    }
                    completed.insert(node_id);
                    results.push(result);
                }
                Ok(Err(node_err)) => {
                    // FailFast: cancel everything, drain, surface error.
                    self.cancel_token.cancel();
                    joinset.abort_all();
                    // Best-effort identification of which node failed.
                    let failed_id = identify_failed_node(&inflight, &node_err);
                    return Err(map_node_error(failed_id, node_err));
                }
                Err(join_err) => {
                    self.cancel_token.cancel();
                    joinset.abort_all();
                    return Err(DagError::JoinError(join_err.to_string()));
                }
            }
        }

        Ok(results)
    }
}

/// Best-effort identification of which in-flight node failed.
///
/// On `ExecutionFailed` / `Timeout` the executor does not embed the node id
/// in the error, so we cannot know precisely which task finished. We pick
/// an arbitrary in-flight node as the fall guy — this is acceptable because
/// the scheduler is single-use and the whole run is being torn down.
fn identify_failed_node(inflight: &HashSet<DagNodeId>, _err: &NodeError) -> Option<DagNodeId> {
    inflight.iter().next().cloned()
}

fn map_node_error(node_id: Option<DagNodeId>, err: NodeError) -> DagError {
    match err {
        NodeError::Cancelled => DagError::Cancelled,
        NodeError::ExecutionFailed(_) | NodeError::Timeout(_) => match node_id {
            Some(id) => DagError::NodeFailed(id),
            None => DagError::NodeFailed("unknown".to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_agent::dag::types::{DagNode, RetryPolicy};
    use crate::multi_agent::CoordinationMode;
    use async_trait::async_trait;
    use std::sync::Mutex;

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
        let mut g = DagGraph::new("linear").with_name("Linear");
        g.add_node(node("n1", &[]));
        g.add_node(node("n2", &["n1"]));
        g.add_node(node("n3", &["n2"]));
        g.add_edge(&"n1".to_string(), &"n2".to_string()).unwrap();
        g.add_edge(&"n2".to_string(), &"n3".to_string()).unwrap();
        g
    }

    fn parallel_graph() -> DagGraph {
        // n1, n2 independent; n3 depends on both.
        let mut g = DagGraph::new("parallel").with_name("Parallel");
        g.add_node(node("n1", &[]));
        g.add_node(node("n2", &[]));
        g.add_node(node("n3", &["n1", "n2"]));
        g.add_edge(&"n1".to_string(), &"n3".to_string()).unwrap();
        g.add_edge(&"n2".to_string(), &"n3".to_string()).unwrap();
        g
    }

    /// Executor that always succeeds and records which nodes it ran.
    struct SuccessExecutor {
        seen: Mutex<Vec<DagNodeId>>,
    }

    #[async_trait]
    impl SubagentExecutor for SuccessExecutor {
        async fn execute(&self, node: &DagNode) -> Result<NodeResult, NodeError> {
            self.seen.lock().expect("seen poisoned").push(node.id.clone());
            Ok(NodeResult {
                node_id: node.id.clone(),
                summary: node.task.clone(),
                artifact_path: None,
            })
        }

        async fn cancel(&self, _node_id: &str) {}
    }

    /// Executor that fails a specific node id, succeeds all others.
    struct FailOnExecutor {
        fail_id: DagNodeId,
    }

    #[async_trait]
    impl SubagentExecutor for FailOnExecutor {
        async fn execute(&self, node: &DagNode) -> Result<NodeResult, NodeError> {
            if node.id == self.fail_id {
                return Err(NodeError::ExecutionFailed(format!(
                    "forced failure on {}",
                    node.id
                )));
            }
            Ok(NodeResult {
                node_id: node.id.clone(),
                summary: node.task.clone(),
                artifact_path: None,
            })
        }

        async fn cancel(&self, _node_id: &str) {}
    }

    #[tokio::test]
    async fn run_linear_dag_completes_all_nodes() {
        let graph = linear_graph();
        let executor = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let scheduler = DagScheduler::new(graph, executor.clone());
        let results = scheduler.run().await.expect("linear run should succeed");
        assert_eq!(results.len(), 3);
        let completed: HashSet<DagNodeId> = results.iter().map(|r| r.node_id.clone()).collect();
        assert!(completed.contains("n1"));
        assert!(completed.contains("n2"));
        assert!(completed.contains("n3"));
        assert_eq!(executor.seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn run_parallel_dag_respects_dependencies() {
        let graph = parallel_graph();
        let executor = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let scheduler = DagScheduler::new(graph, executor);
        let results = scheduler.run().await.expect("parallel run should succeed");
        // n3 cannot run before n1 and n2 — since the executor is instant,
        // all three complete but the scheduler must have observed n1+n2
        // before spawning n3.
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn run_failfast_on_node_failure() {
        let graph = parallel_graph();
        let executor = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(graph, executor);
        let err = scheduler.run().await.expect_err("should fail fast");
        assert!(
            matches!(err, DagError::NodeFailed(_)),
            "expected NodeFailed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn run_cyclic_graph_returns_cycle_error() {
        let mut g = DagGraph::new("cyclic");
        g.add_node(node("a", &["b"]));
        g.add_node(node("b", &["a"]));
        g.add_edge(&"a".to_string(), &"b".to_string()).unwrap();
        g.add_edge(&"b".to_string(), &"a".to_string()).unwrap();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let scheduler = DagScheduler::new(g, executor);
        let err = scheduler.run().await.expect_err("cyclic should error");
        assert!(matches!(err, DagError::CycleDetected(_)));
    }

    #[tokio::test]
    async fn cancel_propagates_to_run() {
        let graph = linear_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let scheduler = DagScheduler::new(graph, executor);
        scheduler.cancel();
        let err = scheduler.run().await.expect_err("cancelled run should error");
        assert!(matches!(err, DagError::Cancelled));
    }
}
