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

/// 改进点 12:`<evidence>` 段最大字符数。
/// 超出时从头部裁剪(保留最新的证据),避免证据段挤占其他段的容量。
pub const EVIDENCE_MAX_CHARS: usize = 4_096;

/// `<decisions>` 段最大字符数。
///
/// LLM 通过 `notebook_update` 向 decisions 段追加内容时不做任何去重,
/// 同一主题的思考过程会被反复记录(实测单个会话可堆积 14 条近似重复),
/// 导致该段膨胀至 7.6K 字符、占冻结槽位块 token 的 45%。超出此上限时
/// 从头部裁剪(保留最新的决策),与 `<attempted>` 段容量策略一致。
pub const DECISIONS_MAX_CHARS: usize = 6_144;

/// 对 `<decisions>` 段做确定性去重(源头防膨胀,无损)。
///
/// 处理三类冗余(实测 7,614 字符 → 6,435,压缩 ~15%;语义级重复
/// 需 LLM 压缩,本函数只做规则可判定的部分):
/// 1. **bash 过程噪音**:纯工具调用记录(`bash` 行 + 紧随的 `{json}` 行),
///    非决策内容,整块剔除;
/// 2. **行级去重**:trim 后完全相同的行只保留首次出现;
/// 3. **同 ID 首句归并**:`- [dXXXX] 首句...` 条目中,会话 ID 相同且首句
///    (去 `- [id] ` 前缀后前 20 字符)相同的视为同一主题的重复记录,
///    保留信息最全(字符数最多)的一条;
/// 4. **上限裁剪**:超过 [`DECISIONS_MAX_CHARS`] 时从头部裁剪,保留最新。
///
/// 保序输出(首次出现顺序),不改变剩余条目的相对位置。
#[must_use]
pub fn dedupe_decisions_section(content: &str) -> String {
    // 1) 剔除 bash 噪音块
    let lines: Vec<&str> = content.split('\n').collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let is_bash_noise = lines[i].trim() == "bash"
            && lines
                .get(i + 1)
                .is_some_and(|nxt| nxt.trim().starts_with('{'));
        if is_bash_noise {
            i += 2; // 跳过 bash 行与首个 JSON 行
                    // 跳过 JSON 的续行(以 { / } / " 开头的行)
            while i < lines.len()
                && (lines[i].trim().starts_with('{')
                    || lines[i].trim().starts_with('}')
                    || lines[i].trim().starts_with('"'))
            {
                i += 1;
            }
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }

    // 2) 拆条目(以 "- [d" 开头的行作为新条目起点)
    let mut entries: Vec<Vec<&str>> = Vec::new();
    for line in kept {
        if entries.is_empty() || line.trim().starts_with("- [d") {
            entries.push(Vec::new());
        }
        let last = entries.last_mut().expect("entry exists");
        last.push(line);
    }

    // 3) 行级去重(trim 后相同只保留首次)
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut deduped: Vec<Vec<String>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut ne: Vec<String> = Vec::with_capacity(entry.len());
        for line in entry {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                ne.push(line.to_string());
                continue;
            }
            if seen.contains(trimmed) {
                continue;
            }
            seen.insert(trimmed.to_string());
            ne.push(line.to_string());
        }
        deduped.push(ne);
    }

    // 4) 同 ID 首句归并:key = (id, 首句前20字符),保留字符数最多的条目
    #[derive(Clone)]
    struct Group {
        key: (String, String),
        text: String,
        order: usize,
    }
    let mut groups: Vec<Group> = Vec::new();
    let entry_texts: Vec<String> = deduped
        .iter()
        .map(|e| e.join("\n"))
        .collect::<Vec<String>>();

    for (idx, text) in entry_texts.iter().enumerate() {
        let first = text
            .lines()
            .find(|l| l.trim().starts_with("- [d"))
            .unwrap_or("")
            .trim();
        let key = if let Some(rest) = first.strip_prefix("- [") {
            if let Some(end) = rest.find(']') {
                let id = rest[..end].to_string();
                let sentence = rest[end + 1..].trim().chars().take(20).collect::<String>();
                (id, sentence)
            } else {
                ("__noid__".to_string(), first.chars().take(20).collect())
            }
        } else {
            ("__noid__".to_string(), first.chars().take(20).collect())
        };
        if let Some(g) = groups.iter_mut().find(|g| g.key == key) {
            if text.len() > g.text.len() {
                g.text = text.clone();
            }
        } else {
            groups.push(Group {
                key,
                text: text.clone(),
                order: idx,
            });
        }
    }
    groups.sort_by_key(|g| g.order);

    // 5) 拼接
    let joined = groups
        .iter()
        .map(|g| g.text.clone())
        .collect::<Vec<_>>()
        .join("\n\n");

    // 6) 上限裁剪:从头部裁剪保留最新(尾部)
    if joined.chars().count() <= DECISIONS_MAX_CHARS {
        return joined;
    }
    let overflow = joined.chars().count() - DECISIONS_MAX_CHARS;
    let skipped: String = joined.chars().skip(overflow).collect();
    if let Some(nl) = skipped.find('\n') {
        skipped[nl + 1..].to_string()
    } else {
        skipped
    }
}

