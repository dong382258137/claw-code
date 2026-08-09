//! Epic 5: 结构化 handoff 协议 — TRAE 架构对齐(见 docs/2026-08-06-subagent-trae-alignment-design.md §3.4)。
//!
//! 替代纯文本结果文件,引入 Markdown + YAML frontmatter 的结构化格式:
//! - frontmatter(machine-parseable):subagent_id / capability / changed_files / summary / details 等
//! - body(human-readable):`# Subagent Result: {name}` + Summary + Details
//!
//! 主 agent 读取时解析 frontmatter,`summary`(≤500 字符,含 changed_files 列表)进主上下文,
//! `details` 通过 `result_ref` 路径按需 Read,避免子智能体完整输出污染主上下文(§8.4)。
//!
//! **向后兼容**:旧格式纯文本文件(无 frontmatter)能被降级解析 — 整体作为 `details`,
//! `status` 标记为 `Legacy`,主 agent 仍可读取。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::multi_agent::{SubagentCapability, TaskComplexity};

/// summary 字段的硬上限(字符数)。超长时截断并追加 `…` 标记(§3.4)。
const SUMMARY_MAX_CHARS: usize = 500;

/// Handoff 状态 — 标记子智能体执行的最终结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    /// 正常完成(无截断)。
    #[default]
    Completed,
    /// 执行失败(LLLM 调用错误、guard 拒绝等)。
    Failed,
    /// 超过 max_iterations 截断(§8.1:返回 Err + Truncated handoff)。
    Truncated,
    /// 旧格式纯文本文件(无 frontmatter,降级解析)。
    Legacy,
}

/// 子智能体结构化 handoff — 写入 `.claw/subagents/{id}.md`。
///
/// 同时作为机器解析源(frontmatter)和人类可读文档(body)。主 agent 解析 frontmatter
/// 提取 `summary` 进上下文,`changed_files` 喂给 validation gate(§8.4)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubagentHandoff {
    /// 子智能体全局唯一 ID。
    pub subagent_id: String,
    /// 人类可读名称。
    pub name: String,
    /// 原始任务描述(可追溯性)。
    #[serde(default)]
    pub task: String,
    /// 能力分级(决定工具白名单)。
    pub capability: SubagentCapability,
    /// 任务复杂度。
    pub complexity: TaskComplexity,
    /// 多轮循环迭代次数(单轮 = 1)。
    pub iterations: usize,
    /// 实际调用的工具名列表(按调用顺序)。
    pub tools_used: Vec<String>,
    /// 修改的文件路径列表(规范化后,去重)。
    pub changed_files: Vec<String>,
    /// 执行状态。
    pub status: HandoffStatus,
    /// Unix epoch 秒。
    pub timestamp: u64,
    /// ≤500 字符的摘要(主 agent 进上下文用)。
    pub summary: String,
    /// 完整输出(按需 Read,不进主上下文)。
    pub details: String,
}

impl SubagentHandoff {
    /// 构造一个 `Completed` 状态的 handoff,自动截断 summary 到 [`SUMMARY_MAX_CHARS`]。
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        subagent_id: impl Into<String>,
        name: impl Into<String>,
        capability: SubagentCapability,
        complexity: TaskComplexity,
        iterations: usize,
        tools_used: Vec<String>,
        changed_files: Vec<String>,
        summary: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self {
            subagent_id: subagent_id.into(),
            name: name.into(),
            task: String::new(),
            capability,
            complexity,
            iterations,
            tools_used,
            changed_files,
            status: HandoffStatus::Completed,
            timestamp: now_secs(),
            summary: truncate_summary(&summary.into()),
            details: details.into(),
        }
    }

    /// builder:设置原始任务描述(可追溯性)。
    #[must_use]
    pub fn with_task(mut self, task: impl Into<String>) -> Self {
        self.task = task.into();
        self
    }

    /// builder:设置状态(默认 Completed)。
    #[must_use]
    pub fn with_status(mut self, status: HandoffStatus) -> Self {
        self.status = status;
        self
    }

    /// builder:覆盖 timestamp(测试用;默认取当前时间)。
    #[must_use]
    pub fn with_timestamp(mut self, ts: u64) -> Self {
        self.timestamp = ts;
        self
    }
}

