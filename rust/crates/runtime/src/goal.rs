//! Tier S #1 Goal 持续驱动（Goal Persistence Driver）。
//!
//! 外挂式 Goal 状态机，跨轮 prompt 注入驱动 LLM 持续向目标推进。
//! 设计原则：
//! - **完全外挂**：不修改 query engine，不依赖 ConversationRuntime 的内部状态。
//!   Goal 文本在 `LiveCli::run_turn` 调用 `runtime.run_turn` 之前 prepend 到 user input。
//! - **单文件持久化**：`<workspace>/.claw/goal.json`，覆盖写。简单可靠，避免
//!   污染 session.jsonl 的成熟加载逻辑（session.rs 的 `other =>` 分支会拒绝未知类型）。
//! - **进程级状态**：`GoalManager` 持有 `active: Option<Goal>`，进程启动时从文件
//!   加载，进程结束时无需显式保存（每次状态变更即写入）。
//! - **网络中断暂停**：`LiveCli::run_turn` 错误路径检测网络关键词时调用 `pause()`。
//! - **blocked 三次阈值**：`record_blocked()` 达到 3 次自动 `clear()`。
//!
//! 状态机：
//! ```text
//! Idle ──set──▶ Active ──pause──▶ Paused ──resume──▶ Active
//!                  │                  │
//!                  └──blocked×3──┐    └──clear──▶ Idle
//!                                 ▼
//!                               Idle (auto-clear)
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::json::JsonValue;

/// Goal 状态机当前所处状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoalState {
    /// 活跃推进中。每轮 prompt 注入都会包含 goal 文本。
    Active,
    /// 暂停（通常是网络中断或用户手动暂停）。不再注入 prompt 前缀，
    /// 但保留状态供恢复。`reason` 记录暂停原因，`paused_at_ms` 记录时间戳。
    Paused { reason: String, paused_at_ms: u64 },
    /// 阻塞中（连续失败但未达 3 次阈值）。仍会注入 prompt 前缀，
    /// 但会附带阻塞计数提醒 LLM 改变策略。
    Blocked { reason: String, blocked_at_ms: u64 },
}

/// 一个活跃 goal 的完整状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// Goal 文本（用户期望 LLM 持续推进的目标描述）。
    pub text: String,
    /// 创建时间戳（毫秒）。
    pub created_at_ms: u64,
    /// 当前状态机位置。
    pub state: GoalState,
    /// 连续阻塞次数。达到 3 次自动 clear。
    pub blocked_count: u32,
    /// Token budget（可选）。超过时不再强制注入，由 LLM 自主决定是否继续。
    pub token_budget: Option<u64>,
    /// 已消耗 token 数（累加各轮 usage.total_tokens）。
    pub tokens_used: u64,
}

/// Goal 持久化文件根目录：与 memory.json 平级。
/// 路径布局：`<workspace>/.claw/goal.json`
pub fn goal_json_path(workspace: &Path) -> PathBuf {
    workspace.join(".claw").join("goal.json")
}

/// Goal 持久化管理器。进程级单例，由 LiveCli 持有。
pub struct GoalManager {
    active: Option<Goal>,
    path: PathBuf,
}

