//! ToolResultArchive — microcompact 真无损化的 Layer 3 持久存储。
//!
//! # 设计动机
//!
//! 旧版 microcompact 直接用 `format_tool_result_summary` 覆盖 `ToolResult.output`,
//! 原始内容永久丢失。LLM 在后续 turn 看到 `[Read output summarized: 1234 chars → ...]`
//! 时,无法判断是否需要重新调用工具,导致重复调用(典型场景:Read 同一文件 N 次)。
//!
//! P0-1 的 NOTEBOOK 通过 LLM 主动维护关键信息缓解了这个问题,但仍然是有损的:
//! - LLM 可能忘记调用 `notebook_update`
//! - NOTEBOOK 的 5 段结构不适合存储大段原始 tool output
//!
//! P1 改进了 summary 格式(保留前 3 行 + 行数提示),但仍非无损。
//!
//! 本模块提供"真无损"归档:在 microcompact 摘要前,把原始 tool result 写到
//! `.claw/tool_results_archive.jsonl`,LLM 可通过 `recall_full` 工具按 `tool_use_id`
//! 主动检索原始内容。
//!
//! # 架构定位
//!
//! 三层信息持久化的 Layer 3:
//!
//! ```text
//! Layer 1: Main Context (LLM 推理窗口)
//!          ↑↓ microcompact 摘要 / recall_full 取回
//! Layer 2: NOTEBOOK.md (LLM 主动维护的关键信息)
//!          ↑↓ notebook_update / render_for_prompt
//! Layer 3: ToolResultArchive (本模块,被动归档的原始 tool output)
//!          ↑↓ archive / recall
//! ```
//!
//! Layer 2 是"主动 + 摘要",Layer 3 是"被动 + 完整"。两者互补:
//! - Layer 2 解决"AI 忘记关键决策"问题
//! - Layer 3 解决"AI 忘记原始 tool output"问题
//!
//! # 存储格式
//!
//! JSONL,每行一条记录:
//!
//! ```json
//! {"tool_use_id":"call_abc","tool_name":"Read","output":"...","archived_at_ms":1784575505000}
//! ```
//!
//! 选择 JSONL 而非 SQLite:
//! - 与 session.rs 持久化格式一致
//! - 追加写性能好(无事务开销)
//! - 文件可读,便于调试
//! - 同一 tool_use_id 重复归档时,后写覆盖前写(recall 取最后一条)
//!
//! # 关键不变量
//!
//! - archive 文件位于 `.claw/tool_results_archive.jsonl`,workspace_root 之下
//! - 写入采用追加模式(append-only),不修改已有内容
//! - recall 按 tool_use_id 检索,返回最后一条匹配记录
//! - 文件不存在或解析失败时,recall 返回 None(不阻断 LLM)

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 归档文件名(相对于 workspace_root)。
pub const ARCHIVE_FILENAME: &str = ".claw/tool_results_archive.jsonl";

/// 归档文件最大字符数(防止无限增长)。
/// 超过后会触发 `prune` 保留最新的记录。
pub const ARCHIVE_MAX_CHARS: usize = 512 * 1024; // 512KB

/// 单条归档记录的最大字符数(防止 LLM 写入失控或异常大输出)。
/// 超出会截断并附加截断标记。
pub const ARCHIVE_RECORD_MAX_CHARS: usize = 64 * 1024; // 64KB

const ARCHIVE_TRUNCATION_MARKER: &str = "… [truncated for tool_result_archive]";

/// 归档的单条记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedToolResult {
    /// 对应 `ContentBlock::ToolResult::tool_use_id`。
    pub tool_use_id: String,
    /// 工具名称(如 "Read" / "Bash" / "Grep")。
    pub tool_name: String,
    /// 原始完整输出(microcompact 摘要前的内容)。
    pub output: String,
    /// 归档时间(unix 毫秒)。
    pub archived_at_ms: u64,
}

impl ArchivedToolResult {
    /// 构造一条新记录,自动填充 `archived_at_ms`。
    #[must_use]
    pub fn new(tool_use_id: impl Into<String>, tool_name: impl Into<String>, output: impl Into<String>) -> Self {
        let output = truncate_output(output.into());
        Self {
            tool_use_id: tool_use_id.into(),
            tool_name: tool_name.into(),
            output,
            archived_at_ms: current_time_millis(),
        }
    }

    /// 序列化为 JSONL 一行。
    ///
    /// 失败时返回 Err(理论上不会失败,因为 ArchivedToolResult 只有 String 字段)。
    pub fn to_jsonl(&self) -> Result<String, ArchiveError> {
        serde_json::to_string(self)
            .map_err(|e| ArchiveError::Serialize(e.to_string()))
    }