/// 截断 summary 到 `SUMMARY_MAX_CHARS` 字符,超长追加 `…` 标记(§3.4)。
/// 不做硬截断导致语义断裂,而是按字符数截断后追加省略号。
fn truncate_summary(s: &str) -> String {
    if s.chars().count() <= SUMMARY_MAX_CHARS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(SUMMARY_MAX_CHARS).collect();
    format!("{truncated}…")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// frontmatter 起始/结束标记。
const FRONTMATTER_DELIMITER: &str = "---";

/// 将 handoff 序列化为 `YAML frontmatter + Markdown body` 字符串。
///
/// 格式:
/// ```text
/// ---
/// <YAML frontmatter: 所有字段,serde_yaml 自动转义>
/// ---
///
/// # Subagent Result: {name}
///
/// ## Summary
/// {summary}
///
/// ## Details
/// {details}
/// ```
///
/// `serde_yaml` 自动为含特殊字符(`:`、`---`、换行)的 `summary` 选用双引号风格,
/// 为多行 `details` 选用 literal block scalar(`|`),确保 frontmatter 解析安全(§3.4)。
#[must_use]
pub fn serialize_handoff(handoff: &SubagentHandoff) -> String {
    // serde_yaml::to_string 产生一个 YAML 文档(不含 `---` 文档起始标记,
    // serde_yaml 0.9 默认省略)。我们手动包裹 `---` ... `---` 作为 frontmatter。
    let yaml = serde_yaml::to_string(handoff).unwrap_or_else(|e| {
        // 序列化失败时降级:把错误写入 details,保证文件可写
        format!(
            "subagent_id: \"{}\"\nname: \"serialization-error\"\nstatus: failed\nsummary: \
             \"serde_yaml error: {e}\"\ndetails: \"\"\n",
            handoff.subagent_id
        )
    });

    let body = format!(
        "# Subagent Result: {name}\n\n## Summary\n{summary}\n\n## Details\n{details}",
        name = handoff.name,
        summary = handoff.summary,
        details = handoff.details,
    );

    format!("{FRONTMATTER_DELIMITER}\n{yaml}{FRONTMATTER_DELIMITER}\n\n{body}")
}

/// 解析 `YAML frontmatter + Markdown body` 字符串为 [`SubagentHandoff`]。
///
/// **向后兼容**(§Epic 5 测试):无 frontmatter 的旧格式纯文本文件 → 整体作为 `details`,
/// `status` 标记为 `Legacy`。主 agent 仍可读取。
///
/// # 错误
/// - frontmatter 存在但 YAML 解析失败 → 返回 Err
pub fn parse_handoff(content: &str) -> Result<SubagentHandoff, String> {
    let trimmed = content.trim_start_matches('\u{feff}');
    let lines: Vec<&str> = trimmed.lines().collect();

    // 检测 frontmatter:首行必须恰好是 "---"
    if lines.first().is_none_or(|l| *l != FRONTMATTER_DELIMITER) {
        return Ok(legacy_handoff(content));
    }

    // 查找闭合 "---"(column 0,非缩进)。frontmatter 内的 YAML 块标量内容
    // 均有缩进,故 column-0 的 "---" 无歧义标记 frontmatter 结束。
    let close_idx = lines
        .iter()
        .skip(1)
        .position(|l| *l == FRONTMATTER_DELIMITER)
        .map(|i| i + 1)
        .ok_or_else(|| "handoff frontmatter missing closing --- delimiter".to_string())?;

    let yaml_str: String = lines[1..close_idx].join("\n");
    let mut handoff: SubagentHandoff =
        serde_yaml::from_str(&yaml_str).map_err(|e| format!("handoff YAML parse error: {e}"))?;

    // 若 frontmatter 未含 summary/details(body 中有但 frontmatter 缺失),
    // 从 body 提取。正常序列化路径下 frontmatter 已含这两个字段,此为防御性补充。
    if handoff.summary.is_empty() || handoff.details.is_empty() {
        let body: String = lines[close_idx + 1..].join("\n");
        let (summary, details) = extract_body_sections(&body);
        if handoff.summary.is_empty() {
            handoff.summary = summary;
        }
        if handoff.details.is_empty() {
            handoff.details = details;
        }
    }

    Ok(handoff)
}

/// 旧格式降级:无 frontmatter 的纯文本 → `Legacy` 状态,整体作为 details。
fn legacy_handoff(content: &str) -> SubagentHandoff {
    SubagentHandoff {
        subagent_id: String::new(),
        name: "legacy".to_string(),
        task: String::new(),
        capability: SubagentCapability::Analyze,
        complexity: TaskComplexity::Simple,
        iterations: 1,
        tools_used: Vec::new(),
        changed_files: Vec::new(),
        status: HandoffStatus::Legacy,
        timestamp: now_secs(),
        summary: truncate_summary(content),
        details: content.to_string(),
    }
}

/// 从 Markdown body 提取 `## Summary` 和 `## Details` 段落(防御性补充)。
fn extract_body_sections(body: &str) -> (String, String) {
    let mut summary = String::new();
    let mut details = String::new();
    let mut current: Option<&mut String> = None;

    for line in body.lines() {
        if line.starts_with("## Summary") {
            current = Some(&mut summary);
            continue;
        }
        if line.starts_with("## Details") {
            current = Some(&mut details);
            continue;
        }
        if let Some(buf) = current.as_deref_mut() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(line);
        }
    }

    (summary.trim().to_string(), details.trim().to_string())
}

