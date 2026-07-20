#![allow(clippy::must_use_candidate)]
//! In-memory registries for Team and Cron lifecycle management.
//!
//! Provides TeamCreate/Delete and CronCreate/Delete/List runtime backing
//! to replace the stub implementations in the tools crate.
//!
//! Step 3.2-b: 支持可选 JSON 文件持久化。通过 [`TeamRegistry::with_persistence`]
//! 或 [`CronRegistry::with_persistence`] 启用;每次 mutation 自动 flush。
//! 未配置持久化时退化为纯内存(向后兼容)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub team_id: String,
    pub name: String,
    pub task_ids: Vec<String>,
    pub status: TeamStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    Created,
    Running,
    Completed,
    Deleted,
}

impl std::fmt::Display for TeamStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Deleted => write!(f, "deleted"),
        }
    }
}

/// 持久化镜像 — JSON 文件的 wire format。
/// 与 `TeamInner` 一对一映射,但 `file_path` 不序列化。
#[derive(Debug, Default, Serialize, Deserialize)]
struct TeamPersistenceMirror {
    teams: Vec<Team>,
    counter: u64,
}

#[derive(Debug, Default)]
struct TeamInner {
    teams: HashMap<String, Team>,
    counter: u64,
    /// 启用持久化时的 JSON 文件路径。`None` 表示纯内存模式。
    file_path: Option<PathBuf>,
    /// 最近一次 `persist_internal` 失败的错误信息。`None` 表示无错误或未启用持久化。
    last_persist_error: Option<String>,
}

impl TeamInner {
    fn to_mirror(&self) -> TeamPersistenceMirror {
        let mut teams: Vec<Team> = self.teams.values().cloned().collect();
        // 按 team_id 排序,保证 wire format 稳定(便于 diff/dedupe)。
        teams.sort_by(|a, b| a.team_id.cmp(&b.team_id));
        TeamPersistenceMirror {
            teams,
            counter: self.counter,
        }
    }

