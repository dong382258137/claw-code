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
//! - Applies FailFast semantics: any node failure (after exhausting retries)
//!   cancels all in-flight work and short-circuits the run.
//!
//! The scheduler is decoupled from the dispatch mechanism via the
//! [`SubagentExecutor`](super::executor_trait::SubagentExecutor) trait.
//!
//! # v0.2 capabilities (TODO 1 / 3 / 4 resolved)
//! - **DagRun bridging**: when configured with a [`DagStore`] and
//!   `DagRunId` via [`DagScheduler::with_dag_run`], the scheduler mirrors
//!   per-node progress (Running / Succeeded / Failed) and overall run
//!   status into the persistent [`DagRun`], so `dag_status` reflects
//!   async execution.
//! - **Retry with backoff**: the scheduler consumes
//!   [`DagNode::max_retries`] and [`RetryPolicy`] directly. On a node
//!   failure, if attempts remain, the scheduler sleeps for the computed
//!   backoff (cancellable) and re-spawns the node. Only after retries are
//!   exhausted does FailFast propagate. The executor is now single-shot —
//!   it should NOT retry internally.
//! - **Precise failure attribution**: each spawned task returns
//!   `(node_id, result)`, so the scheduler knows exactly which node failed
//!   without guessing from the in-flight set.
//!
//! [`DagStore`]: super::DagStore
//! [`DagRun`]: super::types::DagRun
//! [`DagNode::max_retries`]: super::types::DagNode::max_retries
//! [`RetryPolicy`]: super::types::RetryPolicy

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::executor_trait::{NodeError, SubagentExecutor};
use super::types::{
    DagError, DagGraph, DagNodeId, DagNodeStatus, DagStatus, NodeResult, RetryPolicy,
};
use super::DagStore;

/// Progress event emitted by [`DagScheduler::run_with_progress`].
///
/// Allows external observers (e.g. `dag_status` tool, telemetry) to react
/// to per-node and per-DAG lifecycle transitions without polling.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A node was spawned (entered Running state).
    NodeStarted {
        node_id: DagNodeId,
    },
    /// A node completed successfully.
    NodeSucceeded {
        node_id: DagNodeId,
    },
    /// A node failed. `attempt` is 0-indexed (0 = first try).
    /// `will_retry` is `true` if the scheduler will re-spawn the node.
    NodeFailed {
        node_id: DagNodeId,
        error: String,
        attempt: u32,
        will_retry: bool,
    },
    /// The entire DAG completed successfully.
    DagCompleted,
    /// The DAG failed (a node exhausted retries and FailFast propagated).
    DagFailed {
        node_id: DagNodeId,
    },
    /// The DAG was cancelled via [`DagScheduler::cancel`] or a token fire.
    DagCancelled,
}

/// Boxed progress callback used by [`DagScheduler::run_inner`].
type ProgressCallback = Option<Box<dyn FnMut(ProgressEvent) + Send>>;