/// 将 handoff 原子写入 `<workspace_root>/.claw/subagents/{subagent_id}.md`。
///
/// 原子写:先写 `.tmp` 再 rename,防止并发读取半写文件。
///
/// # 返回
/// 写入文件的相对路径(如 `.claw/subagents/{id}.md`),便于主 agent 在 tool result 中引用。
pub fn write_handoff(workspace_root: &Path, handoff: &SubagentHandoff) -> Result<String, String> {
    let subagents_dir = workspace_root.join(".claw").join("subagents");
    std::fs::create_dir_all(&subagents_dir)
        .map_err(|e| format!("failed to create subagents dir: {e}"))?;

    let result_path = subagents_dir.join(format!("{}.md", handoff.subagent_id));
    let tmp_path = subagents_dir.join(format!("{}.md.tmp", handoff.subagent_id));

    let content = serialize_handoff(handoff);
    std::fs::write(&tmp_path, &content)
        .map_err(|e| format!("failed to write handoff tmp file: {e}"))?;
    std::fs::rename(&tmp_path, &result_path)
        .map_err(|e| format!("failed to rename handoff file: {e}"))?;

    Ok(format!(".claw/subagents/{}.md", handoff.subagent_id))
}

/// 从文件读取并解析 handoff。
pub fn read_handoff(path: &Path) -> Result<SubagentHandoff, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read handoff file: {e}"))?;
    parse_handoff(&content)
}

/// 从 `edit`/`write` 工具调用输入(JSON)提取文件路径,用于填充 `changed_files`。
///
/// `edit` 工具的 `file_path` 字段 / `write` 工具的 `file_path` 字段。
/// 支持单次调用修改多文件(批量编辑场景):遍历输入中所有 `file_path` 值。
///
/// **路径规范化**(与 Epic 4 一致):`workspace_root.join` + `canonicalize` 规范化,
/// 失败时用 `workspace_root.join` 后去重作为 fallback。返回去重后的路径列表。
pub fn extract_changed_files(tool_input: &str, workspace_root: &Path) -> Vec<String> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(tool_input) else {
        return Vec::new();
    };

    let mut paths: Vec<PathBuf> = Vec::new();

    // 单对象:`file_path` 字段
    if let Some(fp) = parsed.get("file_path").and_then(|v| v.as_str()) {
        paths.push(workspace_root.join(fp));
    }
    // 批量:`files` 数组(每项含 file_path)— 预留,当前 edit/write 均为单文件
    if let Some(files) = parsed.get("files").and_then(|v| v.as_array()) {
        for f in files {
            if let Some(fp) = f.get("file_path").and_then(|v| v.as_str()) {
                paths.push(workspace_root.join(fp));
            }
        }
    }

    // 规范化 + 去重
    let mut normalized: Vec<String> = paths
        .iter()
        .map(|p| normalize_path(p, workspace_root))
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

/// 规范化路径:`canonicalize` 成功则用规范化结果;失败则用 join 后的相对路径 fallback。
fn normalize_path(abs_path: &Path, workspace_root: &Path) -> String {
    if let Ok(canon) = std::fs::canonicalize(abs_path) {
        return path_to_string(&canon, workspace_root);
    }
    // fallback:相对 workspace_root 的路径(保持与 git diff 一致的可读形式)
    path_to_string(abs_path, workspace_root)
}

