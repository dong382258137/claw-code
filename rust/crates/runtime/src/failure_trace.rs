//! FailureTrace — 失败轨迹切片（tool 调用级失败点定位）。
//!
//! # 设计动机
//!
//! [`TraceAnalyzer`](crate::trace_analyzer::TraceAnalyzer) 记录的是 **turn 级**
//! 失败信号：`failure_kind`（如 `runtime_error`）只说明"哪一类失败"，不说明
//! "在哪一步工具调用失败、前面哪几步是正确的"。自进化闭环
//! [`harness_evolution`](crate::harness_evolution) 因此只能学会
//! "old_string not found 要先 grep"这类静态规则，学不会"读文件第 N 步用错了
//! 工具，换 Glob 就成功"这种带上下文的经验。
//!
//! 本模块（阶段 1 失败点定位）把数据粒度从 turn 级降到 tool 调用级：
//! 从每个 turn 的 [`TurnSummary`](crate::conversation::TurnSummary) 投影出
//! 完整的工具调用序列，标记 `is_error=true` 的失败点，形成 [`FailureTrace`]。
//!
//! # 数据来源
//!
//! **纯投影，零新采集**。工具序列本就存在于 turn 出口的 `TurnSummary` 里：
//! - `assistant_messages` 携带 `ContentBlock::ToolUse { id, name, input }`
//! - `tool_results` 携带 `ContentBlock::ToolResult { tool_use_id, tool_name, output, is_error }`
//!
//! 二者通过 `id == tool_use_id` 配对，得到按执行顺序排列的 `steps`。
//! 若 `steps` 中没有任何 `is_error=true`，返回 `None`（只记失败轨迹，
//! 不记全成功轨迹，避免冗余落盘）。
//!
//! # 存储格式
//!
//! JSONL，每行一条 [`FailureTrace`]，位于 `.claw/failure_traces.jsonl`。
//! 选择 JSONL 而非 CSV：`steps` 是嵌套结构，CSV 装不下；且
//! [`tool_result_archive`](crate::tool_result_archive) 已验证 JSONL 独立
//! 通道的可行性，本模块沿用同一模式（append-only + 超限 prune）。
//!
//! # 关键不变量
//!
//! - 文件位于 `.claw/failure_traces.jsonl`，workspace_root 之下
//! - 追加写（append-only），不修改已有内容
//! - 落盘失败不阻断主流程（调用方吞掉错误）
//! - 单条步骤的 input/output 截断到 [`TRACE_STEP_FIELD_MAX_CHARS`]

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::session::{ContentBlock, ConversationMessage};

/// 失败轨迹文件（相对于 workspace_root）。
pub const FAILURE_TRACES_FILENAME: &str = ".claw/failure_traces.jsonl";

/// 失败轨迹文件最大字符数（防止无限增长，超过触发 prune）。
pub const FAILURE_TRACES_MAX_CHARS: usize = 512 * 1024; // 512KB

/// 单条步骤 input/output 的最大字符数（防止单步失控膨胀）。
pub const TRACE_STEP_FIELD_MAX_CHARS: usize = 2000;

/// prune 时保留的最新记录数（与 tool_result_archive 一致）。
const KEEP_RECORDS: usize = 500;

/// 单条工具调用步骤。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceToolStep {
    /// 工具名称（如 "Read" / "edit_file" / "grep_search"）。
    pub tool_name: String,
    /// 工具输入（已截断到 [`TRACE_STEP_FIELD_MAX_CHARS`]）。
    pub input: String,
    /// 工具输出（已截断到 [`TRACE_STEP_FIELD_MAX_CHARS`]）。
    pub output: String,
    /// 是否失败（工具执行返回 Err 或 guard 拒绝）。
    pub is_error: bool,
}

/// 一个含失败工具调用的 turn 的完整轨迹切片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureTrace {
    /// turn 唯一标识（与 [`TraceRecord::turn_id`](crate::trace_analyzer::TraceRecord) 同构）。
    pub turn_id: String,
    /// 来源 session_id。
    pub session_id: String,
    /// 失败类别（继承 turn 级的 coarse 分类，供下游回溯）。
    pub failure_kind: String,
    /// 完整工具调用序列（按执行顺序），含 is_error 标记。
    pub steps: Vec<TraceToolStep>,
    /// 记录时间（unix 毫秒）。
    pub recorded_at_ms: u64,
}

