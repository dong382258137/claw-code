//! DAG scheduler (G8.10) — concurrent DAG execution with parallelism control.
//!
//! Unlike [`DagExecutor`](super::executor::DagExecutor) which processes nodes
//! sequentially, the scheduler dispatches ready nodes concurrently (up to a
//! configurable parallelism limit).

use super::executor::DagExecutor;
use crate::multi_agent::dag::types::{Dag, DagNodeId, DagRun};

/// Maximum number of nodes that can execute concurrently.
const DEFAULT_MAX_PARALLELISM: usize = 4;

/// Concurrent DAG scheduler.
///
/// Wraps [`DagExecutor`] and adds parallelism control.
/// Currently implements a synchronous polling model; async support
/// (via tokio channels) is planned for a future iteration.
#[derive(Debug, Clone)]
pub struct DagScheduler {
    executor: DagExecutor,
    max_parallelism: usize,
    /// Nodes currently in-flight.
    in_flight: Vec<DagNodeId>,
}

impl Default for DagScheduler {
    fn default() -> Self {
        Self {
            executor: DagExecutor::new(),
            max_parallelism: DEFAULT_MAX_PARALLELISM,
            in_flight: Vec::new(),
        }
    }
}

impl DagScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum number of concurrent nodes.
    #[must_use]
    pub fn with_max_parallelism(mut self, limit: usize) -> Self {
        self.max_parallelism = limit.max(1);
        self
    }

    /// Start a new DAG run.
    pub fn start_run(&mut self, dag: &Dag) -> DagRun {
        self.in_flight.clear();
        self.executor.start_run(dag)
    }

    /// Get the next batch of nodes ready to execute (up to `max_parallelism`).
    ///
    /// Returns node IDs that are ready (dependencies met, not terminal,
    /// not already in-flight).
    #[must_use]
    pub fn next_batch(&mut self, dag: &Dag) -> Vec<DagNodeId> {
        let available_slots = self.max_parallelism.saturating_sub(self.in_flight.len());
        if available_slots == 0 {
            return Vec::new();
        }

        let ready: Vec<DagNodeId> = self
            .executor
            .next_ready(dag)
            .into_iter()
            .filter(|id| !self.in_flight.contains(id))
            .take(available_slots)
            .collect();

        for id in &ready {
            self.in_flight.push(id.clone());
        }

        ready
    }

    /// Mark a node as completed and remove it from in-flight.
    pub fn complete_node(&mut self, dag: &Dag, node_id: &str, success: bool) {
        self.in_flight.retain(|id| id != node_id);
        if success {
            self.executor.mark_succeeded(node_id);
        } else {
            self.executor.mark_failed(dag, node_id);
        }
    }

    /// Get the current run status.
    #[must_use]
    pub fn status(&self) -> Option<&DagRun> {
        self.executor.active_run()
    }

    /// Check if all work is done (no in-flight, all nodes terminal).
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.in_flight.is_empty()
            && self
                .executor
                .active_run()
                .is_none_or(|run| run.is_terminal())
    }

    /// Cancel the run.
    pub fn cancel(&mut self) {
        self.in_flight.clear();
        self.executor.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_agent::dag::types::DagNode;

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
                },
                DagNode {
                    id: "n2".to_string(),
                    label: "Step 2".to_string(),
                    task: "Do step 2".to_string(),
                    depends_on: vec![],
                    acceptance_criteria: "Step 2 done".to_string(),
                    verify_command: None,
                    max_retries: 1,
                },
                DagNode {
                    id: "n3".to_string(),
                    label: "Step 3".to_string(),
                    task: "Do step 3".to_string(),
                    depends_on: vec!["n1".to_string()],
                    acceptance_criteria: "Step 3 done".to_string(),
                    verify_command: None,
                    max_retries: 1,
                },
            ],
        }
    }

    #[test]
    fn next_batch_respects_parallelism_limit() {
        let mut scheduler = DagScheduler::new().with_max_parallelism(2);
        let dag = sample_dag();
        scheduler.start_run(&dag);
        let batch = scheduler.next_batch(&dag);
        // n1 and n2 have no deps; n3 depends on n1
        assert_eq!(batch.len(), 2);
        assert!(batch.contains(&"n1".to_string()));
        assert!(batch.contains(&"n2".to_string()));
    }

    #[test]
    fn next_batch_empty_when_at_parallelism_limit() {
        let mut scheduler = DagScheduler::new().with_max_parallelism(1);
        let dag = sample_dag();
        scheduler.start_run(&dag);
        let first = scheduler.next_batch(&dag);
        assert_eq!(first.len(), 1);
        let second = scheduler.next_batch(&dag);
        assert!(second.is_empty());
    }

    #[test]
    fn complete_node_frees_slot_for_next() {
        let mut scheduler = DagScheduler::new().with_max_parallelism(1);
        let dag = sample_dag();
        scheduler.start_run(&dag);
        let first = scheduler.next_batch(&dag);
        scheduler.complete_node(&dag, &first[0], true);
        let second = scheduler.next_batch(&dag);
        assert!(!second.is_empty());
    }
}
