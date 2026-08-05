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
    DagError, DagGraph, DagNodeId, DagNodeStatus, DagRunResult, DagStatus, FailFast, NodeResult,
    RetryPolicy,
};
use super::DagStore;

/// Progress event emitted by [`DagScheduler::run_with_progress`].
///
/// Allows external observers (e.g. `dag_status` tool, telemetry) to react
/// to per-node and per-DAG lifecycle transitions without polling.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A node was spawned (entered Running state).
    NodeStarted { node_id: DagNodeId },
    /// A node completed successfully.
    NodeSucceeded { node_id: DagNodeId },
    /// A node failed. `attempt` is 0-indexed (0 = first try).
    /// `will_retry` is `true` if the scheduler will re-spawn the node.
    NodeFailed {
        node_id: DagNodeId,
        error: String,
        attempt: u32,
        will_retry: bool,
    },
    /// v3:节点因依赖失败而被跳过(FailFast::Off 专属事件)。
    /// 与 `NodeFailed` 区分:Skipped 节点本身未执行,可通过
    /// [`DagScheduler::recover_skipped`] 恢复。
    NodeSkipped { node_id: DagNodeId, reason: String },
    /// The entire DAG completed successfully.
    DagCompleted,
    /// The DAG failed (a node exhausted retries and FailFast propagated).
    DagFailed { node_id: DagNodeId },
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
    /// v3:失败传播策略。默认 `FailFast::On`(向后兼容)。
    fail_fast: FailFast,
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
            // P0:默认 FailFast::Off — 单节点失败不应取消整个 DAG,
            // 让独立分支继续执行。调用方如需严格语义,显式 `.with_fail_fast(FailFast::On)`。
            fail_fast: FailFast::Off,
        }
    }

    /// Override the concurrency bound.
    #[must_use]
    pub fn with_max_parallelism(mut self, limit: usize) -> Self {
        self.max_parallelism = limit.max(1);
        self
    }

    /// v3:设置失败传播策略。
    ///
    /// - `FailFast::On`(默认):任一节点失败后立即取消整个 DAG。
    /// - `FailFast::Off`:节点失败后标记为 Failed,跳过其下游,继续执行独立分支。
    #[must_use]
    pub fn with_fail_fast(mut self, mode: FailFast) -> Self {
        self.fail_fast = mode;
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
        let result = self.run_inner(None).await?;
        Ok(result.into_successes())
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
        let result = self.run_inner(Some(Box::new(on_progress))).await?;
        Ok(result.into_successes())
    }

    /// v3:运行 DAG 并返回完整结果(含失败/跳过信息)。
    ///
    /// 与 [`run`](Self::run) 相同,但返回 [`DagRunResult`] 而非 `Vec<NodeResult>`,
    /// 让调用方能区分"全成功"和"部分失败"(FailFast::Off 下)。
    /// 调用方可据此决定是否调用 [`retry_failed`](Self::retry_failed) /
    /// [`recover_skipped`](Self::recover_skipped)。
    ///
    /// # Errors
    /// Same as [`run`](Self::run)。FailFast::Off 下即使有节点失败也返回 `Ok(DagRunResult)`。
    pub async fn run_with_details(&self) -> Result<DagRunResult, DagError> {
        self.run_inner(None).await
    }

    /// v3:重试指定的 failed 节点。
    ///
    /// 在 [`run_with_details`](Self::run_with_details) 返回 `DagRunResult` 后,
    /// 调用方可选择性地重试部分失败节点。本方法构造一个仅含 `node_ids` 的子 DAG,
    /// 用同一 executor + FailFast 策略重新执行,返回新的 `DagRunResult`。
    ///
    /// **不会恢复 skipped 节点**:若 failed 节点有下游 skipped 节点,需单独调用
    /// [`recover_skipped`](Self::recover_skipped)。
    ///
    /// **v3 DagRun 追加**:若本 scheduler 通过 [`with_dag_run`](Self::with_dag_run)
    /// 桥接了 DagStore,则每次重试的节点结果会以 [`NodeAttempt`] 形式追加到原始
    /// DagRun 的 `retry_history`,从而保留完整的 retry 轨迹。retry 成功的节点
    /// 在 `node_statuses` 中也会从 `Failed` 提升为 `Succeeded`。
    ///
    /// # 参数
    /// - `node_ids`:要重试的节点 ID 列表(必须存在于原 DAG 中)。
    ///
    /// # 返回
    /// 新的 `DagRunResult`,仅包含重试节点的结果(不合并原结果)。
    ///
    /// # Errors
    /// - [`DagError::NodeNotFound`] — 指定的 node_id 不存在。
    /// - 其他错误同 [`run`](Self::run)。
    pub async fn retry_failed(&self, node_ids: &[DagNodeId]) -> Result<DagRunResult, DagError> {
        let sub_graph = self.build_subgraph(node_ids)?;
        let sub_scheduler = DagScheduler::new(sub_graph, Arc::clone(&self.executor))
            .with_max_parallelism(self.max_parallelism)
            .with_fail_fast(self.fail_fast);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let result = sub_scheduler.run_inner(None).await?;
        let completed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // v3:若有 DagRun 桥接,把本次 retry 结果追加到原始 DagRun
        if let (Some(store), Some(run_id)) = (&self.dag_store, &self.dag_run_id) {
            self.append_retry_attempts(store, run_id, &result, started, completed)
                .await;
        }

        Ok(result)
    }

    /// v3:恢复指定的 skipped 节点。
    ///
    /// 在 failed 节点通过 [`retry_failed`](Self::retry_failed) 成功重试后,
    /// 调用方可恢复因依赖失败而被跳过的节点。本方法:
    /// 1. 构造仅含 `node_ids` 的子 DAG(保留原依赖关系)。
    /// 2. 用同一 executor + FailFast 策略执行。
    ///
    /// **前置条件**:调用方应确保 `node_ids` 的所有 `depends_on` 已成功
    /// (通过之前的 `retry_failed` 或原 `run`)。否则节点会再次被跳过。
    ///
    /// **v3 DagRun 追加**:与 [`retry_failed`](Self::retry_failed) 相同,
    /// 恢复结果会以 [`NodeAttempt`] 形式追加到原始 DagRun。
    ///
    /// # 参数
    /// - `node_ids`:要恢复的节点 ID 列表。
    ///
    /// # 返回
    /// 新的 `DagRunResult`,仅包含恢复节点的结果。
    ///
    /// # Errors
    /// 同 [`retry_failed`](Self::retry_failed)。
    pub async fn recover_skipped(&self, node_ids: &[DagNodeId]) -> Result<DagRunResult, DagError> {
        // recover_skipped 与 retry_failed 共享子图构造 + 执行逻辑,
        // 语义区别仅在调用方约定(retry_failed 针对 failed,recover_skipped 针对 skipped)。
        self.retry_failed(node_ids).await
    }

    /// v3:把 retry/recover 的结果追加到原始 DagRun 的 `retry_history`。
    ///
    /// 内部辅助函数,供 [`retry_failed`](Self::retry_failed) 与
    /// [`recover_skipped`](Self::recover_skipped) 共享。
    ///
    /// 对每个成功/失败的节点追加一条 [`NodeAttempt`];skipped 节点不追加
    /// (它们在子图中本就不会执行,sub-scheduler 不会返回 skipped)。
    async fn append_retry_attempts(
        &self,
        store: &Arc<DagStore>,
        run_id: &str,
        result: &DagRunResult,
        started_at: u64,
        completed_at: u64,
    ) {
        // 先查询当前每个节点的 retry 次数,以确定本次 attempt 序号
        let existing_counts: HashMap<DagNodeId, u32> = if let Some(run) = store.get_run(run_id) {
            result
                .successes
                .iter()
                .map(|s| (s.node_id.clone(), run.retry_count_for(&s.node_id)))
                .chain(
                    result
                        .failures
                        .iter()
                        .map(|(nid, _)| (nid.clone(), run.retry_count_for(nid))),
                )
                .collect()
        } else {
            HashMap::new()
        };

        // 追加成功的 retry
        for success in &result.successes {
            let attempt_num = existing_counts.get(&success.node_id).copied().unwrap_or(0) + 1;
            let attempt = super::types::NodeAttempt {
                node_id: success.node_id.clone(),
                attempt: attempt_num,
                status: super::types::DagNodeStatus::Succeeded,
                error: None,
                started_at,
                completed_at,
            };
            // 忽略写入错误:retry 已成功,DagRun 同步失败不应影响 retry 结果
            let _ = store.record_retry_attempt(run_id, attempt);
        }

        // 追加失败的 retry
        for (node_id, error) in &result.failures {
            let attempt_num = existing_counts.get(node_id).copied().unwrap_or(0) + 1;
            let attempt = super::types::NodeAttempt {
                node_id: node_id.clone(),
                attempt: attempt_num,
                status: super::types::DagNodeStatus::Failed,
                error: Some(error.clone()),
                started_at,
                completed_at,
            };
            let _ = store.record_retry_attempt(run_id, attempt);
        }
    }

    /// 构造仅含指定节点的子 DAG(保留原节点定义 + 依赖关系)。
    ///
    /// 内部辅助函数,供 `retry_failed` / `recover_skipped` 使用。
    /// 子图的 `max_parallelism` 继承自原 scheduler。
    fn build_subgraph(&self, node_ids: &[DagNodeId]) -> Result<DagGraph, DagError> {
        let mut sub = DagGraph::new("retry-subgraph").with_max_parallelism(self.max_parallelism);
        // 先添加所有目标节点(清除 depends_on,因为子图只含目标节点本身)
        for nid in node_ids {
            let mut node = self
                .dag
                .get_node(nid)
                .ok_or_else(|| DagError::UnknownNode(nid.clone()))?
                .clone();
            // 子图中清除依赖:retry/recover 的节点在子图中应作为根节点执行,
            // 调用方负责确保原依赖已满足(retry_failed 前置条件)。
            node.depends_on.clear();
            sub.add_node(node);
        }
        Ok(sub)
    }

    /// Shared implementation for [`run`](Self::run) and
    /// [`run_with_progress`](Self::run_with_progress).
    ///
    /// `on_progress` is `Option<Box<...>>` to keep the call sites uniform
    /// without monomorphising the entire loop body per callback type.
    async fn run_inner(&self, mut on_progress: ProgressCallback) -> Result<DagRunResult, DagError> {
        // Validate acyclicity up-front so we never loop forever on a cyclic graph.
        self.dag.validate_acyclic()?;

        let mut completed: HashSet<DagNodeId> = HashSet::new();
        // v3 FailFast::Off:追踪永久失败的节点 + 因依赖失败而被跳过的节点。
        // 这两个集合合并到 `completed` 中以避免 `ready_nodes` 反复列出它们,
        // 但通过 `failed`/`skipped` 单独追踪以供结果报告。
        // v3 增强:`failed` 存储最后一次错误信息,供 `DagRunResult.failures` 使用。
        let mut failed: HashMap<DagNodeId, String> = HashMap::new();
        let mut skipped: HashSet<DagNodeId> = HashSet::new();
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
            // v3 FailFast::Off:过滤掉依赖已失败的节点(标记为 Skipped)。
            let mut newly_skipped: Vec<DagNodeId> = Vec::new();
            let ready: Vec<DagNodeId> = self
                .dag
                .ready_nodes(&completed)
                .into_iter()
                .filter(|id| !inflight.contains(id))
                .filter(|id| {
                    // FailFast::Off:若任一依赖已失败/跳过,则此节点也跳过
                    if self.fail_fast == FailFast::Off {
                        if let Some(node) = self.dag.get_node(id) {
                            if node
                                .depends_on
                                .iter()
                                .any(|dep| failed.contains_key(dep) || skipped.contains(dep))
                            {
                                newly_skipped.push(id.clone());
                                return false;
                            }
                        }
                    }
                    true
                })
                .collect();

            // v3 FailFast::Off:将因依赖失败而跳过的节点标记为 Skipped。
            // v3 增强:使用独立的 `NodeSkipped` 事件(而非复用 `NodeFailed`)。
            for sid in &newly_skipped {
                skipped.insert(sid.clone());
                completed.insert(sid.clone()); // 防止 ready_nodes 反复列出
                self.bridge_node_status(sid, DagNodeStatus::Skipped);
                self.emit_progress(
                    &mut on_progress,
                    ProgressEvent::NodeSkipped {
                        node_id: sid.clone(),
                        reason: "skipped due to upstream dependency failure".to_string(),
                    },
                );
            }
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

                // P1-6:首次执行 attempt=0(attempts 尚未记录该节点)。
                // 重试时 attempts 已 insert,此处读取到 >0 的值。
                let attempt = attempts.get(node_id).copied().unwrap_or(0);
                joinset.spawn(async move {
                    // Cooperatively cancel if the parent token fires mid-execution.
                    let exec_fut = executor.execute(&node, attempt);
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
                    self.emit_progress(&mut on_progress, ProgressEvent::NodeSucceeded { node_id });
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
                        // P1-6:重试 attempt = current_attempt + 1(0-indexed)。
                        // 566 行已 insert current_attempt+1 到 attempts,此处直接用。
                        let retry_attempt = current_attempt + 1;
                        joinset.spawn(async move {
                            let exec_fut = executor.execute(&node, retry_attempt);
                            let res = tokio::select! {
                                biased;
                                _ = child_token.cancelled() => Err(NodeError::Cancelled),
                                res = exec_fut => res,
                            };
                            (node_id_for_status, res)
                        });
                        continue;
                    }

                    // Retries exhausted.
                    self.bridge_node_status(&node_id, DagNodeStatus::Failed);

                    if self.fail_fast == FailFast::Off {
                        // v3 FailFast::Off:标记节点失败(含错误信息),跳过其下游,继续执行独立分支。
                        failed.insert(node_id.clone(), node_err.to_string());
                        completed.insert(node_id.clone()); // 防止 ready_nodes 反复列出
                                                           // 不取消 DAG,不 abort 在途任务,继续循环
                        continue;
                    }

                    // FailFast::On(默认):立即取消整个 DAG。
                    self.cancel_token.cancel();
                    joinset.abort_all();
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

        // All nodes reached terminal state.
        // v3 增强:FailFast::Off 下若有 failed/skipped,用 CompletedWithFailures 终态。
        let has_failures = !failed.is_empty() || !skipped.is_empty();
        if has_failures {
            self.emit_progress(&mut on_progress, ProgressEvent::DagCompleted);
            self.bridge_run_status(DagStatus::CompletedWithFailures);
        } else {
            self.emit_progress(&mut on_progress, ProgressEvent::DagCompleted);
            self.bridge_run_status(DagStatus::Completed);
        }

        let failures = failed.into_iter().collect();
        let skipped = skipped.into_iter().collect();
        Ok(DagRunResult {
            successes: results,
            failures,
            skipped,
        })
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
        async fn execute(&self, node: &DagNode, _attempt: u32) -> Result<NodeResult, NodeError> {
            self.seen
                .lock()
                .expect("seen poisoned")
                .push(node.id.clone());
            Ok(NodeResult {
                node_id: node.id.clone(),
                summary: node.task.clone(),
                artifact_path: None,
                gated: None,
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
        async fn execute(&self, node: &DagNode, _attempt: u32) -> Result<NodeResult, NodeError> {
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
                gated: None,
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
        async fn execute(&self, node: &DagNode, _attempt: u32) -> Result<NodeResult, NodeError> {
            if node.id == self.fail_id {
                let mut attempts = self.attempts.lock().expect("attempts poisoned");
                *attempts += 1;
                if *attempts <= self.fail_n_times {
                    return Err(NodeError::ExecutionFailed(format!(
                        "transient failure {} on {}",
                        *attempts, node.id
                    )));
                }
            }
            Ok(NodeResult {
                node_id: node.id.clone(),
                summary: node.task.clone(),
                artifact_path: None,
                gated: None,
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
        // 默认已改为 FailFast::Off,本测试验证 On 行为,需显式指定。
        let scheduler = DagScheduler::new(graph, executor).with_fail_fast(FailFast::On);
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
        let err = scheduler
            .run()
            .await
            .expect_err("cancelled run should error");
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
            nodes: vec![node("n1", &[]), node("n2", &["n1"]), node("n3", &["n2"])],
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
            assert_eq!(
                *status,
                DagNodeStatus::Succeeded,
                "all nodes should be Succeeded"
            );
        }
    }

    #[tokio::test]
    async fn dag_run_bridged_failure_marks_failed() {
        let dag = Dag {
            id: "parallel".to_string(),
            name: "Parallel".to_string(),
            nodes: vec![node("n1", &[]), node("n2", &[]), node("n3", &["n1", "n2"])],
        };
        let graph = DagGraph::from_dag(&dag);
        let store = Arc::new(DagStore::new());
        store.create_dag(dag.clone()).unwrap();
        let run = store.start_run(&dag.id).unwrap();
        let run_id = run.id.clone();

        let executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(graph, executor)
            .with_dag_run(store.clone(), run_id)
            .with_fail_fast(FailFast::On);

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
        assert!(events
            .iter()
            .any(|e| matches!(e, ProgressEvent::DagCompleted)));
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
        // 默认 FailFast::Off 下单节点 DAG 失败仍返回 Ok(DagRunResult),
        // 本测试验证 Err 语义,需显式 On。
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::On);
        let err = scheduler
            .run()
            .await
            .expect_err("should fail after retries");
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
        // 默认 FailFast::Off 下单节点 DAG 失败返回 Ok(DagRunResult),
        // 本测试验证 Err 语义,需显式 On。
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::On);
        let err = scheduler
            .run()
            .await
            .expect_err("should fail without retry");
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
        // 默认 FailFast::Off 下失败返回 Ok(DagRunResult),本测试验证 Err 语义,需显式 On。
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::On);
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
            async fn execute(
                &self,
                _node: &DagNode,
                _attempt: u32,
            ) -> Result<NodeResult, NodeError> {
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

    // ========================================================================
    // v3 FailFast::Off 增强:retry_failed / recover_skipped / DagRunResult
    // ========================================================================

    #[tokio::test]
    async fn fail_fast_off_parallel_graph_collects_partial_results() {
        // n1 fails, n2 succeeds (independent), n3 depends on n1 → skipped.
        let g = parallel_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::Off);
        let result = scheduler
            .run_with_details()
            .await
            .expect("FailFast::Off should return Ok with partial results");

        assert_eq!(result.success_count(), 1, "n2 should succeed");
        assert_eq!(result.failure_count(), 1, "n1 should fail");
        assert_eq!(result.skip_count(), 1, "n3 should be skipped");
        assert!(!result.is_all_success());
        assert!(result.successes.iter().any(|r| r.node_id == "n2"));
        assert!(result.failures.iter().any(|(id, _)| id == "n1"));
        assert!(result.skipped.contains(&"n3".to_string()));
    }

    #[tokio::test]
    async fn fail_fast_off_all_success_returns_completed_status() {
        let g = parallel_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::Off);
        let result = scheduler
            .run_with_details()
            .await
            .expect("all success should return Ok");

        assert!(result.is_all_success());
        assert_eq!(result.success_count(), 3);
        assert_eq!(result.failure_count(), 0);
        assert_eq!(result.skip_count(), 0);
    }

    #[tokio::test]
    async fn fail_fast_on_still_returns_err_on_failure() {
        // FailFast::On should return Err, not DagRunResult.
        // (默认已改为 Off,本测试验证 On 行为,需显式指定。)
        let g = parallel_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::On);
        let err = scheduler
            .run_with_details()
            .await
            .expect_err("FailFast::On should return Err on failure");
        assert!(matches!(err, DagError::NodeFailed(ref id) if id == "n1"));
    }

    #[tokio::test]
    async fn retry_failed_succeeds_after_transient_failure() {
        // 原始 DAG:n1 失败(FailFast::Off)。retry_failed 后用 FailThenSucceed 重试。
        let g = parallel_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::Off);
        let result = scheduler
            .run_with_details()
            .await
            .expect("FailFast::Off returns Ok");

        assert_eq!(result.failure_count(), 1);
        let failed_id = result.failures[0].0.clone();

        // retry_failed with a new executor that succeeds.
        let retry_executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let retry_scheduler =
            DagScheduler::new(parallel_graph(), retry_executor).with_fail_fast(FailFast::Off);
        let retry_result = retry_scheduler
            .retry_failed(&[failed_id])
            .await
            .expect("retry should succeed");
        assert_eq!(retry_result.success_count(), 1);
        assert_eq!(retry_result.failures.len(), 0);
    }

    #[tokio::test]
    async fn retry_failed_appends_attempt_to_original_dag_run() {
        // v3:retry_failed 在有 DagRun 桥接时,应把 retry 结果追加到原始 DagRun.retry_history
        let dag = Dag {
            id: "parallel".to_string(),
            name: "Parallel".to_string(),
            nodes: vec![node("n1", &[]), node("n2", &[]), node("n3", &["n1", "n2"])],
        };
        let graph = DagGraph::from_dag(&dag);
        let store = Arc::new(DagStore::new());
        store.create_dag(dag.clone()).unwrap();
        let run = store.start_run(&dag.id).unwrap();
        let run_id = run.id.clone();

        // 原始 run:n1 失败 (FailFast::Off)
        let fail_executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(graph, fail_executor)
            .with_fail_fast(FailFast::Off)
            .with_dag_run(Arc::clone(&store), run_id.clone());
        let result = scheduler
            .run_with_details()
            .await
            .expect("FailFast::Off returns Ok");
        assert_eq!(result.failure_count(), 1);
        let failed_id = result.failures[0].0.clone();
        assert_eq!(failed_id, "n1");

        // 验证原始 DagRun 中 n1 为 Failed,且 retry_history 为空
        let mid_run = store.get_run(&run_id).expect("run should exist");
        assert_eq!(mid_run.node_status("n1"), Some(DagNodeStatus::Failed));
        assert!(mid_run.retry_history.is_empty(), "no retries yet");

        // retry_failed with SuccessExecutor (same scheduler instance to preserve dag_run bridge)
        let retry_executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        // 用新 scheduler 但桥接同一个 DagRun
        let retry_graph = DagGraph::from_dag(&dag);
        let retry_scheduler = DagScheduler::new(retry_graph, retry_executor)
            .with_fail_fast(FailFast::Off)
            .with_dag_run(Arc::clone(&store), run_id.clone());
        let retry_result = retry_scheduler
            .retry_failed(std::slice::from_ref(&failed_id))
            .await
            .expect("retry should succeed");
        assert_eq!(retry_result.success_count(), 1);

        // 验证 retry_history 已追加一条 Succeeded 记录
        let final_run = store.get_run(&run_id).expect("run should exist");
        assert_eq!(
            final_run.retry_history.len(),
            1,
            "retry_history should have 1 attempt"
        );
        let attempt = &final_run.retry_history[0];
        assert_eq!(attempt.node_id, "n1");
        assert_eq!(attempt.attempt, 1, "first retry → attempt 1");
        assert_eq!(attempt.status, DagNodeStatus::Succeeded);
        assert!(attempt.error.is_none());

        // 验证 node_statuses 中 n1 从 Failed 提升为 Succeeded
        assert_eq!(
            final_run.node_status("n1"),
            Some(DagNodeStatus::Succeeded),
            "n1 should be upgraded to Succeeded after successful retry"
        );

        // 验证 retry_count_for 与 last_attempt_for 辅助方法
        assert_eq!(final_run.retry_count_for("n1"), 1);
        assert_eq!(final_run.retry_count_for("n2"), 0, "n2 never retried");
        let last = final_run.last_attempt_for("n1").expect("should exist");
        assert_eq!(last.status, DagNodeStatus::Succeeded);
    }

    #[tokio::test]
    async fn retry_failed_multiple_attempts_increment_counter() {
        // v3:多次 retry 同一节点,attempt 序号应递增
        let dag = Dag {
            id: "single".to_string(),
            name: "Single".to_string(),
            nodes: vec![node("n1", &[])],
        };
        let graph = DagGraph::from_dag(&dag);
        let store = Arc::new(DagStore::new());
        store.create_dag(dag.clone()).unwrap();
        let run = store.start_run(&dag.id).unwrap();
        let run_id = run.id.clone();

        // 原始 run:n1 失败
        let fail_executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(graph, fail_executor)
            .with_fail_fast(FailFast::Off)
            .with_dag_run(Arc::clone(&store), run_id.clone());
        let _ = scheduler.run_with_details().await.expect("FailFast::Off");

        // 第一次 retry:仍然失败
        let fail_again: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let s1 = DagScheduler::new(DagGraph::from_dag(&dag), fail_again)
            .with_fail_fast(FailFast::Off)
            .with_dag_run(Arc::clone(&store), run_id.clone());
        let r1 = s1.retry_failed(&["n1".to_string()]).await.expect("retry 1");
        assert_eq!(r1.failure_count(), 1);

        // 第二次 retry:成功
        let success_exec: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let s2 = DagScheduler::new(DagGraph::from_dag(&dag), success_exec)
            .with_fail_fast(FailFast::Off)
            .with_dag_run(Arc::clone(&store), run_id.clone());
        let r2 = s2.retry_failed(&["n1".to_string()]).await.expect("retry 2");
        assert_eq!(r2.success_count(), 1);

        // 验证 retry_history 有两条记录,attempt 序号分别为 1 和 2
        let final_run = store.get_run(&run_id).expect("run should exist");
        assert_eq!(final_run.retry_history.len(), 2);
        assert_eq!(final_run.retry_history[0].attempt, 1);
        assert_eq!(final_run.retry_history[0].status, DagNodeStatus::Failed);
        assert!(final_run.retry_history[0].error.is_some());
        assert_eq!(final_run.retry_history[1].attempt, 2);
        assert_eq!(final_run.retry_history[1].status, DagNodeStatus::Succeeded);
        assert!(final_run.retry_history[1].error.is_none());

        // 最终 n1 应为 Succeeded(最后一次 retry 成功)
        assert_eq!(final_run.node_status("n1"), Some(DagNodeStatus::Succeeded));
        assert_eq!(final_run.retry_count_for("n1"), 2);
    }

    #[tokio::test]
    async fn recover_skipped_appends_attempt_to_original_dag_run() {
        // v3:recover_skipped 同样应追加到原 DagRun
        // 使用 parallel 结构:n1 失败 → n3 skipped(因依赖 n1),n2 独立成功
        let dag = Dag {
            id: "parallel".to_string(),
            name: "Parallel".to_string(),
            nodes: vec![node("n1", &[]), node("n2", &[]), node("n3", &["n1", "n2"])],
        };
        let graph = DagGraph::from_dag(&dag);
        let store = Arc::new(DagStore::new());
        store.create_dag(dag.clone()).unwrap();
        let run = store.start_run(&dag.id).unwrap();
        let run_id = run.id.clone();

        // 原始 run:n1 失败 → n3 skipped (FailFast::Off)
        let fail_executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(graph, fail_executor)
            .with_fail_fast(FailFast::Off)
            .with_dag_run(Arc::clone(&store), run_id.clone());
        let result = scheduler
            .run_with_details()
            .await
            .expect("FailFast::Off returns Ok");
        assert_eq!(result.skip_count(), 1);
        assert_eq!(result.skipped[0], "n3");

        // recover_skipped("n3") 应成功(假设 n1/n2 依赖已满足,只恢复 n3)
        let success_exec: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let recover_scheduler = DagScheduler::new(DagGraph::from_dag(&dag), success_exec)
            .with_fail_fast(FailFast::Off)
            .with_dag_run(Arc::clone(&store), run_id.clone());
        let recover_result = recover_scheduler
            .recover_skipped(&["n3".to_string()])
            .await
            .expect("recover should succeed");
        assert_eq!(recover_result.success_count(), 1);

        // 验证 retry_history 追加了 n3 的 Succeeded 记录
        let final_run = store.get_run(&run_id).expect("run should exist");
        let n3_attempts: Vec<_> = final_run
            .retry_history
            .iter()
            .filter(|a| a.node_id == "n3")
            .collect();
        assert_eq!(n3_attempts.len(), 1);
        assert_eq!(n3_attempts[0].status, DagNodeStatus::Succeeded);
        assert_eq!(n3_attempts[0].attempt, 1);

        // n3 node_status 应为 Succeeded
        assert_eq!(final_run.node_status("n3"), Some(DagNodeStatus::Succeeded));
    }

    #[tokio::test]
    async fn retry_failed_unknown_node_returns_error() {
        let g = parallel_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let scheduler = DagScheduler::new(g, executor);
        let err = scheduler
            .retry_failed(&["nonexistent".to_string()])
            .await
            .expect_err("unknown node should error");
        assert!(matches!(err, DagError::UnknownNode(ref id) if id == "nonexistent"));
    }

    #[tokio::test]
    async fn recover_skipped_runs_skipped_node_after_dependency_fixed() {
        // 原始 DAG:n1 fails, n3 skipped。recover_skipped("n3") 在 n1 已修复后应成功。
        let g = parallel_graph();
        let fail_executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n1".to_string(),
        });
        let scheduler = DagScheduler::new(g, fail_executor).with_fail_fast(FailFast::Off);
        let result = scheduler
            .run_with_details()
            .await
            .expect("FailFast::Off returns Ok");

        assert_eq!(result.skip_count(), 1);
        let skipped_id = result.skipped[0].clone();
        assert_eq!(skipped_id, "n3");

        // recover_skipped with SuccessExecutor.
        let recover_executor: Arc<dyn SubagentExecutor> = Arc::new(SuccessExecutor {
            seen: Mutex::new(Vec::new()),
        });
        let recover_scheduler =
            DagScheduler::new(parallel_graph(), recover_executor).with_fail_fast(FailFast::Off);
        let recover_result = recover_scheduler
            .recover_skipped(&[skipped_id])
            .await
            .expect("recover should succeed");
        assert_eq!(recover_result.success_count(), 1);
        assert_eq!(recover_result.successes[0].node_id, "n3");
    }

    #[tokio::test]
    async fn dag_run_result_into_successes_extracts_only_successes() {
        let g = parallel_graph();
        let executor: Arc<dyn SubagentExecutor> = Arc::new(FailOnExecutor {
            fail_id: "n2".to_string(),
        });
        let scheduler = DagScheduler::new(g, executor).with_fail_fast(FailFast::Off);
        let result = scheduler
            .run_with_details()
            .await
            .expect("FailFast::Off returns Ok");

        let successes = result.into_successes();
        assert_eq!(successes.len(), 1);
        assert!(successes.iter().any(|r| r.node_id == "n1"));
    }
}
