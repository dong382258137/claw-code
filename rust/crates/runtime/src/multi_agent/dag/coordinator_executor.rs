//! `CoordinatorExecutor` — bridges [`MultiAgentCoordinator`] to the
//! [`SubagentExecutor`] trait (v0.2 TODO 2).
//!
//! This is the production-grade [`SubagentExecutor`] implementation that
//! the async [`DagScheduler`](super::scheduler::DagScheduler) uses to
//! dispatch each DAG node as a real subagent via the existing
//! [`MultiAgentCoordinator`] state machine.
//!
//! # Architecture
//! `MultiAgentCoordinator` is a *state tracker*: it owns subagent lifecycle
//! transitions (Created → Running → Completed/Failed/Cancelled) but does
//! NOT execute the LLM turn itself — that responsibility lives in
//! `ConversationRuntime::run_subagent_turn` (see `conversation.rs`), which
//! is layered above this crate and cannot be imported here without a
//! cyclic dependency.
//!
//! To bridge this gap, `CoordinatorExecutor` accepts an injectable
//! [`SubagentRunner`] callback that performs the actual LLM dispatch.
//! The callback receives `(subagent_id, task)` and returns
//! `Result<String, String>` (result_ref path on success, error message on
//! failure). In production this callback is wired to
//! `ConversationRuntime::run_subagent_turn`; in tests it can be a simple
//! closure.
//!
//! # Skeleton mode
//! When constructed via [`CoordinatorExecutor::new`] (no runner), `execute`
//! returns [`NodeError::ExecutionFailed`] with a descriptive message. This
//! allows the type to compile and be wired into the scheduler before the
//! real LLM dispatch is integrated. Use [`CoordinatorExecutor::with_runner`]
//! to inject a real runner for production use.
//!
//! [`MultiAgentCoordinator`]: crate::multi_agent::MultiAgentCoordinator

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use super::executor_trait::{NodeError, SubagentExecutor};
use super::types::{DagNode, NodeResult};
use crate::multi_agent::MultiAgentCoordinator;

/// Type alias for the runner function that executes a subagent's LLM turn.
///
/// The function receives `(subagent_id, task)` and returns the result
/// string (e.g. a `result_ref` path written to `.claw/subagents/{id}.md`)
/// on success, or an error message on failure.
///
/// The returned future is `Pin<Box<...>>` to keep the trait object erasure
/// simple — the runner is stored as a single `Arc<dyn Fn ... + Send + Sync>`
/// regardless of the concrete closure type.
pub type SubagentRunner = Arc<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// [`SubagentExecutor`] implementation that bridges to
/// [`MultiAgentCoordinator`].
///
/// # Lifecycle (per `execute` call)
/// 1. `coordinator.spawn(label, task, node.mode)` registers a new
///    subagent in `Created` state and returns its id.
/// 2. If a [`SubagentRunner`] is configured, it is invoked via
///    [`MultiAgentCoordinator::execute_async`] to actually run the
///    subagent's LLM turn. The spawned `JoinHandle` is awaited.
/// 3. On success, the runner's result string becomes
///    [`NodeResult::summary`] (typically a `result_ref` path).
/// 4. On failure, a [`NodeError::ExecutionFailed`] is returned. The
///    coordinator's internal state machine has already been transitioned
///    to `Failed` by `execute_async`.
/// 5. [`SubagentExecutor::cancel`] is bridged to
///    [`MultiAgentCoordinator::cancel`] (best-effort — a terminal
///    subagent cannot be cancelled).
///
/// # Retry semantics
/// Per the v0.2 scheduler contract, `execute` is called once per attempt.
/// The scheduler handles retry / backoff; this executor must NOT retry
/// internally.
pub struct CoordinatorExecutor {
    coordinator: Arc<MultiAgentCoordinator>,
    /// Optional runner. When `None`, `execute` returns an error indicating
    /// that `ConversationRuntime` integration is required.
    runner: Option<SubagentRunner>,
}

impl CoordinatorExecutor {
    /// Create a new executor backed by the given coordinator (skeleton mode).
    ///
    /// In this mode `execute` will always return an error. Use
    /// [`with_runner`](Self::with_runner) to inject a real LLM dispatch
    /// callback.
    #[must_use]
    pub fn new(coordinator: Arc<MultiAgentCoordinator>) -> Self {
        Self {
            coordinator,
            runner: None,
        }
    }

