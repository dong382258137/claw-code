//! Notebook — Step P0-1 Structured Note-taking(Anthropic 推荐).
//!
//! 设计依据:
//! - Anthropic《Effective Context Engineering for AI Agents》(2025):
//!   "Structured note-taking, or agentic memory, is a technique where the
//!   agent regularly writes notes persisted to memory outside of the
//!   context window. These notes get pulled back into the context window
//!   at later times."
//! - Anthropic《Multi-Agent Research System》(2025):
//!   "The LeadResearcher begins by thinking through the approach and
//!   saving its plan to Memory to persist the context, since if the
//!   context window exceeds 200,000 tokens it will be truncated and it
//!   is important to retain the plan."
//! - MemGPT (arXiv:2310.08560):OS-inspired 分层内存,NOTEBOOK 作为
//!   "working context"(L1 cache),跨压缩持久化。
//!
//! # 架构定位
//!
//! NOTEBOOK.md 是 **Layer 2 — Working Notes**,位于 Main Context(LLM 推理窗口)
//! 与 External Storage(磁盘文件)之间:
//!
//! ```text
//! Layer 1: Main Context (LLM 推理窗口)
//!          ↑↓ page in/out
//! Layer 2: NOTEBOOK.md (本模块) — 跨压缩持久化
//!          ↑↓ fetch/deref
//! Layer 3: External Storage (.claw/subagents/*, trace CSV, ...)
//! ```
//!
//! # 关键不变量
//!
//! - NOTEBOOK.md **不在 message history 中**,因此 microcompact / compact_session
//!   不会影响它。它通过 system_prompt 的变动区每个 turn 重新注入。
//! - NOTEBOOK.md 的内容由 LLM 通过 `notebook_update` 工具主动维护,
//!   类似 TodoWrite 模式(Anthropic 明确推荐)。
//! - 写入采用原子写(`.tmp` + `rename`),避免崩溃破坏文件。
//!
//! # 段结构(XML 标签,Anthropic 推荐)
//!
//! ```xml
//! <plan>
//!   当前任务的关键决策、约束、进度
//! </plan>
//!
//! <subagents>
//!   - subagent-1: 分析缠论线段定义 | status=completed | result_ref=.claw/subagents/subagent-1.md
//!   - subagent-2: ... | status=running
//! </subagents>
//!
//! <attempted>
//!   - 方案A(2026-07-21):尝试用 X 方法,失败,原因:...
//!   - 方案B(2026-07-21):成功,关键:...
//! </attempted>
//!
//! <preferences>
//!   - 用户偏好:复杂任务用 Plan 模式
//!   - 用户约束:不允许 ...
//! </preferences>
//!
//! <key_files>
//!   - src/conversation.rs: ConversationRuntime 主循环
//!   - src/compact.rs: microcompact 实现
//! </key_files>
//! ```

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// NOTEBOOK.md 默认文件名(相对于 workspace_root)。
pub const NOTEBOOK_FILENAME: &str = ".claw/NOTEBOOK.md";

/// NOTEBOOK.md 内容的最大字符数(防止 LLM 写入失控)。
pub const NOTEBOOK_MAX_CHARS: usize = 16_000;

/// NOTEBOOK.md 的段标识(Anthropic 推荐的 XML 标签分段)。
///
/// 顺序固定,便于 LLM 解析和人类阅读。
pub const SECTION_TAGS: &[&str] = &["plan", "subagents", "attempted", "preferences", "key_files"];

/// NOTEBOOK.md 的渲染头部,解释文档用途,引导 LLM 正确维护。
pub const NOTEBOOK_HEADER: &str = "# NOTEBOOK — Structured Working Memory\n\
    \n\
    本文件是 AI 助手的工作记忆,跨压缩持久化。**microcompact / compact_session 不会影响本文件**。\n\
    通过 `notebook_update` 工具维护。每个 turn 开始时注入到 system_prompt 变动区。\n\
    \n\
    ## 段说明\n\
    - `<plan>`:当前任务的关键决策、约束、进度\n\
    - `<subagents>`:已 dispatch 的子智能体注册表(name | status | result_ref)\n\
    - `<attempted>`:已尝试的方案及结论(成功/失败 + 原因)\n\
    - `<preferences>`:用户明确表达的偏好/约束\n\
    - `<key_files>`:关键文件引用 + 一句话摘要\n";

