//! Multi-Agent Coordinator — Step 3.2 多 agent 协调器。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 3.2
//!
//! 架构(参考 Claude Code 源码泄露):
//! - 三种编排模式:
//!   - [`CoordinationMode::Fork`]:主 agent 派生子 agent 并行执行,主 agent 收集结果。
//!   - [`CoordinationMode::Teammate`]:多个 agent 协作,通过共享 TaskRegistry 通信。
//!   - [`CoordinationMode::Worktree`]:每个 agent 独立 git worktree,避免文件冲突。
//! - [`MultiAgentCoordinator`]:统一入口,管理 agent 生命周期 + 任务分派。
//! - 与 [`TaskRegistry`](crate::task_registry::TaskRegistry) 对接。
//! - 与 [`VerifierAgent`](crate::verifier::VerifierAgent) 对接:子 agent 完成后校验。
//!
//! **缓存保护**(详见 §5.2):
//! 每个子 agent 走独立 LLM 请求 + 独立 prompt cache,不污染主 agent 缓存。
//! "Subagent as Tool" 模式 — 主 agent 通过 tool call 接口调用子 agent。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 多 agent 编排模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationMode {
    /// Fork:主 agent 派生子 agent 并行执行,主 agent 收集结果。
    Fork,
    /// Teammate:多个 agent 协作,通过共享 TaskRegistry 通信。
    Teammate,
    /// Worktree:每个 agent 独立 git worktree,避免文件冲突。
    Worktree,
}

/// 子 agent 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    /// 已创建,未启动。
    Created,
    /// 运行中。
    Running,
    /// 已完成(成功)。
    Completed,
    /// 已失败。
    Failed,
    /// 已取消。
    Cancelled,
}

/// 子 agent 描述符。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subagent {
    /// 全局唯一 ID。
    pub id: String,
    /// 人类可读名称。
    pub name: String,
    /// 编排模式。
    pub mode: CoordinationMode,
    /// 分配的任务描述。
    pub task: String,
    /// 当前状态。
    pub status: SubagentStatus,
    /// 工作目录(Worktree 模式下为独立 git worktree 路径)。
    pub workdir: Option<PathBuf>,
    /// 创建时间(unix epoch 秒)。
    pub created_at: u64,
    /// 完成时间(unix epoch 秒,None 表示未完成)。
    pub completed_at: Option<u64>,
    /// 结果(完成后填充)。
    pub result: Option<String>,
}

/// 多 agent 协调器 — 管理 agent 生命周期 + 任务分派。
#[derive(Debug, Clone, Default)]
pub struct MultiAgentCoordinator {
    /// 已注册的子 agent(按 ID 索引)。
    subagents: Arc<Mutex<HashMap<String, Subagent>>>,
    /// ID 计数器。
    id_counter: Arc<Mutex<u64>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl MultiAgentCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 派生子 agent。
    ///
    /// 根据 `mode` 创建子 agent:
    /// - `Fork` → 创建子 agent,workdir=None(共享主 agent 工作目录)
    /// - `Teammate` → 创建子 agent,workdir=None(通过 TaskRegistry 通信)
    /// - `Worktree` → 创建子 agent,workdir=Some(worktree_path)(独立 git worktree)
    pub fn spawn(
        &self,
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
    ) -> String {
        let name = name.into();
        let task = task.into();
        let mut counter = self.id_counter.lock().expect("id counter lock poisoned");
        *counter += 1;
        let id = format!("subagent-{}", *counter);
        drop(counter);

        let workdir = match mode {
            CoordinationMode::Worktree => Some(PathBuf::from(format!(".claw/worktrees/{id}"))),
            _ => None,
        };

        // P2-2:Worktree 模式下检测 branch lock 碰撞(宽松模式)。
        // 碰撞时记录警告到 stderr(不阻止 spawn,向后兼容)。
        if mode == CoordinationMode::Worktree {
            let intent = crate::branch_lock::BranchLockIntent {
                lane_id: id.clone(),
                branch: format!("worktree-{}", id),
                worktree: workdir.as_ref().map(|p| p.to_string_lossy().to_string()),
                modules: Vec::new(),
            };
            let collisions = crate::branch_lock::detect_branch_lock_collisions(&[intent]);
            if !collisions.is_empty() {
                eprintln!(
                    "[branch_lock] {} collision(s) detected for worktree spawn {}, proceeding anyway",
                    collisions.len(),
                    id
                );
            }
        }

        let subagent = Subagent {
            id: id.clone(),
            name,
            mode,
            task,
            status: SubagentStatus::Created,
            workdir,
            created_at: now_secs(),
            completed_at: None,
            result: None,
        };

        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        agents.insert(id.clone(), subagent);
        id
    }