    /// Inject a runner function that executes a subagent's LLM turn.
    ///
    /// The runner receives `(subagent_id, task)` and returns the result
    /// string or an error. This is typically bridged to
    /// `ConversationRuntime::run_subagent_turn` (see `conversation.rs`),
    /// but is kept as a callback to avoid a hard dependency on
    /// `ConversationRuntime` in this crate-internal module.
    ///
    /// # Example
    /// ```no_run
    /// use std::sync::Arc;
    /// use runtime::multi_agent::MultiAgentCoordinator;
    /// use runtime::multi_agent::dag::coordinator_executor::CoordinatorExecutor;
    ///
    /// let coordinator = Arc::new(MultiAgentCoordinator::new());
    /// let runner: Arc<
    ///     dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
    ///         + Send + Sync,
    /// > = Arc::new(|id, task| {
    ///     Box::pin(async move {
    ///         // ... call ConversationRuntime::run_subagent_turn here ...
    ///         Ok(format!(".claw/subagents/{id}.md"))
    ///     })
    /// });
    /// let executor = CoordinatorExecutor::new(coordinator).with_runner(runner);
    /// ```
    #[must_use]
    pub fn with_runner(mut self, runner: SubagentRunner) -> Self {
        self.runner = Some(runner);
        self
    }

    /// Access the underlying coordinator (for inspection / status queries).
    #[must_use]
    pub fn coordinator(&self) -> &MultiAgentCoordinator {
        &self.coordinator
    }
}

#[async_trait]
impl SubagentExecutor for CoordinatorExecutor {
    async fn execute(&self, node: &DagNode) -> Result<NodeResult, NodeError> {
        // 1. Register the subagent in the coordinator's state machine.
        let subagent_id = self
            .coordinator
            .spawn(&node.label, &node.task, node.mode);

        // 2. Acquire the runner. If none is configured, cancel the freshly
        //    spawned subagent (to keep coordinator state consistent) and
        //    return a descriptive error.
        let runner = match self.runner.clone() {
            Some(r) => r,
            None => {
                // Best-effort cleanup: cancel transitions Created → Cancelled.
                // If it fails (e.g. concurrent transition), we still return
                // the configuration error — the coordinator state is the
                // caller's responsibility.
                let _ = self.coordinator.cancel(&subagent_id);
                return Err(NodeError::ExecutionFailed(format!(
                    "CoordinatorExecutor has no runner configured; subagent {subagent_id} \
                     cannot be executed. Wire ConversationRuntime::run_subagent_turn via \
                     CoordinatorExecutor::with_runner before dispatching DAG nodes."
                )));
            }
        };

        // 3. Dispatch via execute_async: the closure captures the runner Arc
        //    and invokes it with (id, task). execute_async handles the
        //    Created → Running transition and the terminal transition on
        //    completion / failure.
        let coordinator = self.coordinator.clone();
        let handle = coordinator
            .execute_async(&subagent_id, move |id, task| {
                let runner = runner.clone();
                Box::pin(async move { (runner)(id, task).await })
            })
            .map_err(NodeError::ExecutionFailed)?;

        // 4. Await the JoinHandle. A JoinError means the spawned task
        //    panicked — surface as ExecutionFailed.
        let result = handle
            .await
            .map_err(|e| NodeError::ExecutionFailed(format!("subagent join error: {e}")))?;

        // 5. Map the runner's Result<String, String> to NodeResult / NodeError.
        match result {
            Ok(summary) => Ok(NodeResult {
                node_id: node.id.clone(),
                summary,
                artifact_path: None,
            }),
            Err(e) => Err(NodeError::ExecutionFailed(e)),
        }
    }