/// NOTEBOOK 数据模型 — 按 XML 段组织的内容。
///
/// `BTreeMap` 保证段顺序稳定(与 [`SECTION_TAGS`] 一致),
/// 便于 diff 和测试断言。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notebook {
    /// 段名 → 段内容(纯文本,不含 XML 标签)。
    ///
    /// 缺失的段在渲染时输出空段(保持结构稳定)。
    pub sections: BTreeMap<String, String>,
}

impl Notebook {
    /// 创建空 NOTEBOOK。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 NOTEBOOK.md 文件加载。
    ///
    /// 文件不存在时返回空 NOTEBOOK(不报错,支持首次创建)。
    /// 文件存在但解析失败时返回 Err(避免静默丢失用户数据)。
    pub fn load(workspace_root: &Path) -> Result<Self, NotebookError> {
        let path = workspace_root.join(NOTEBOOK_FILENAME);
        if !path.exists() {
            return Ok(Self::new());
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| NotebookError::Io(path.clone(), e.to_string()))?;
        Self::parse(&content).map_err(|e| NotebookError::Parse(path, e))
    }

    /// 原子写入 NOTEBOOK.md 文件。
    ///
    /// 写入流程:`.claw/` 目录创建 → 写 `NOTEBOOK.md.tmp` → `rename` 为 `NOTEBOOK.md`。
    /// 这样即使写入中途崩溃,原文件也保持完整。
    pub fn save(&self, workspace_root: &Path) -> Result<(), NotebookError> {
        let path = workspace_root.join(NOTEBOOK_FILENAME);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| NotebookError::Io(parent.to_path_buf(), e.to_string()))?;
        }
        let tmp_path = path.with_extension("md.tmp");
        let content = self.render();
        if content.chars().count() > NOTEBOOK_MAX_CHARS {
            return Err(NotebookError::TooLarge {
                actual: content.chars().count(),
                max: NOTEBOOK_MAX_CHARS,
            });
        }
        {
            let mut file = std::fs::File::create(&tmp_path)
                .map_err(|e| NotebookError::Io(tmp_path.clone(), e.to_string()))?;
            file.write_all(content.as_bytes())
                .map_err(|e| NotebookError::Io(tmp_path.clone(), e.to_string()))?;
            file.flush()
                .map_err(|e| NotebookError::Io(tmp_path.clone(), e.to_string()))?;
        }
        std::fs::rename(&tmp_path, &path)
            .map_err(|e| NotebookError::Io(path.clone(), e.to_string()))?;
        Ok(())
    }

    /// 解析 NOTEBOOK.md 文本内容为 Notebook 结构。
    ///
    /// 解析规则:
    /// - 跳过头部(直到第一个**行首** `<section_tag>` 开始)
    /// - 每个 `<tag>...</tag>` 段的内容(去除首尾空白)作为该段的值
    /// - 标签必须**独立成行**(行首匹配),避免 NOTEBOOK_HEADER 中
    ///   `` `<plan>` `` 等字面引用被误识别为段标识
    /// - 未识别的标签被忽略(向前兼容)
    /// - 段可以以任意顺序出现,但 [`render`] 输出按 [`SECTION_TAGS`] 顺序
    pub fn parse(content: &str) -> Result<Self, String> {
        // 逐字符扫描行首位置,正确处理:
        // - 空段(`<tag>\n</tag>`):close tag 前是 \n,直接匹配
        // - 文件结尾的标签(无 trailing \n):end_pos == content_after_open.len()
        // - NOTEBOOK_HEADER 中 `` `<tag>` `` 字面引用(不在行首):跳过
        // - 文件开头就是 `<tag>` 的边界情况:abs_pos == 0 也算行首
        let mut sections: BTreeMap<String, String> = BTreeMap::new();
        let bytes = content.as_bytes();
        for tag in SECTION_TAGS {
            let open_tag = format!("<{tag}>");
            let close_tag = format!("</{tag}>");
            // 扫描所有 <tag> 出现位置,筛选行首的(前一字符是 \n 或文件开头)
            let mut open_pos: Option<usize> = None;
            let mut search_from = 0;
            while let Some(pos) = content[search_from..].find(&open_tag) {
                let abs_pos = search_from + pos;
                let at_line_start = abs_pos == 0 || bytes.get(abs_pos - 1) == Some(&b'\n');
                if at_line_start {
                    open_pos = Some(abs_pos);
                    break;
                }
                search_from = abs_pos + open_tag.len();
            }
            let Some(open_pos) = open_pos else {
                continue; // 该段缺失,跳过(向前兼容)
            };
            // 跳过 `<tag>` 本身,若紧跟 \n 则跳过换行
            let mut content_start = open_pos + open_tag.len();
            if bytes.get(content_start) == Some(&b'\n') {
                content_start += 1;
            }
            let content_after_open = &content[content_start..];
            // close tag 优先找行首(多行 XML),找不到再找任意位置(单行 `<tag>x</tag>`)。
            // 不要求行首是为了支持测试用例 `<plan>real plan</plan>` 这类单行格式;
            // open tag 仍严格要求行首(避免误匹配 NOTEBOOK_HEADER 中的字面引用)。
            let mut close_pos: Option<usize> = None;
            let mut search_from = 0;
            let bytes_after = content_after_open.as_bytes();
            while let Some(pos) = content_after_open[search_from..].find(&close_tag) {
                let abs_pos = search_from + pos;
                let at_line_start = abs_pos == 0 || bytes_after.get(abs_pos - 1) == Some(&b'\n');
                if at_line_start {
                    close_pos = Some(abs_pos);
                    break;
                }
                search_from = abs_pos + close_tag.len();
            }
            // 退而求其次:找任意位置的 close tag(支持单行格式)
            if close_pos.is_none() {
                close_pos = content_after_open.find(&close_tag);
            }
            let Some(close_pos) = close_pos else {
                return Err(format!("unterminated <{tag}> section"));
            };
            let section_content = content_after_open[..close_pos].trim().to_string();
            // 空段(内容为空)不加入 sections,与 set_section 语义一致,
            // 确保 round-trip:render → parse → 相同的 Notebook。
            if !section_content.is_empty() {
                sections.insert((*tag).to_string(), section_content);
            }
        }
        Ok(Self { sections })
    }

    /// 渲染 Notebook 为 NOTEBOOK.md 文本(头部 + 各 XML 段)。
    ///
    /// 段顺序固定为 [`SECTION_TAGS`],缺失的段输出为空段(保持结构稳定)。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(NOTEBOOK_HEADER);
        for tag in SECTION_TAGS {
            out.push('\n');
            out.push_str(&format!("<{tag}>\n"));
            if let Some(content) = self.sections.get(*tag) {
                if !content.is_empty() {
                    out.push_str(content);
                    out.push('\n');
                }
            }
            out.push_str(&format!("</{tag}>\n"));
        }
        out
    }

    /// 渲染为 system_prompt 变动区注入文本(精简版,不含头部)。
    ///
    /// 这是每个 turn 注入到 LLM system_prompt 的内容。空段被跳过,
    /// 避免浪费 token。完全空 NOTEBOOK 返回空字符串(不注入)。
    #[must_use]
    pub fn render_for_prompt(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for tag in SECTION_TAGS {
            let Some(content) = self.sections.get(*tag) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }
            parts.push(format!("<{tag}>\n{content}\n</{tag}>"));
        }
        if parts.is_empty() {
            return String::new();
        }
        format!(
            "# Working Memory (NOTEBOOK.md)\n\
             以下是持久化的工作记忆,跨压缩不会丢失。\
             使用 `notebook_update` 工具维护本记忆。\n\n\
             {}\n\n\
             提示:完成非平凡修复后,调用 `log_decision` 持久化经验(问题签名+根因+方案+验证结果);\
             开始非平凡修复前,先 `search_past_decisions` 查是否遇到过类似问题。",
            parts.join("\n\n")
        )
    }

    /// 更新某个段的内容。空字符串等价于删除该段。
    pub fn set_section(&mut self, tag: &str, content: &str) {
        if content.trim().is_empty() {
            self.sections.remove(tag);
        } else {
            self.sections
                .insert(tag.to_string(), content.trim().to_string());
        }
    }

    /// 读取某个段的内容。段不存在返回 None。
    #[must_use]
    pub fn get_section(&self, tag: &str) -> Option<&str> {
        self.sections.get(tag).map(String::as_str)
    }

    /// 追加一行到某个段(自动换行)。段不存在时创建。
    pub fn append_to_section(&mut self, tag: &str, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let entry = self.sections.entry(tag.to_string()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(trimmed);
    }

    /// NOTEBOOK 是否完全为空(所有段都缺失或空)。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.values().all(|s| s.trim().is_empty())
    }
}