impl FailureTrace {
    /// 构造一条新记录，自动填充 `recorded_at_ms`。
    #[must_use]
    pub fn new(
        turn_id: impl Into<String>,
        session_id: impl Into<String>,
        failure_kind: impl Into<String>,
        steps: Vec<TraceToolStep>,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            session_id: session_id.into(),
            failure_kind: failure_kind.into(),
            steps,
            recorded_at_ms: current_time_millis(),
        }
    }
}

/// 归档操作错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureTraceError {
    /// 序列化失败。
    Serialize(String),
    /// 反序列化失败。
    Deserialize(String),
    /// IO 错误（文件读写）。
    Io(String),
}

impl std::fmt::Display for FailureTraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(msg) => write!(f, "failure_trace serialize error: {msg}"),
            Self::Deserialize(msg) => write!(f, "failure_trace deserialize error: {msg}"),
            Self::Io(msg) => write!(f, "failure_trace io error: {msg}"),
        }
    }
}

impl std::error::Error for FailureTraceError {}

/// 从 turn 出口的 assistant/tool 消息投影出失败轨迹切片。
///
/// 配对规则：`assistant_messages` 里的 `ToolUse { id, .. }` 与
/// `tool_results` 里的 `ToolResult { tool_use_id, .. }` 通过 `id == tool_use_id`
/// 关联；`steps` 按 `tool_results` 的顺序排列（即工具实际执行顺序）。
///
/// # 返回
///
/// - `Some(FailureTrace)`：存在至少一个 `is_error=true` 的步骤
/// - `None`：所有步骤均成功（全成功 turn 不落盘）
#[must_use]
pub fn extract_from_turn_summary(
    turn_id: &str,
    session_id: &str,
    failure_kind: &str,
    assistant_messages: &[ConversationMessage],
    tool_results: &[ConversationMessage],
) -> Option<FailureTrace> {
    // 收集 ToolUse（id → (name, input)）。
    let mut tool_uses: HashMap<String, (String, String)> = HashMap::new();
    for msg in assistant_messages {
        for block in &msg.blocks {
            if let ContentBlock::ToolUse { id, name, input } = block {
                tool_uses.insert(id.clone(), (name.clone(), input.clone()));
            }
        }
    }

    // 按 tool_results 顺序构造 steps。
    let mut steps: Vec<TraceToolStep> = Vec::new();
    for msg in tool_results {
        let Some(ContentBlock::ToolResult {
            tool_use_id,
            tool_name,
            output,
            is_error,
        }) = msg.blocks.first()
        else {
            continue;
        };
        // 配对失败（无对应 ToolUse）时 input 置空，保证 step 仍按执行顺序保留。
        let input = tool_uses
            .get(tool_use_id)
            .map(|(_, input)| input.clone())
            .unwrap_or_default();
        steps.push(TraceToolStep {
            tool_name: tool_name.clone(),
            input: truncate_chars(&input, TRACE_STEP_FIELD_MAX_CHARS),
            output: truncate_chars(output, TRACE_STEP_FIELD_MAX_CHARS),
            is_error: *is_error,
        });
    }

    // 无失败点 → 不落盘（只记失败轨迹）。
    if !steps.iter().any(|s| s.is_error) {
        return None;
    }

    Some(FailureTrace::new(turn_id, session_id, failure_kind, steps))
}

/// 追加一条失败轨迹到 `.claw/failure_traces.jsonl`。
///
/// # 行为
///
/// - 文件不存在时自动创建（含 `.claw/` 目录）
/// - 追加写（append-only）
/// - 文件超过 [`FAILURE_TRACES_MAX_CHARS`] 时自动 prune（保留最新 N 条）
///
/// # 错误处理
///
/// 落盘失败不阻断主流程，调用方应吞掉错误：
///
/// ```ignore
/// let _ = append(&workspace_root, &trace);
/// ```
pub fn append(workspace_root: &Path, trace: &FailureTrace) -> Result<(), FailureTraceError> {
    let path = workspace_root.join(FAILURE_TRACES_FILENAME);
    let dir = path.parent().ok_or_else(|| {
        FailureTraceError::Io(format!("invalid failure_traces path: {}", path.display()))
    })?;
    fs::create_dir_all(dir).map_err(|e| FailureTraceError::Io(e.to_string()))?;

    let line =
        serde_json::to_string(trace).map_err(|e| FailureTraceError::Serialize(e.to_string()))?;

    let needs_prune = path.exists()
        && fs::metadata(&path)
            .map(|m| m.len() as usize > FAILURE_TRACES_MAX_CHARS)
            .unwrap_or(false);
    if needs_prune {
        // prune 失败不阻断写入。
        let _ = prune(&path);
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| FailureTraceError::Io(e.to_string()))?;
    writeln!(file, "{line}").map_err(|e| FailureTraceError::Io(e.to_string()))?;
    Ok(())
}

