//! DAG executor (G8.10) — executes DAG nodes in dependency order.
//!
//! Uses [`SubagentCoordinator`] to dispatch individual nodes as sub-agents
//! and tracks their completion.

use std::collections::HashSet;

use crate::multi_agent::dag::types::{Dag, DagNodeId, DagNodeStatus, DagRun, DagStatus};

use super::types;

/// Executes a DAG by dispatching nodes as sub-agents in topological order.
///
/// This is a synchronous (blocking) executor that processes nodes one at a time.
/// For parallel execution, use [`DagScheduler`](super::scheduler::DagScheduler).
#[derive(Debug, Clone, Default)]
pub struct DagExecutor {
    /// Currently active run, if any.
    active_run: Option<DagRun>,
}

impl DagExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new DAG run.
    pub fn start_run(&mut self, dag: &Dag) -> DagRun {
        let run = DagRun::new(dag);
        self.active_run = Some(run.clone());
        run
    }

    /// Get the currently active run.
    #[must_use]
    pub fn active_run(&self) -> Option<&DagRun> {
        self.active_run.as_ref()
    }

    /// Get mutable reference to the active run.
    pub fn active_run_mut(&mut self) -> Option<&mut DagRun> {
        self.active_run.as_mut()
    }

    /// Get the next ready nodes (dependencies satisfied, not yet terminal).
    #[must_use]
    pub fn next_ready(&self, dag: &Dag) -> Vec<DagNodeId> {
        let Some(run) = &self.active_run else {
            return Vec::new();
        };

        let completed: Vec<DagNodeId> = run
            .node_statuses
            .iter()
            .filter(|(_, s)| *s == DagNodeStatus::Succeeded)
            .map(|(id, _)| id.clone())
            .collect();

        let terminal: HashSet<&DagNodeId> = run
            .node_statuses
            .iter()
            .filter(|(_, s)| {
                matches!(
                    s,
                    DagNodeStatus::Succeeded | DagNodeStatus::Failed | DagNodeStatus::Skipped
                )
            })
            .map(|(id, _)| id)
            .collect();

        dag.ready_nodes(&completed)
            .into_iter()
            .map(|n| n.id.clone())
            .filter(|id| !terminal.contains(id))
            .collect()
    }

    /// Mark a node as started.
    pub fn mark_started(&mut self, node_id: &str) {
        if let Some(run) = &mut self.active_run {
            if run.status == DagStatus::Pending {
                run.status = DagStatus::Running;
                run.started_at = Some(types::now_secs());
            }
            run.set_node_status(node_id, DagNodeStatus::Running);
        }
    }

    /// Mark a node as succeeded.
    pub fn mark_succeeded(&mut self, node_id: &str) {
        if let Some(run) = &mut self.active_run {
            run.set_node_status(node_id, DagNodeStatus::Succeeded);
            if run.is_terminal() {
                let all_ok = run
                    .node_statuses
                    .iter()
                    .all(|(_, s)| *s == DagNodeStatus::Succeeded);
                run.status = if all_ok {
                    DagStatus::Completed
                } else {
                    DagStatus::Failed
                };
                run.completed_at = Some(types::now_secs());
            }
        }
    }

    /// Mark a node as failed. Also skips nodes that depend on it.
    pub fn mark_failed(&mut self, dag: &Dag, node_id: &str) {
        if let Some(run) = &mut self.active_run {
            run.set_node_status(node_id, DagNodeStatus::Failed);

            // Skip nodes that depend on the failed node
            let mut failed_ids = vec![node_id.to_string()];
            let mut changed = true;
            while changed {
                changed = false;
                for node in &dag.nodes {
                    if node.depends_on.iter().any(|dep| failed_ids.contains(dep))
                        && run.node_status(&node.id).is_none_or(|s| {
                            s == DagNodeStatus::Pending || s == DagNodeStatus::Ready
                        })
                    {
                        run.set_node_status(&node.id, DagNodeStatus::Skipped);
                        failed_ids.push(node.id.clone());
                        changed = true;
                    }
                }
            }

            if run.is_terminal() {
                run.status = DagStatus::Failed;
                run.completed_at = Some(types::now_secs());
            }
        }
    }

    /// Cancel the entire run.
    pub fn cancel(&mut self) {
        if let Some(run) = &mut self.active_run {
            run.status = DagStatus::Cancelled;
            run.completed_at = Some(types::now_secs());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_agent::dag::types::{DagNode, RetryPolicy};
    use crate::multi_agent::CoordinationMode;

    fn sample_dag() -> Dag {
        Dag {
            id: "test-dag".to_string(),
            name: "Test DAG".to_string(),
            nodes: vec![
                DagNode {
                    id: "n1".to_string(),
                    label: "Step 1".to_string(),
                    task: "Do step 1".to_string(),
                    depends_on: vec![],
                    acceptance_criteria: "Step 1 done".to_string(),
                    verify_command: None,
                    max_retries: 1,
                    mode: CoordinationMode::Fork,
                    retry_policy: RetryPolicy::default(),
                    capability: crate::multi_agent::SubagentCapability::Analyze,
                },
                DagNode {
                    id: "n2".to_string(),
                    label: "Step 2".to_string(),
                    task: "Do step 2".to_string(),
                    depends_on: vec!["n1".to_string()],
                    acceptance_criteria: "Step 2 done".to_string(),
                    verify_command: None,
                    max_retries: 1,
                    mode: CoordinationMode::Fork,
                    retry_policy: RetryPolicy::default(),
                    capability: crate::multi_agent::SubagentCapability::Analyze,
                },
            ],
        }
    }

    #[test]
    fn start_run_creates_pending_run() {
        let mut executor = DagExecutor::new();
        let dag = sample_dag();
        let run = executor.start_run(&dag);
        assert_eq!(run.status, DagStatus::Pending);
        assert_eq!(run.node_statuses.len(), 2);
    }

    #[test]
    fn next_ready_returns_nodes_without_deps() {
        let mut executor = DagExecutor::new();
        let dag = sample_dag();
        executor.start_run(&dag);
        let ready = executor.next_ready(&dag);
        assert_eq!(ready, vec!["n1"]);
    }

    #[test]
    fn next_ready_returns_n2_after_n1_succeeds() {
        let mut executor = DagExecutor::new();
        let dag = sample_dag();
        executor.start_run(&dag);
        executor.mark_started("n1");
        executor.mark_succeeded("n1");
        let ready = executor.next_ready(&dag);
        assert_eq!(ready, vec!["n2"]);
    }

    #[test]
    fn mark_failed_skips_dependents() {
        let mut executor = DagExecutor::new();
        let dag = sample_dag();
        executor.start_run(&dag);
        executor.mark_failed(&dag, "n1");
        let run = executor.active_run().unwrap();
        assert_eq!(run.node_status("n1"), Some(DagNodeStatus::Failed));
        assert_eq!(run.node_status("n2"), Some(DagNodeStatus::Skipped));
        assert_eq!(run.status, DagStatus::Failed);
    }
}
