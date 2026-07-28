//! DAG (Directed Acyclic Graph) orchestration subsystem (G8.10).
//!
//! Provides dependency-aware execution of work items as a directed acyclic graph.
//! Nodes are dispatched as sub-agents via [`SubagentCoordinator`].
//!
//! Module structure:
//! - [`types`]: Data model (Dag, DagNode, DagRun, status enums) + v0.2
//!   petgraph-backed [`DagGraph`] with SCC cycle detection and ready-node
//!   computation.
//! - [`executor`]: Sequential execution in topological order (v0.1, retained
//!   for `dag_status` tool compat).
//! - [`executor_trait`]: v0.2 [`SubagentExecutor`] trait abstracting how a
//!   single node's subagent is dispatched.
//! - [`coordinator_executor`]: v0.2 [`CoordinatorExecutor`] bridging
//!   [`MultiAgentCoordinator`] to the [`SubagentExecutor`] trait.
//! - [`scheduler`]: v0.2 async concurrent scheduler (JoinSet +
//!   CancellationToken, FailFast, retry-with-backoff, DagRun bridging).
//! - [`status`]: Human-readable status rendering.

pub mod coordinator_executor;
pub mod executor;
pub mod executor_trait;
pub mod scheduler;
pub mod status;
pub mod subagent_dispatcher;
pub mod types;

// v0.2 re-exports: petgraph-backed graph + async scheduler primitives.
pub use coordinator_executor::{CoordinatorExecutor, SubagentRunner};
pub use executor_trait::{NodeError, SubagentExecutor};
pub use scheduler::{DagScheduler, ProgressEvent};
pub use subagent_dispatcher::SubagentDispatcher;
pub use types::{DagError, DagGraph, DagId, DagNode, DagRunResult, DagStatus, FailFast, NodeResult, RetryPolicy, DEFAULT_MAX_PARALLELISM};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use types::{Dag, DagNodeStatus, DagRun};

/// A thread-safe store for DAG definitions and their runs (G8.11).
#[derive(Debug, Clone, Default)]
pub struct DagStore {
    dags: Arc<Mutex<HashMap<String, Dag>>>,
    runs: Arc<Mutex<HashMap<String, DagRun>>>,
}

impl DagStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new DAG definition.
    pub fn create_dag(&self, dag: Dag) -> Result<String, String> {
        let id = dag.id.clone();
        let mut dags = self.dags.lock().unwrap_or_else(|e| e.into_inner());
        if dags.contains_key(&id) {
            return Err(format!("DAG with id '{}' already exists", id));
        }
        dags.insert(id.clone(), dag);
        Ok(id)
    }

    /// Start a new run for a DAG.
    pub fn start_run(&self, dag_id: &str) -> Result<DagRun, String> {
        let dags = self.dags.lock().unwrap_or_else(|e| e.into_inner());
        let dag = dags
            .get(dag_id)
            .ok_or_else(|| format!("DAG not found: {dag_id}"))?
            .clone();
        drop(dags);

        let run = DagRun::new(&dag);
        let run_id = run.id.clone();
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        runs.insert(run_id, run.clone());
        Ok(run)
    }

    /// Get a DAG run by ID.
    #[must_use]
    pub fn get_run(&self, run_id: &str) -> Option<DagRun> {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned()
    }

    /// List all DAG definitions.
    #[must_use]
    pub fn list_dags(&self) -> Vec<Dag> {
        self.dags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// List all runs.
    #[must_use]
    pub fn list_runs(&self) -> Vec<DagRun> {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Get the DAG definition for a run.
    #[must_use]
    pub fn dag_for_run(&self, run_id: &str) -> Option<Dag> {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let run = runs.get(run_id)?;
        let dags = self.dags.lock().unwrap_or_else(|e| e.into_inner());
        dags.get(&run.dag_id).cloned()
    }

    /// Update a node's status within a run (v0.2 bridge).
    ///
    /// Called by the async [`DagScheduler`](super::scheduler::DagScheduler)
    /// to propagate per-node progress into the persistent [`DagRun`] so that
    /// `dag_status` tool queries reflect async execution.
    ///
    /// # Errors
    /// - `run not found: {run_id}` — the run was never started or was evicted.
    pub fn update_node_status(
        &self,
        run_id: &str,
        node_id: &str,
        status: DagNodeStatus,
    ) -> Result<(), String> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let run = runs
            .get_mut(run_id)
            .ok_or_else(|| format!("run not found: {run_id}"))?;
        run.set_node_status(node_id, status);
        Ok(())
    }

    /// Update the overall run status (v0.2 bridge).
    ///
    /// Side effects:
    /// - Transitioning to [`DagStatus::Running`] stamps `started_at` if unset.
    /// - Transitioning to a terminal status (`Completed` / `Failed` /
    ///   `Cancelled`) stamps `completed_at`.
    ///
    /// # Errors
    /// - `run not found: {run_id}` — the run was never started or was evicted.
    pub fn update_run_status(
        &self,
        run_id: &str,
        status: DagStatus,
    ) -> Result<(), String> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let run = runs
            .get_mut(run_id)
            .ok_or_else(|| format!("run not found: {run_id}"))?;
        run.status = status;
        if status == DagStatus::Running && run.started_at.is_none() {
            run.started_at = Some(types::now_secs());
        }
        if matches!(
            status,
            DagStatus::Completed | DagStatus::Failed | DagStatus::Cancelled
        ) {
            run.completed_at = Some(types::now_secs());
        }
        Ok(())
    }
}

// Re-export DagStore input types for tool registration.
pub use dag_input::{DagRunInput, DagStatusInput};

mod dag_input {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct DagRunInput {
        pub dag_id: String,
        #[serde(default)]
        pub action: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct DagStatusInput {
        pub run_id: String,
    }
}
