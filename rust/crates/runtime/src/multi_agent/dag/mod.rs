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
pub mod yaml_loader;

// v0.2 re-exports: petgraph-backed graph + async scheduler primitives.
pub use coordinator_executor::{CoordinatorExecutor, SubagentRunner};
pub use executor_trait::{NodeError, SubagentExecutor};
pub use scheduler::{DagScheduler, ProgressEvent};
pub use subagent_dispatcher::SubagentDispatcher;
pub use types::{
    Dag, DagError, DagGraph, DagId, DagNode, DagRunResult, DagStatus, FailFast, NodeAttempt,
    NodeResult, RetryPolicy, DEFAULT_MAX_PARALLELISM,
};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use types::{DagNodeStatus, DagRun};

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
    pub fn update_run_status(&self, run_id: &str, status: DagStatus) -> Result<(), String> {
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

    /// v3:向指定 DagRun 追加一条 retry 尝试记录。
    ///
    /// 由 [`DagScheduler::retry_failed`](super::scheduler::DagScheduler::retry_failed)
    /// 与 [`recover_skipped`](super::scheduler::DagScheduler::recover_skipped)
    /// 在每次重试完成后调用,把尝试结果写回原始 DagRun,从而保留完整的
    /// retry 历史轨迹。
    ///
    /// # Errors
    /// - `run not found: {run_id}` — 目标 DagRun 不存在。
    ///
    /// # Side effects
    /// - 若 `attempt.status == Succeeded`,对应节点的 `node_statuses` 会被
    ///   提升为 `Succeeded`(覆盖原始 `Failed` 状态)。
    /// - `retry_history` 追加新条目。
    pub fn record_retry_attempt(
        &self,
        run_id: &str,
        attempt: types::NodeAttempt,
    ) -> Result<(), String> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let run = runs
            .get_mut(run_id)
            .ok_or_else(|| format!("run not found: {run_id}"))?;
        run.record_retry_attempt(attempt);
        Ok(())
    }
}

// Re-export DagStore input types for tool registration.
pub use dag_input::{
    build_dag_from_define, DagDefineInput, DagNodeDefine, DagRunInput, DagStatusInput,
};

mod dag_input {
    use std::collections::HashSet;

    use serde::Deserialize;

    use crate::multi_agent::{CoordinationMode, SubagentCapability};

    use super::types::{Dag, DagNode, DagNodeId};

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

    /// `dag_define` 工具输入 — 模型定义 DAG 时的序列化结构。
    ///
    /// 字段名与 `DagNode` 保持一致(snake_case),可选字段带 `serde(default)`,
    /// 因此模型可省略,由 [`build_dag_from_define`] 填充默认值。
    #[derive(Debug, Deserialize)]
    pub struct DagDefineInput {
        /// 唯一 DAG 标识。
        pub dag_id: String,
        /// 人类可读名称(缺省回退为 dag_id)。
        #[serde(default)]
        pub name: String,
        /// 节点列表(至少一个)。
        pub nodes: Vec<DagNodeDefine>,
    }

    /// `dag_define` 工具中单个节点的定义。
    #[derive(Debug, Deserialize)]
    pub struct DagNodeDefine {
        /// 节点 ID(进程内唯一)。
        pub id: String,
        /// 人类可读标签(缺省回退为 id)。
        #[serde(default)]
        pub label: String,
        /// 子智能体的任务描述(prompt)。
        pub task: String,
        /// 本节点开始前必须完成的节点 ID 列表。
        #[serde(default)]
        pub depends_on: Vec<DagNodeId>,
        /// 验收标准(缺省生成兜底文案)。
        #[serde(default)]
        pub acceptance_criteria: String,
        /// 可选 verify 命令(如 `cargo test -p my_crate`)。
        #[serde(default)]
        pub verify_command: Option<String>,
        /// 失败前最大重试次数(缺省 2)。
        #[serde(default)]
        pub max_retries: Option<u32>,
        /// 派发模式:fork / teammate / worktree(缺省 fork)。
        #[serde(default)]
        pub mode: Option<CoordinationMode>,
        /// 能力分级:analyze / read-only / execute(缺省 execute,保证节点可用工具)。
        #[serde(default)]
        pub capability: Option<SubagentCapability>,
    }

    /// 校验并构造 `Dag`,供 `dag_define` 工具调用。
    ///
    /// 校验规则:
    /// 1. `dag_id` 非空。
    /// 2. `nodes` 非空。
    /// 3. 每个 `depends_on` 引用必须存在于节点集合(环检测由调用方经
    ///    [`DagGraph::validate_acyclic`] 完成)。
    ///
    /// 缺省值:label/id、acceptance_criteria 兜底文案、max_retries=2、
    /// mode=Fork、capability=Execute。
    pub fn build_dag_from_define(input: DagDefineInput) -> Result<Dag, String> {
        let id = input.dag_id.trim().to_string();
        if id.is_empty() {
            return Err("dag_id must not be empty".to_string());
        }
        if input.nodes.is_empty() {
            return Err(format!("dag '{id}': nodes must not be empty"));
        }

        let ids: HashSet<&str> = input.nodes.iter().map(|n| n.id.as_str()).collect();
        for n in &input.nodes {
            for dep in &n.depends_on {
                if !ids.contains(dep.as_str()) {
                    return Err(format!(
                        "dag '{id}': node '{}' depends on unknown node '{dep}'",
                        n.id
                    ));
                }
            }
        }

        let name = if input.name.trim().is_empty() {
            id.clone()
        } else {
            input.name.trim().to_string()
        };

        let nodes = input
            .nodes
            .into_iter()
            .map(|n| DagNode {
                id: n.id.clone(),
                label: if n.label.trim().is_empty() {
                    n.id.clone()
                } else {
                    n.label
                },
                task: n.task,
                depends_on: n.depends_on,
                acceptance_criteria: if n.acceptance_criteria.trim().is_empty() {
                    format!("Verify node '{}' completes successfully", n.id)
                } else {
                    n.acceptance_criteria
                },
                verify_command: n.verify_command.filter(|c| !c.trim().is_empty()),
                max_retries: n.max_retries.unwrap_or(2),
                mode: n.mode.unwrap_or(CoordinationMode::Fork),
                retry_policy: Default::default(),
                capability: n.capability.unwrap_or(SubagentCapability::Execute),
            })
            .collect();

        Ok(Dag { id, name, nodes })
    }
}

