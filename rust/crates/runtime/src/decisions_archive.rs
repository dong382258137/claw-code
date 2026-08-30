//! DecisionsArchive — decisions 段 LLM 语义压缩/规则去重的"备份兜底"。
//!
//! # 设计动机
//!
//! NOTEBOOK.md `<decisions>` 段承载跨会话的设计决策历史。为控制注入体量,
//! 该段会被 **LLM 语义压缩**(合并同主题重复条目)和**规则去重**
//! (`dedupe_decisions_section`)缩减 —— 压缩必然有损,可能丢失独特决策细节。
//! AI 后续若需要被压缩掉的细节(如某决策的 rationale / alternatives),无法找回。
//!
//! 本模块提供"备份兜底":在 decisions 段发生**内容缩减**(去重 / 裁剪)前,
//! 把缩减前的完整旧段追加到 `.claw/decisions_archive.jsonl`,AI 需要细节时
//! 可通过 `recall_full`(按 tool_use_id)或直接 read 该文件找回历史决策。
//!
//! # 架构定位
//!
//! 与 [`crate::tool_result_archive`](crate::tool_result_archive) 同构 ——
//! 都是"缩减前归档原始内容"的 Layer 3 兜底,JSONL append-only:
//!
//! ```text
//! Layer 2: NOTEBOOK.md <decisions> 段(压缩后的活跃决策)
//!          ↑↓ 语义压缩 / dedupe / recall
//! Layer 3: decisions_archive.jsonl(压缩前的完整历史)
//! ```
//!
//! # 存储格式
//!
//! JSONL,每行一条记录:
//!
//! ```json
//! {"seq":12,"archived_at_ms":1784575505000,"content":"- [d1786936] B2: ..."}
//! ```
//!
//! # 关键不变量
//!
//! - 归档文件位于 `.claw/decisions_archive.jsonl`,workspace_root 之下
//! - 写入采用追加模式(append-only),不修改已有内容
//! - 文件超过 [`DECISIONS_ARCHIVE_MAX_CHARS`] 时从头部裁剪(保留最新记录)
//! - 归档失败静默忽略(备份兜底不应阻断主流程)

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 归档文件名(相对于 workspace_root)。
pub const DECISIONS_ARCHIVE_FILENAME: &str = ".claw/decisions_archive.jsonl";

/// 归档文件最大字节数(防止无限增长)。超过后从头部裁剪,保留最新记录。
pub const DECISIONS_ARCHIVE_MAX_BYTES: u64 = 512 * 1024; // 512KB

/// 裁剪时预留的余量:单条 decisions 快照最大约 6K 字符(见 `DECISIONS_MAX_CHARS`),
/// 序列化后 < 16KB。裁剪目标设为 `MAX - 64KB`,保证追加一条新快照后仍在上限内,
/// 避免"写前裁到上限、写后又超限"的边界抖动。
pub const DECISIONS_ARCHIVE_PRUNE_RESERVE: u64 = 64 * 1024; // 64KB

/// 归档的单条记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedDecision {
    /// 自增序号(便于追溯写入顺序)。
    pub seq: u64,
    /// 归档时间(unix 毫秒)。
    pub archived_at_ms: u64,
    /// 缩减前 decisions 段的完整内容(压缩/裁剪前的旧段)。
    pub content: String,
}

impl ArchivedDecision {
    /// 构造一条记录,自动填充时间戳。`seq` 由调用方维护。
    #[must_use]
    pub fn new(seq: u64, content: impl Into<String>) -> Self {
        Self {
            seq,
            archived_at_ms: current_time_millis(),
            content: content.into(),
        }
    }
}

/// 读取归档文件的当前行数,用于分配下一个 seq。
fn next_seq(archive_path: &Path) -> u64 {
    let Ok(content) = fs::read_to_string(archive_path) else {
        return 0;
    };
    content
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<ArchivedDecision>(line)
                .ok()
                .map(|r| r.seq)
        })
        .max()
        .map_or(0, |s| s + 1)
}