/// 前缀冻结区稳定段快照持久化文件名(位于 `.claw/` 下)。
///
/// NOTEBOOK 稳定段(decisions + evidence)低频变化但体量偏大,请求构造时按
/// TTL 热窗决策注入 messages 前缀(与 [`crate::fixed_memory::FixedMemorySnapshot`]
/// 同机制)。热窗内复用旧快照字节 → 前缀命中缓存;TTL 过期后重建注入。
pub const NOTEBOOK_STABLE_SNAPSHOT_FILE: &str = "notebook_stable_snapshot.json";

/// 返回稳定段快照的落盘路径:`<root>/.claw/notebook_stable_snapshot.json`。
#[must_use]
pub fn notebook_stable_snapshot_path(root: &Path) -> PathBuf {
    root.join(".claw").join(NOTEBOOK_STABLE_SNAPSHOT_FILE)
}

/// 从磁盘加载 NOTEBOOK 稳定段快照,失败/不存在返回 None。
///
/// 复用 [`crate::fixed_memory::FixedMemorySnapshot`] 结构(含 last_summary_msg_index
/// 字段,本场景恒为 0,仅作结构复用),保证与 fixed_memory 的热窗/TTL 决策
/// 逻辑(`next_injection`)完全一致。
#[must_use]
pub fn load_stable_snapshot(root: &Path) -> Option<crate::fixed_memory::FixedMemorySnapshot> {
    let content = std::fs::read_to_string(notebook_stable_snapshot_path(root)).ok()?;
    serde_json::from_str(&content).ok()
}

