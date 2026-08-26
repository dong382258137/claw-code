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
use crate::multi_agent::{MultiAgentCoordinator, SubagentCapability};

/// Type alias for the runner function that executes a subagent's LLM turn.
///
/// The function receives `(subagent_id, task, capability)` and returns the result
/// string (e.g. a `result_ref` path written to `.claw/subagents/{id}.md`)
/// on success, or an error message on failure.
///
/// The returned future is `Pin<Box<...>>` to keep the trait object erasure
/// simple — the runner is stored as a single `Arc<dyn Fn ... + Send + Sync>`
/// regardless of the concrete closure type.
pub type SubagentRunner = Arc<
    dyn Fn(
            String,
            String,
            SubagentCapability,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
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
    /// use std::future::Future;
    /// use std::pin::Pin;
    /// use std::sync::Arc;
    /// use runtime::multi_agent::{MultiAgentCoordinator, SubagentCapability};
    /// use runtime::multi_agent::dag::coordinator_executor::CoordinatorExecutor;
    ///
    /// let coordinator = Arc::new(MultiAgentCoordinator::new());
    /// let runner: Arc<
    ///     dyn Fn(String, String, SubagentCapability) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
    ///         + Send + Sync,
    /// > = Arc::new(|id, _task, _cap| {
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
    async fn execute(&self, node: &DagNode, attempt: u32) -> Result<NodeResult, NodeError> {
        // 知识新鲜度门控(事前评估):
        // - attempt=0(首次):正常评估 freshness,Novel 任务触发调研
        // - attempt>0(重试):急则治标旁路,跳过调研直接重试
        //   (gate_task 内部 derive_urgent_from_attempt 处理)
        let gated = crate::knowledge_freshness::gate_task(&node.task, attempt).await;

        // 1. Register the subagent in the coordinator's state machine.
        let subagent_id = self.coordinator.spawn(&node.label, &node.task, node.mode);

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
        let capability = node.capability;
        let handle = coordinator
            .execute_async(&subagent_id, move |id, task| {
                let runner = runner.clone();
                Box::pin(async move { (runner)(id, task, capability).await })
            })
            .map_err(NodeError::ExecutionFailed)?;

        // 4. Await the JoinHandle. A JoinError means the spawned task
        //    panicked — surface as ExecutionFailed.
        let result = handle
            .await
            .map_err(|e| NodeError::ExecutionFailed(format!("subagent join error: {e}")))?;

        // 5. Map the runner's Result<String, String> to NodeResult / NodeError.
        //    P0-d:并行路径补齐 validation gate + checkpoint,与单路径
        //    (`execute_dispatch_subagent_async` §4.4-4.6)行为对齐:
        //    - turn 成功 → complete() → save_checkpoint() → validate()
        //    - validation 通过 → 返回 NodeResult(正常终态)
        //    - validation 失败 → 标记 Failed,返回 ExecutionFailed
        //      (scheduler 按 max_retries 决定是否重试)
        //    - turn 失败 → execute_async 已处理 fail 转换,直接返回错误
        match result {
            Ok(summary) => {
                // turn 成功 → 标记 Completed
                let _ = coordinator.complete(&subagent_id, &summary);

                // checkpoint:保存当前状态(借鉴 LangGraph durable execution)
                // 即使后续 validation 失败,checkpoint 也已保存,可用于恢复
                let _ = coordinator.save_checkpoint(&subagent_id);

                // validation gate:调用所有注册的 gate
                match coordinator.validate(&subagent_id) {
                    Ok(()) => Ok(NodeResult {
                        node_id: node.id.clone(),
                        summary,
                        artifact_path: None,
                        gated: Some(gated),
                    }),
                    Err(ve) => {
                        // validation 失败 — 标记 subagent 为 Failed
                        let _ = coordinator.fail(&subagent_id, ve.to_string());

                        // 返回错误,scheduler 按 max_retries 决定是否重试。
                        // 错误消息中标记 retryable/fatal,方便诊断。
                        Err(NodeError::ExecutionFailed(format!(
                            "validation failed (retryable={}): {ve}",
                            ve.retryable
                        )))
                    }
                }
            }
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
    use crate::multi_agent::dag::types::RetryPolicy;
    use crate::multi_agent::dag::{DagError, DagGraph, DagScheduler, FailFast};
    use crate::multi_agent::CoordinationMode;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;
    use tempfile::TempDir;

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
            capability: SubagentCapability::Analyze,
        }
    }

    #[tokio::test]
    async fn execute_without_runner_returns_config_error() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator.clone());
        let node = sample_node();
        let err = executor
            .execute(&node, 0)
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
        let _ = executor.execute(&node, 0).await;
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
        let runner: SubagentRunner = Arc::new(|_id, _task, _cap| {
            Box::pin(async { Ok(".claw/subagents/fake.md".to_string()) })
        });
        let executor = CoordinatorExecutor::new(coordinator).with_runner(runner);
        let node = sample_node();
        let result = executor
            .execute(&node, 0)
            .await
            .expect("runner should succeed");
        assert_eq!(result.node_id, "n1");
        assert_eq!(result.summary, ".claw/subagents/fake.md");
    }

    #[tokio::test]
    async fn execute_with_failing_runner_returns_execution_failed() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let runner: SubagentRunner =
            Arc::new(|_id, _task, _cap| Box::pin(async { Err("llm error".to_string()) }));
        let executor = CoordinatorExecutor::new(coordinator).with_runner(runner);
        let node = sample_node();
        let err = executor
            .execute(&node, 0)
            .await
            .expect_err("runner should fail");
        assert!(matches!(err, NodeError::ExecutionFailed(_)));
        let msg = format!("{err}");
        assert!(msg.contains("llm error"), "msg was: {msg}");
    }

    #[tokio::test]
    async fn execute_with_runner_transitions_coordinator_to_completed() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let runner: SubagentRunner =
            Arc::new(|_id, _task, _cap| Box::pin(async { Ok("result".to_string()) }));
        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let node = sample_node();
        executor.execute(&node, 0).await.expect("should succeed");
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
            Arc::new(|_id, _task, _cap| Box::pin(async { Err("boom".to_string()) }));
        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let node = sample_node();
        let _ = executor.execute(&node, 0).await;
        let agents = coordinator.list();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, crate::multi_agent::SubagentStatus::Failed);
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

    // ========================================================================
    // Production-path tests: inject a more realistic SubagentRunner (mirroring
    // SubagentDispatcher behaviour) and verify end-to-end execution via
    // DagScheduler.
    // ========================================================================

    /// Helper: build a DagNode with the given id and dependencies.
    fn dag_node(id: &str, deps: &[&str]) -> DagNode {
        DagNode {
            id: id.to_string(),
            label: id.to_string(),
            task: format!("task-{id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            acceptance_criteria: "ok".to_string(),
            verify_command: None,
            max_retries: 0,
            mode: CoordinationMode::Fork,
            retry_policy: RetryPolicy::default(),
            capability: SubagentCapability::Analyze,
        }
    }

    /// Test (a): realistic runner with simulated LLM latency returns
    /// result_ref paths for every node in a small linear DAG.
    ///
    /// Verifies that CoordinatorExecutor + DagScheduler end-to-end execution
    /// works when the runner mirrors a real LLM dispatch (sleep + result_ref
    /// path), and that the runner's return value flows through to NodeResult.
    #[tokio::test]
    async fn coordinator_executor_with_realistic_runner_executes_dag_node() {
        // Linear DAG: n1 -> n2 -> n3
        let mut graph = DagGraph::new("realistic");
        graph.add_node(dag_node("n1", &[]));
        graph.add_node(dag_node("n2", &["n1"]));
        graph.add_node(dag_node("n3", &["n2"]));
        graph
            .add_edge(&"n1".to_string(), &"n2".to_string())
            .unwrap();
        graph
            .add_edge(&"n2".to_string(), &"n3".to_string())
            .unwrap();

        // Track dispatched (subagent_id, task) pairs to verify the runner was
        // actually invoked and that summaries match the dispatched ids.
        let dispatched: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = dispatched.clone();

        // Realistic runner: simulates LLM latency, then returns the result_ref
        // path (mirroring SubagentDispatcher::dispatch's return format).
        let runner: SubagentRunner =
            Arc::new(move |id: String, task: String, _cap: SubagentCapability| {
                let dispatched = dispatched_clone.clone();
                Box::pin(async move {
                    // Simulate LLM call latency.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    dispatched
                        .lock()
                        .expect("dispatched poisoned")
                        .push((id.clone(), task));
                    Ok(format!(".claw/subagents/{id}.md"))
                })
            });

        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator).with_runner(runner);
        let executor: Arc<dyn SubagentExecutor> = Arc::new(executor);

        let scheduler = DagScheduler::new(graph, executor);
        let results = scheduler
            .run()
            .await
            .expect("realistic DAG should complete");

        assert_eq!(results.len(), 3, "all 3 nodes should succeed");

        // Build the set of expected result_ref paths from dispatched ids.
        let dispatched = dispatched.lock().expect("dispatched poisoned");
        assert_eq!(
            dispatched.len(),
            3,
            "runner should have been called 3 times, got {}",
            dispatched.len()
        );
        let expected_paths: Vec<String> = dispatched
            .iter()
            .map(|(id, _)| format!(".claw/subagents/{id}.md"))
            .collect();

        // Every result summary should be one of the expected result_ref paths.
        for result in &results {
            assert!(
                expected_paths.contains(&result.summary),
                "summary {} should be one of {:?}",
                result.summary,
                expected_paths
            );
        }

        // All 3 subagent_ids should be distinct (distinct paths).
        let unique_paths: std::collections::HashSet<&str> =
            results.iter().map(|r| r.summary.as_str()).collect();
        assert_eq!(
            unique_paths.len(),
            3,
            "all result_ref paths should be distinct"
        );
    }

    /// Test (b): a runner that fails for a specific subagent_id propagates the
    /// failure through DagScheduler as DagError::NodeFailed.
    ///
    /// Verifies that when the runner returns Err, CoordinatorExecutor maps it
    /// to NodeError::ExecutionFailed, the scheduler applies FailFast (with
    /// max_retries=0), and the coordinator's subagent state reflects the
    /// failure.
    #[tokio::test]
    async fn coordinator_executor_with_failing_runner_reports_node_failure() {
        // Linear DAG: n1 -> n2. n1 will fail (subagent-1), so n2 never runs.
        let mut graph = DagGraph::new("failing");
        graph.add_node(dag_node("n1", &[]));
        graph.add_node(dag_node("n2", &["n1"]));
        graph
            .add_edge(&"n1".to_string(), &"n2".to_string())
            .unwrap();

        // Runner that fails for the first spawned subagent ("subagent-1").
        // Since n1 is the only root, it will be spawned first.
        let runner: SubagentRunner =
            Arc::new(|id: String, _task: String, _cap: SubagentCapability| {
                Box::pin(async move {
                    if id == "subagent-1" {
                        return Err(format!("simulated LLM failure for {id}"));
                    }
                    Ok(format!(".claw/subagents/{id}.md"))
                })
            });

        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let executor: Arc<dyn SubagentExecutor> = Arc::new(executor);

        let scheduler = DagScheduler::new(graph, executor).with_fail_fast(FailFast::On);
        let err = scheduler.run().await.expect_err("DAG should fail");
        match err {
            DagError::NodeFailed(node_id) => {
                assert_eq!(
                    node_id, "n1",
                    "expected n1 to be the failing node, got {node_id}"
                );
            }
            other => panic!("expected DagError::NodeFailed, got {other:?}"),
        }

        // The coordinator's subagent state should reflect the failure.
        let agents = coordinator.list();
        assert!(
            agents
                .iter()
                .any(|a| a.status == crate::multi_agent::SubagentStatus::Failed),
            "expected at least one Failed subagent, got {:?}",
            agents.iter().map(|a| a.status).collect::<Vec<_>>()
        );
    }

    /// Test (c): a runner that mirrors SubagentDispatcher's file-writing
    /// behaviour creates the result file on disk and returns a matching path.
    ///
    /// Verifies that the SubagentDispatcher pattern (write file to
    /// {workspace_root}/.claw/subagents/{id}.md, return relative path) works
    /// end-to-end through CoordinatorExecutor::execute, and that the returned
    /// summary matches the actual file location.
    #[tokio::test]
    async fn coordinator_executor_with_subagent_dispatcher_pattern() {
        // Use a temp dir as the workspace_root, mirroring SubagentDispatcher.
        let tmp = TempDir::new().expect("temp dir creation failed");
        let workspace_root: PathBuf = tmp.path().to_path_buf();

        // Track dispatched subagent_ids for later verification.
        let dispatched_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = dispatched_ids.clone();
        let workspace_for_runner = workspace_root.clone();

        // Runner that mirrors SubagentDispatcher::dispatch_impl:
        // 1. Creates {workspace_root}/.claw/subagents/ dir
        // 2. Writes {workspace_root}/.claw/subagents/{id}.md (via tmp + rename)
        // 3. Returns the relative result_ref path ".claw/subagents/{id}.md"
        let runner: SubagentRunner =
            Arc::new(move |id: String, task: String, _cap: SubagentCapability| {
                let workspace = workspace_for_runner.clone();
                let dispatched = dispatched_clone.clone();
                Box::pin(async move {
                    dispatched
                        .lock()
                        .expect("dispatched poisoned")
                        .push(id.clone());

                    let subagents_dir = workspace.join(".claw").join("subagents");
                    std::fs::create_dir_all(&subagents_dir)
                        .map_err(|e| format!("failed to create subagents dir: {e}"))?;
                    let result_path = subagents_dir.join(format!("{id}.md"));
                    let tmp_path = subagents_dir.join(format!("{id}.md.tmp"));

                    let file_content = format!(
                        "# Subagent Result: {id}\n\n\
                     **Task:** {task}\n\n\
                     Result content."
                    );

                    std::fs::write(&tmp_path, &file_content)
                        .map_err(|e| format!("failed to write tmp file: {e}"))?;
                    std::fs::rename(&tmp_path, &result_path)
                        .map_err(|e| format!("failed to rename tmp file: {e}"))?;

                    Ok(format!(".claw/subagents/{id}.md"))
                })
            });

        let coordinator = Arc::new(MultiAgentCoordinator::new());
        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let node = sample_node();
        let result = executor
            .execute(&node, 0)
            .await
            .expect("dispatcher-style runner should succeed");

        assert_eq!(result.node_id, "n1");

        // Inspect which subagent_id was actually dispatched.
        let dispatched = dispatched_ids.lock().expect("dispatched poisoned");
        assert_eq!(
            dispatched.len(),
            1,
            "exactly one subagent should have been dispatched"
        );
        let dispatched_id = dispatched[0].clone();
        drop(dispatched);

        // The returned summary should match the SubagentDispatcher's format.
        let expected_summary = format!(".claw/subagents/{dispatched_id}.md");
        assert_eq!(
            result.summary, expected_summary,
            "returned summary should match the dispatched subagent's result_ref path"
        );

        // The file should actually exist on disk at the workspace_root.
        let file_path = workspace_root
            .join(".claw")
            .join("subagents")
            .join(format!("{dispatched_id}.md"));
        assert!(
            file_path.exists(),
            "expected result file to exist at {}",
            file_path.display()
        );

        // Verify file contents to ensure the runner wrote meaningful data.
        let content =
            std::fs::read_to_string(&file_path).expect("should be able to read the result file");
        assert!(
            content.contains(&dispatched_id),
            "file content should mention the subagent id"
        );
        assert!(
            content.contains("echo hello"),
            "file content should mention the task"
        );

        // The coordinator should have transitioned the subagent to Completed.
        let agents = coordinator.list();
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].status,
            crate::multi_agent::SubagentStatus::Completed
        );
    }

    // ===== P0-d:并行路径 validation gate + checkpoint 测试 =====

    /// P0-d:注册 validation gate 后,即使 runner 成功返回,validation 失败
    /// 也应导致 execute 返回 Err。
    ///
    /// 验证 CoordinatorExecutor::execute 在 runner Ok 后调用了
    /// complete → checkpoint → validate 链路。
    #[tokio::test]
    async fn execute_with_validation_gate_failure_returns_error() {
        use crate::multi_agent::validation::{ValidationContext, ValidationError, ValidationGate};

        /// 总是失败的 gate(模拟编译失败等可重试验证错误)
        struct AlwaysFailGate;
        impl ValidationGate for AlwaysFailGate {
            fn validate(&self, _ctx: &ValidationContext) -> Result<(), ValidationError> {
                Err(ValidationError {
                    message: "simulated validation failure".to_string(),
                    retryable: true,
                })
            }
            fn name(&self) -> &'static str {
                "always-fail"
            }
        }

        let coordinator = Arc::new(MultiAgentCoordinator::new());
        coordinator.add_validation_gate(Box::new(AlwaysFailGate));

        // runner 成功返回(模拟 LLM turn 成功)
        let runner: SubagentRunner =
            Arc::new(|id: String, _task: String, _cap: SubagentCapability| {
                Box::pin(async move { Ok(format!(".claw/subagents/{id}.md")) })
            });

        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let executor: Arc<dyn SubagentExecutor> = Arc::new(executor);

        // 单节点 DAG
        let mut g = DagGraph::new("validation-test");
        g.add_node(dag_node("n1", &[]));

        // FailFast::On + max_retries=0:第一次 validation 失败即整体失败
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::On);
        let err = scheduler.run().await.expect_err("validation should fail");
        assert!(
            matches!(err, DagError::NodeFailed(ref id) if id == "n1"),
            "expected NodeFailed(n1) from validation failure, got {err:?}"
        );

        // coordinator 中应有 Failed 状态的 subagent
        let agents = coordinator.list();
        assert!(
            agents
                .iter()
                .any(|a| a.status == crate::multi_agent::SubagentStatus::Failed),
            "expected Failed subagent after validation failure, got {:?}",
            agents.iter().map(|a| a.status).collect::<Vec<_>>()
        );
    }

    /// P0-d:无 validation gate 时,行为与之前一致(runner Ok → Completed)。
    ///
    /// 确保新增的 complete → validate 链路在无 gate 时不引入回归。
    #[tokio::test]
    async fn execute_without_validation_gate_succeeds() {
        let coordinator = Arc::new(MultiAgentCoordinator::new());
        // 不注册任何 gate

        let runner: SubagentRunner =
            Arc::new(|id: String, _task: String, _cap: SubagentCapability| {
                Box::pin(async move { Ok(format!(".claw/subagents/{id}.md")) })
            });

        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let executor: Arc<dyn SubagentExecutor> = Arc::new(executor);

        let mut g = DagGraph::new("no-validation");
        g.add_node(dag_node("n1", &[]));

        let scheduler = DagScheduler::new(g, executor);
        let results = scheduler.run().await.expect("should succeed");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node_id, "n1");

        // subagent 应为 Completed 且 validated=true
        let agents = coordinator.list();
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].status,
            crate::multi_agent::SubagentStatus::Completed
        );
        assert!(
            agents[0].validated,
            "subagent should be validated when no gates fail"
        );
    }

    /// P0-d:validation 失败后 scheduler 按 max_retries 重试,重试成功后
    /// 整体应返回 Ok(验证 retry 机制与 validation gate 协同工作)。
    #[tokio::test]
    async fn validation_failure_triggers_retry_and_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};

        use crate::multi_agent::validation::{ValidationContext, ValidationError, ValidationGate};

        /// 前 N 次失败,之后成功的 gate(模拟修复后编译通过)
        struct FailNTimesGate {
            fail_n: u32,
            count: AtomicU32,
        }
        impl ValidationGate for FailNTimesGate {
            fn validate(&self, _ctx: &ValidationContext) -> Result<(), ValidationError> {
                let n = self.count.fetch_add(1, Ordering::SeqCst);
                if n < self.fail_n {
                    return Err(ValidationError {
                        message: format!("validation fail #{}", n + 1),
                        retryable: true,
                    });
                }
                Ok(())
            }
            fn name(&self) -> &'static str {
                "fail-n-times"
            }
        }

        let coordinator = Arc::new(MultiAgentCoordinator::new());
        // gate 第 1 次失败,第 2 次成功(配合 max_retries=1)
        coordinator.add_validation_gate(Box::new(FailNTimesGate {
            fail_n: 1,
            count: AtomicU32::new(0),
        }));

        let runner: SubagentRunner =
            Arc::new(|id: String, _task: String, _cap: SubagentCapability| {
                Box::pin(async move { Ok(format!(".claw/subagents/{id}.md")) })
            });

        let executor = CoordinatorExecutor::new(coordinator.clone()).with_runner(runner);
        let executor: Arc<dyn SubagentExecutor> = Arc::new(executor);

        // max_retries=1:允许 1 次重试
        let mut g = DagGraph::new("retry-validation");
        g.add_node(crate::multi_agent::dag::types::DagNode {
            id: "n1".to_string(),
            label: "Retry".to_string(),
            task: "task with validation".to_string(),
            depends_on: vec![],
            acceptance_criteria: String::new(),
            verify_command: None,
            max_retries: 1,
            mode: crate::multi_agent::CoordinationMode::Fork,
            retry_policy: RetryPolicy::default(),
            capability: crate::multi_agent::SubagentCapability::Analyze,
        });

        // FailFast::Off:即使重试后仍失败,也返回 Ok(DagRunResult)
        // 但这里第 2 次 validation 会成功,所以 run() 返回 Ok(Vec<NodeResult>)
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::Off);
        let results = scheduler.run().await.expect("retry should succeed");

        assert_eq!(results.len(), 1, "n1 should succeed after retry");
        assert_eq!(results[0].node_id, "n1");
    }
}