/// 归档一条 decisions 段快照(append-only)。
///
/// 在 decisions 段发生**内容缩减**(LLM 语义压缩 / 规则去重 / 上限裁剪)前,
/// 调用方把缩减前的完整旧段传入,本函数追加到 `.claw/decisions_archive.jsonl`。
///
/// # 行为
///
/// - 文件不存在时自动创建(含 `.claw/` 目录)
/// - 追加写(append-only),不修改已有内容
/// - 文件超过 [`DECISIONS_ARCHIVE_MAX_BYTES`] 时从头部裁剪(保留最新记录)
/// - 归档失败静默返回 Err,调用方应吞掉(备份兜底不阻断主流程):
///
/// ```ignore
/// let _ = crate::decisions_archive::archive_decisions_snapshot(&root, &old_section);
/// ```
pub fn archive_decisions_snapshot(workspace_root: &Path, content: &str) -> Result<(), String> {
    if content.trim().is_empty() {
        return Ok(());
    }
    let archive_path = workspace_root.join(DECISIONS_ARCHIVE_FILENAME);
    let archive_dir = archive_path
        .parent()
        .ok_or_else(|| format!("invalid archive path: {}", archive_path.display()))?;
    fs::create_dir_all(archive_dir).map_err(|e| format!("mkdir archive dir: {e}"))?;

    let seq = next_seq(&archive_path);
    let record = ArchivedDecision::new(seq, content);
    let line = serde_json::to_string(&record).map_err(|e| format!("serialize: {e}"))?;

    // 超过上限时从头部裁剪(保留最新记录)。prune 失败不阻断写入。
    let needs_prune = archive_path
        .metadata()
        .map(|m| m.len() > DECISIONS_ARCHIVE_MAX_BYTES)
        .unwrap_or(false);
    if needs_prune {
        let _ = prune_archive_head(&archive_path);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&archive_path)
        .map_err(|e| format!("open archive: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("append archive: {e}"))?;
    Ok(())
}

/// 从头部裁剪归档文件,直到字节数回到 `MAX - 预留余量` 内(保留最新记录)。
fn prune_archive_head(archive_path: &Path) -> Result<(), String> {
    let content = fs::read_to_string(archive_path).map_err(|e| e.to_string())?;
    // 裁剪目标 = MAX - 预留余量,保证后续追加一条快照后仍 ≤ MAX。
    let target = DECISIONS_ARCHIVE_MAX_BYTES.saturating_sub(DECISIONS_ARCHIVE_PRUNE_RESERVE);
    if content.len() as u64 <= target {
        return Ok(());
    }
    // 逐行丢弃头部,直到剩余内容回到目标内(至少保留一行)。
    let mut remaining = content.as_str();
    let mut dropped = 0usize;
    while remaining.len() as u64 > target {
        let Some(nl) = remaining.find('\n') else {
            break;
        };
        dropped += nl + 1;
        remaining = &remaining[nl + 1..];
        if remaining.is_empty() {
            break;
        }
    }
    if dropped == 0 {
        return Ok(());
    }
    fs::write(archive_path, remaining).map_err(|e| e.to_string())
}

/// 当前 unix 毫秒时间戳。
fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_workspace(tag: &str) -> tempfile::TempDir {
        let tmp = std::env::temp_dir().join(format!(
            "claw-decisions-archive-{tag}-{}-{}",
            std::process::id(),
            current_time_millis()
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        tempfile::TempDir::new_in(tmp).expect("tempdir")
    }

    #[test]
    fn archive_appends_records_with_sequence() {
        let dir = temp_workspace("append");
        let root = dir.path();
        archive_decisions_snapshot(root, "- [d1] 决策 A").expect("first");
        archive_decisions_snapshot(root, "- [d2] 决策 B").expect("second");
        let content = std::fs::read_to_string(root.join(DECISIONS_ARCHIVE_FILENAME)).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "两条记录");
        let first: ArchivedDecision = serde_json::from_str(lines[0]).expect("parse first");
        let second: ArchivedDecision = serde_json::from_str(lines[1]).expect("parse second");
        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
        assert_eq!(first.content, "- [d1] 决策 A");
        assert_eq!(second.content, "- [d2] 决策 B");
    }

    #[test]
    fn archive_ignores_empty_content() {
        let dir = temp_workspace("empty");
        archive_decisions_snapshot(dir.path(), "   ").expect("ok");
        let path = dir.path().join(DECISIONS_ARCHIVE_FILENAME);
        assert!(!path.exists(), "空内容不应创建归档文件");
    }

    #[test]
    fn archive_prunes_head_keeping_newest() {
        let dir = temp_workspace("prune");
        let root = dir.path();
        // 写入 20 条 40KB 记录 → 超 512KB 上限
        for i in 0..20 {
            let content = format!("- [d{i}] {}", "x".repeat(40 * 1024));
            archive_decisions_snapshot(root, &content).expect("archive");
        }
        let path = root.join(DECISIONS_ARCHIVE_FILENAME);
        let meta = std::fs::metadata(&path).expect("meta");
        assert!(
            meta.len() <= DECISIONS_ARCHIVE_MAX_BYTES,
            "归档文件必须被裁剪到上限内, got {}",
            meta.len()
        );
        // 最新的记录(seq 19)必须保留
        let content = std::fs::read_to_string(&path).expect("read");
        let last_seq = content
            .lines()
            .filter_map(|l| serde_json::from_str::<ArchivedDecision>(l).ok())
            .map(|r| r.seq)
            .max();
        assert_eq!(last_seq, Some(19), "保留最新记录");
    }

    #[test]
    fn archive_creates_claw_directory() {
        let dir = temp_workspace("dir");
        let root = dir.path();
        archive_decisions_snapshot(root, "- [d1] 决策").expect("ok");
        let path = root.join(DECISIONS_ARCHIVE_FILENAME);
        assert!(path.exists(), "归档文件应创建在 .claw/ 下");
        // 手动验证 .claw 目录存在
        assert!(root.join(".claw").exists());
    }
}