/// 持久化稳定段快照到磁盘(自动创建 `.claw/` 目录)。失败返回错误信息(不 panic)。
pub fn save_stable_snapshot(
    root: &Path,
    snap: &crate::fixed_memory::FixedMemorySnapshot,
) -> Result<(), String> {
    let path = notebook_stable_snapshot_path(root);
    let content = serde_json::to_string_pretty(snap).map_err(|e| format!("serialize: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(path, content).map_err(|e| format!("write: {e}"))
}

/// NOTEBOOK.md 的段标识(Anthropic 推荐的 XML 标签分段)。
///
/// 顺序固定,便于 LLM 解析和人类阅读。
///
/// §4.7 v3 新增 `decisions` 段:由 `decision_log::persist_decisions_to_notebook`
/// 在 compaction 前自动写入,记录设计决策(为什么选 A 不选 B、权衡了什么)。
/// 与 `attempted` 段(已尝试的方案及结论)正交:attempted 记录"做了什么",
/// decisions 记录"为什么决定这样做"。
pub const SECTION_TAGS: &[&str] = &[
    "plan",
    "subagents",
    "attempted",
    "preferences",
    "key_files",
    "decisions", // §4.7 v3 新增:设计决策持久化段(compaction 前自动提取)
    "evidence",  // 改进点 12 新增:实验证据持久化段(compaction 前自动提取)
];

/// 前缀冻结区段:低频变化(决策/证据通常 compaction 前才追加)、体量偏大。
/// 注入 messages 前缀后字节稳定,热窗内存续命中;与实时段分离的核心依据
/// 是「变化频次 × 变化后长度」—— 稳定段每轮重建的代价远高于实时段,
/// 因此归入可命中的长命区,实时段留在尾部新建区。
pub const STABLE_SECTION_TAGS: &[&str] = &["decisions", "evidence"];

/// 尾部冻结槽位块段:高频/实时变化(计划进度、失败记录、子智能体注册)。
/// 留在 messages 末尾,变化只影响尾块不破坏前缀。
pub const VOLATILE_SECTION_TAGS: &[&str] =
    &["plan", "subagents", "attempted", "preferences", "key_files"];

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
    - `<key_files>`:关键文件引用 + 一句话摘要\n\
    - `<decisions>`:设计决策持久化(§4.7,compaction 前自动提取,LLM 一般不直接写)\n\
    - `<evidence>`:实验证据持久化(改进点 12,compaction 前自动提取,含数值对比/基准数据)\n";

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

    /// 渲染**稳定段**(当前 [`STABLE_SECTION_TAGS`])为前缀冻结区注入文本。
    ///
    /// 这些段低频变化(决策/证据通常在 compaction 前才追加)且体量偏大,
    /// 归入 **messages 前缀冻结区**:注入后字节稳定,配合 DeepSeek 缓存 TTL
    /// 在热窗内持续命中,不再每轮全量重建。空段跳过,全部为空返回空串。
    #[must_use]
    pub fn render_stable_sections(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for tag in STABLE_SECTION_TAGS {
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
            "# 设计决策与实验证据(前缀冻结)\n\
             以下为 NOTEBOOK 中低频变化的稳定段,跨压缩持久化。\n\n\
             {}\n\n\
             提示:需要被压缩掉的历史决策细节时,可用 read_file 读取 `decisions_archive.jsonl`。",
            parts.join("\n\n")
        )
    }

    /// 渲染**实时段**(当前 [`VOLATILE_SECTION_TAGS`])为尾部冻结槽位块注入文本。
    ///
    /// 这些段高频变化(计划进度/失败记录/子智能体注册),
    /// 留在 **messages 末尾冻结槽位块**(变化只影响尾块,不破坏前缀)。
    /// 空段跳过,全部为空返回空串。
    #[must_use]
    pub fn render_volatile_sections(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for tag in VOLATILE_SECTION_TAGS {
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

    /// 改进点 12:追加一条实验证据到 `<evidence>` 段。
    ///
    /// 与普通 `append_to_section` 的区别:有容量限制(`EVIDENCE_MAX_CHARS` = 4K),
    /// 超出时从**头部**裁剪(保留最新的证据),避免证据段挤占 NOTEBOOK 其他段
    /// 的 16K 总容量。裁剪时对齐到行首,不会截断半行。
    pub fn append_evidence(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let entry = self.sections.entry("evidence".to_string()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(trimmed);
        // 容量限制:超出 4K 时从头部裁剪,保留最新的(尾部)
        if entry.chars().count() > EVIDENCE_MAX_CHARS {
            let overflow = entry.chars().count() - EVIDENCE_MAX_CHARS;
            let skipped: String = entry.chars().skip(overflow).collect();
            // 对齐到行首:跳过第一个不完整的行(如果有的话)
            let trimmed_content = if let Some(nl) = skipped.find('\n') {
                skipped[nl + 1..].to_string()
            } else {
                skipped
            };
            *entry = trimmed_content;
        }
    }

    /// NOTEBOOK 是否完全为空(所有段都缺失或空)。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.values().all(|s| s.trim().is_empty())
    }
}

/// `<attempted>` 段自动记录的最大字符数。超出时从头部裁剪,保留最新的失败尝试。
pub const ATTEMPTED_MAX_CHARS: usize = 2048;

/// 运行时自动追加一条失败尝试到 `<attempted>` 段(不依赖 LLM 主动调用)。
///
/// 循环中的 LLM 不会停下来调用 `notebook_update` 记账,本函数由运行时在
/// 工具调用失败路径自动调用,使下一轮/下一 turn 的 prompt 注入能看到
/// "已尝试且失败"的路径,从源头消除重复诊断。
///
/// 特性:
/// - 去重:完全相同的尝试行不重复追加(同一失败循环只记 1 条,不膨胀 prompt)
/// - 容量:超出 [`ATTEMPTED_MAX_CHARS`] 时从头部裁剪(对齐行首,不截断半行)
/// - 失败静默:NOTEBOOK 读写失败返回 Err,由调用方吞掉(不阻塞主流程)
pub fn append_attempt(
    workspace_root: &Path,
    tool_name: &str,
    tool_input: &str,
    output: &str,
) -> Result<(), NotebookError> {
    let mut notebook = Notebook::load(workspace_root)?;
    let line = format!(
        "- [tool] {tool_name} | input={} | failed: {}",
        truncate_for_attempt(tool_input, 80),
        truncate_for_attempt(output, 120),
    );
    let already = notebook
        .get_section("attempted")
        .is_some_and(|s| s.lines().any(|l| l.trim() == line));
    if already {
        return Ok(());
    }
    notebook.append_to_section("attempted", &line);
    if let Some(sec) = notebook.get_section("attempted") {
        if sec.chars().count() > ATTEMPTED_MAX_CHARS {
            let overflow = sec.chars().count() - ATTEMPTED_MAX_CHARS;
            let skipped: String = sec.chars().skip(overflow).collect();
            let trimmed = skipped
                .find('\n')
                .map(|nl| skipped[nl + 1..].to_string())
                .unwrap_or(skipped);
            notebook.set_section("attempted", &trimmed);
        }
    }
    notebook.save(workspace_root)
}

/// 按字符数截断文本,超出时加省略号。
fn truncate_for_attempt(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// 跨会话"plan 需刷新"标记文件名(相对于 workspace_root)。
///
/// 方案 C:会话结束时写入,下一会话首 turn 检测到则注入"刷新 `<plan>`"提醒,
/// LLM 调用 `notebook_update` 后清除。修复"上一会话 `<plan>` 过时/为空导致
/// 下一会话 AI 不知道上次任务"的问题。
///
/// 与 `notebook_refresh_pending`(turn 内 flag)的区别:此 marker 持久化到磁盘,
/// 跨会话存活,专门用于会话边界信号传递。
pub const PLAN_STALE_MARKER: &str = ".claw/.notebook_plan_stale";

/// 标记 NOTEBOOK `<plan>` 段为 stale(会话结束时调用)。
///
/// 写入一个空标记文件 `.claw/.notebook_plan_stale`,下一会话首 turn 通过
/// [`is_plan_stale`] 检测。失败静默忽略(非关键路径,最坏情况是下一会话
/// 不提醒刷新,与现有行为一致)。
pub fn mark_plan_stale(workspace_root: &Path) {
    let path = workspace_root.join(PLAN_STALE_MARKER);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, b"");
}

/// 检测 NOTEBOOK `<plan>` 段是否被标记为 stale。
///
/// 返回 `true` 表示上一会话结束时标记了 stale,当前会话首 turn 应注入
/// 刷新提醒。检测后**不自动删除** —— 删除时机由 `execute_notebook_update`
/// 成功后触发(确认 LLM 已响应提醒)。
pub fn is_plan_stale(workspace_root: &Path) -> bool {
    workspace_root.join(PLAN_STALE_MARKER).exists()
}

/// 清除 plan stale 标记。
///
/// 在 LLM 成功调用 `notebook_update` 后调用,避免重复提醒。
/// 失败静默忽略(最坏情况是下一 turn 多提醒一次)。
pub fn clear_plan_stale(workspace_root: &Path) {
    let _ = std::fs::remove_file(workspace_root.join(PLAN_STALE_MARKER));
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
    "description": "Update the persistent working memory (NOTEBOOK.md). This memory survives context compaction — use it to record key decisions, subagent registry, attempted approaches, user preferences, and key file references. CRITICAL: always record subagent dispatches here so you do not re-dispatch the same task later. Modes: 'set' (overwrite section) or 'append' (add a line). Sections: plan, subagents, attempted, preferences, key_files, decisions, evidence. Note: 'decisions' and 'evidence' sections are auto-populated by compaction-time heuristic extraction; LLMs typically do not need to write them directly.",
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
                "enum": ["plan", "subagents", "attempted", "preferences", "key_files", "decisions", "evidence"],
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
    // 备份兜底:decisions 段即将被修改(set 覆盖 / append 追加后去重都会缩减
    // 旧段),被合并/剔除的独特决策细节可能丢失。先把当前旧段完整归档到
    // `.claw/decisions_archive.jsonl`,AI 需要细节时可 read 找回。
    // 归档失败静默忽略(不阻断写入)。
    if parsed.section == "decisions" {
        if let Some(sec) = notebook.get_section("decisions") {
            let _ = crate::decisions_archive::archive_decisions_snapshot(workspace_root, sec);
        }
    }
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
    // decisions 段去重(源头防膨胀):LLM 通过 append 反复记录同一主题的
    // 思考过程会堆积大量近似重复,每次写入后自动去重,防止段无限膨胀。
    if parsed.section == "decisions" {
        if let Some(sec) = notebook.get_section("decisions") {
            let deduped = dedupe_decisions_section(sec);
            notebook.set_section("decisions", &deduped);
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

    // ---- 改进点 12: <evidence> 段 ----

    #[test]
    fn evidence_section_is_in_section_tags() {
        assert!(
            SECTION_TAGS.contains(&"evidence"),
            "SECTION_TAGS must include 'evidence'"
        );
    }

    #[test]
    fn append_evidence_creates_section() {
        let mut nb = Notebook::new();
        nb.append_evidence("[Bash] 基准结果: 100 req/s");
        let content = nb
            .get_section("evidence")
            .expect("evidence section should exist");
        assert!(content.contains("基准结果"));
        assert!(content.contains("100 req/s"));
    }

    #[test]
    fn append_evidence_accumulates_multiple_items() {
        let mut nb = Notebook::new();
        nb.append_evidence("[Bash] 第一次基准: 100 req/s");
        nb.append_evidence("[Bash] 第二次基准: 120 req/s");
        let content = nb
            .get_section("evidence")
            .expect("evidence section should exist");
        assert!(content.contains("第一次基准"));
        assert!(content.contains("第二次基准"));
        assert_eq!(content.matches('\n').count(), 1); // 两行,一个换行
    }

    #[test]
    fn append_evidence_trims_to_4k_cap_keeping_newest() {
        let mut nb = Notebook::new();
        // 每条 ~100 字符,添加 50 条 → 5000 字符,超过 4K
        for i in 0..50 {
            nb.append_evidence(&format!("[Bash] 基准测试 #{i:03}: {}", "x".repeat(80)));
        }
        let content = nb
            .get_section("evidence")
            .expect("evidence section should exist");
        assert!(
            content.chars().count() <= super::EVIDENCE_MAX_CHARS,
            "evidence section should be <= 4K, got {} chars",
            content.chars().count()
        );
        // 最新的(编号大的)应保留,最旧的应被裁剪
        assert!(
            content.contains("基准测试 #049"),
            "newest evidence should be retained: missing #049"
        );
        assert!(
            !content.contains("基准测试 #000"),
            "oldest evidence should be trimmed: found #000"
        );
    }
    #[test]
    fn evidence_section_included_in_render() {
        let mut nb = Notebook::new();
        nb.append_evidence("[Bash] 对比矩阵: A=100 B=200");
        let rendered = nb.render();
        assert!(rendered.contains("<evidence>"));
        assert!(rendered.contains("对比矩阵"));
        assert!(rendered.contains("</evidence>"));
    }

    // ---- P0: <attempted> 段自动记录(防重复诊断循环) ----

    #[test]
    fn append_attempt_records_and_dedups() {
        let dir = temp_workspace();
        append_attempt(dir.path(), "Bash", "netstat -an", "no output").expect("append");
        append_attempt(dir.path(), "Bash", "netstat -an", "no output").expect("append again");
        let nb = Notebook::load(dir.path()).expect("load");
        let sec = nb
            .get_section("attempted")
            .expect("attempted section exists");
        assert_eq!(sec.lines().count(), 1, "完全相同的尝试只记录一次");
        assert!(sec.contains("netstat -an"));
    }

    #[test]
    fn append_attempt_caps_section_size() {
        let dir = temp_workspace();
        for i in 0..100 {
            append_attempt(dir.path(), "Bash", &format!("cmd {i}"), "failed").expect("append");
        }
        let nb = Notebook::load(dir.path()).expect("load");
        let sec = nb.get_section("attempted").expect("attempted");
        assert!(
            sec.chars().count() <= ATTEMPTED_MAX_CHARS,
            "attempted 段必须被裁剪到容量内"
        );
        assert!(sec.contains("cmd 99"), "保留最新的尝试");
        assert!(!sec.contains("cmd 0"), "最旧的尝试被裁剪");
    }

    // ---- decisions 段去重(源头防膨胀) ----

    #[test]
    fn dedupe_decisions_removes_bash_noise() {
        let content = "\
- [d1] 决策 A: 选 SQLite
bash
{\"command\": \"cargo test\", \"timeout\": 60}
- [d1] 决策 B: 加索引";
        let out = super::dedupe_decisions_section(content);
        assert!(!out.contains("cargo test"), "bash 噪音应被剔除");
        assert!(out.contains("决策 A"), "保留真实决策");
        assert!(out.contains("决策 B"), "保留真实决策");
    }

    #[test]
    fn dedupe_decisions_collapses_duplicate_lines() {
        let content = "\
- [d1] 决策 A
重复的思考过程
重复的思考过程
- [d2] 决策 B";
        let out = super::dedupe_decisions_section(content);
        assert_eq!(out.matches("重复的思考过程").count(), 1, "相同行只保留一次");
        assert!(out.contains("决策 A"));
        assert!(out.contains("决策 B"));
    }

    #[test]
    fn dedupe_decisions_merges_same_id_same_opening() {
        let content = "\
- [d1] 方案:用递归脱敏函数
  细节 1
- [d1] 方案:用递归脱敏函数
  细节 1
  细节 2";
        let out = super::dedupe_decisions_section(content);
        // 同 ID + 同首句 → 合并,保留信息最全(含细节 2)的一条
        assert!(out.contains("细节 2"), "保留信息更全的条目");
        assert!(out.matches("细节 1").count() == 1, "重复行合并");
    }

    #[test]
    fn dedupe_decisions_preserves_order_and_distinct_ids() {
        let content = "\
- [d1] 主题 A
- [d2] 主题 B
- [d3] 主题 C";
        let out = super::dedupe_decisions_section(content);
        let pos_a = out.find("主题 A").expect("A");
        let pos_b = out.find("主题 B").expect("B");
        let pos_c = out.find("主题 C").expect("C");
        assert!(pos_a < pos_b && pos_b < pos_c, "保序输出");
    }

    #[test]
    fn dedupe_decisions_caps_at_max_chars_keeping_newest() {
        // 构造超过 DECISIONS_MAX_CHARS 的内容
        let long = "x".repeat(super::DECISIONS_MAX_CHARS / 2);
        let content = format!("- [d1] 旧决策\n{long}\n- [d2] 新决策\n{long}");
        let out = super::dedupe_decisions_section(&content);
        assert!(
            out.chars().count() <= super::DECISIONS_MAX_CHARS,
            "必须被裁剪到上限内, got {}",
            out.chars().count()
        );
        assert!(out.contains("新决策"), "保留最新决策");
    }

    #[test]
    fn notebook_update_decisions_auto_dedupes() {
        let dir = temp_workspace();
        let input =
            r#"{"mode": "append", "section": "decisions", "content": "- [d1] 同一主题的重复思考"}"#;
        execute_notebook_update(dir.path(), input).expect("append 1");
        execute_notebook_update(dir.path(), input).expect("append 2");
        let nb = Notebook::load(dir.path()).expect("load");
        let sec = nb.get_section("decisions").expect("decisions");
        assert_eq!(
            sec.matches("同一主题的重复思考").count(),
            1,
            "重复追加应被去重"
        );
        // 备份兜底:每次写入前旧段归档到 decisions_archive.jsonl
        let archive_path = dir
            .path()
            .join(crate::decisions_archive::DECISIONS_ARCHIVE_FILENAME);
        assert!(archive_path.exists(), "decisions 归档文件应被创建");
        let archive_content = std::fs::read_to_string(&archive_path).expect("read archive");
        assert_eq!(
            archive_content.lines().count(),
            1,
            "首次写入前 decisions 段为空不归档,第二次写入前归档首条"
        );
        let record: crate::decisions_archive::ArchivedDecision =
            serde_json::from_str(archive_content.lines().next().expect("line")).expect("parse");
        assert_eq!(record.content, "- [d1] 同一主题的重复思考");
    }

    // ---- 分段双轨:稳定段(decisions/evidence) vs 实时段(plan/attempted 等) ----

    #[test]
    fn render_stable_sections_only_includes_decisions_and_evidence() {
        let mut nb = Notebook::new();
        nb.set_section("decisions", "- [d1] 选 SQLite");
        nb.set_section("evidence", "[Bash] 基准: 100 req/s");
        nb.set_section("plan", "任务计划");
        nb.set_section("attempted", "- 方案A 失败");
        let stable = nb.render_stable_sections();
        assert!(stable.contains("<decisions>"), "稳定段应含 decisions");
        assert!(stable.contains("选 SQLite"));
        assert!(stable.contains("<evidence>"), "稳定段应含 evidence");
        assert!(!stable.contains("任务计划"), "稳定段不应含 plan");
        assert!(!stable.contains("方案A"), "稳定段不应含 attempted");
    }

    #[test]
    fn render_volatile_sections_excludes_stable_sections() {
        let mut nb = Notebook::new();
        nb.set_section("decisions", "- [d1] 选 SQLite");
        nb.set_section("evidence", "[Bash] 基准: 100 req/s");
        nb.set_section("plan", "任务计划");
        nb.set_section("attempted", "- 方案A 失败");
        let volatile = nb.render_volatile_sections();
        assert!(volatile.contains("任务计划"), "实时段应含 plan");
        assert!(volatile.contains("方案A"), "实时段应含 attempted");
        assert!(!volatile.contains("选 SQLite"), "实时段不应含 decisions");
        assert!(!volatile.contains("基准: 100"), "实时段不应含 evidence");
    }

    #[test]
    fn render_stable_sections_empty_when_no_stable_content() {
        let mut nb = Notebook::new();
        nb.set_section("plan", "任务计划");
        assert_eq!(nb.render_stable_sections(), "", "无稳定段时应返回空");
        assert!(nb.render_volatile_sections().contains("任务计划"));
    }

    #[test]
    fn stable_snapshot_round_trips_to_disk() {
        let dir = temp_workspace();
        let snap = crate::fixed_memory::FixedMemorySnapshot {
            content: "# 设计决策与实验证据(前缀冻结)\n<decisions>\n- [d1] 选 SQLite\n</decisions>"
                .to_string(),
            fingerprint: crate::fixed_memory::fingerprint("test"),
            injected_at_ms: 1_700_000_000_000,
            last_summary_msg_index: 0,
        };
        super::save_stable_snapshot(dir.path(), &snap).expect("save");
        let loaded = super::load_stable_snapshot(dir.path()).expect("load");
        assert_eq!(loaded, snap, "快照应能无损往返");
        let path = super::notebook_stable_snapshot_path(dir.path());
        assert!(path.exists(), "快照文件应位于 .claw/ 下");
    }
}