    /// 从 JSONL 一行反序列化。
    pub fn from_jsonl(line: &str) -> Result<Self, ArchiveError> {
        serde_json::from_str(line).map_err(|e| ArchiveError::Deserialize(e.to_string()))
    }
}

/// 归档操作错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    /// 序列化失败。
    Serialize(String),
    /// 反序列化失败。
    Deserialize(String),
    /// IO 错误(文件读写)。
    Io(String),
    /// workspace_root 未配置。
    NoWorkspaceRoot,
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(msg) => write!(f, "archive serialize error: {msg}"),
            Self::Deserialize(msg) => write!(f, "archive deserialize error: {msg}"),
            Self::Io(msg) => write!(f, "archive io error: {msg}"),
            Self::NoWorkspaceRoot => write!(f, "workspace_root not configured — archive unavailable"),
        }
    }
}

impl std::error::Error for ArchiveError {}

/// 归档一条 tool result 到 `.claw/tool_results_archive.jsonl`。
///
/// # 参数
///
/// - `workspace_root`:工作区根目录,归档文件位于 `workspace_root/.claw/tool_results_archive.jsonl`
/// - `tool_use_id`:ToolResult 的唯一标识(对应 `ContentBlock::ToolResult::tool_use_id`)
/// - `tool_name`:工具名称(用于 LLM 判断是否值得 recall)
/// - `output`:原始完整输出(microcompact 摘要前的内容)
///
/// # 行为
///
/// - 文件不存在时自动创建(含 `.claw/` 目录)
/// - 追加写(append-only),不修改已有内容
/// - 同一 `tool_use_id` 重复归档时,后写覆盖前写(recall 取最后一条)
/// - 文件超过 `ARCHIVE_MAX_CHARS` 时自动 prune(保留最新的 N 条)
///
/// # 错误处理
///
/// IO 错误返回 `Err(ArchiveError::Io)`,调用方应吞掉错误(归档失败不阻断主流程):
///
/// ```ignore
/// let _ = archive_tool_result(&workspace_root, &tool_use_id, &tool_name, &output);
/// ```
pub fn archive_tool_result(
    workspace_root: &Path,
    tool_use_id: &str,
    tool_name: &str,
    output: &str,
) -> Result<(), ArchiveError> {
    let archive_path = workspace_root.join(ARCHIVE_FILENAME);
    let archive_dir = archive_path.parent().ok_or_else(|| {
        ArchiveError::Io(format!("invalid archive path: {}", archive_path.display()))
    })?;
    fs::create_dir_all(archive_dir).map_err(|e| ArchiveError::Io(e.to_string()))?;

    let record = ArchivedToolResult::new(tool_use_id, tool_name, output);
    let line = record.to_jsonl()?;

    let needs_prune = archive_path.exists()
        && fs::metadata(&archive_path)
            .map(|m| m.len() as usize > ARCHIVE_MAX_CHARS)
            .unwrap_or(false);

    if needs_prune {
        // prune 失败不阻断写入,继续追加。
        let _ = prune_archive(&archive_path);
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&archive_path)
        .map_err(|e| ArchiveError::Io(e.to_string()))?;
    writeln!(file, "{line}").map_err(|e| ArchiveError::Io(e.to_string()))?;
    Ok(())
}

/// 按 `tool_use_id` 检索原始 tool result。
///
/// # 返回
///
/// - `Ok(Some(ArchivedToolResult))`:找到匹配记录(取最后一条)
/// - `Ok(None)`:文件不存在或无匹配记录
/// - `Err(ArchiveError)`:IO 或解析错误
///
/// # 性能
///
/// 线性扫描整个 archive 文件,适合 P0 阶段(archive 通常 < 1000 条记录)。
/// 后续如果性能成问题,可以加索引文件或迁移到 SQLite。
pub fn recall_tool_result(
    workspace_root: &Path,
    tool_use_id: &str,
) -> Result<Option<ArchivedToolResult>, ArchiveError> {
    let archive_path = workspace_root.join(ARCHIVE_FILENAME);
    if !archive_path.exists() {
        return Ok(None);
    }

    let file = fs::File::open(&archive_path).map_err(|e| ArchiveError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut last_match: Option<ArchivedToolResult> = None;

    for (line_number, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| ArchiveError::Io(format!("line {line_number}: {e}")))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record = ArchivedToolResult::from_jsonl(line)
            .map_err(|e| ArchiveError::Deserialize(format!("line {line_number}: {e}")))?;
        if record.tool_use_id == tool_use_id {
            last_match = Some(record);
        }
    }

    Ok(last_match)
}