/// NOTEBOOK 操作错误。
#[derive(Debug)]
pub enum NotebookError {
    /// 文件 IO 错误(路径 + 消息)。
    Io(PathBuf, String),
    /// 文件解析错误(路径 + 解析错误消息)。
    Parse(PathBuf, String),
    /// 内容超过 [`NOTEBOOK_MAX_CHARS`]。
    TooLarge { actual: usize, max: usize },
}

impl std::fmt::Display for NotebookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(path, msg) => write!(f, "notebook io error at {}: {msg}", path.display()),
            Self::Parse(path, msg) => {
                write!(f, "notebook parse error at {}: {msg}", path.display())
            }
            Self::TooLarge { actual, max } => write!(
                f,
                "notebook content too large: {actual} chars > {max} chars"
            ),
        }
    }
}

impl std::error::Error for NotebookError {}

/// `notebook_update` 工具的输入参数。
///
/// LLM 通过此工具维护 NOTEBOOK。支持两种模式:
/// - `set`:整体覆盖某个段(用于 plan / preferences 等需要重写的段)
/// - `append`:追加一行到某个段(用于 subagents / attempted / key_files 等增量段)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookUpdateInput {
    /// 操作模式:`set` 或 `append`。
    pub mode: String,
    /// 目标段名(必须是 [`SECTION_TAGS`] 之一)。
    pub section: String,
    /// 段内容(set 模式)或要追加的行(append 模式)。
    pub content: String,
}

