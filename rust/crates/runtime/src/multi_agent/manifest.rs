//! 子代理生命周期清单(manifest) — 目录层级控制设计文档 §2.4 (Epic 2 A2.3b)。
//!
//! 父会话将每个子代理的 `subagent_id → workspace` 映射与状态流转持久化到
//! `<workspace_root>/.claw/subagents/manifest.json`,使跨会话 `/subagent list`、
//! steer、kill 可基于磁盘恢复可见性(而非仅内存 registry)。
//!
//! 文件为 JSON 数组,原子写(先写 `.tmp` 再 rename,沿用
//! [`write_handoff`](super::handoff::write_handoff) 已验证的 Windows 原子写模式)。
//! 写入为 best-effort:失败静默,不阻塞子代理生命周期。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// manifest 文件名(相对 `.claw/subagents/`)。
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// 单个子代理在 manifest 中的条目(精简投影,不携带内部运行时字段)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentManifestEntry {
    /// 子代理全局唯一 ID。
    pub id: String,
    /// 人类可读名称。
    pub name: String,
    /// 状态(created / running / completed / failed / cancelled)。
    pub status: String,
    /// 绑定的子工作区目录(None = 主会话 cwd)。
    pub workspace: Option<PathBuf>,
    /// 创建时间(unix epoch 秒)。
    pub created_at: u64,
    /// 完成时间(None = 未完成)。
    pub completed_at: Option<u64>,
    /// 结果引用(通常是 handoff 相对路径 `.claw/subagents/{id}.md`)。
    pub result_ref: Option<String>,
}

/// manifest 文件的绝对路径:`{workspace_root}/.claw/subagents/manifest.json`。
#[must_use]
pub fn manifest_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".claw")
        .join("subagents")
        .join(MANIFEST_FILENAME)
}

/// 读取 manifest(best-effort:缺失/损坏返回空列表)。
#[must_use]
pub fn read_manifest(workspace_root: &Path) -> Vec<SubagentManifestEntry> {
    let Ok(bytes) = std::fs::read(manifest_path(workspace_root)) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// 全量同步 manifest(原子写:先写 `.tmp` 再 rename,防并发读半写文件)。
///
/// best-effort:失败返回 `Err` 供调用方决定是否降级(coordinator 通常忽略)。
pub fn sync_manifest(
    workspace_root: &Path,
    entries: &[SubagentManifestEntry],
) -> Result<(), String> {
    let path = manifest_path(workspace_root);
    let dir = path
        .parent()
        .ok_or_else(|| "manifest path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create subagents dir: {e}"))?;
    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_vec_pretty(entries)
        .map_err(|e| format!("failed to serialize manifest: {e}"))?;
    std::fs::write(&tmp_path, &content)
        .map_err(|e| format!("failed to write manifest tmp file: {e}"))?;
    std::fs::rename(&tmp_path, &path).map_err(|e| format!("failed to rename manifest file: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-manifest-{nanos}"))
    }

    fn sample_entries() -> Vec<SubagentManifestEntry> {
        vec![
            SubagentManifestEntry {
                id: "subagent-1".to_string(),
                name: "api-worker".to_string(),
                status: "completed".to_string(),
                workspace: Some(PathBuf::from("crates/api")),
                created_at: 1000,
                completed_at: Some(1200),
                result_ref: Some(".claw/subagents/subagent-1.md".to_string()),
            },
            SubagentManifestEntry {
                id: "subagent-2".to_string(),
                name: "core-worker".to_string(),
                status: "running".to_string(),
                workspace: None,
                created_at: 1100,
                completed_at: None,
                result_ref: None,
            },
        ]
    }

    #[test]
    fn sync_and_read_roundtrip() {
        let root = temp_dir();
        sync_manifest(&root, &sample_entries()).expect("sync should succeed");
        let read = read_manifest(&root);
        assert_eq!(read, sample_entries());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_missing_manifest_returns_empty() {
        let root = temp_dir();
        assert_eq!(read_manifest(&root), Vec::<SubagentManifestEntry>::new());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resync_replaces_previous_entries() {
        let root = temp_dir();
        sync_manifest(&root, &sample_entries()).expect("first sync");
        // 状态流转:subagent-1 已取消,仅保留一个
        let updated = vec![SubagentManifestEntry {
            id: "subagent-1".to_string(),
            name: "api-worker".to_string(),
            status: "cancelled".to_string(),
            workspace: Some(PathBuf::from("crates/api")),
            created_at: 1000,
            completed_at: None,
            result_ref: None,
        }];
        sync_manifest(&root, &updated).expect("resync");
        assert_eq!(read_manifest(&root), updated);
        fs::remove_dir_all(&root).ok();
    }
}
