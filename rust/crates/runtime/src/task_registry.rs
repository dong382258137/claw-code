#![allow(clippy::must_use_candidate, clippy::unnecessary_map_or)]
//! In-memory task registry for sub-agent task lifecycle management.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::multi_agent::{CoordinationMode, MultiAgentCoordinator, Subagent};
use crate::{validate_packet, TaskPacket, TaskPacketValidationError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Running,
    Blocked,
    Completed,
    Failed,
    Stopped,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Blocked => write!(f, "blocked"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub prompt: String,
    pub description: Option<String>,
    pub task_packet: Option<TaskPacket>,
    pub status: TaskStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<TaskMessage>,
    pub output: String,
    pub team_id: Option<String>,
    pub heartbeat: Option<LaneHeartbeat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneFreshness {
    Healthy,
    Stalled,
    TransportDead,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneHeartbeat {
    pub observed_at: u64,
    pub transport_alive: bool,
    pub status: String,
}

impl LaneHeartbeat {
    #[must_use]
    pub fn freshness_at(&self, now: u64, stalled_after_secs: u64) -> LaneFreshness {
        if !self.transport_alive {
            return LaneFreshness::TransportDead;
        }
        if now.saturating_sub(self.observed_at) > stalled_after_secs {
            return LaneFreshness::Stalled;
        }
        LaneFreshness::Healthy
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneBoardEntry {
    pub task_id: String,
    pub prompt: String,
    pub status: TaskStatus,
    pub team_id: Option<String>,
    pub heartbeat: Option<LaneHeartbeat>,
    pub freshness: LaneFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneBoard {
    pub generated_at: u64,
    pub active: Vec<LaneBoardEntry>,
    pub blocked: Vec<LaneBoardEntry>,
    pub finished: Vec<LaneBoardEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub role: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    /// Optional multi-agent coordinator for sub-agent orchestration (Step 3.2).
    ///
    /// 当配置后,TaskRegistry 可通过 `spawn_subagent_for_task` 把 task 派发给
    /// coordinator 作为 subagent 管理,建立 task → subagent 的派发链路。
    coordinator: Option<MultiAgentCoordinator>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    tasks: HashMap<String, Task>,
    counter: u64,
    /// task_id → associated subagent IDs (仅当 coordinator 配置后填充)。
    task_subagents: HashMap<String, Vec<String>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl TaskRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 配置 MultiAgentCoordinator,启用 sub-agent 编排能力(Step 3.2)。
    #[must_use]
    pub fn with_multi_agent_coordinator(mut self, coord: MultiAgentCoordinator) -> Self {
        self.coordinator = Some(coord);
        self
    }

    /// 获取已配置的 MultiAgentCoordinator 引用(若存在)。
    #[must_use]
    pub fn coordinator(&self) -> Option<&MultiAgentCoordinator> {
        self.coordinator.as_ref()
    }

    /// 派生 subagent 关联指定 task(Step 3.2)。
    ///
    /// 将 `task.prompt` 作为子 agent 的任务描述,在 coordinator 上 `spawn`,
    /// 并记录 task_id → subagent_id 的映射。后续可通过 `start_subagent` /
    /// `complete_subagent` / `fail_subagent` 驱动子 agent 生命周期。
    pub fn spawn_subagent_for_task(
        &self,
        task_id: &str,
        name: &str,
        mode: CoordinationMode,
    ) -> Result<String, String> {
        let coord = self.coordinator.as_ref().ok_or_else(|| {
            "multi-agent coordinator not configured; call with_multi_agent_coordinator first"
                .to_string()
        })?;

        let prompt = {
            let inner = self.inner.lock().expect("registry lock poisoned");
            let task = inner
                .tasks
                .get(task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))?;
            task.prompt.clone()
        };

        let subagent_id = coord.spawn(name, prompt, mode);

        {
            let mut inner = self.inner.lock().expect("registry lock poisoned");
            inner
                .task_subagents
                .entry(task_id.to_string())
                .or_default()
                .push(subagent_id.clone());
        }

        Ok(subagent_id)
    }

    /// 启动 task 关联的 subagent。
    pub fn start_subagent(&self, subagent_id: &str) -> Result<(), String> {
        let coord = self
            .coordinator
            .as_ref()
            .ok_or_else(|| "multi-agent coordinator not configured".to_string())?;
        coord.start(subagent_id)
    }

    /// 标记 subagent 完成,并把结果写回关联的 task.output。
    pub fn complete_subagent(&self, subagent_id: &str, result: &str) -> Result<(), String> {
        let coord = self
            .coordinator
            .as_ref()
            .ok_or_else(|| "multi-agent coordinator not configured".to_string())?;
        coord.complete(subagent_id, result)?;

        if let Some(task_id) = self.find_task_for_subagent(subagent_id) {
            let _ = self.append_output(&task_id, result);
        }
        Ok(())
    }

    /// 标记 subagent 失败,并把错误写回关联的 task.messages。
    pub fn fail_subagent(&self, subagent_id: &str, error: &str) -> Result<(), String> {
        let coord = self
            .coordinator
            .as_ref()
            .ok_or_else(|| "multi-agent coordinator not configured".to_string())?;
        coord.fail(subagent_id, error)?;

        if let Some(task_id) = self.find_task_for_subagent(subagent_id) {
            let _ = self.update(&task_id, &format!("subagent {subagent_id} failed: {error}"));
        }
        Ok(())
    }

    /// 取消 subagent。
    pub fn cancel_subagent(&self, subagent_id: &str) -> Result<(), String> {
        let coord = self
            .coordinator
            .as_ref()
            .ok_or_else(|| "multi-agent coordinator not configured".to_string())?;
        coord.cancel(subagent_id)
    }

    /// 列出 task 关联的所有 subagent。
    #[must_use]
    pub fn list_subagents_for_task(&self, task_id: &str) -> Vec<Subagent> {
        let Some(coord) = self.coordinator.as_ref() else {
            return Vec::new();
        };
        let subagent_ids = {
            let inner = self.inner.lock().expect("registry lock poisoned");
            inner
                .task_subagents
                .get(task_id)
                .cloned()
                .unwrap_or_default()
        };
        subagent_ids.iter().filter_map(|id| coord.get(id)).collect()
    }

    /// 查找 subagent 关联的 task_id(若存在)。
    fn find_task_for_subagent(&self, subagent_id: &str) -> Option<String> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner
            .task_subagents
            .iter()
            .find(|(_, ids)| ids.iter().any(|id| id == subagent_id))
            .map(|(tid, _)| tid.clone())
    }

    pub fn create(&self, prompt: &str, description: Option<&str>) -> Task {
        self.create_task(prompt.to_owned(), description.map(str::to_owned), None)
    }

    pub fn create_from_packet(
        &self,
        packet: TaskPacket,
    ) -> Result<Task, TaskPacketValidationError> {
        let packet = validate_packet(packet)?.into_inner();
        // Use scope_path as description if available, otherwise use scope as string
        let description = packet
            .scope_path
            .clone()
            .or_else(|| Some(packet.scope.to_string()));
        Ok(self.create_task(packet.objective.clone(), description, Some(packet)))
    }

    fn create_task(
        &self,
        prompt: String,
        description: Option<String>,
        task_packet: Option<TaskPacket>,
    ) -> Task {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let task_id = format!("task_{:08x}_{}", ts, inner.counter);
        let task = Task {
            task_id: task_id.clone(),
            prompt,
            description,
            task_packet,
            status: TaskStatus::Created,
            created_at: ts,
            updated_at: ts,
            messages: Vec::new(),
            output: String::new(),
            team_id: None,
            heartbeat: None,
        };
        inner.tasks.insert(task_id, task.clone());
        task
    }

    pub fn get(&self, task_id: &str) -> Option<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.get(task_id).cloned()
    }

    pub fn list(&self, status_filter: Option<TaskStatus>) -> Vec<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner
            .tasks
            .values()
            .filter(|t| status_filter.map_or(true, |s| t.status == s))
            .cloned()
            .collect()
    }

    pub fn update_heartbeat(&self, task_id: &str, heartbeat: LaneHeartbeat) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.heartbeat = Some(heartbeat);
        task.updated_at = now_secs();
        Ok(())
    }

    #[must_use]
    pub fn lane_board(&self, stalled_after_secs: u64) -> LaneBoard {
        let now = now_secs();
        self.lane_board_at(now, stalled_after_secs)
    }

    #[must_use]
    pub fn lane_board_at(&self, now: u64, stalled_after_secs: u64) -> LaneBoard {
        let inner = self.inner.lock().expect("registry lock poisoned");
        let mut board = LaneBoard {
            generated_at: now,
            active: Vec::new(),
            blocked: Vec::new(),
            finished: Vec::new(),
        };

        for task in inner.tasks.values() {
            let freshness = task
                .heartbeat
                .as_ref()
                .map_or(LaneFreshness::Unknown, |heartbeat| {
                    heartbeat.freshness_at(now, stalled_after_secs)
                });
            let entry = LaneBoardEntry {
                task_id: task.task_id.clone(),
                prompt: task.prompt.clone(),
                status: task.status,
                team_id: task.team_id.clone(),
                heartbeat: task.heartbeat.clone(),
                freshness,
            };

            match task.status {
                TaskStatus::Running | TaskStatus::Created => board.active.push(entry),
                TaskStatus::Blocked => board.blocked.push(entry),
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped => {
                    board.finished.push(entry);
                }
            }
        }

        board
    }

    #[must_use]
    pub fn lane_status_json_at(&self, now: u64, stalled_after_secs: u64) -> serde_json::Value {
        serde_json::to_value(self.lane_board_at(now, stalled_after_secs))
            .expect("lane board should serialize")
    }

    pub fn stop(&self, task_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        match task.status {
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped => {
                return Err(format!(
                    "task {task_id} is already in terminal state: {}",
                    task.status
                ));
            }
            _ => {}
        }

        task.status = TaskStatus::Stopped;
        task.updated_at = now_secs();
        Ok(task.clone())
    }

    pub fn update(&self, task_id: &str, message: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        task.messages.push(TaskMessage {
            role: String::from("user"),
            content: message.to_owned(),
            timestamp: now_secs(),
        });
        task.updated_at = now_secs();
        Ok(task.clone())
    }

    pub fn output(&self, task_id: &str) -> Result<String, String> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        Ok(task.output.clone())
    }

    pub fn append_output(&self, task_id: &str, output: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.output.push_str(output);
        task.updated_at = now_secs();
        Ok(())
    }

    pub fn set_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.status = status;
        task.updated_at = now_secs();
        Ok(())
    }

    pub fn assign_team(&self, task_id: &str, team_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.team_id = Some(team_id.to_owned());
        task.updated_at = now_secs();
        Ok(())
    }

    pub fn remove(&self, task_id: &str) -> Option<Task> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.remove(task_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_retrieves_tasks() {
        let registry = TaskRegistry::new();
        let task = registry.create("Do something", Some("A test task"));
        assert_eq!(task.status, TaskStatus::Created);
        assert_eq!(task.prompt, "Do something");
        assert_eq!(task.description.as_deref(), Some("A test task"));
        assert_eq!(task.task_packet, None);

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.task_id, task.task_id);
    }

    #[test]
    fn spawn_subagent_for_task_requires_coordinator() {
        let registry = TaskRegistry::new();
        let task = registry.create("Do something", None);
        let err = registry
            .spawn_subagent_for_task(&task.task_id, "worker", CoordinationMode::Fork)
            .expect_err("should fail without coordinator");
        assert!(err.contains("coordinator not configured"));
    }

    #[test]
    fn spawn_subagent_for_task_unknown_task_fails() {
        let registry =
            TaskRegistry::new().with_multi_agent_coordinator(MultiAgentCoordinator::new());
        let err = registry
            .spawn_subagent_for_task("task_does_not_exist", "worker", CoordinationMode::Fork)
            .expect_err("should fail for unknown task");
        assert!(err.contains("task not found"));
    }

    #[test]
    fn spawn_subagent_for_task_links_task_to_subagent() {
        let registry =
            TaskRegistry::new().with_multi_agent_coordinator(MultiAgentCoordinator::new());
        let task = registry.create("Refactor auth module", None);

        let subagent_id = registry
            .spawn_subagent_for_task(&task.task_id, "worker-1", CoordinationMode::Fork)
            .expect("spawn should succeed");
        assert!(!subagent_id.is_empty());

        // subagent 应在 coordinator 上注册,且 task prompt 透传
        let coord = registry.coordinator().expect("coordinator should be set");
        let subagent = coord.get(&subagent_id).expect("subagent should exist");
        assert_eq!(subagent.task, "Refactor auth module");
        assert_eq!(subagent.mode, CoordinationMode::Fork);

        // list_subagents_for_task 应返回该 subagent
        let linked = registry.list_subagents_for_task(&task.task_id);
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, subagent_id);
    }

    #[test]
    fn complete_subagent_writes_result_back_to_task_output() {
        let registry =
            TaskRegistry::new().with_multi_agent_coordinator(MultiAgentCoordinator::new());
        let task = registry.create("Run tests", None);
        let subagent_id = registry
            .spawn_subagent_for_task(&task.task_id, "runner", CoordinationMode::Fork)
            .expect("spawn should succeed");
        registry
            .start_subagent(&subagent_id)
            .expect("start should succeed");
        registry
            .complete_subagent(&subagent_id, "all tests passed")
            .expect("complete should succeed");

        // task.output 应被回写
        let output = registry.output(&task.task_id).expect("output should exist");
        assert!(output.contains("all tests passed"));
    }

    #[test]
    fn fail_subagent_writes_error_back_to_task_messages() {
        let registry =
            TaskRegistry::new().with_multi_agent_coordinator(MultiAgentCoordinator::new());
        let task = registry.create("Build feature", None);
        let subagent_id = registry
            .spawn_subagent_for_task(&task.task_id, "builder", CoordinationMode::Worktree)
            .expect("spawn should succeed");
        registry
            .start_subagent(&subagent_id)
            .expect("start should succeed");
        registry
            .fail_subagent(&subagent_id, "compilation error")
            .expect("fail should succeed");

        // task.messages 应包含失败信息
        let task_after = registry.get(&task.task_id).expect("task should exist");
        assert!(task_after
            .messages
            .iter()
            .any(|m| m.content.contains("subagent") && m.content.contains("compilation error")));
    }

    #[test]
    fn coordinator_accessor_returns_none_by_default() {
        let registry = TaskRegistry::new();
        assert!(registry.coordinator().is_none());
    }

    #[test]
    fn creates_task_from_packet() {
        use crate::task_packet::TaskScope;
        let registry = TaskRegistry::new();
        let packet = TaskPacket {
            objective: "Ship task packet support".to_string(),
            scope: TaskScope::Module,
            scope_path: Some("runtime/task system".to_string()),
            worktree: Some("/tmp/wt-task".to_string()),
            repo: "claw-code-parity".to_string(),
            branch_policy: "origin/main only".to_string(),
            acceptance_tests: vec!["cargo test --workspace".to_string()],
            acceptance_criteria: vec!["task is inspectable".to_string()],
            resources: vec![crate::TaskResource {
                kind: "module".to_string(),
                value: "runtime/task system".to_string(),
            }],
            model: Some("gpt-5.5".to_string()),
            provider: Some("openai".to_string()),
            permission_profile: Some("workspace-write".to_string()),
            commit_policy: "single commit".to_string(),
            reporting_contract: "print commit sha".to_string(),
            reporting_targets: vec!["leader".to_string()],
            escalation_policy: "manual escalation".to_string(),
            recovery_policy: Some("retry once".to_string()),
            verification_plan: vec!["cargo test --workspace".to_string()],
        };

        let task = registry
            .create_from_packet(packet.clone())
            .expect("packet-backed task should be created");

        assert_eq!(task.prompt, packet.objective);
        assert_eq!(task.description.as_deref(), Some("runtime/task system"));
        // P3-5:validate_packet 会清空 legacy acceptance_tests,只保留 canonical
        // acceptance_criteria。因此 task.task_packet 与原始 packet 不同,
        // 需要比较 validated 后的版本。
        let mut validated_packet = packet.clone();
        validated_packet.acceptance_tests.clear();
        assert_eq!(task.task_packet, Some(validated_packet.clone()));

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.task_packet, Some(validated_packet));
    }

    #[test]
    fn lists_tasks_with_optional_filter() {
        let registry = TaskRegistry::new();
        registry.create("Task A", None);
        let task_b = registry.create("Task B", None);
        registry
            .set_status(&task_b.task_id, TaskStatus::Running)
            .expect("set status should succeed");

        let all = registry.list(None);
        assert_eq!(all.len(), 2);

        let running = registry.list(Some(TaskStatus::Running));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task_id, task_b.task_id);

        let created = registry.list(Some(TaskStatus::Created));
        assert_eq!(created.len(), 1);
    }

    #[test]
    fn stops_running_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Stoppable", None);
        registry
            .set_status(&task.task_id, TaskStatus::Running)
            .unwrap();

        let stopped = registry.stop(&task.task_id).expect("stop should succeed");
        assert_eq!(stopped.status, TaskStatus::Stopped);

        // Stopping again should fail
        let result = registry.stop(&task.task_id);
        assert!(result.is_err());
    }

    #[test]
    fn updates_task_with_messages() {
        let registry = TaskRegistry::new();
        let task = registry.create("Messageable", None);
        let updated = registry
            .update(&task.task_id, "Here's more context")
            .expect("update should succeed");
        assert_eq!(updated.messages.len(), 1);
        assert_eq!(updated.messages[0].content, "Here's more context");
        assert_eq!(updated.messages[0].role, "user");
    }

    #[test]
    fn appends_and_retrieves_output() {
        let registry = TaskRegistry::new();
        let task = registry.create("Output task", None);
        registry
            .append_output(&task.task_id, "line 1\n")
            .expect("append should succeed");
        registry
            .append_output(&task.task_id, "line 2\n")
            .expect("append should succeed");

        let output = registry.output(&task.task_id).expect("output should exist");
        assert_eq!(output, "line 1\nline 2\n");
    }

    #[test]
    fn lane_board_groups_active_blocked_finished_and_reports_freshness() {
        let registry = TaskRegistry::new();
        let active = registry.create("active", None);
        let blocked = registry.create("blocked", None);
        let finished = registry.create("finished", None);

        registry
            .set_status(&active.task_id, TaskStatus::Running)
            .expect("running status");
        registry
            .set_status(&blocked.task_id, TaskStatus::Blocked)
            .expect("blocked status");
        registry
            .set_status(&finished.task_id, TaskStatus::Completed)
            .expect("completed status");
        registry
            .update_heartbeat(
                &active.task_id,
                LaneHeartbeat {
                    observed_at: 100,
                    transport_alive: true,
                    status: "running".to_string(),
                },
            )
            .expect("heartbeat");
        registry
            .update_heartbeat(
                &blocked.task_id,
                LaneHeartbeat {
                    observed_at: 10,
                    transport_alive: true,
                    status: "waiting".to_string(),
                },
            )
            .expect("heartbeat");
        registry
            .update_heartbeat(
                &finished.task_id,
                LaneHeartbeat {
                    observed_at: 100,
                    transport_alive: false,
                    status: "done".to_string(),
                },
            )
            .expect("heartbeat");

        let board = registry.lane_board_at(110, 30);

        assert_eq!(board.active.len(), 1);
        assert_eq!(board.active[0].freshness, LaneFreshness::Healthy);
        assert_eq!(board.blocked.len(), 1);
        assert_eq!(board.blocked[0].freshness, LaneFreshness::Stalled);
        assert_eq!(board.finished.len(), 1);
        assert_eq!(board.finished[0].freshness, LaneFreshness::TransportDead);

        let json = registry.lane_status_json_at(110, 30);
        assert_eq!(json["active"][0]["status"], "running");
        assert_eq!(json["blocked"][0]["freshness"], "stalled");
        assert_eq!(json["finished"][0]["freshness"], "transport_dead");
    }

    #[test]
    fn assigns_team_and_removes_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Team task", None);
        registry
            .assign_team(&task.task_id, "team_abc")
            .expect("assign should succeed");

        let fetched = registry.get(&task.task_id).unwrap();
        assert_eq!(fetched.team_id.as_deref(), Some("team_abc"));

        let removed = registry.remove(&task.task_id);
        assert!(removed.is_some());
        assert!(registry.get(&task.task_id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn rejects_operations_on_missing_task() {
        let registry = TaskRegistry::new();
        assert!(registry.stop("nonexistent").is_err());
        assert!(registry.update("nonexistent", "msg").is_err());
        assert!(registry.output("nonexistent").is_err());
        assert!(registry.append_output("nonexistent", "data").is_err());
        assert!(registry
            .set_status("nonexistent", TaskStatus::Running)
            .is_err());
    }

    #[test]
    fn task_status_display_all_variants() {
        // given
        let cases = [
            (TaskStatus::Created, "created"),
            (TaskStatus::Running, "running"),
            (TaskStatus::Blocked, "blocked"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Stopped, "stopped"),
        ];

        // when
        let rendered: Vec<_> = cases
            .into_iter()
            .map(|(status, expected)| (status.to_string(), expected))
            .collect();

        // then
        assert_eq!(
            rendered,
            vec![
                ("created".to_string(), "created"),
                ("running".to_string(), "running"),
                ("blocked".to_string(), "blocked"),
                ("completed".to_string(), "completed"),
                ("failed".to_string(), "failed"),
                ("stopped".to_string(), "stopped"),
            ]
        );
    }

    #[test]
    fn stop_rejects_completed_task() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("done", None);
        registry
            .set_status(&task.task_id, TaskStatus::Completed)
            .expect("set status should succeed");

        // when
        let result = registry.stop(&task.task_id);

        // then
        let error = result.expect_err("completed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("completed"));
    }

    #[test]
    fn stop_rejects_failed_task() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("failed", None);
        registry
            .set_status(&task.task_id, TaskStatus::Failed)
            .expect("set status should succeed");

        // when
        let result = registry.stop(&task.task_id);

        // then
        let error = result.expect_err("failed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("failed"));
    }

    #[test]
    fn stop_succeeds_from_created_state() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("created task", None);

        // when
        let stopped = registry.stop(&task.task_id).expect("stop should succeed");

        // then
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert!(stopped.updated_at >= task.updated_at);
    }

    #[test]
    fn new_registry_is_empty() {
        // given
        let registry = TaskRegistry::new();

        // when
        let all_tasks = registry.list(None);

        // then
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(all_tasks.is_empty());
    }

    #[test]
    fn create_without_description() {
        // given
        let registry = TaskRegistry::new();

        // when
        let task = registry.create("Do the thing", None);

        // then
        assert!(task.task_id.starts_with("task_"));
        assert_eq!(task.description, None);
        assert_eq!(task.task_packet, None);
        assert!(task.messages.is_empty());
        assert!(task.output.is_empty());
        assert_eq!(task.team_id, None);
        assert_eq!(task.heartbeat, None);
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        // given
        let registry = TaskRegistry::new();

        // when
        let removed = registry.remove("missing");

        // then
        assert!(removed.is_none());
    }

    #[test]
    fn assign_team_rejects_missing_task() {
        // given
        let registry = TaskRegistry::new();

        // when
        let result = registry.assign_team("missing", "team_123");

        // then
        let error = result.expect_err("missing task should be rejected");
        assert_eq!(error, "task not found: missing");
    }
}