/// 加载所有失败轨迹（供测试与后续 harness_evolution 消费）。
///
/// 容错：跳过无法解析的行，不因单行损坏阻断整体读取。
pub fn load_all(workspace_root: &Path) -> Result<Vec<FailureTrace>, FailureTraceError> {
    let path = workspace_root.join(FAILURE_TRACES_FILENAME);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&path).map_err(|e| FailureTraceError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut traces = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| FailureTraceError::Io(e.to_string()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(trace) = serde_json::from_str::<FailureTrace>(line) {
            traces.push(trace);
        }
    }
    Ok(traces)
}

/// 清理归档文件，保留最新的 N 条记录（原子写：`.tmp` + rename）。
///
/// 返回清理后的记录数。
pub fn prune(path: &Path) -> Result<usize, FailureTraceError> {
    if !path.exists() {
        return Ok(0);
    }

    let file = fs::File::open(path).map_err(|e| FailureTraceError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut records: Vec<FailureTrace> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| FailureTraceError::Io(e.to_string()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<FailureTrace>(line) {
            records.push(record);
        }
    }

    // 按 recorded_at_ms 降序（最新在前）。
    records.sort_by_key(|r| std::cmp::Reverse(r.recorded_at_ms));
    records.truncate(KEEP_RECORDS);

    // 原子写：.tmp + rename。
    let tmp_path = path.with_extension("jsonl.tmp");
    {
        let mut tmp_file =
            fs::File::create(&tmp_path).map_err(|e| FailureTraceError::Io(e.to_string()))?;
        for record in &records {
            let line = serde_json::to_string(record)
                .map_err(|e| FailureTraceError::Serialize(e.to_string()))?;
            writeln!(tmp_file, "{line}").map_err(|e| FailureTraceError::Io(e.to_string()))?;
        }
        tmp_file
            .flush()
            .map_err(|e| FailureTraceError::Io(e.to_string()))?;
    }
    fs::rename(&tmp_path, path).map_err(|e| FailureTraceError::Io(e.to_string()))?;

    Ok(records.len())
}

/// 返回失败轨迹文件的路径（workspace_root 之下）。
#[must_use]
pub fn failure_traces_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(FAILURE_TRACES_FILENAME)
}