    fn from_mirror(mirror: TeamPersistenceMirror) -> Self {
        let teams: HashMap<String, Team> = mirror
            .teams
            .into_iter()
            .map(|t| (t.team_id.clone(), t))
            .collect();
        Self {
            teams,
            counter: mirror.counter,
            file_path: None,
            last_persist_error: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TeamRegistry {
    inner: Arc<Mutex<TeamInner>>,
}

impl TeamRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建带 JSON 文件持久化的 registry。
    ///
    /// 若 `path` 存在且为合法 JSON,则加载已有 teams/counter。
    /// 若 `path` 不存在,则创建空 registry,首次 mutation 时创建文件。
    /// 父目录必须存在(本方法不创建目录)。
    pub fn with_persistence(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let inner = if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            let mirror: TeamPersistenceMirror = serde_json::from_str(&data).map_err(|err| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string())
            })?;
            let mut inner = TeamInner::from_mirror(mirror);
            inner.file_path = Some(path);
            inner
        } else {
            TeamInner {
                file_path: Some(path),
                ..TeamInner::default()
            }
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    pub fn create(&self, name: &str, task_ids: Vec<String>) -> Team {
        let mut inner = self.inner.lock().expect("team registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let team_id = format!("team_{:08x}_{}", ts, inner.counter);
        let team = Team {
            team_id: team_id.clone(),
            name: name.to_owned(),
            task_ids,
            status: TeamStatus::Created,
            created_at: ts,
            updated_at: ts,
        };
        inner.teams.insert(team_id, team.clone());
        persist_team_inner(&mut inner);
        team
    }

    pub fn get(&self, team_id: &str) -> Option<Team> {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        inner.teams.get(team_id).cloned()
    }

    pub fn list(&self) -> Vec<Team> {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        inner.teams.values().cloned().collect()
    }

    pub fn delete(&self, team_id: &str) -> Result<Team, String> {
        let mut inner = self.inner.lock().expect("team registry lock poisoned");
        let team = inner
            .teams
            .get_mut(team_id)
            .ok_or_else(|| format!("team not found: {team_id}"))?;
        team.status = TeamStatus::Deleted;
        team.updated_at = now_secs();
        let team = team.clone();
        persist_team_inner(&mut inner);
        Ok(team)
    }

    pub fn remove(&self, team_id: &str) -> Option<Team> {
        let mut inner = self.inner.lock().expect("team registry lock poisoned");
        let removed = inner.teams.remove(team_id);
        if removed.is_some() {
            persist_team_inner(&mut inner);
        }
        removed
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        inner.teams.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 显式 flush 到磁盘。未启用持久化时返回 Ok(())。
    pub fn flush(&self) -> std::io::Result<()> {
        let mut inner = self.inner.lock().expect("team registry lock poisoned");
        persist_team_inner_result(&mut inner)
    }

    /// 返回最近一次持久化失败的错误信息(若有)。
    /// `None` 表示无错误或未启用持久化。
    #[must_use]
    pub fn last_persist_error(&self) -> Option<String> {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        inner.last_persist_error.clone()
    }

    /// 当前是否启用了 JSON 持久化。
    #[must_use]
    pub fn is_persistent(&self) -> bool {
        let inner = self.inner.lock().expect("team registry lock poisoned");
        inner.file_path.is_some()
    }
}

/// 把 `TeamInner` 持久化到 `file_path`(若已配置)。
/// 失败时记录错误到 `last_persist_error`,**不**回滚内存中的 mutation
/// (mutation 已成功,丢失持久化不应让用户操作看起来失败)。
fn persist_team_inner(inner: &mut TeamInner) {
    if let Err(err) = persist_team_inner_result(inner) {
        inner.last_persist_error = Some(err.to_string());
    }
}

fn persist_team_inner_result(inner: &mut TeamInner) -> std::io::Result<()> {
    let Some(path) = inner.file_path.as_ref() else {
        // 纯内存模式,无需持久化。清空 last_persist_error。
        inner.last_persist_error = None;
        return Ok(());
    };
    let mirror = inner.to_mirror();
    let json = serde_json::to_string_pretty(&mirror)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    // 原子写入:先写临时文件,再 rename,避免崩溃导致部分写入。
    atomic_write(path, &json)?;
    inner.last_persist_error = None;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEntry {
    pub cron_id: String,
    pub schedule: String,
    pub prompt: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_run_at: Option<u64>,
    pub run_count: u64,
}

/// 持久化镜像 — JSON 文件的 wire format。
#[derive(Debug, Default, Serialize, Deserialize)]
struct CronPersistenceMirror {
    entries: Vec<CronEntry>,
    counter: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CronRegistry {
    inner: Arc<Mutex<CronInner>>,
}

#[derive(Debug, Default)]
struct CronInner {
    entries: HashMap<String, CronEntry>,
    counter: u64,
    file_path: Option<PathBuf>,
    last_persist_error: Option<String>,
}

impl CronInner {
    fn to_mirror(&self) -> CronPersistenceMirror {
        let mut entries: Vec<CronEntry> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| a.cron_id.cmp(&b.cron_id));
        CronPersistenceMirror {
            entries,
            counter: self.counter,
        }
    }

    fn from_mirror(mirror: CronPersistenceMirror) -> Self {
        let entries: HashMap<String, CronEntry> = mirror
            .entries
            .into_iter()
            .map(|e| (e.cron_id.clone(), e))
            .collect();
        Self {
            entries,
            counter: mirror.counter,
            file_path: None,
            last_persist_error: None,
        }
    }
}

impl CronRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建带 JSON 文件持久化的 registry。
    pub fn with_persistence(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let inner = if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            let mirror: CronPersistenceMirror = serde_json::from_str(&data).map_err(|err| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string())
            })?;
            let mut inner = CronInner::from_mirror(mirror);
            inner.file_path = Some(path);
            inner
        } else {
            CronInner {
                file_path: Some(path),
                ..CronInner::default()
            }
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    pub fn create(&self, schedule: &str, prompt: &str, description: Option<&str>) -> CronEntry {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let cron_id = format!("cron_{:08x}_{}", ts, inner.counter);
        let entry = CronEntry {
            cron_id: cron_id.clone(),
            schedule: schedule.to_owned(),
            prompt: prompt.to_owned(),
            description: description.map(str::to_owned),
            enabled: true,
            created_at: ts,
            updated_at: ts,
            last_run_at: None,
            run_count: 0,
        };
        inner.entries.insert(cron_id, entry.clone());
        persist_cron_inner(&mut inner);
        entry
    }

    pub fn get(&self, cron_id: &str) -> Option<CronEntry> {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.entries.get(cron_id).cloned()
    }

    pub fn list(&self, enabled_only: bool) -> Vec<CronEntry> {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner
            .entries
            .values()
            .filter(|e| !enabled_only || e.enabled)
            .cloned()
            .collect()
    }

    pub fn delete(&self, cron_id: &str) -> Result<CronEntry, String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .remove(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        persist_cron_inner(&mut inner);
        Ok(entry)
    }

    /// Disable a cron entry without removing it.
    pub fn disable(&self, cron_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        entry.enabled = false;
        entry.updated_at = now_secs();
        persist_cron_inner(&mut inner);
        Ok(())
    }

    /// Record a cron run.
    pub fn record_run(&self, cron_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        entry.last_run_at = Some(now_secs());
        entry.run_count += 1;
        entry.updated_at = now_secs();
        persist_cron_inner(&mut inner);
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 显式 flush 到磁盘。未启用持久化时返回 Ok(())。
    pub fn flush(&self) -> std::io::Result<()> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        persist_cron_inner_result(&mut inner)
    }

    #[must_use]
    pub fn last_persist_error(&self) -> Option<String> {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.last_persist_error.clone()
    }

    #[must_use]
    pub fn is_persistent(&self) -> bool {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.file_path.is_some()
    }
}

fn persist_cron_inner(inner: &mut CronInner) {
    if let Err(err) = persist_cron_inner_result(inner) {
        inner.last_persist_error = Some(err.to_string());
    }
}

fn persist_cron_inner_result(inner: &mut CronInner) -> std::io::Result<()> {
    let Some(path) = inner.file_path.as_ref() else {
        inner.last_persist_error = None;
        return Ok(());
    };
    let mirror = inner.to_mirror();
    let json = serde_json::to_string_pretty(&mirror)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    atomic_write(path, &json)?;
    inner.last_persist_error = None;
    Ok(())
}

/// 原子写入:先写到 `<path>.tmp`,再 rename 到 `path`。
/// 避免崩溃导致部分写入破坏 JSON 文件。
fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json")
    ));
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Team tests ──────────────────────────────────────

    #[test]
    fn creates_and_retrieves_team() {
        let registry = TeamRegistry::new();
        let team = registry.create("Alpha Squad", vec!["task_001".into(), "task_002".into()]);
        assert_eq!(team.name, "Alpha Squad");
        assert_eq!(team.task_ids.len(), 2);
        assert_eq!(team.status, TeamStatus::Created);

        let fetched = registry.get(&team.team_id).expect("team should exist");
        assert_eq!(fetched.team_id, team.team_id);
    }

    #[test]
    fn lists_and_deletes_teams() {
        let registry = TeamRegistry::new();
        let t1 = registry.create("Team A", vec![]);
        let t2 = registry.create("Team B", vec![]);

        let all = registry.list();
        assert_eq!(all.len(), 2);

        let deleted = registry.delete(&t1.team_id).expect("delete should succeed");
        assert_eq!(deleted.status, TeamStatus::Deleted);

        // Team is still listable (soft delete)
        let still_there = registry.get(&t1.team_id).unwrap();
        assert_eq!(still_there.status, TeamStatus::Deleted);

        // Hard remove
        registry.remove(&t2.team_id);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn rejects_missing_team_operations() {
        let registry = TeamRegistry::new();
        assert!(registry.delete("nonexistent").is_err());
        assert!(registry.get("nonexistent").is_none());
    }

    // ── Cron tests ──────────────────────────────────────

    #[test]
    fn creates_and_retrieves_cron() {
        let registry = CronRegistry::new();
        let entry = registry.create("0 * * * *", "Check status", Some("hourly check"));
        assert_eq!(entry.schedule, "0 * * * *");
        assert_eq!(entry.prompt, "Check status");
        assert!(entry.enabled);
        assert_eq!(entry.run_count, 0);
        assert!(entry.last_run_at.is_none());

        let fetched = registry.get(&entry.cron_id).expect("cron should exist");
        assert_eq!(fetched.cron_id, entry.cron_id);
    }

    #[test]
    fn lists_with_enabled_filter() {
        let registry = CronRegistry::new();
        let c1 = registry.create("* * * * *", "Task 1", None);
        let c2 = registry.create("0 * * * *", "Task 2", None);
        registry
            .disable(&c1.cron_id)
            .expect("disable should succeed");

        let all = registry.list(false);
        assert_eq!(all.len(), 2);

        let enabled_only = registry.list(true);
        assert_eq!(enabled_only.len(), 1);
        assert_eq!(enabled_only[0].cron_id, c2.cron_id);
    }

    #[test]
    fn deletes_cron_entry() {
        let registry = CronRegistry::new();
        let entry = registry.create("* * * * *", "To delete", None);
        let deleted = registry
            .delete(&entry.cron_id)
            .expect("delete should succeed");
        assert_eq!(deleted.cron_id, entry.cron_id);
        assert!(registry.get(&entry.cron_id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn records_cron_runs() {
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);
        registry.record_run(&entry.cron_id).unwrap();
        registry.record_run(&entry.cron_id).unwrap();

        let fetched = registry.get(&entry.cron_id).unwrap();
        assert_eq!(fetched.run_count, 2);
        assert!(fetched.last_run_at.is_some());
    }

    #[test]
    fn rejects_missing_cron_operations() {
        let registry = CronRegistry::new();
        assert!(registry.delete("nonexistent").is_err());
        assert!(registry.disable("nonexistent").is_err());
        assert!(registry.record_run("nonexistent").is_err());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn team_status_display_all_variants() {
        // given
        let cases = [
            (TeamStatus::Created, "created"),
            (TeamStatus::Running, "running"),
            (TeamStatus::Completed, "completed"),
            (TeamStatus::Deleted, "deleted"),
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
                ("completed".to_string(), "completed"),
                ("deleted".to_string(), "deleted"),
            ]
        );
    }

    #[test]
    fn new_team_registry_is_empty() {
        // given
        let registry = TeamRegistry::new();

        // when
        let teams = registry.list();

        // then
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(teams.is_empty());
    }

    #[test]
    fn team_remove_nonexistent_returns_none() {
        // given
        let registry = TeamRegistry::new();

        // when
        let removed = registry.remove("missing");

        // then
        assert!(removed.is_none());
    }

    #[test]
    fn team_len_transitions() {
        // given
        let registry = TeamRegistry::new();

        // when
        let alpha = registry.create("Alpha", vec![]);
        let beta = registry.create("Beta", vec![]);
        let after_create = registry.len();
        registry.remove(&alpha.team_id);
        let after_first_remove = registry.len();
        registry.remove(&beta.team_id);

        // then
        assert_eq!(after_create, 2);
        assert_eq!(after_first_remove, 1);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn cron_list_all_disabled_returns_empty_for_enabled_only() {
        // given
        let registry = CronRegistry::new();
        let first = registry.create("* * * * *", "Task 1", None);
        let second = registry.create("0 * * * *", "Task 2", None);
        registry
            .disable(&first.cron_id)
            .expect("disable should succeed");
        registry
            .disable(&second.cron_id)
            .expect("disable should succeed");

        // when
        let enabled_only = registry.list(true);
        let all_entries = registry.list(false);

        // then
        assert!(enabled_only.is_empty());
        assert_eq!(all_entries.len(), 2);
    }

    #[test]
    fn cron_create_without_description() {
        // given
        let registry = CronRegistry::new();

        // when
        let entry = registry.create("*/15 * * * *", "Check health", None);

        // then
        assert!(entry.cron_id.starts_with("cron_"));
        assert_eq!(entry.description, None);
        assert!(entry.enabled);
        assert_eq!(entry.run_count, 0);
        assert_eq!(entry.last_run_at, None);
    }

    #[test]
    fn new_cron_registry_is_empty() {
        // given
        let registry = CronRegistry::new();

        // when
        let enabled_only = registry.list(true);
        let all_entries = registry.list(false);

        // then
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(enabled_only.is_empty());
        assert!(all_entries.is_empty());
    }

    #[test]
    fn cron_record_run_updates_timestamp_and_counter() {
        // given
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);

        // when
        registry
            .record_run(&entry.cron_id)
            .expect("first run should succeed");
        registry
            .record_run(&entry.cron_id)
            .expect("second run should succeed");
        let fetched = registry.get(&entry.cron_id).expect("entry should exist");

        // then
        assert_eq!(fetched.run_count, 2);
        assert!(fetched.last_run_at.is_some());
        assert!(fetched.updated_at >= entry.updated_at);
    }

    #[test]
    fn cron_disable_updates_timestamp() {
        // given
        let registry = CronRegistry::new();
        let entry = registry.create("0 0 * * *", "Nightly", None);

        // when
        registry
            .disable(&entry.cron_id)
            .expect("disable should succeed");
        let fetched = registry.get(&entry.cron_id).expect("entry should exist");

        // then
        assert!(!fetched.enabled);
        assert!(fetched.updated_at >= entry.updated_at);
    }

    // ── Step 3.2-b: JSON 持久化测试 ──────────────────────

    fn temp_path(suffix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("claw-team-cron-{nanos}-{suffix}.json"));
        p
    }

    #[test]
    fn team_registry_persistence_round_trip() {
        let path = temp_path("team-rt");
        let _ = std::fs::remove_file(&path);

        // 创建持久化 registry + 添加 2 个 team
        let registry = TeamRegistry::with_persistence(&path).expect("create persistent registry");
        assert!(registry.is_persistent());
        let t1 = registry.create("Alpha", vec!["t1".into()]);
        let t2 = registry.create("Beta", vec![]);
        registry.delete(&t1.team_id).unwrap();
        assert_eq!(registry.len(), 2);
        assert!(registry.last_persist_error().is_none());

        // 重新加载 — 数据应该一致
        let reloaded = TeamRegistry::with_persistence(&path).expect("reload registry");
        assert_eq!(reloaded.len(), 2);
        let t1_reloaded = reloaded.get(&t1.team_id).expect("t1 should persist");
        assert_eq!(t1_reloaded.status, TeamStatus::Deleted);
        let t2_reloaded = reloaded.get(&t2.team_id).expect("t2 should persist");
        assert_eq!(t2_reloaded.name, "Beta");

        // counter 应该恢复 — 新创建的 team 不应覆盖已有 ID
        let t3 = reloaded.create("Gamma", vec![]);
        assert_ne!(t3.team_id, t1.team_id);
        assert_ne!(t3.team_id, t2.team_id);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn team_registry_persistence_creates_file_on_first_mutation() {
        let path = temp_path("team-new");
        let _ = std::fs::remove_file(&path);
        assert!(!path.exists());

        let registry = TeamRegistry::with_persistence(&path).expect("create registry");
        // 还没 mutation,文件不应存在
        assert!(!path.exists());

        registry.create("First", vec![]);
        // mutation 后文件应该被创建
        assert!(path.exists(), "persistence file should be created after mutation");

        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"counter\": 1"));
        assert!(json.contains("First"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cron_registry_persistence_round_trip() {
        let path = temp_path("cron-rt");
        let _ = std::fs::remove_file(&path);

        let registry = CronRegistry::with_persistence(&path).expect("create persistent cron");
        assert!(registry.is_persistent());
        let c1 = registry.create("0 * * * *", "Hourly task", Some("hourly"));
        let c2 = registry.create("0 0 * * *", "Daily", None);
        registry.record_run(&c1.cron_id).unwrap();
        registry.disable(&c2.cron_id).unwrap();

        // 重载
        let reloaded = CronRegistry::with_persistence(&path).expect("reload cron");
        assert_eq!(reloaded.len(), 2);
        let c1_reloaded = reloaded.get(&c1.cron_id).unwrap();
        assert_eq!(c1_reloaded.run_count, 1);
        assert!(c1_reloaded.last_run_at.is_some());
        let c2_reloaded = reloaded.get(&c2.cron_id).unwrap();
        assert!(!c2_reloaded.enabled);

        // counter 恢复 — 新 entry 不覆盖
        let c3 = reloaded.create("*/5 * * * *", "New", None);
        assert_ne!(c3.cron_id, c1.cron_id);
        assert_ne!(c3.cron_id, c2.cron_id);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cron_registry_persistence_delete_survives_reload() {
        let path = temp_path("cron-del");
        let _ = std::fs::remove_file(&path);

        let registry = CronRegistry::with_persistence(&path).expect("create");
        let c1 = registry.create("* * * * *", "Keep", None);
        let c2 = registry.create("0 0 * * *", "Delete", None);
        registry.delete(&c2.cron_id).unwrap();
        assert_eq!(registry.len(), 1);

        let reloaded = CronRegistry::with_persistence(&path).expect("reload");
        assert_eq!(reloaded.len(), 1);
        assert!(reloaded.get(&c1.cron_id).is_some());
        assert!(reloaded.get(&c2.cron_id).is_none());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn team_registry_in_memory_mode_is_not_persistent() {
        let registry = TeamRegistry::new();
        assert!(!registry.is_persistent());
        registry.create("X", vec![]);
        assert!(registry.last_persist_error().is_none());
        assert!(registry.flush().is_ok());
    }

    #[test]
    fn cron_registry_in_memory_mode_is_not_persistent() {
        let registry = CronRegistry::new();
        assert!(!registry.is_persistent());
        registry.create("* * * * *", "X", None);
        assert!(registry.last_persist_error().is_none());
        assert!(registry.flush().is_ok());
    }

    #[test]
    fn team_registry_with_persistence_nonexistent_path_is_ok() {
        let path = temp_path("team-nonexist");
        let _ = std::fs::remove_file(&path);
        // 路径不存在时应返回 Ok,registry 为空且 file_path=Some(path)
        let registry = TeamRegistry::with_persistence(&path).expect("nonexistent path should be ok");
        assert!(registry.is_persistent());
        assert!(registry.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cron_registry_with_persistence_invalid_json_returns_err() {
        let path = temp_path("cron-bad");
        std::fs::write(&path, "not valid json {{{").unwrap();
        let err = CronRegistry::with_persistence(&path).expect_err("invalid JSON should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn flush_writes_to_disk_for_persistent_registry() {
        let path = temp_path("team-flush");
        let _ = std::fs::remove_file(&path);

        let registry = TeamRegistry::with_persistence(&path).unwrap();
        registry.create("A", vec![]);
        // create 已经触发自动持久化,但 flush 应再次成功
        registry.flush().expect("flush should succeed");
        assert!(path.exists());

        std::fs::remove_file(&path).ok();
    }
}