impl GoalManager {
    /// 创建空的 GoalManager（不加载文件）。用于新建会话或测试。
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { active: None, path }
    }

    /// 从文件加载 GoalManager。文件不存在或解析失败时返回空管理器
    /// （不阻断启动，goal 状态丢失被视为可接受的退化）。
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let mut manager = Self::new(path.clone());
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(goal) = serde_json::from_str::<Goal>(&contents) {
                manager.active = Some(goal);
            } else if let Ok(json) = JsonValue::parse(&contents) {
                // 兼容旧格式或手动编辑的 JSON。
                if let Some(goal) = Goal::from_json_value(&json) {
                    manager.active = Some(goal);
                }
            }
        }
        manager
    }

    /// 返回当前活跃 goal 的引用（任何状态都算）。
    #[must_use]
    pub fn active(&self) -> Option<&Goal> {
        self.active.as_ref()
    }

    /// 返回当前 goal 是否处于 Active 状态（仅 Active，不含 Paused/Blocked）。
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|g| matches!(g.state, GoalState::Active))
    }

    /// 设置新 goal。如果已有活跃 goal，会被覆盖（旧 goal 不保留历史）。
    /// `text` 为空时返回错误。
    pub fn set(&mut self, text: &str, token_budget: Option<u64>) -> Result<(), GoalError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(GoalError::EmptyText);
        }
        let now = current_time_millis();
        self.active = Some(Goal {
            text: trimmed.to_string(),
            created_at_ms: now,
            state: GoalState::Active,
            blocked_count: 0,
            token_budget,
            tokens_used: 0,
        });
        self.save()
    }

    /// 清除当前 goal。`reason` 记录到日志（不影响状态）。
    pub fn clear(&mut self, _reason: &str) -> Result<(), GoalError> {
        self.active = None;
        self.save()
    }

    /// 暂停当前 goal。仅 Active 状态可暂停；Paused/Blocked 状态调用为 no-op。
    pub fn pause(&mut self, reason: &str) -> Result<(), GoalError> {
        let now = current_time_millis();
        if let Some(goal) = &mut self.active {
            if matches!(goal.state, GoalState::Active) {
                goal.state = GoalState::Paused {
                    reason: reason.to_string(),
                    paused_at_ms: now,
                };
                return self.save();
            }
        }
        Ok(())
    }

    /// 恢复暂停的 goal。仅 Paused 状态可恢复。
    pub fn resume(&mut self) -> Result<(), GoalError> {
        if let Some(goal) = &mut self.active {
            if matches!(goal.state, GoalState::Paused { .. }) {
                goal.state = GoalState::Active;
                return self.save();
            }
        }
        Ok(())
    }

    /// 记录一次阻塞。返回 `true` 表示达到阈值已自动清除。
    /// Active 状态转入 Blocked 状态；Blocked 状态累加计数；Paused 状态忽略。
    pub fn record_blocked(&mut self, reason: &str) -> Result<bool, GoalError> {
        let now = current_time_millis();
        let Some(goal) = self.active.as_mut() else {
            return Ok(false);
        };
        if matches!(goal.state, GoalState::Paused { .. }) {
            return Ok(false);
        }
        goal.blocked_count += 1;
        goal.state = GoalState::Blocked {
            reason: reason.to_string(),
            blocked_at_ms: now,
        };
        if goal.blocked_count >= 3 {
            // 达到阈值自动清除。
            self.active = None;
            self.save()?;
            return Ok(true);
        }
        self.save()?;
        Ok(false)
    }

    /// 累加已消耗 token 数。超过 budget 时不报错（由调用方决定是否暂停）。
    pub fn record_tokens(&mut self, tokens: u64) -> Result<(), GoalError> {
        if let Some(goal) = self.active.as_mut() {
            goal.tokens_used = goal.tokens_used.saturating_add(tokens);
            return self.save();
        }
        Ok(())
    }

    /// 渲染跨轮 prompt 前缀。Active 或 Blocked 状态返回 `Some(prefix)`，
    /// Paused 或无 goal 返回 `None`（不注入）。
    ///
    /// 格式：
    /// ```text
    /// [Goal (active, blocked 0/3, 1500/10000 tokens)]
    /// <goal text>
    ///
    /// ```
    #[must_use]
    pub fn render_prompt_prefix(&self) -> Option<String> {
        let goal = self.active.as_ref()?;
        match goal.state {
            GoalState::Active | GoalState::Blocked { .. } => {
                let state_label = match &goal.state {
                    GoalState::Active => "active".to_string(),
                    GoalState::Blocked { reason, .. } => {
                        format!("blocked: {}", truncate(reason, 40))
                    }
                    GoalState::Paused { .. } => return None,
                };
                let budget_str = match goal.token_budget {
                    Some(budget) => format!("{}/{} tokens", goal.tokens_used, budget),
                    None => format!("{} tokens", goal.tokens_used),
                };
                Some(format!(
                    "[Goal ({state_label}, blocked {}/3, {budget_str})]\n{}\n\n",
                    goal.blocked_count, goal.text
                ))
            }
            GoalState::Paused { .. } => None,
        }
    }

    /// 序列化当前 active goal 到文件。覆盖写（原子写由调用方保证，
    /// 这里用 fs::write 简化实现；goal.json 体积小，覆盖写足够）。
    fn save(&self) -> Result<(), GoalError> {
        if let Some(goal) = &self.active {
            let json = serde_json::to_string_pretty(goal)
                .map_err(|e| GoalError::Serialize(e.to_string()))?;
            // 确保父目录存在。
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent).map_err(|e| GoalError::Io(e.to_string()))?;
            }
            fs::write(&self.path, json).map_err(|e| GoalError::Io(e.to_string()))?;
        } else {
            // 无 active goal：删除文件（如果存在）。
            if self.path.exists() {
                let _ = fs::remove_file(&self.path);
            }
        }
        Ok(())
    }
}