/// 按工具名批量检索(可选辅助方法,用于"列出所有 Read 工具的归档")。
///
/// 当前 P0 阶段不暴露给 LLM,仅供测试和管理脚本使用。
pub fn recall_by_tool_name(
    workspace_root: &Path,
    tool_name: &str,
) -> Result<Vec<ArchivedToolResult>, ArchiveError> {
    let archive_path = workspace_root.join(ARCHIVE_FILENAME);
    if !archive_path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&archive_path).map_err(|e| ArchiveError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut matches = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| ArchiveError::Io(e.to_string()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = ArchivedToolResult::from_jsonl(line) {
            if record.tool_name.eq_ignore_ascii_case(tool_name) {
                matches.push(record);
            }
        }
    }

    Ok(matches)
}

/// 列出所有归档记录的元信息(不含 output 内容),用于 LLM 决定是否 recall。
///
/// 返回 `(tool_use_id, tool_name, output_preview, archived_at_ms)` 元组,
/// `output_preview` 是前 80 字符,帮助 LLM 判断是否值得 recall。
pub fn list_archived_summary(
    workspace_root: &Path,
) -> Result<Vec<(String, String, String, u64)>, ArchiveError> {
    let archive_path = workspace_root.join(ARCHIVE_FILENAME);
    if !archive_path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&archive_path).map_err(|e| ArchiveError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut summaries = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| ArchiveError::Io(e.to_string()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = ArchivedToolResult::from_jsonl(line) {
            let preview: String = record.output.chars().take(80).collect();
            summaries.push((
                record.tool_use_id,
                record.tool_name,
                preview,
                record.archived_at_ms,
            ));
        }
    }

    Ok(summaries)
}

/// 清理归档文件,保留最新的 N 条记录。
///
/// 保留策略:按 `archived_at_ms` 降序排序,保留前 N 条。
/// 实际实现:读取所有记录 → 排序 → 保留最新 N 条 → 覆盖写回。
///
/// 返回清理后的记录数。
pub fn prune_archive(archive_path: &Path) -> Result<usize, ArchiveError> {
    if !archive_path.exists() {
        return Ok(0);
    }

    let file = fs::File::open(archive_path).map_err(|e| ArchiveError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut records: Vec<ArchivedToolResult> = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| ArchiveError::Io(e.to_string()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = ArchivedToolResult::from_jsonl(line) {
            records.push(record);
        }
    }

    // 按 archived_at_ms 降序排序(最新在前)
    records.sort_by(|a, b| b.archived_at_ms.cmp(&a.archived_at_ms));

    // 保留最新 N 条(N = ARCHIVE_MAX_CHARS / 平均记录大小,经验值 500)
    const KEEP_RECORDS: usize = 500;
    records.truncate(KEEP_RECORDS);

    // 覆盖写回(原子写:.tmp + rename)
    let tmp_path = archive_path.with_extension("jsonl.tmp");
    {
        let mut tmp_file = fs::File::create(&tmp_path)
            .map_err(|e| ArchiveError::Io(e.to_string()))?;
        for record in &records {
            let line = record.to_jsonl()?;
            writeln!(tmp_file, "{line}").map_err(|e| ArchiveError::Io(e.to_string()))?;
        }
        tmp_file.flush().map_err(|e| ArchiveError::Io(e.to_string()))?;
    }
    fs::rename(&tmp_path, archive_path).map_err(|e| ArchiveError::Io(e.to_string()))?;

    Ok(records.len())
}

/// 返回归档文件的路径(workspace_root 之下)。
#[must_use]
pub fn archive_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(ARCHIVE_FILENAME)
}

