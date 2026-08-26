//! ToolCallStats — 工具调用统计（tool_name + is_error），阶段 3 工具级晋升门控的分母来源。
//!
//! # 设计动机
//!
//! [`FailureTrace`](crate::failure_trace::FailureTrace) 只记录**含失败的 turn** 的
//! 轨迹切片，无法直接计算失败率（缺"全成功 turn 的总调用次数"分母）。
//! 本模块在每次工具执行后记录 `(tool_name, is_error)`，为工具级 candidate 的
//! 失败率 z-test 提供分母。
//!
//! # 失败率定义
//!
//! 工具级 candidate 的 pathology 形如 `"{tool_name}:{keyword}"`（如
//! `edit_file:old_string not found`）。其失败率：
//! - **分子**：`FailureTrace` 里 `tool_signature == pathology` 的失败记录数
//! - **分母**：本模块里 `tool_name == pathology 的 tool_name` 的总调用次数
//!
//! 即"该工具的调用中，特定错误签名失败所占的比例"。晋升门控对比观察窗口
//! 失败率与基线失败率（z-test），详见
//! [`harness_evolution::tool_level_significance_gate`]。
//!
//! # 存储格式
//!
//! JSONL，每行一条，位于 `.claw/tool_call_stats.jsonl`（append-only，超限 prune）。

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 工具调用统计文件（相对于 workspace_root）。
pub const TOOL_CALL_STATS_FILENAME: &str = ".claw/tool_call_stats.jsonl";

/// 统计文件最大字符数（防止无限增长，超过触发 prune）。
pub const TOOL_CALL_STATS_MAX_CHARS: usize = 512 * 1024; // 512KB

/// prune 时保留的最新记录数。
const KEEP_RECORDS: usize = 2000;

/// 单条工具调用统计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallStat {
    /// 工具名称（如 "edit_file" / "Read" / "Bash"）。
    pub tool_name: String,
    /// 该次调用是否失败。
    pub is_error: bool,
    /// 记录时间（unix 毫秒），用于时间窗口划分。
    pub recorded_at_ms: u64,
}

impl ToolCallStat {
    /// 构造一条新记录，自动填充 `recorded_at_ms`。
    #[must_use]
    pub fn new(tool_name: impl Into<String>, is_error: bool) -> Self {
        Self {
            tool_name: tool_name.into(),
            is_error,
            recorded_at_ms: current_time_millis(),
        }
    }
}

/// 统计操作错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallStatsError {
    Serialize(String),
    Io(String),
}

impl std::fmt::Display for ToolCallStatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(msg) => write!(f, "tool_call_stats serialize error: {msg}"),
            Self::Io(msg) => write!(f, "tool_call_stats io error: {msg}"),
        }
    }
}

impl std::error::Error for ToolCallStatsError {}

/// 追加一条工具调用统计。落盘失败不阻断主流程（调用方吞掉错误）。
///
/// ```ignore
/// let _ = record(&workspace_root, &tool_name, is_error);
/// ```
pub fn record(
    workspace_root: &Path,
    tool_name: &str,
    is_error: bool,
) -> Result<(), ToolCallStatsError> {
    let path = workspace_root.join(TOOL_CALL_STATS_FILENAME);
    let dir = path.parent().ok_or_else(|| {
        ToolCallStatsError::Io(format!("invalid tool_call_stats path: {}", path.display()))
    })?;
    fs::create_dir_all(dir).map_err(|e| ToolCallStatsError::Io(e.to_string()))?;

    let stat = ToolCallStat::new(tool_name, is_error);
    let line =
        serde_json::to_string(&stat).map_err(|e| ToolCallStatsError::Serialize(e.to_string()))?;

    let needs_prune = path.exists()
        && fs::metadata(&path)
            .map(|m| m.len() as usize > TOOL_CALL_STATS_MAX_CHARS)
            .unwrap_or(false);
    if needs_prune {
        let _ = prune(&path);
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
    writeln!(file, "{line}").map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
    Ok(())
}

/// 加载所有工具调用统计（按追加顺序，即时间顺序）。
///
/// 容错：跳过无法解析的行。
pub fn load_all(workspace_root: &Path) -> Result<Vec<ToolCallStat>, ToolCallStatsError> {
    let path = workspace_root.join(TOOL_CALL_STATS_FILENAME);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&path).map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut stats = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(stat) = serde_json::from_str::<ToolCallStat>(line) {
            stats.push(stat);
        }
    }
    Ok(stats)
}

/// 清理统计文件，保留最新的 N 条记录（原子写）。
fn prune(path: &Path) -> Result<usize, ToolCallStatsError> {
    if !path.exists() {
        return Ok(0);
    }
    let file = fs::File::open(path).map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut records: Vec<ToolCallStat> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(stat) = serde_json::from_str::<ToolCallStat>(line.trim()) {
            records.push(stat);
        }
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.recorded_at_ms));
    records.truncate(KEEP_RECORDS);

    let tmp_path = path.with_extension("jsonl.tmp");
    {
        let mut tmp_file =
            fs::File::create(&tmp_path).map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
        for stat in &records {
            let line = serde_json::to_string(stat)
                .map_err(|e| ToolCallStatsError::Serialize(e.to_string()))?;
            writeln!(tmp_file, "{line}").map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
        }
        tmp_file
            .flush()
            .map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
    }
    fs::rename(&tmp_path, path).map_err(|e| ToolCallStatsError::Io(e.to_string()))?;
    Ok(records.len())
}

/// 返回统计文件的路径（workspace_root 之下）。
#[must_use]
pub fn tool_call_stats_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TOOL_CALL_STATS_FILENAME)
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    #[test]
    fn record_and_load_roundtrip() {
        let ws = temp_workspace();
        record(ws.path(), "edit_file", false).unwrap();
        record(ws.path(), "edit_file", true).unwrap();
        record(ws.path(), "Read", false).unwrap();

        let stats = load_all(ws.path()).unwrap();
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].tool_name, "edit_file");
        assert!(!stats[0].is_error);
        assert!(stats[1].is_error);
        assert_eq!(stats[2].tool_name, "Read");
    }

    #[test]
    fn load_returns_empty_when_absent() {
        let ws = temp_workspace();
        assert!(load_all(ws.path()).unwrap().is_empty());
    }

    #[test]
    fn load_skips_corrupt_lines() {
        let ws = temp_workspace();
        let path = tool_call_stats_path(ws.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json\n{}\n").unwrap();
        assert!(load_all(ws.path()).unwrap().is_empty());
    }
}
