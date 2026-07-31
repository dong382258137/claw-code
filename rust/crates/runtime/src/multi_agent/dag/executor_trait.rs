//! Subagent executor abstraction (v0.2).
//!
//! Defines the [`SubagentExecutor`] trait that decouples the async
//! [`DagScheduler`](super::scheduler::DagScheduler) from any specific
//! subagent dispatch mechanism (Fork / Teammate / Worktree, in-process
//! mock, remote ACP, …).
//!
//! The scheduler calls [`SubagentExecutor::execute`] for each ready node
//! and treats [`NodeError`] variants as FailFast / cancel signals.
//!
//! # Retry ownership (v0.2 contract change)
//! As of v0.2, the **scheduler** owns retry / backoff logic — it consults
//! [`DagNode::max_retries`](super::types::DagNode::max_retries) and
//! [`RetryPolicy`](super::types::RetryPolicy) and re-invokes `execute` on
//! each attempt. Implementations should therefore execute the node **once**
//! per `execute` call and return immediately on failure; they must NOT
//! retry internally. Centralising retry in the scheduler keeps backoff
//! timing, attempt counting, and DagRun status updates consistent.
//!
//! Implementations:
//! - [`CoordinatorExecutor`](super::coordinator_executor::CoordinatorExecutor) —
//!   bridges to the existing
//!   [`MultiAgentCoordinator`](crate::multi_agent::MultiAgentCoordinator)
//!   via `spawn` + `execute_async` + an injectable `SubagentRunner` callback.
//! - `AcpSubagentExecutor` — Phase 4: dispatches over ACP channels.
//! - `MockSubagentExecutor` — unit tests (see scheduler tests).

use async_trait::async_trait;

use super::types::{DagNode, NodeResult};

/// Executor abstraction for a single DAG node's subagent run (v0.2).
///
/// Implementations are responsible for:
/// 1. Spawning / dispatching the subagent according to `node.mode`.
/// 2. Honouring the `cancel` signal cooperatively (best-effort).
/// 3. Returning a [`NodeResult`] on success or a [`NodeError`] variant on
///    failure / timeout / cancellation.
///
/// # Retry contract (v0.2)
/// The scheduler owns retry logic — it re-invokes `execute` up to
/// `node.max_retries` times with backoff derived from `node.retry_policy`.
/// Implementations must execute the node **once** per call and return
/// immediately on failure; they must NOT retry internally. Once the executor
/// returns `Err(NodeError::ExecutionFailed(_))`, the scheduler decides
/// whether to retry or apply FailFast.
#[async_trait]
pub trait SubagentExecutor: Send + Sync {
    /// Execute a single DAG node attempt, returning its result on success.
    ///
    /// `attempt` is 0-indexed (0 = first try, 1 = first retry, …).
    /// Implementations may use it to adjust behaviour on retries — e.g. the
    /// knowledge-freshness gate treats `attempt > 0` as "urgent" and skips
    /// research (`crate::knowledge_freshness::gate_task`).
    ///
    /// Implementations should NOT retry — the scheduler handles retries
    /// via [`DagNode::max_retries`](super::types::DagNode::max_retries) +
    /// [`RetryPolicy`](super::types::RetryPolicy).
    ///
    /// # Errors
    /// - [`NodeError::ExecutionFailed`] — the subagent finished but failed
    ///   its task. The scheduler may retry per `max_retries`.
    /// - [`NodeError::Timeout`] — the subagent did not complete within the
    ///   per-node timeout. Retriable.
    /// - [`NodeError::Cancelled`] — the subagent was cancelled. Not retriable;
    ///   the scheduler propagates this as [`DagError::Cancelled`].
    async fn execute(&self, node: &DagNode, attempt: u32) -> Result<NodeResult, NodeError>;

    /// Request cancellation of an in-flight node (best-effort).
    ///
    /// The scheduler invokes this on every running node when the DAG-level
    /// [`CancellationToken`](tokio_util::sync::CancellationToken) is fired.
    /// Implementations may drop the underlying task or signal it via a
    /// shared channel; `execute` should then return
    /// [`NodeError::Cancelled`].
    async fn cancel(&self, node_id: &str);
}

/// Errors that can occur while executing a single DAG node (v0.2).
///
/// Consumed by the async scheduler to decide between FailFast propagation
/// (`ExecutionFailed` / `Timeout`) and graceful wind-down (`Cancelled`).
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The subagent finished but failed its task and exhausted retries.
    #[error("subagent execution failed: {0}")]
    ExecutionFailed(String),
    /// The subagent did not complete within the per-node timeout.
    #[error("subagent timed out after {0}s")]
    Timeout(u64),
    /// The subagent was cancelled (via [`SubagentExecutor::cancel`] or a
    /// DAG-level cancellation token).
    #[error("subagent cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_agent::dag::types::RetryPolicy;
    use crate::multi_agent::CoordinationMode;

    /// A minimal in-process executor used to exercise the trait contract.
    struct EchoExecutor;

    #[async_trait]
    impl SubagentExecutor for EchoExecutor {
        async fn execute(&self, node: &DagNode, _attempt: u32) -> Result<NodeResult, NodeError> {
            Ok(NodeResult {
                node_id: node.id.clone(),
                summary: node.task.clone(),
                artifact_path: None,
                gated: None,
            })
        }

        async fn cancel(&self, _node_id: &str) {}
    }

    fn sample_node() -> DagNode {
        DagNode {
            id: "n1".to_string(),
            label: "Sample".to_string(),
            task: "echo hello".to_string(),
            depends_on: vec![],
            acceptance_criteria: "says hello".to_string(),
            verify_command: None,
            max_retries: 1,
            mode: CoordinationMode::Fork,
            retry_policy: RetryPolicy::default(),
        }
    }

    #[tokio::test]
    async fn echo_executor_returns_node_result() {
        let executor = EchoExecutor;
        let node = sample_node();
        let result = executor.execute(&node, 0).await.expect("echo should succeed");
        assert_eq!(result.node_id, "n1");
        assert_eq!(result.summary, "echo hello");
    }

    #[tokio::test]
    async fn echo_executor_cancel_is_noop() {
        let executor = EchoExecutor;
        // Should not panic / hang.
        executor.cancel("n1").await;
    }

    #[test]
    fn node_error_display_is_informative() {
        let fail = NodeError::ExecutionFailed("boom".to_string());
        assert!(format!("{fail}").contains("boom"));
        let timeout = NodeError::Timeout(30);
        assert!(format!("{timeout}").contains("30"));
        let cancelled = NodeError::Cancelled;
        assert!(format!("{cancelled}").contains("cancelled"));
    }
}