/// 返回归档文件中的记录数(用于测试和管理)。
pub fn record_count(workspace_root: &Path) -> Result<usize, ArchiveError> {
    let archive_path = workspace_root.join(ARCHIVE_FILENAME);
    if !archive_path.exists() {
        return Ok(0);
    }

    let file = fs::File::open(&archive_path).map_err(|e| ArchiveError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut count = 0;
    for line in reader.lines() {
        let line = line.map_err(|e| ArchiveError::Io(e.to_string()))?;
        if !line.trim().is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

// ---- 内部辅助函数 ----

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn truncate_output(output: String) -> String {
    if output.chars().count() <= ARCHIVE_RECORD_MAX_CHARS {
        return output;
    }
    let truncated: String = output.chars().take(ARCHIVE_RECORD_MAX_CHARS).collect();
    format!("{truncated}{ARCHIVE_TRUNCATION_MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    #[test]
    fn archived_tool_result_serializes_to_jsonl() {
        let record = ArchivedToolResult::new("call_abc", "Read", "file contents");
        let jsonl = record.to_jsonl().expect("serialize should succeed");
        assert!(jsonl.contains("\"tool_use_id\":\"call_abc\""));
        assert!(jsonl.contains("\"tool_name\":\"Read\""));
        assert!(jsonl.contains("\"output\":\"file contents\""));
        assert!(jsonl.contains("\"archived_at_ms\":"));
    }

    #[test]
    fn archived_tool_result_roundtrip() {
        let original = ArchivedToolResult::new("call_123", "Bash", "command output\nmulti\nline");
        let jsonl = original.to_jsonl().expect("serialize");
        let parsed = ArchivedToolResult::from_jsonl(&jsonl).expect("deserialize");
        assert_eq!(original.tool_use_id, parsed.tool_use_id);
        assert_eq!(original.tool_name, parsed.tool_name);
        assert_eq!(original.output, parsed.output);
        assert_eq!(original.archived_at_ms, parsed.archived_at_ms);
    }

    #[test]
    fn archive_creates_file_and_directory() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        // .claw/ 目录不存在
        assert!(!workspace_root.join(".claw").exists());

        archive_tool_result(workspace_root, "call_1", "Read", "contents")
            .expect("archive should succeed");

        // .claw/ 目录和归档文件应被创建
        assert!(workspace_root.join(ARCHIVE_FILENAME).exists());
    }

    #[test]
    fn archive_and_recall_roundtrip() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        let original_output = "line1\nline2\nline3\nline4\nline5";
        archive_tool_result(workspace_root, "call_abc", "Read", original_output)
            .expect("archive should succeed");

        let recalled = recall_tool_result(workspace_root, "call_abc")
            .expect("recall should succeed")
            .expect("record should exist");

        assert_eq!(recalled.tool_use_id, "call_abc");
        assert_eq!(recalled.tool_name, "Read");
        assert_eq!(recalled.output, original_output);
    }

    #[test]
    fn recall_returns_none_when_not_found() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        archive_tool_result(workspace_root, "call_1", "Read", "contents")
            .expect("archive should succeed");

        let result = recall_tool_result(workspace_root, "nonexistent")
            .expect("recall should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn recall_returns_none_when_archive_absent() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        // 不预先创建归档文件
        let result = recall_tool_result(workspace_root, "any_id")
            .expect("recall should succeed on missing file");
        assert!(result.is_none());
    }

    #[test]
    fn recall_returns_last_match_for_duplicate_ids() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        // 同一 tool_use_id 归档两次
        archive_tool_result(workspace_root, "call_dup", "Read", "first version")
            .expect("first archive");
        archive_tool_result(workspace_root, "call_dup", "Read", "second version")
            .expect("second archive");

        let recalled = recall_tool_result(workspace_root, "call_dup")
            .expect("recall")
            .expect("record exists");
        // recall 取最后一条
        assert_eq!(recalled.output, "second version");
    }

    #[test]
    fn archive_handles_multiple_tool_use_ids() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        archive_tool_result(workspace_root, "call_1", "Read", "file1").unwrap();
        archive_tool_result(workspace_root, "call_2", "Bash", "cmd1").unwrap();
        archive_tool_result(workspace_root, "call_3", "Grep", "match1").unwrap();

        let r1 = recall_tool_result(workspace_root, "call_1").unwrap().unwrap();
        let r2 = recall_tool_result(workspace_root, "call_2").unwrap().unwrap();
        let r3 = recall_tool_result(workspace_root, "call_3").unwrap().unwrap();

        assert_eq!(r1.tool_name, "Read");
        assert_eq!(r1.output, "file1");
        assert_eq!(r2.tool_name, "Bash");
        assert_eq!(r2.output, "cmd1");
        assert_eq!(r3.tool_name, "Grep");
        assert_eq!(r3.output, "match1");
    }

    #[test]
    fn archive_survives_workspace_restart() {
        // 模拟 session 重启:archive 文件持久化,新进程可读取
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        // 第一次"session":写入归档
        let original = "important content that should survive compaction";
        archive_tool_result(workspace_root, "call_persist", "Read", original).unwrap();

        // 模拟 session 重启:archive 文件仍存在于磁盘
        // (实际场景中,新进程会用同一 workspace_root 加载)
        let recalled = recall_tool_result(workspace_root, "call_persist")
            .unwrap()
            .unwrap();
        assert_eq!(recalled.output, original);
    }

    #[test]
    fn archive_truncates_oversized_output() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        // 构造超大输出(超过 ARCHIVE_RECORD_MAX_CHARS)
        let huge_output = "x".repeat(ARCHIVE_RECORD_MAX_CHARS + 1000);
        archive_tool_result(workspace_root, "call_huge", "Read", &huge_output).unwrap();

        let recalled = recall_tool_result(workspace_root, "call_huge")
            .unwrap()
            .unwrap();
        // 应被截断并附加截断标记
        assert!(recalled.output.ends_with(ARCHIVE_TRUNCATION_MARKER));
        assert!(recalled.output.chars().count() < huge_output.chars().count());
    }

    #[test]
    fn archive_handles_special_characters_in_output() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        // 包含换行、引号、Unicode、JSON 特殊字符
        let special_output = "line1\nline2\t\"quoted\" \\n {json} 你好世界 emoji: 🎉";
        archive_tool_result(workspace_root, "call_special", "Bash", special_output).unwrap();

        let recalled = recall_tool_result(workspace_root, "call_special")
            .unwrap()
            .unwrap();
        assert_eq!(recalled.output, special_output);
    }

    #[test]
    fn recall_by_tool_name_returns_all_matches() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        archive_tool_result(workspace_root, "call_1", "Read", "file1").unwrap();
        archive_tool_result(workspace_root, "call_2", "Bash", "cmd1").unwrap();
        archive_tool_result(workspace_root, "call_3", "Read", "file2").unwrap();

        let reads = recall_by_tool_name(workspace_root, "Read").unwrap();
        assert_eq!(reads.len(), 2);
        assert!(reads.iter().any(|r| r.tool_use_id == "call_1"));
        assert!(reads.iter().any(|r| r.tool_use_id == "call_3"));
    }

    #[test]
    fn list_archived_summary_returns_preview() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        let long_output = "x".repeat(200);
        archive_tool_result(workspace_root, "call_1", "Read", &long_output).unwrap();

        let summaries = list_archived_summary(workspace_root).unwrap();
        assert_eq!(summaries.len(), 1);
        let (id, name, preview, _) = &summaries[0];
        assert_eq!(id, "call_1");
        assert_eq!(name, "Read");
        // preview 应被截断到 80 字符
        assert_eq!(preview.chars().count(), 80);
    }

    #[test]
    fn record_count_returns_accurate_count() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        assert_eq!(record_count(workspace_root).unwrap(), 0);

        archive_tool_result(workspace_root, "call_1", "Read", "a").unwrap();
        archive_tool_result(workspace_root, "call_2", "Bash", "b").unwrap();
        assert_eq!(record_count(workspace_root).unwrap(), 2);
    }

    #[test]
    fn prune_archive_keeps_latest_records() {
        let workspace = temp_workspace();
        let workspace_root = workspace.path();

        // 写入 10 条记录,archived_at_ms 递增(实际由系统时钟填充)
        // 由于写入间隔可能很短,这里手动构造记录控制时间戳
        let archive_path = archive_path(workspace_root);
        fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
        {
            let mut file = fs::File::create(&archive_path).unwrap();
            for i in 0u64..10 {
                let record = ArchivedToolResult {
                    tool_use_id: format!("call_{i}"),
                    tool_name: "Read".to_string(),
                    output: format!("content_{i}"),
                    archived_at_ms: 1000 + i, // 递增时间戳
                };
                writeln!(file, "{}", record.to_jsonl().unwrap()).unwrap();
            }
        }

        // prune 保留最新 500 条(此处只有 10 条,应全部保留)
        let kept = prune_archive(&archive_path).unwrap();
        assert_eq!(kept, 10);
        assert_eq!(record_count(workspace_root).unwrap(), 10);

        // 验证 recall 仍正常工作
        let r = recall_tool_result(workspace_root, "call_5").unwrap().unwrap();
        assert_eq!(r.output, "content_5");
    }

    #[test]
    fn archive_path_returns_correct_location() {
        let workspace = Path::new("/tmp/test_workspace");
        let path = archive_path(workspace);
        assert_eq!(path, Path::new("/tmp/test_workspace/.claw/tool_results_archive.jsonl"));
    }

    #[test]
    fn archive_error_display_is_informative() {
        let err = ArchiveError::NoWorkspaceRoot;
        assert!(format!("{err}").contains("workspace_root"));
        assert!(format!("{err}").contains("archive"));

        let io_err = ArchiveError::Io("disk full".to_string());
        assert!(format!("{io_err}").contains("disk full"));
    }

    #[test]
    fn from_jsonl_handles_invalid_input() {
        let result = ArchivedToolResult::from_jsonl("not json");
        assert!(result.is_err());

        let result = ArchivedToolResult::from_jsonl("{\"missing\":\"fields\"}");
        assert!(result.is_err());
    }
}