/// 将绝对路径转为相对 workspace_root 的字符串(若可剥离前缀),否则返回绝对路径字符串。
fn path_to_string(abs_path: &Path, workspace_root: &Path) -> String {
    if let Ok(rel) = abs_path.strip_prefix(workspace_root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    abs_path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_handoff() -> SubagentHandoff {
        SubagentHandoff::new(
            "subagent-1",
            "fix-tool",
            SubagentCapability::Execute,
            TaskComplexity::Architectural,
            3,
            vec!["read_file".into(), "edit_file".into(), "bash".into()],
            vec!["src/foo.rs".into(), "src/bar.rs".into()],
            "修复了 edit 工具的路径问题",
            "完整分析过程\n多行内容\n含 --- 分隔符也不影响解析",
        )
    }

    #[test]
    fn serialize_parse_roundtrip() {
        let original = sample_handoff().with_timestamp(1786015370);
        let serialized = serialize_handoff(&original);
        assert!(serialized.starts_with("---\n"));
        assert!(serialized.contains("subagent_id: subagent-1"));
        assert!(serialized.contains("# Subagent Result: fix-tool"));

        let parsed = parse_handoff(&serialized).expect("parse should succeed");
        assert_eq!(parsed, original);
    }

    #[test]
    fn summary_truncation() {
        let long_summary = "x".repeat(SUMMARY_MAX_CHARS + 100);
        let h = SubagentHandoff::new(
            "id",
            "n",
            SubagentCapability::Analyze,
            TaskComplexity::Simple,
            1,
            vec![],
            vec![],
            long_summary.clone(),
            "details",
        );
        assert_eq!(h.summary.chars().count(), SUMMARY_MAX_CHARS + 1); // +1 for …
        assert!(h.summary.ends_with('…'));
    }

    #[test]
    fn special_chars_in_summary_and_details() {
        // summary 含 : 和 ---(YAML 特殊字符)
        let h = SubagentHandoff::new(
            "id",
            "n",
            SubagentCapability::Execute,
            TaskComplexity::Simple,
            1,
            vec![],
            vec![],
            "修复: 相对路径未 join workspace_root --- 已验证",
            "line1\n---\nline3: with colon",
        )
        .with_timestamp(123);

        let serialized = serialize_handoff(&h);
        let parsed = parse_handoff(&serialized).expect("parse should succeed");
        assert_eq!(parsed, h);
        assert_eq!(
            parsed.summary,
            "修复: 相对路径未 join workspace_root --- 已验证"
        );
        assert_eq!(parsed.details, "line1\n---\nline3: with colon");
    }

    #[test]
    fn legacy_pure_text_backward_compat() {
        let old_content =
            "# Subagent Result: old-agent\n\nSome plain text result without frontmatter.";
        let parsed = parse_handoff(old_content).expect("legacy parse should succeed");
        assert_eq!(parsed.status, HandoffStatus::Legacy);
        assert_eq!(parsed.details, old_content);
        assert_eq!(parsed.capability, SubagentCapability::Analyze);
    }

    #[test]
    fn truncated_status_preserved() {
        let h = sample_handoff()
            .with_status(HandoffStatus::Truncated)
            .with_timestamp(999);
        let serialized = serialize_handoff(&h);
        let parsed = parse_handoff(&serialized).expect("parse should succeed");
        assert_eq!(parsed.status, HandoffStatus::Truncated);
        assert_eq!(parsed.timestamp, 999);
    }

    #[test]
    fn write_and_read_handoff_file() {
        let dir = std::env::temp_dir().join("claw_handoff_test");
        std::fs::create_dir_all(&dir).unwrap();
        let h = sample_handoff().with_timestamp(42);

        let rel_path = write_handoff(&dir, &h).expect("write should succeed");
        assert!(rel_path.starts_with(".claw/subagents/"));

        let abs_path = dir.join(rel_path);
        let read_back = read_handoff(&abs_path).expect("read should succeed");
        assert_eq!(read_back, h);

        // cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_changed_files_from_edit_input() {
        let input = r#"{"file_path": "src/foo.rs", "old_string": "a", "new_string": "b"}"#;
        let ws = Path::new("/workspace");
        let files = extract_changed_files(input, ws);
        assert_eq!(files, vec!["src/foo.rs"]);
    }

    #[test]
    fn extract_changed_files_invalid_json() {
        let files = extract_changed_files("not json", Path::new("/ws"));
        assert!(files.is_empty());
    }

    #[test]
    fn extract_changed_files_dedup() {
        // 同一路径多次出现应去重
        let input = r#"{"file_path": "src/dup.rs"}"#;
        let ws = Path::new("/ws");
        let files = extract_changed_files(input, ws);
        assert_eq!(files.len(), 1);
    }
}