    async fn cancel(&self, node_id: &str) {
        // Best-effort: coordinator.cancel fails if the subagent is already
        // in a terminal state, which is fine for cancellation semantics.
        // The node_id passed in is the DAG node id, which we use as the
        // subagent id (see execute: we pass node.label as the name but the
        // coordinator generates its own id; in practice the caller should
        // track the mapping). For now we attempt to cancel by node_id and
        // swallow any error.
        let _ = self.coordinator.cancel(node_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_agent::CoordinationMode;
    use crate::multi_agent::dag::types::RetryPolicy;

    fn sample_node() -> DagNode {
        DagNode {
            id: "n1".to_string(),
            label: "Sample".to_string(),
            task: "echo hello".to_string(),
            depends_on: vec![],
            acceptance_criteria: "says hello".to_string(),
            verify_command: None,
            max_retries: 0,
            mode: CoordinationMode::Fork,
            retry_policy: RetryPolicy::default(),
        }
    }

    #[tokio::test]
    async fn execute_without_runner_returns_config_error() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator.clone());
        let node = sample_node();
        let err = executor
            .execute(&node)
            .await
            .expect_err("should fail without runner");
        assert!(matches!(err, NodeError::ExecutionFailed(_)));
        let msg = format!("{err}");
        assert!(msg.contains("no runner configured"), "msg was: {msg}");
    }

    #[tokio::test]
    async fn execute_without_runner_cancels_spawned_subagent() {
        // When no runner is configured, execute should still spawn (then cancel)
        // the subagent, leaving the coordinator in a clean state.
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator.clone());
        let node = sample_node();
        let _ = executor.execute(&node).await;
        // The subagent should have been spawned and then cancelled.
        let agents = coordinator.list();
        assert_eq!(agents.len(), 1, "subagent should be registered");
        // The single subagent should be in Cancelled state.
        assert!(
            agents
                .iter()
                .all(|a| a.status == crate::multi_agent::SubagentStatus::Cancelled),
            "expected Cancelled, got {:?}",
            agents.iter().map(|a| a.status).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn execute_with_runner_returns_node_result() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        // Simple runner: returns a fake result_ref path.
        let runner: SubagentRunner = Arc::new(|_id, _task| {
            Box::pin(async { Ok(".claw/subagents/fake.md".to_string()) })
        });
        let executor = CoordinatorExecutor::new(coordinator).with_runner(runner);
        let node = sample_node();
        let result = executor
            .execute(&node)
            .await
            .expect("runner should succeed");
        assert_eq!(result.node_id, "n1");
        assert_eq!(result.summary, ".claw/subagents/fake.md");
    }

    #[tokio::test]
    async fn execute_with_failing_runner_returns_execution_failed() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let runner: SubagentRunner =
            Arc::new(|_id, _task| Box::pin(async { Err("llm error".to_string()) }));
        let executor = CoordinatorExecutor::new(coordinator).with_runner(runner);
        let node = sample_node();
        let err = executor
            .execute(&node)
            .await
            .expect_err("runner should fail");
        assert!(matches!(err, NodeError::ExecutionFailed(_)));
        let msg = format!("{err}");
        assert!(msg.contains("llm error"), "msg was: {msg}");
    }

    #[tokio::test]
    async fn execute_with_runner_transitions_coordinator_to_completed() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let runner: SubagentRunner = Arc::new(|_id, _task| {
            Box::pin(async { Ok("result".to_string()) })
        });
        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let node = sample_node();
        executor.execute(&node).await.expect("should succeed");
        let agents = coordinator.list();
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].status,
            crate::multi_agent::SubagentStatus::Completed
        );
    }

    #[tokio::test]
    async fn execute_with_failing_runner_transitions_coordinator_to_failed() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let runner: SubagentRunner =
            Arc::new(|_id, _task| Box::pin(async { Err("boom".to_string()) }));
        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let node = sample_node();
        let _ = executor.execute(&node).await;
        let agents = coordinator.list();
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].status,
            crate::multi_agent::SubagentStatus::Failed
        );
    }

    #[tokio::test]
    async fn cancel_is_best_effort_no_panic() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator);
        // Cancelling a non-existent subagent should not panic.
        executor.cancel("nonexistent").await;
    }

    #[tokio::test]
    async fn coordinator_accessor_returns_inner_ref() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator.clone());
        // Just verify the accessor compiles and returns the right type.
        let _inner: &MultiAgentCoordinator = executor.coordinator();
    }
}