#[cfg(test)]
mod dag_input_tests {
    use super::*;

    #[test]
    fn build_dag_from_define_basic_dag() {
        let input = DagDefineInput {
            dag_id: "test-dag".to_string(),
            name: String::new(),
            nodes: vec![
                DagNodeDefine {
                    id: "analyze".to_string(),
                    label: String::new(),
                    task: "Analyze the code".to_string(),
                    depends_on: vec![],
                    acceptance_criteria: String::new(),
                    verify_command: None,
                    max_retries: None,
                    mode: None,
                    capability: None,
                },
                DagNodeDefine {
                    id: "implement".to_string(),
                    label: "Implement".to_string(),
                    task: "Implement the fix".to_string(),
                    depends_on: vec!["analyze".to_string()],
                    acceptance_criteria: "Tests pass".to_string(),
                    verify_command: Some("cargo test".to_string()),
                    max_retries: Some(3),
                    mode: None,
                    capability: Some(crate::multi_agent::SubagentCapability::Execute),
                },
            ],
        };
        let dag = build_dag_from_define(input).expect("should build valid dag");
        assert_eq!(dag.id, "test-dag");
        assert_eq!(dag.name, "test-dag"); // 缺省 name 回退到 id
        assert_eq!(dag.nodes.len(), 2);
        // analyze node:默认值
        assert_eq!(dag.nodes[0].id, "analyze");
        assert_eq!(dag.nodes[0].label, "analyze"); // 空 label 回退到 id
        assert!(dag.nodes[0].depends_on.is_empty());
        assert!(dag.nodes[0]
            .acceptance_criteria
            .contains("Verify node 'analyze'"));
        assert_eq!(dag.nodes[0].max_retries, 2); // 默认 2
        assert_eq!(
            dag.nodes[0].mode,
            crate::multi_agent::CoordinationMode::Fork
        );
        assert_eq!(
            dag.nodes[0].capability,
            crate::multi_agent::SubagentCapability::Execute
        );
        // implement node:显式值
        assert_eq!(dag.nodes[1].id, "implement");
        assert_eq!(dag.nodes[1].label, "Implement");
        assert_eq!(dag.nodes[1].depends_on, vec!["analyze"]);
        assert_eq!(dag.nodes[1].acceptance_criteria, "Tests pass");
        assert_eq!(dag.nodes[1].verify_command.as_deref(), Some("cargo test"));
        assert_eq!(dag.nodes[1].max_retries, 3);
    }

    #[test]
    fn build_dag_from_define_empty_id_rejected() {
        let input = DagDefineInput {
            dag_id: "  ".to_string(),
            name: String::new(),
            nodes: vec![DagNodeDefine {
                id: "n1".to_string(),
                label: String::new(),
                task: "task".to_string(),
                depends_on: vec![],
                acceptance_criteria: String::new(),
                verify_command: None,
                max_retries: None,
                mode: None,
                capability: None,
            }],
        };
        let err = build_dag_from_define(input).expect_err("should reject empty dag_id");
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn build_dag_from_define_empty_nodes_rejected() {
        let input = DagDefineInput {
            dag_id: "test".to_string(),
            name: String::new(),
            nodes: vec![],
        };
        let err = build_dag_from_define(input).expect_err("should reject empty nodes");
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn build_dag_from_define_unknown_dependency_rejected() {
        let input = DagDefineInput {
            dag_id: "test".to_string(),
            name: String::new(),
            nodes: vec![DagNodeDefine {
                id: "n1".to_string(),
                label: String::new(),
                task: "task".to_string(),
                depends_on: vec!["nonexistent".to_string()],
                acceptance_criteria: String::new(),
                verify_command: None,
                max_retries: None,
                mode: None,
                capability: None,
            }],
        };
        let err = build_dag_from_define(input).expect_err("should reject unknown dependency");
        assert!(err.contains("unknown node 'nonexistent'"));
    }

    #[test]
    fn build_dag_from_define_explicit_name_used() {
        let input = DagDefineInput {
            dag_id: "test".to_string(),
            name: "My DAG".to_string(),
            nodes: vec![DagNodeDefine {
                id: "n1".to_string(),
                label: "Node 1".to_string(),
                task: "task".to_string(),
                depends_on: vec![],
                acceptance_criteria: String::new(),
                verify_command: None,
                max_retries: None,
                mode: None,
                capability: None,
            }],
        };
        let dag = build_dag_from_define(input).expect("should build");
        assert_eq!(dag.name, "My DAG");
    }
}