/// `notebook_update` 工具的 Tool specification(JSON schema)。
///
/// 注册到 LLM 的 tool list,让 LLM 知道可以维护 NOTEBOOK。
pub const NOTEBOOK_UPDATE_TOOL_SPEC: &str = r#"{
    "name": "notebook_update",
    "description": "Update the persistent working memory (NOTEBOOK.md). This memory survives context compaction — use it to record key decisions, subagent registry, attempted approaches, user preferences, and key file references. CRITICAL: always record subagent dispatches here so you do not re-dispatch the same task later. Modes: 'set' (overwrite section) or 'append' (add a line). Sections: plan, subagents, attempted, preferences, key_files.",
    "input_schema": {
        "type": "object",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["set", "append"],
                "description": "Operation mode: 'set' overwrites the entire section; 'append' adds a single line to the section."
            },
            "section": {
                "type": "string",
                "enum": ["plan", "subagents", "attempted", "preferences", "key_files"],
                "description": "Target section name."
            },
            "content": {
                "type": "string",
                "description": "For 'set': full section content. For 'append': a single line to add (newline-terminated automatically)."
            }
        },
        "required": ["mode", "section", "content"]
    }
}"#;

/// 执行 `notebook_update` 工具调用。
///
/// 调用方:`ConversationRuntime::execute_notebook_update` 内部拦截。
/// 流程:解析 JSON 输入 → 加载 NOTEBOOK → 修改 → 原子写回 → 返回成功消息。
///
/// 返回值:面向 LLM 的可读消息(成功 / 失败 + 原因)。
pub fn execute_notebook_update(workspace_root: &Path, input: &str) -> Result<String, String> {
    let parsed: NotebookUpdateInput = serde_json::from_str(input)
        .map_err(|e| format!("invalid notebook_update input JSON: {e}"))?;
    // 验证 section 名
    if !SECTION_TAGS.contains(&parsed.section.as_str()) {
        return Err(format!(
            "invalid section '{}': must be one of {:?}",
            parsed.section, SECTION_TAGS
        ));
    }
    // 加载现有 NOTEBOOK
    let mut notebook =
        Notebook::load(workspace_root).map_err(|e| format!("failed to load notebook: {e}"))?;
    // 应用更新
    match parsed.mode.as_str() {
        "set" => {
            notebook.set_section(&parsed.section, &parsed.content);
        }
        "append" => {
            notebook.append_to_section(&parsed.section, &parsed.content);
        }
        other => {
            return Err(format!("invalid mode '{other}': must be 'set' or 'append'"));
        }
    }
    // 原子写回
    notebook
        .save(workspace_root)
        .map_err(|e| format!("failed to save notebook: {e}"))?;
    Ok(format!(
        "NOTEBOOK.md updated: section '{}' {} ({} chars total).",
        parsed.section,
        match parsed.mode.as_str() {
            "set" => "set",
            "append" => "appended",
            _ => "modified",
        },
        notebook.render().chars().count()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    #[test]
    fn notebook_new_is_empty() {
        let nb = Notebook::new();
        assert!(nb.is_empty());
        assert_eq!(nb.render_for_prompt(), "");
    }

    #[test]
    fn notebook_set_and_get_section() {
        let mut nb = Notebook::new();
        nb.set_section("plan", "  实施 NOTEBOOK 模块  ");
        assert_eq!(nb.get_section("plan"), Some("实施 NOTEBOOK 模块"));
        assert!(!nb.is_empty());
    }

    #[test]
    fn notebook_set_empty_removes_section() {
        let mut nb = Notebook::new();
        nb.set_section("plan", "some plan");
        assert!(nb.sections.contains_key("plan"));
        nb.set_section("plan", "   ");
        assert!(!nb.sections.contains_key("plan"));
        assert!(nb.is_empty());
    }

    #[test]
    fn notebook_append_to_section_creates_if_absent() {
        let mut nb = Notebook::new();
        nb.append_to_section("subagents", "subagent-1: task A | status=completed");
        nb.append_to_section("subagents", "subagent-2: task B | status=running");
        let content = nb.get_section("subagents").unwrap();
        assert!(content.contains("subagent-1"));
        assert!(content.contains("subagent-2"));
        assert_eq!(content.matches('\n').count(), 1); // 两行,一个换行
    }

    #[test]
    fn notebook_append_ignores_empty_line() {
        let mut nb = Notebook::new();
        nb.append_to_section("plan", "   ");
        assert!(nb.is_empty());
    }

    #[test]
    fn notebook_render_includes_header_and_all_sections() {
        let mut nb = Notebook::new();
        nb.set_section("plan", "test plan");
        nb.set_section("subagents", "subagent-1: done");
        let rendered = nb.render();
        assert!(rendered.contains(NOTEBOOK_HEADER));
        assert!(rendered.contains("<plan>"));
        assert!(rendered.contains("test plan"));
        assert!(rendered.contains("</plan>"));
        assert!(rendered.contains("<subagents>"));
        assert!(rendered.contains("subagent-1: done"));
        assert!(rendered.contains("</subagents>"));
        // 缺失的段也应该有空段
        assert!(rendered.contains("<attempted>"));
        assert!(rendered.contains("</attempted>"));
        assert!(rendered.contains("<preferences>"));
        assert!(rendered.contains("</preferences>"));
        assert!(rendered.contains("<key_files>"));
        assert!(rendered.contains("</key_files>"));
    }

    #[test]
    fn notebook_render_for_prompt_skips_empty_sections() {
        let mut nb = Notebook::new();
        nb.set_section("plan", "only plan");
        let prompt = nb.render_for_prompt();
        assert!(prompt.contains("Working Memory"));
        assert!(prompt.contains("<plan>"));
        assert!(prompt.contains("only plan"));
        // 空段不应该出现在 prompt 注入中
        assert!(!prompt.contains("<subagents>"));
        assert!(!prompt.contains("<attempted>"));
    }

    #[test]
    fn notebook_render_for_prompt_empty_returns_empty() {
        let nb = Notebook::new();
        assert_eq!(nb.render_for_prompt(), "");
    }

    #[test]
    fn notebook_parse_round_trip() {
        let mut original = Notebook::new();
        original.set_section("plan", "实施 P0-1\n多行内容");
        original.set_section("subagents", "subagent-1: done");
        let rendered = original.render();
        let parsed = Notebook::parse(&rendered).expect("parse should succeed");
        assert_eq!(parsed, original);
    }

    #[test]
    fn notebook_parse_handles_missing_sections() {
        let content =
            format!("{NOTEBOOK_HEADER}\n<plan>\nonly plan\n</plan>\n<subagents>\n</subagents>\n");
        let parsed = Notebook::parse(&content).expect("parse should succeed");
        assert_eq!(parsed.get_section("plan"), Some("only plan"));
        // 空段等价于缺失段(与 set_section 语义一致:空内容 = 删除)
        // 这样保证 round-trip:render 只输出有内容的段 → parse 后缺失段保持缺失
        assert_eq!(parsed.get_section("subagents"), None);
        assert_eq!(parsed.get_section("attempted"), None);
    }

    #[test]
    fn notebook_parse_errors_on_unterminated_section() {
        let content = "<plan>unterminated content without closing tag";
        let result = Notebook::parse(content);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("unterminated"));
        assert!(err.contains("plan"));
    }

    #[test]
    fn notebook_parse_ignores_unknown_tags() {
        let content = "<unknown_tag>should be ignored</unknown_tag>\n<plan>real plan</plan>";
        let parsed = Notebook::parse(content).expect("parse should succeed");
        assert_eq!(parsed.get_section("plan"), Some("real plan"));
        assert_eq!(parsed.get_section("unknown_tag"), None);
    }

    #[test]
    fn notebook_load_returns_empty_when_file_missing() {
        let dir = temp_workspace();
        let nb = Notebook::load(dir.path()).expect("load should succeed");
        assert!(nb.is_empty());
    }

    #[test]
    fn notebook_save_and_load_round_trip() {
        let dir = temp_workspace();
        let mut original = Notebook::new();
        original.set_section("plan", "save & load test");
        original.set_section("subagents", "subagent-1: completed");
        original.save(dir.path()).expect("save should succeed");

        // 验证文件存在
        let path = dir.path().join(NOTEBOOK_FILENAME);
        assert!(path.exists(), "NOTEBOOK.md should exist after save");

        // 加载并比较
        let loaded = Notebook::load(dir.path()).expect("load should succeed");
        assert_eq!(loaded, original);
    }

    #[test]
    fn notebook_save_creates_claw_directory() {
        let dir = temp_workspace();
        let mut nb = Notebook::new();
        nb.set_section("plan", "test");
        nb.save(dir.path()).expect("save should succeed");
        let claw_dir = dir.path().join(".claw");
        assert!(claw_dir.exists(), ".claw/ directory should be created");
    }

    #[test]
    fn notebook_save_uses_atomic_write() {
        let dir = temp_workspace();
        let mut nb = Notebook::new();
        nb.set_section("plan", "first version");
        nb.save(dir.path()).expect("save should succeed");
        let path = dir.path().join(NOTEBOOK_FILENAME);

        // 第一次写入后,.tmp 文件应该已被 rename,不应该存在
        let tmp_path = path.with_extension("md.tmp");
        assert!(
            !tmp_path.exists(),
            "temp file should not exist after atomic save"
        );

        // 再次写入
        nb.set_section("plan", "second version");
        nb.save(dir.path()).expect("save should succeed");
        let loaded = Notebook::load(dir.path()).expect("load should succeed");
        assert_eq!(loaded.get_section("plan"), Some("second version"));
    }

    #[test]
    fn notebook_save_rejects_oversized_content() {
        let dir = temp_workspace();
        let mut nb = Notebook::new();
        // 创建超过 NOTEBOOK_MAX_CHARS 的内容(实际 render 总字符数 = header + 段)
        let huge_content = "x".repeat(NOTEBOOK_MAX_CHARS);
        nb.set_section("plan", &huge_content);
        let result = nb.save(dir.path());
        assert!(result.is_err());
        match result {
            Err(NotebookError::TooLarge { actual, max }) => {
                // actual 是 render() 总字符数(包含 header),一定 > max
                assert!(actual > max, "actual ({actual}) should be > max ({max})");
                assert_eq!(max, NOTEBOOK_MAX_CHARS);
            }
            other => panic!("expected TooLarge error, got {other:?}"),
        }
    }

    #[test]
    fn notebook_load_returns_error_on_corrupted_file() {
        let dir = temp_workspace();
        let path = dir.path().join(NOTEBOOK_FILENAME);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        // 写入未闭合的 XML 标签
        writeln!(file, "<plan>unterminated").unwrap();
        let result = Notebook::load(dir.path());
        assert!(result.is_err());
        match result {
            Err(NotebookError::Parse(_, msg)) => {
                assert!(msg.contains("unterminated"));
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn notebook_update_tool_spec_is_valid_json() {
        let spec: serde_json::Value = serde_json::from_str(NOTEBOOK_UPDATE_TOOL_SPEC)
            .expect("NOTEBOOK_UPDATE_TOOL_SPEC must be valid JSON");
        assert_eq!(spec["name"], "notebook_update");
        assert_eq!(
            spec["input_schema"]["properties"]["mode"]["enum"],
            serde_json::json!(["set", "append"])
        );
        let sections: Vec<String> = spec["input_schema"]["properties"]["section"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(sections, SECTION_TAGS);
    }

    #[test]
    fn execute_notebook_update_set_mode_works() {
        let dir = temp_workspace();
        let input = serde_json::json!({
            "mode": "set",
            "section": "plan",
            "content": "实施 NOTEBOOK 模块"
        })
        .to_string();
        let result = execute_notebook_update(dir.path(), &input);
        assert!(result.is_ok(), "execute should succeed: {:?}", result);
        // 验证写入
        let loaded = Notebook::load(dir.path()).unwrap();
        assert_eq!(loaded.get_section("plan"), Some("实施 NOTEBOOK 模块"));
    }

    #[test]
    fn execute_notebook_update_append_mode_works() {
        let dir = temp_workspace();
        // 先 set 一行
        let input1 = serde_json::json!({
            "mode": "append",
            "section": "subagents",
            "content": "subagent-1: task A | status=completed"
        })
        .to_string();
        execute_notebook_update(dir.path(), &input1).unwrap();
        // 再 append 一行
        let input2 = serde_json::json!({
            "mode": "append",
            "section": "subagents",
            "content": "subagent-2: task B | status=running"
        })
        .to_string();
        execute_notebook_update(dir.path(), &input2).unwrap();
        // 验证两行都在
        let loaded = Notebook::load(dir.path()).unwrap();
        let content = loaded.get_section("subagents").unwrap();
        assert!(content.contains("subagent-1"));
        assert!(content.contains("subagent-2"));
    }

    #[test]
    fn execute_notebook_update_rejects_invalid_section() {
        let dir = temp_workspace();
        let input = serde_json::json!({
            "mode": "set",
            "section": "invalid_section",
            "content": "test"
        })
        .to_string();
        let result = execute_notebook_update(dir.path(), &input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("invalid section"));
    }

    #[test]
    fn execute_notebook_update_rejects_invalid_mode() {
        let dir = temp_workspace();
        let input = serde_json::json!({
            "mode": "delete",
            "section": "plan",
            "content": "test"
        })
        .to_string();
        let result = execute_notebook_update(dir.path(), &input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("invalid mode"));
    }

    #[test]
    fn execute_notebook_update_rejects_malformed_json() {
        let dir = temp_workspace();
        let result = execute_notebook_update(dir.path(), "not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn notebook_render_for_prompt_includes_working_memory_header() {
        let mut nb = Notebook::new();
        nb.set_section("plan", "test");
        let prompt = nb.render_for_prompt();
        assert!(prompt.contains("Working Memory"));
        assert!(prompt.contains("NOTEBOOK.md"));
        assert!(prompt.contains("notebook_update"));
    }

    #[test]
    fn notebook_section_tags_order_is_stable() {
        let mut nb = Notebook::new();
        for tag in SECTION_TAGS {
            nb.set_section(tag, &format!("content for {tag}"));
        }
        let rendered = nb.render();
        let positions: Vec<usize> = SECTION_TAGS
            .iter()
            .map(|tag| rendered.find(&format!("<{tag}>")).unwrap())
            .collect();
        // 验证段顺序与 SECTION_TAGS 一致
        for i in 1..positions.len() {
            assert!(
                positions[i] > positions[i - 1],
                "section order should match SECTION_TAGS"
            );
        }
    }
}