/// Async concurrent DAG scheduler (v0.2).
///
/// Owns a [`DagGraph`] + a [`SubagentExecutor`] and runs the graph to
/// completion (or first failure after retries). The scheduler is
/// single-use: construct a fresh one per DAG run.
///
/// # Cancellation
/// [`DagScheduler::cancel`] fires the internal `CancellationToken`, which
/// propagates to every spawned task via `child_token()`. In-flight
/// `execute` calls should observe the child token (or the executor's own
/// `cancel`) and return [`NodeError::Cancelled`].
///
/// # Retry semantics
/// The scheduler retries failed nodes up to [`DagNode::max_retries`] times,
/// with backoff derived from [`RetryPolicy`]. The executor's `execute` is
/// called once per attempt — it must NOT retry internally. This contract
/// change (v0.2) centralises retry logic in the scheduler so that backoff
/// timing, attempt counting, and DagRun status updates are consistent.
///
/// [`DagNode::max_retries`]: super::types::DagNode::max_retries
pub struct DagScheduler {
    dag: DagGraph,
    executor: Arc<dyn SubagentExecutor>,
    cancel_token: CancellationToken,
    max_parallelism: usize,
    /// Optional DagStore bridge for persisting per-node + overall run status.
    /// When `None`, the scheduler runs without DagRun side-effects (e.g. in unit tests).
    dag_store: Option<Arc<DagStore>>,
    /// ID of the DagRun to update when `dag_store` is `Some`.
    dag_run_id: Option<String>,
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
            dag_store: None,
            dag_run_id: None,
        }
    }

    /// Override the concurrency bound.
    #[must_use]
    pub fn with_max_parallelism(mut self, limit: usize) -> Self {
        self.max_parallelism = limit.max(1);
        self
    }

    /// Bridge scheduler progress into a persistent [`DagRun`] (v0.2 TODO 1).
    ///
    /// When configured, the scheduler will:
    /// - Set the run status to `Running` on the first node spawn.
    /// - Update each node's status to `Running` / `Succeeded` / `Failed`
    ///   as it transitions.
    /// - Set the overall run status to `Completed` / `Failed` / `Cancelled`
    ///   at the end of the run.
    ///
    /// The `dag_store` must already contain a run with `run_id` (typically
    /// created via [`DagStore::start_run`]).
    ///
    /// [`DagRun`]: super::types::DagRun
    /// [`DagStore::start_run`]: super::DagStore::start_run
    #[must_use]
    pub fn with_dag_run(mut self, dag_store: Arc<DagStore>, run_id: impl Into<String>) -> Self {
        self.dag_store = Some(dag_store);
        self.dag_run_id = Some(run_id.into());
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

    /// Run the DAG to completion (or first failure after retries).
    ///
    /// See [`Self::run_with_progress`] for a variant that emits
    /// [`ProgressEvent`] callbacks.
    ///
    /// # Errors
    /// - [`DagError::NodeFailed`] — a node returned [`NodeError::ExecutionFailed`]
    ///   or [`NodeError::Timeout`] and exhausted retries. FailFast: no further
    ///   nodes are spawned.
    /// - [`DagError::Cancelled`] — the DAG-level token was cancelled.
    /// - [`DagError::JoinError`] — a spawned task panicked.
    pub async fn run(&self) -> Result<Vec<NodeResult>, DagError> {
        self.run_inner(None).await
    }

    /// Run the DAG, invoking `on_progress` for each lifecycle event.
    ///
    /// This is the same as [`run`](Self::run) but additionally emits
    /// [`ProgressEvent`]s as nodes start / succeed / fail / retry and as
    /// the DAG reaches a terminal state. Useful for `dag_status` tool
    /// integration and telemetry.
    ///
    /// # Errors
    /// Same as [`run`](Self::run).
    pub async fn run_with_progress<F>(&self, on_progress: F) -> Result<Vec<NodeResult>, DagError>
    where
        F: FnMut(ProgressEvent) + Send + 'static,
    {
        self.run_inner(Some(Box::new(on_progress))).await
    }

    /// Shared implementation for [`run`](Self::run) and
    /// [`run_with_progress`](Self::run_with_progress).
    ///
    /// `on_progress` is `Option<Box<...>>` to keep the call sites uniform
    /// without monomorphising the entire loop body per callback type.
    async fn run_inner(&self, mut on_progress: ProgressCallback) -> Result<Vec<NodeResult>, DagError> {
        // Validate acyclicity up-front so we never loop forever on a cyclic graph.
        self.dag.validate_acyclic()?;

        let mut completed: HashSet<DagNodeId> = HashSet::new();
        let mut results: Vec<NodeResult> = Vec::with_capacity(self.dag.node_count());
        // Each spawned task returns (node_id, result) so we can precisely
        // identify which node failed (v0.2 TODO 4) without guessing from
        // the in-flight set.
        let mut joinset: JoinSet<(DagNodeId, Result<NodeResult, NodeError>)> = JoinSet::new();
        // Track node ids whose tasks are currently spawned in the JoinSet.
        let mut inflight: HashSet<DagNodeId> = HashSet::new();
        // Per-node attempt counter for retry backoff (v0.2 TODO 3).
        // Local to this run — the scheduler is single-use.
        let mut attempts: HashMap<DagNodeId, u32> = HashMap::new();
        // Whether we've already stamped the DagRun as Running.
        let mut run_marked_running = false;

        loop {
            if self.cancel_token.is_cancelled() {
                joinset.abort_all();
                self.emit_dag_cancelled(&mut on_progress);
                self.bridge_run_status(DagStatus::Cancelled);
                return Err(DagError::Cancelled);
            }

            // Spawn ready nodes up to the parallelism cap.
            let ready: Vec<DagNodeId> = self
                .dag
                .ready_nodes(&completed)
                .into_iter()
                .filter(|id| !inflight.contains(id))
                .collect();
            let available_slots = self.max_parallelism.saturating_sub(inflight.len());
            for node_id in ready.iter().take(available_slots) {
                let Some(node) = self.dag.get_node(node_id) else {
                    continue;
                };
                let node = node.clone();
                let executor = self.executor.clone();
                let child_token = self.cancel_token.child_token();
                let node_id_for_status = node.id.clone();
                inflight.insert(node_id.clone());

                // Mark the DagRun as Running (once) and stamp this node as Running.
                if !run_marked_running {
                    self.bridge_run_status(DagStatus::Running);
                    run_marked_running = true;
                }
                self.bridge_node_status(&node.id, DagNodeStatus::Running);
                self.emit_progress(
                    &mut on_progress,
                    ProgressEvent::NodeStarted {
                        node_id: node.id.clone(),
                    },
                );

                joinset.spawn(async move {
                    // Cooperatively cancel if the parent token fires mid-execution.
                    let exec_fut = executor.execute(&node);
                    let res = tokio::select! {
                        biased;
                        _ = child_token.cancelled() => Err(NodeError::Cancelled),
                        res = exec_fut => res,
                    };
                    (node_id_for_status, res)
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

            match joined {
                Ok((node_id, Ok(result))) => {
                    // Success: remove from inflight, mark completed.
                    inflight.remove(&node_id);
                    completed.insert(node_id.clone());
                    results.push(result);
                    self.bridge_node_status(&node_id, DagNodeStatus::Succeeded);
                    self.emit_progress(
                        &mut on_progress,
                        ProgressEvent::NodeSucceeded { node_id },
                    );
                }
                Ok((node_id, Err(node_err))) => {
                    // Precise failure attribution (v0.2 TODO 4): we know
                    // exactly which node failed because the task returned
                    // its node_id.
                    inflight.remove(&node_id);

                    // Cancelled errors are not retriable — treat as DAG cancellation.
                    if matches!(node_err, NodeError::Cancelled) {
                        self.cancel_token.cancel();
                        joinset.abort_all();
                        self.bridge_node_status(&node_id, DagNodeStatus::Failed);
                        self.emit_dag_cancelled(&mut on_progress);
                        self.bridge_run_status(DagStatus::Cancelled);
                        return Err(DagError::Cancelled);
                    }

                    // Retry logic (v0.2 TODO 3).
                    let current_attempt = attempts.get(&node_id).copied().unwrap_or(0);
                    let max_retries = self
                        .dag
                        .get_node(&node_id)
                        .map(|n| n.max_retries)
                        .unwrap_or(0);
                    let will_retry = current_attempt < max_retries;

                    self.emit_progress(
                        &mut on_progress,
                        ProgressEvent::NodeFailed {
                            node_id: node_id.clone(),
                            error: node_err.to_string(),
                            attempt: current_attempt,
                            will_retry,
                        },
                    );

                    if will_retry {
                        // Increment attempt counter and compute backoff.
                        attempts.insert(node_id.clone(), current_attempt + 1);
                        let retry_policy = self
                            .dag
                            .get_node(&node_id)
                            .map(|n| n.retry_policy.clone())
                            .unwrap_or_default();
                        let delay = calculate_backoff(&retry_policy, current_attempt);

                        // Cancellable backoff sleep: if the DAG is cancelled
                        // during the sleep, bail out immediately.
                        tokio::select! {
                            biased;
                            _ = self.cancel_token.cancelled() => {
                                joinset.abort_all();
                                self.bridge_node_status(&node_id, DagNodeStatus::Failed);
                                self.emit_dag_cancelled(&mut on_progress);
                                self.bridge_run_status(DagStatus::Cancelled);
                                return Err(DagError::Cancelled);
                            }
                            _ = tokio::time::sleep(delay) => {
                                // Backoff elapsed; fall through to re-spawn.
                            }
                        }

                        // Re-spawn the node: stamp Running and spawn a fresh task.
                        self.bridge_node_status(&node_id, DagNodeStatus::Running);
                        self.emit_progress(
                            &mut on_progress,
                            ProgressEvent::NodeStarted {
                                node_id: node_id.clone(),
                            },
                        );
                        let Some(node) = self.dag.get_node(&node_id) else {
                            continue;
                        };
                        let node = node.clone();
                        let executor = self.executor.clone();
                        let child_token = self.cancel_token.child_token();
                        let node_id_for_status = node.id.clone();
                        inflight.insert(node_id);
                        joinset.spawn(async move {
                            let exec_fut = executor.execute(&node);
                            let res = tokio::select! {
                                biased;
                                _ = child_token.cancelled() => Err(NodeError::Cancelled),
                                res = exec_fut => res,
                            };
                            (node_id_for_status, res)
                        });
                        continue;
                    }

                    // Retries exhausted → FailFast.
                    self.cancel_token.cancel();
                    joinset.abort_all();
                    self.bridge_node_status(&node_id, DagNodeStatus::Failed);
                    self.emit_progress(
                        &mut on_progress,
                        ProgressEvent::DagFailed {
                            node_id: node_id.clone(),
                        },
                    );
                    self.bridge_run_status(DagStatus::Failed);
                    return Err(map_node_error(node_id, node_err));
                }
                Err(join_err) => {
                    self.cancel_token.cancel();
                    joinset.abort_all();
                    self.emit_dag_cancelled(&mut on_progress);
                    self.bridge_run_status(DagStatus::Failed);
                    return Err(DagError::JoinError(join_err.to_string()));
                }
            }
        }

        // All nodes completed successfully.
        self.emit_progress(&mut on_progress, ProgressEvent::DagCompleted);
        self.bridge_run_status(DagStatus::Completed);
        Ok(results)
    }

    /// Best-effort: stamp a node status into the bridged DagRun (if any).
    fn bridge_node_status(&self, node_id: &str, status: DagNodeStatus) {
        if let (Some(store), Some(run_id)) = (&self.dag_store, &self.dag_run_id) {
            // Best-effort: a missing run is a programming error, not a runtime
            // failure — we swallow the error to keep the scheduler resilient.
            let _ = store.update_node_status(run_id, node_id, status);
        }
    }

    /// Best-effort: stamp the overall run status into the bridged DagRun.
    fn bridge_run_status(&self, status: DagStatus) {
        if let (Some(store), Some(run_id)) = (&self.dag_store, &self.dag_run_id) {
            let _ = store.update_run_status(run_id, status);
        }
    }

    /// Invoke the progress callback (if any) with `event`.
    fn emit_progress(&self, on_progress: &mut ProgressCallback, event: ProgressEvent) {
        if let Some(cb) = on_progress.as_mut() {
            cb(event);
        }
    }

    /// Emit the `DagCancelled` event (helper to avoid repeated boilerplate).
    fn emit_dag_cancelled(&self, on_progress: &mut ProgressCallback) {
        self.emit_progress(on_progress, ProgressEvent::DagCancelled);
    }
}

/// Compute the backoff duration for a retry attempt (v0.2 TODO 3).
///
/// Uses exponential backoff: `base_delay_ms * backoff_factor^attempt`,
/// capped at `max_delay_ms`. With the default `RetryPolicy`
/// (base=500ms, factor=2.0, max=30s):
/// - attempt 0 → 500ms
/// - attempt 1 → 1000ms
/// - attempt 2 → 2000ms
/// - attempt 6+ → 30000ms (capped)
fn calculate_backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    let base = policy.base_delay_ms as f64;
    let factor = policy.backoff_factor.powi(attempt as i32);
    let delay_ms = (base * factor).min(policy.max_delay_ms as f64);
    // Guard against NaN / negative from a misconfigured factor.
    if delay_ms.is_finite() && delay_ms >= 0.0 {
        Duration::from_millis(delay_ms as u64)
    } else {
        Duration::from_millis(policy.base_delay_ms)
    }
}

fn map_node_error(node_id: DagNodeId, err: NodeError) -> DagError {
    match err {
        NodeError::Cancelled => DagError::Cancelled,
        NodeError::ExecutionFailed(_) | NodeError::Timeout(_) => DagError::NodeFailed(node_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_agent::dag::types::{Dag, DagNode, DagStatus, RetryPolicy};
    use crate::multi_agent::dag::DagStore;
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
            // Default to no retries so FailFast tests don't pay backoff delay.
            // Retry-specific tests construct nodes with `node_with_retries`.
            max_retries: 0,
            mode: CoordinationMode::Fork,
            retry_policy: RetryPolicy::default(),
        }
    }

    fn node_with_retries(id: &str, deps: &[&str], max_retries: u32) -> DagNode {
        let mut n = node(id, deps);
        n.max_retries = max_retries;
        n
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

    /// Executor that fails the first `n_failures` attempts on a node id,
    /// then succeeds. Used to exercise retry logic.
    struct FailThenSucceedExecutor {
        fail_id: DagNodeId,
        fail_n_times: u32,
        attempts: Mutex<u32>,
    }

    #[async_trait]
    impl SubagentExecutor for FailThenSucceedExecutor {
        async fn execute(&self, node: &DagNode) -> Result<NodeResult, NodeError> {
            if node.id == self.fail_id {
                let mut attempts = self.attempts.lock().expect("attempts poisoned");
                *attempts += 1;
                if *attempts <= self.fail_n_times {
                    return Err(NodeError::ExecutionFailed(format!(
                        "transient failure {} on {}",
                        *attempts,
                        node.id
                    )));
                }
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
            matches!(err, DagError::NodeFailed(ref id) if id == "n1"),
            "expected NodeFailed(n1), got {err:?}"
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

    // ========================================================================
    // v0.2 TODO 1: DagRun bridging
    // ========================================================================

    #[tokio::test]
    async fn dag_run_bridged_status_reflects_progress() {
        // Build a Dag + DagStore + DagRun, then run the scheduler with the bridge.
        let dag = Dag {
            id: "linear".to_string(),
            name: "Linear".to_string(),
            nodes: vec![
                node("n1", &[]),
                node("n2", &["n1"]),
                node("n3", &["n2"]),
            ],
        };
        let graph = DagGraph::from_dag(&dag);
        let store = Arc::new(DagStore::new());
        store.create_dag(dag.clone()).unwrap();
        let run = store.start_run(&dag.id).unwrap();
        let run_id = run.id.clone();

        let executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let scheduler = DagScheduler::new(graph, executor).with_dag_run(store.clone(), run_id);

        scheduler.run().await.expect("run should succeed");

        // The bridged DagRun should now be Completed with all nodes Succeeded.
        let final_run = store.get_run(&run.id).expect("run should exist");
        assert_eq!(final_run.status, DagStatus::Completed);
        assert!(final_run.completed_at.is_some());
        for (_, status) in &final_run.node_statuses {
            assert_eq!(*status, DagNodeStatus::Succeeded, "all nodes should be Succeeded");
        }
    }

    #[tokio::test]
    async fn dag_run_bridged_failure_marks_failed() {
        let dag = Dag {
            id: "parallel".to_string(),
            name: "Parallel".to_string(),
            nodes: vec![
                node("n1", &[]),
                node("n2", &[]),
                node("n3", &["n1", "n2"]),
            ],
        };
        let graph = DagGraph::from_dag(&dag);
        let store = Arc::new(DagStore::new());
        store.create_dag(dag.clone()).unwrap();
        let run = store.start_run(&dag.id).unwrap();
        let run_id = run.id.clone();

        let executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(graph, executor).with_dag_run(store.clone(), run_id);

        let _ = scheduler.run().await;

        let final_run = store.get_run(&run.id).expect("run should exist");
        assert_eq!(final_run.status, DagStatus::Failed);
        // The failed node should be marked Failed in the DagRun.
        assert_eq!(
            final_run.node_status("n1"),
            Some(DagNodeStatus::Failed),
            "n1 should be Failed in the bridged DagRun"
        );
    }

    #[tokio::test]
    async fn run_with_progress_emits_lifecycle_events() {
        let graph = linear_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let scheduler = DagScheduler::new(graph, executor);
        scheduler
            .run_with_progress(move |ev| {
                events_clone.lock().unwrap().push(ev);
            })
            .await
            .expect("run should succeed");

        let events = events.lock().unwrap();
        // 3 NodeStarted + 3 NodeSucceeded + 1 DagCompleted = 7 events.
        assert_eq!(events.len(), 7);
        assert!(events.iter().any(|e| matches!(
            e,
            ProgressEvent::DagCompleted
        )));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, ProgressEvent::NodeStarted { .. }))
                .count(),
            3
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, ProgressEvent::NodeSucceeded { .. }))
                .count(),
            3
        );
    }

    // ========================================================================
    // v0.2 TODO 3: Retry with backoff
    // ========================================================================

    #[tokio::test]
    async fn retry_recovers_from_transient_failure() {
        // n1 fails once then succeeds; max_retries=1 should allow recovery.
        let mut g = DagGraph::new("retry");
        g.add_node(node_with_retries("n1", &[], 1));
        let executor = Arc::new(FailThenSucceedExecutor {
            fail_id: "n1".to_string(),
            fail_n_times: 1,
            attempts: Mutex::new(0),
        });
        let scheduler = DagScheduler::new(g, executor);
        let results = scheduler.run().await.expect("retry should recover");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node_id, "n1");
    }

    #[tokio::test]
    async fn retry_exhausted_surfaces_failure() {
        // n1 always fails (fail_n_times=10 > max_retries=1).
        let mut g = DagGraph::new("retry-exhausted");
        g.add_node(node_with_retries("n1", &[], 1));
        let executor = Arc::new(FailThenSucceedExecutor {
            fail_id: "n1".to_string(),
            fail_n_times: 10,
            attempts: Mutex::new(0),
        });
        let scheduler = DagScheduler::new(g, executor);
        let err = scheduler.run().await.expect_err("should fail after retries");
        assert!(
            matches!(err, DagError::NodeFailed(ref id) if id == "n1"),
            "expected NodeFailed(n1) after retry exhaustion, got {err:?}"
        );
    }

    #[tokio::test]
    async fn retry_with_zero_max_retries_fails_immediately() {
        let mut g = DagGraph::new("no-retry");
        g.add_node(node_with_retries("n1", &[], 0));
        let executor = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(g, executor);
        let err = scheduler.run().await.expect_err("should fail without retry");
        assert!(matches!(err, DagError::NodeFailed(_)));
    }

    #[test]
    fn calculate_backoff_respects_factor_and_cap() {
        let policy = RetryPolicy {
            base_delay_ms: 100,
            backoff_factor: 2.0,
            max_delay_ms: 1_000,
        };
        assert_eq!(calculate_backoff(&policy, 0), Duration::from_millis(100));
        assert_eq!(calculate_backoff(&policy, 1), Duration::from_millis(200));
        assert_eq!(calculate_backoff(&policy, 2), Duration::from_millis(400));
        assert_eq!(calculate_backoff(&policy, 3), Duration::from_millis(800));
        // Capped at max_delay_ms.
        assert_eq!(calculate_backoff(&policy, 4), Duration::from_millis(1_000));
        assert_eq!(calculate_backoff(&policy, 10), Duration::from_millis(1_000));
    }

    // ========================================================================
    // v0.2 TODO 4: Precise failure attribution
    // ========================================================================

    #[tokio::test]
    async fn failed_node_is_precisely_identified() {
        // n1 and n2 both in flight; n2 fails. The scheduler must report n2,
        // not an arbitrary in-flight node.
        let mut g = DagGraph::new("precise-fail");
        g.add_node(node("n1", &[]));
        g.add_node(node("n2", &[]));
        g.add_node(node("n3", &["n1", "n2"]));
        g.add_edge(&"n1".to_string(), &"n3".to_string()).unwrap();
        g.add_edge(&"n2".to_string(), &"n3".to_string()).unwrap();

        let executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n2".to_string(),
        });
        let scheduler = DagScheduler::new(g, executor);
        let err = scheduler.run().await.expect_err("should fail");
        // The error must point at n2, not n1.
        assert!(
            matches!(err, DagError::NodeFailed(ref id) if id == "n2"),
            "expected NodeFailed(n2), got {err:?}"
        );
    }

    #[tokio::test]
    async fn cancelled_node_error_propagates_as_dag_cancelled() {
        // If the executor returns NodeError::Cancelled directly, the scheduler
        // should treat it as DAG cancellation (not a retriable failure).
        struct CancelExecutor;
        #[async_trait]
        impl SubagentExecutor for CancelExecutor {
            async fn execute(&self, _node: &DagNode) -> Result<NodeResult, NodeError> {
                Err(NodeError::Cancelled)
            }
            async fn cancel(&self, _node_id: &str) {}
        }

        let graph = linear_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(CancelExecutor);
        let scheduler = DagScheduler::new(graph, executor);
        let err = scheduler.run().await.expect_err("should be cancelled");
        assert!(matches!(err, DagError::Cancelled));
    }
}