impl Goal {
    /// 兼容从 JsonValue（crate 内部 JSON 表示）反序列化 goal。
    /// 用于支持手动编辑的 JSON 或未来格式迁移。
    fn from_json_value(json: &JsonValue) -> Option<Self> {
        let object = json.as_object()?;
        let text = object.get("text")?.as_str()?.to_string();
        let created_at_ms = object.get("created_at_ms")?.as_i64()? as u64;
        let blocked_count = object
            .get("blocked_count")
            .and_then(JsonValue::as_i64)
            .unwrap_or(0) as u32;
        let token_budget = object
            .get("token_budget")
            .and_then(JsonValue::as_i64)
            .map(|v| v as u64);
        let tokens_used = object
            .get("tokens_used")
            .and_then(JsonValue::as_i64)
            .unwrap_or(0) as u64;
        let state = if let Some(state_obj) = object.get("state").and_then(JsonValue::as_object) {
            let kind = state_obj.get("kind")?.as_str()?;
            match kind {
                "active" => GoalState::Active,
                "paused" => GoalState::Paused {
                    reason: state_obj
                        .get("reason")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    paused_at_ms: state_obj
                        .get("paused_at_ms")
                        .and_then(JsonValue::as_i64)
                        .map(|v| v as u64)
                        .unwrap_or(created_at_ms),
                },
                "blocked" => GoalState::Blocked {
                    reason: state_obj
                        .get("reason")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    blocked_at_ms: state_obj
                        .get("blocked_at_ms")
                        .and_then(JsonValue::as_i64)
                        .map(|v| v as u64)
                        .unwrap_or(created_at_ms),
                },
                _ => return None,
            }
        } else {
            GoalState::Active
        };
        Some(Self {
            text,
            created_at_ms,
            state,
            blocked_count,
            token_budget,
            tokens_used,
        })
    }
}

/// Goal 操作错误。所有变体都不应阻断会话主流程——调用方应记录并继续。
#[derive(Debug)]
pub enum GoalError {
    EmptyText,
    Serialize(String),
    Io(String),
}