// ---- 内部辅助函数 ----

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// UTF-8 安全截断到 `max_chars` 字符（非字节）。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ContentBlock, ConversationMessage, MessageRole};

    fn tool_use(id: &str, name: &str, input: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
        }
    }

    fn tool_result(id: &str, name: &str, output: &str, is_error: bool) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            output: output.to_string(),
            is_error,
        }
    }

    fn assistant_with_blocks(blocks: Vec<ContentBlock>) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::Assistant,
            blocks,
            usage: None,
        }
    }

    fn tool_msg(block: ContentBlock) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![block],
            usage: None,
        }
    }

    fn temp_workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    #[test]
    fn extract_pairs_tool_use_and_result_by_id() {
        let assistant = vec![assistant_with_blocks(vec![
            tool_use("call_1", "Read", "{\"file_path\":\"a.rs\"}"),
            tool_use("call_2", "edit_file", "{\"file_path\":\"a.rs\"}"),
        ])];
        let results = vec![
            tool_msg(tool_result("call_1", "Read", "file contents", false)),
            tool_msg(tool_result(
                "call_2",
                "edit_file",
                "old_string not found",
                true,
            )),
        ];

        let trace = extract_from_turn_summary("sess-1", "sess", "tool_error", &assistant, &results)
            .expect("should produce trace with failure");

        assert_eq!(trace.steps.len(), 2);
        assert_eq!(trace.steps[0].tool_name, "Read");
        assert!(!trace.steps[0].is_error);
        assert_eq!(trace.steps[0].input, "{\"file_path\":\"a.rs\"}");
        assert_eq!(trace.steps[1].tool_name, "edit_file");
        assert!(trace.steps[1].is_error);
        assert_eq!(trace.steps[1].output, "old_string not found");
    }

    #[test]
    fn extract_returns_none_when_all_steps_succeed() {
        let assistant = vec![assistant_with_blocks(vec![tool_use(
            "call_1", "Read", "{}",
        )])];
        let results = vec![tool_msg(tool_result("call_1", "Read", "ok", false))];

        let trace = extract_from_turn_summary("sess-1", "sess", "tool_error", &assistant, &results);
        assert!(
            trace.is_none(),
            "all-success turn should not produce a trace"
        );
    }

    #[test]
    fn extract_pairs_missing_tool_use_with_empty_input() {
        // guard 拒绝等场景可能只有 tool_result 而无对应 tool_use。
        let assistant = vec![assistant_with_blocks(vec![])];
        let results = vec![tool_msg(tool_result("call_x", "Read", "denied", true))];

        let trace = extract_from_turn_summary("sess-1", "sess", "tool_error", &assistant, &results)
            .expect("should still produce trace");
        assert_eq!(trace.steps.len(), 1);
        assert_eq!(trace.steps[0].input, "");
        assert!(trace.steps[0].is_error);
    }

    #[test]
    fn extract_truncates_oversized_fields() {
        let huge = "x".repeat(TRACE_STEP_FIELD_MAX_CHARS + 500);
        let assistant = vec![assistant_with_blocks(vec![tool_use(
            "call_1", "Read", &huge,
        )])];
        let results = vec![tool_msg(tool_result("call_1", "Read", &huge, true))];

        let trace = extract_from_turn_summary("sess-1", "sess", "tool_error", &assistant, &results)
            .expect("trace");
        assert_eq!(
            trace.steps[0].input.chars().count(),
            TRACE_STEP_FIELD_MAX_CHARS
        );
        assert_eq!(
            trace.steps[0].output.chars().count(),
            TRACE_STEP_FIELD_MAX_CHARS
        );
    }

    #[test]
    fn append_and_load_all_roundtrip() {
        let workspace = temp_workspace();
        let root = workspace.path();

        let step = TraceToolStep {
            tool_name: "Read".to_string(),
            input: "{}".to_string(),
            output: "contents".to_string(),
            is_error: true,
        };
        let trace = FailureTrace::new("sess-1", "sess", "tool_error", vec![step]);

        append(root, &trace).expect("append should succeed");
        let loaded = load_all(root).expect("load should succeed");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].turn_id, "sess-1");
        assert_eq!(loaded[0].steps[0].tool_name, "Read");
        assert!(loaded[0].steps[0].is_error);
    }

    #[test]
    fn load_all_returns_empty_when_absent() {
        let workspace = temp_workspace();
        let loaded = load_all(workspace.path()).expect("load should succeed");
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_all_skips_corrupt_lines() {
        let workspace = temp_workspace();
        let path = failure_traces_path(workspace.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json\n{}\n").unwrap();

        let loaded = load_all(workspace.path()).expect("load should succeed");
        assert!(
            loaded.is_empty(),
            "corrupt lines should be skipped, got {loaded:?}"
        );
    }

    #[test]
    fn prune_keeps_latest_records() {
        let workspace = temp_workspace();
        let path = failure_traces_path(workspace.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        // 写入 10 条，recorded_at_ms 递增。
        {
            let mut file = fs::File::create(&path).unwrap();
            for i in 0u64..10 {
                let trace = FailureTrace {
                    turn_id: format!("turn-{i}"),
                    session_id: "sess".to_string(),
                    failure_kind: "tool_error".to_string(),
                    steps: vec![],
                    recorded_at_ms: 1000 + i,
                };
                writeln!(file, "{}", serde_json::to_string(&trace).unwrap()).unwrap();
            }
        }

        // 10 条 < KEEP_RECORDS，prune 应全保留。
        let kept = prune(&path).unwrap();
        assert_eq!(kept, 10);
        assert_eq!(load_all(workspace.path()).unwrap().len(), 10);
    }

    #[test]
    fn truncate_chars_is_utf8_safe() {
        assert_eq!(truncate_chars("hello", 3), "hel");
        assert_eq!(truncate_chars("你好世界", 2), "你好");
        assert_eq!(truncate_chars("short", 100), "short");
    }
}