    /// 启动子 agent(标记为 Running)。
    pub fn start(&self, subagent_id: &str) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status != SubagentStatus::Created {
            return Err(format!(
                "subagent {subagent_id} cannot start from status {:?}",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Running;
        Ok(())
    }

    /// 标记子 agent 完成(成功)。
    pub fn complete(&self, subagent_id: &str, result: impl Into<String>) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status != SubagentStatus::Running {
            return Err(format!(
                "subagent {subagent_id} cannot complete from status {:?}",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Completed;
        agent.completed_at = Some(now_secs());
        agent.result = Some(result.into());
        Ok(())
    }

    /// 标记子 agent 失败。
    pub fn fail(&self, subagent_id: &str, error: impl Into<String>) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status != SubagentStatus::Running {
            return Err(format!(
                "subagent {subagent_id} cannot fail from status {:?}",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Failed;
        agent.completed_at = Some(now_secs());
        agent.result = Some(format!("error: {}", error.into()));
        Ok(())
    }

    /// 取消子 agent。
    pub fn cancel(&self, subagent_id: &str) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if agent.status == SubagentStatus::Completed
            || agent.status == SubagentStatus::Failed
            || agent.status == SubagentStatus::Cancelled
        {
            return Err(format!(
                "subagent {subagent_id} cannot cancel from terminal status {:?}",
                agent.status
            ));
        }
        agent.status = SubagentStatus::Cancelled;
        agent.completed_at = Some(now_secs());
        Ok(())
    }

    /// 获取子 agent 引用。
    #[must_use]
    pub fn get(&self, subagent_id: &str) -> Option<Subagent> {
        self.subagents
            .lock()
            .expect("subagents lock poisoned")
            .get(subagent_id)
            .cloned()
    }

    /// 获取所有子 agent。
    #[must_use]
    pub fn list(&self) -> Vec<Subagent> {
        self.subagents
            .lock()
            .expect("subagents lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// 获取按状态过滤的子 agent。
    #[must_use]
    pub fn list_by_status(&self, status: SubagentStatus) -> Vec<Subagent> {
        self.subagents
            .lock()
            .expect("subagents lock poisoned")
            .values()
            .filter(|agent| agent.status == status)
            .cloned()
            .collect()
    }

    /// 等待所有子 agent 完成(轮询,返回最终状态统计)。
    ///
    /// 当前为同步实现(无实际异步等待),返回当前快照。
    /// 未来扩展:接入 tokio 异步等待。
    #[must_use]
    pub fn join_all(&self) -> JoinStats {
        let agents = self
            .subagents
            .lock()
            .expect("subagents lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let total = agents.len() as u64;
        let completed = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Completed)
            .count() as u64;
        let failed = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Failed)
            .count() as u64;
        let running = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Running)
            .count() as u64;
        let cancelled = agents
            .iter()
            .filter(|a| a.status == SubagentStatus::Cancelled)
            .count() as u64;
        JoinStats {
            total,
            completed,
            failed,
            running,
            cancelled,
        }
    }
}

/// join_all 返回的统计信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinStats {
    pub total: u64,
    pub completed: u64,
    pub failed: u64,
    pub running: u64,
    pub cancelled: u64,
}

impl JoinStats {
    /// 是否所有子 agent 都已到达终态(completed/failed/cancelled)。
    #[must_use]
    pub fn all_done(&self) -> bool {
        self.running == 0 && self.completed + self.failed + self.cancelled == self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_fork_mode_creates_subagent_without_workdir() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("test-agent", "do something", CoordinationMode::Fork);
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.mode, CoordinationMode::Fork);
        assert!(agent.workdir.is_none());
        assert_eq!(agent.status, SubagentStatus::Created);
    }

    #[test]
    fn spawn_worktree_mode_creates_subagent_with_workdir() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("worktree-agent", "refactor", CoordinationMode::Worktree);
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.mode, CoordinationMode::Worktree);
        assert!(agent.workdir.is_some());
        assert!(agent
            .workdir
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .contains(&id));
    }

    #[test]
    fn start_transitions_created_to_running() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).expect("start should succeed");
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Running);
    }

    #[test]
    fn start_fails_from_terminal_status() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "done").unwrap();
        // Cannot start from Completed
        let err = coord.start(&id).unwrap_err();
        assert!(err.contains("cannot start"));
    }

    #[test]
    fn complete_transitions_running_to_completed() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "all done").unwrap();
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Completed);
        assert_eq!(agent.result.as_deref(), Some("all done"));
        assert!(agent.completed_at.is_some());
    }

    #[test]
    fn fail_transitions_running_to_failed() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.fail(&id, "compilation error").unwrap();
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Failed);
        assert!(agent.result.as_ref().unwrap().contains("compilation error"));
    }

    #[test]
    fn cancel_transitions_non_terminal_to_cancelled() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.cancel(&id).unwrap();
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.status, SubagentStatus::Cancelled);
    }

    #[test]
    fn cancel_fails_from_terminal_status() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("agent", "task", CoordinationMode::Fork);
        coord.start(&id).unwrap();
        coord.complete(&id, "done").unwrap();
        let err = coord.cancel(&id).unwrap_err();
        assert!(err.contains("cannot cancel"));
    }

    #[test]
    fn list_returns_all_subagents() {
        let coord = MultiAgentCoordinator::new();
        coord.spawn("a1", "t1", CoordinationMode::Fork);
        coord.spawn("a2", "t2", CoordinationMode::Teammate);
        coord.spawn("a3", "t3", CoordinationMode::Worktree);
        assert_eq!(coord.list().len(), 3);
    }

    #[test]
    fn list_by_status_filters_correctly() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        coord.start(&id1).unwrap();
        coord.start(&id2).unwrap();
        coord.complete(&id1, "done").unwrap();
        assert_eq!(coord.list_by_status(SubagentStatus::Completed).len(), 1);
        assert_eq!(coord.list_by_status(SubagentStatus::Running).len(), 1);
        assert_eq!(coord.list_by_status(SubagentStatus::Created).len(), 0);
    }

    #[test]
    fn join_all_returns_correct_stats() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        let id3 = coord.spawn("a3", "t3", CoordinationMode::Fork);
        coord.start(&id1).unwrap();
        coord.start(&id2).unwrap();
        coord.start(&id3).unwrap();
        coord.complete(&id1, "done").unwrap();
        coord.fail(&id2, "error").unwrap();

        let stats = coord.join_all();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.running, 1);
        assert_eq!(stats.cancelled, 0);
        assert!(!stats.all_done());
    }

    #[test]
    fn join_all_all_done_when_no_running() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        coord.start(&id1).unwrap();
        coord.start(&id2).unwrap();
        coord.complete(&id1, "done").unwrap();
        coord.cancel(&id2).unwrap();

        let stats = coord.join_all();
        assert!(stats.all_done());
    }

    #[test]
    fn teammate_mode_creates_subagent_without_workdir() {
        let coord = MultiAgentCoordinator::new();
        let id = coord.spawn("teammate", "collab task", CoordinationMode::Teammate);
        let agent = coord.get(&id).expect("agent should exist");
        assert_eq!(agent.mode, CoordinationMode::Teammate);
        assert!(agent.workdir.is_none());
    }

    #[test]
    fn subagent_ids_are_unique() {
        let coord = MultiAgentCoordinator::new();
        let id1 = coord.spawn("a1", "t1", CoordinationMode::Fork);
        let id2 = coord.spawn("a2", "t2", CoordinationMode::Fork);
        assert_ne!(id1, id2);
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let coord = MultiAgentCoordinator::new();
        assert!(coord.get("nonexistent").is_none());
    }

    #[test]
    fn start_returns_error_for_unknown_id() {
        let coord = MultiAgentCoordinator::new();
        assert!(coord.start("nonexistent").is_err());
    }
}