impl std::fmt::Display for GoalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyText => write!(f, "goal text cannot be empty"),
            Self::Serialize(msg) => write!(f, "serialize error: {msg}"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for GoalError {}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "claw-goal-test-{}-{}-{}",
            std::process::id(),
            current_time_millis(),
            id
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn new_manager_has_no_active_goal() {
        let manager = GoalManager::new(PathBuf::from("/nonexistent/goal.json"));
        assert!(manager.active().is_none());
        assert!(!manager.is_active());
        assert!(manager.render_prompt_prefix().is_none());
    }

    #[test]
    fn set_goal_creates_active_goal() {
        let dir = temp_dir();
        let path = dir.join("goal.json");
        let mut manager = GoalManager::new(path.clone());

        manager.set("Refactor auth module", Some(10_000)).unwrap();

        assert!(manager.is_active());
        let goal = manager.active().unwrap();
        assert_eq!(goal.text, "Refactor auth module");
        assert_eq!(goal.blocked_count, 0);
        assert_eq!(goal.token_budget, Some(10_000));
        assert_eq!(goal.tokens_used, 0);
        assert!(matches!(goal.state, GoalState::Active));
        assert!(path.exists());
    }

    #[test]
    fn set_empty_text_rejected() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        let result = manager.set("   ", None);
        assert!(matches!(result, Err(GoalError::EmptyText)));
    }

    #[test]
    fn clear_removes_goal_and_file() {
        let dir = temp_dir();
        let path = dir.join("goal.json");
        let mut manager = GoalManager::new(path.clone());
        manager.set("Test goal", None).unwrap();
        assert!(path.exists());

        manager.clear("test done").unwrap();
        assert!(manager.active().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn pause_and_resume_state_transitions() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));

        manager.set("Active goal", None).unwrap();
        assert!(manager.is_active());

        manager.pause("network down").unwrap();
        assert!(!manager.is_active());
        assert!(matches!(
            manager.active().unwrap().state,
            GoalState::Paused { .. }
        ));
        // Paused 状态不注入 prompt 前缀。
        assert!(manager.render_prompt_prefix().is_none());

        manager.resume().unwrap();
        assert!(manager.is_active());
        assert!(manager.render_prompt_prefix().is_some());
    }

    #[test]
    fn pause_when_already_paused_is_noop() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("Goal", None).unwrap();
        manager.pause("first").unwrap();

        // 再次 pause 不应该报错也不应该改变 reason。
        let original_state = manager.active().unwrap().state.clone();
        manager.pause("second").unwrap();
        assert_eq!(manager.active().unwrap().state, original_state);
    }

    #[test]
    fn record_blocked_increments_count() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("Goal", None).unwrap();

        let cleared = manager.record_blocked("tool error 1").unwrap();
        assert!(!cleared);
        assert_eq!(manager.active().unwrap().blocked_count, 1);
        assert!(matches!(
            manager.active().unwrap().state,
            GoalState::Blocked { .. }
        ));

        let cleared = manager.record_blocked("tool error 2").unwrap();
        assert!(!cleared);
        assert_eq!(manager.active().unwrap().blocked_count, 2);
    }

    #[test]
    fn record_blocked_three_times_auto_clears() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("Goal", None).unwrap();

        manager.record_blocked("err1").unwrap();
        manager.record_blocked("err2").unwrap();
        let cleared = manager.record_blocked("err3").unwrap();

        assert!(cleared);
        assert!(manager.active().is_none());
    }

    #[test]
    fn record_blocked_when_paused_is_noop() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("Goal", None).unwrap();
        manager.pause("network").unwrap();

        let cleared = manager.record_blocked("should not count").unwrap();
        assert!(!cleared);
        assert_eq!(manager.active().unwrap().blocked_count, 0);
        assert!(matches!(
            manager.active().unwrap().state,
            GoalState::Paused { .. }
        ));
    }

    #[test]
    fn record_tokens_accumulates() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("Goal", Some(10_000)).unwrap();

        manager.record_tokens(500).unwrap();
        assert_eq!(manager.active().unwrap().tokens_used, 500);

        manager.record_tokens(1500).unwrap();
        assert_eq!(manager.active().unwrap().tokens_used, 2000);
    }

    #[test]
    fn render_prompt_prefix_includes_text_and_stats() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("Refactor module X", Some(10_000)).unwrap();
        manager.record_tokens(1500).unwrap();

        let prefix = manager.render_prompt_prefix().unwrap();
        assert!(prefix.contains("Refactor module X"));
        assert!(prefix.contains("1500/10000 tokens"));
        assert!(prefix.contains("blocked 0/3"));
        assert!(prefix.contains("active"));
    }

    #[test]
    fn render_prompt_prefix_blocked_state_includes_reason() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("Goal", None).unwrap();
        manager
            .record_blocked("tool failure: permission denied")
            .unwrap();

        let prefix = manager.render_prompt_prefix().unwrap();
        assert!(prefix.contains("blocked:"));
        assert!(prefix.contains("permission denied"));
        assert!(prefix.contains("blocked 1/3"));
    }

    #[test]
    fn load_restores_active_goal() {
        let dir = temp_dir();
        let path = dir.join("goal.json");
        {
            let mut manager = GoalManager::new(path.clone());
            manager.set("Persisted goal", Some(5000)).unwrap();
            manager.record_tokens(1000).unwrap();
            manager.record_blocked("err").unwrap();
        }

        let loaded = GoalManager::load(path);
        // 注意：record_blocked 后状态是 Blocked 而非 Active，所以 is_active() 返回 false。
        // 但 active() 仍返回 Some(goal)，说明 goal 已被持久化并恢复。
        assert!(loaded.active().is_some());
        assert!(!loaded.is_active());
        let goal = loaded.active().unwrap();
        assert_eq!(goal.text, "Persisted goal");
        assert_eq!(goal.token_budget, Some(5000));
        assert_eq!(goal.tokens_used, 1000);
        assert_eq!(goal.blocked_count, 1);
        assert!(matches!(goal.state, GoalState::Blocked { .. }));
    }

    #[test]
    fn load_missing_file_returns_empty_manager() {
        let manager = GoalManager::load(PathBuf::from("/nonexistent/path/goal.json"));
        assert!(manager.active().is_none());
    }

    #[test]
    fn load_corrupt_file_returns_empty_manager() {
        let dir = temp_dir();
        let path = dir.join("goal.json");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"not valid json {{{").unwrap();

        let manager = GoalManager::load(path);
        assert!(manager.active().is_none());
    }

    #[test]
    fn set_overwrites_existing_goal() {
        let dir = temp_dir();
        let mut manager = GoalManager::new(dir.join("goal.json"));
        manager.set("First goal", None).unwrap();
        manager.set("Second goal", Some(2000)).unwrap();

        let goal = manager.active().unwrap();
        assert_eq!(goal.text, "Second goal");
        assert_eq!(goal.token_budget, Some(2000));
        assert_eq!(goal.blocked_count, 0); // 重置计数
        assert_eq!(goal.tokens_used, 0); // 重置 token
    }

    #[test]
    fn goal_json_path_under_claw_dir() {
        let workspace = Path::new("/tmp/project");
        let path = goal_json_path(workspace);
        assert_eq!(path, Path::new("/tmp/project/.claw/goal.json"));
    }
}
