#![cfg(feature = "full-tui")]

//! 结构化输出视图 — 窗口化渲染（瘦前端）。
//!
//! 设计（docs/2026-08-11-tui-windowed-renderer-design.md）：
//! TUI 是瘦渲染器，数据权威在后端 session JSONL。本模块只保留"窗口"大小的
//! 结构化条目（供 ToolCard 更新 / 折叠 / 点击命中 / J-K-E 跳转），窗口滑出即
//! 丢弃 —— 无内存预算、无淘汰策略、无跨窗口缓存一致性，从根上消灭
//! "内容被吞 / 渲染卡住" 这类 bug。
//!
//! - `OutputEntry::Text` — 普通文本流（AI 回复、用户 echo）
//! - `OutputEntry::ToolCard` — 工具调用卡片，可折叠/展开
//! - `OutputEntry::Thinking` — Thinking 块摘要
//! - `OutputEntry::Timeline` — 工具时间线
//!
//! 历史回看 = 从 session JSONL 流式重放（见 session_replay.rs）。

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ansi_to_tui::IntoText;
use ratatui::text::Line;

/// 窗口最大条目数（结构化存储上限）。
/// 超出后从头部弹出（滚出窗口即丢弃）——数据权威在后端 session JSONL，
/// TUI 不承担无限历史存储。
const MAX_WINDOW_ENTRIES: usize = 400;

/// 生成当前本地时间戳字符串（HH:MM:SS 格式）。
fn now_timestamp() -> String {
    use chrono::Local;
    Local::now().format("%H:%M:%S").to_string()
}

/// 工具结果的展现优先级（信息重要性 > 内容长度）。
///
/// 决定默认折叠行为与视觉突出程度：
/// - `P0` 永不折叠 + 高亮（AI 文本 / error / emphasis=high / 命令失败）
/// - `P1` 默认展开（短输出 ≤8 行 / normal / diff）
/// - `P2` 默认折叠 + 预览（长输出 >40 行 / 长 JSON）
/// - `P3` 折叠 + 单行摘要（emphasis=low / interrupted / 成功确认）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl Priority {
    /// 该优先级默认是否折叠。
    pub(crate) fn default_collapsed(self) -> bool {
        matches!(self, Priority::P2 | Priority::P3)
    }
}
/// bash/编辑结果中的错误标记，命中即 P0 展开（内容信号覆盖行数）。
/// 大小写分别收录："error:"(Rust/cargo/shell) 与 "Error:"(Node/Python)。
/// FAILED 用大写：cargo test 失败输出大写 `FAILED`，ls 的小写文件名
/// "failed.txt" 不误伤。
const BASH_ERROR_MARKERS: &[&str] = &[
    "error[E",
    "error:",
    "Error:",
    "panic!",
    "fatal:",
    "FAILED",
    "command not found",
    "Traceback",
];

/// 根据工具名、输入、结果、is_error 计算展现优先级。
///
/// 优先级链：模型 emphasis > is_error > bash returnCodeInterpretation > 行数启发式。
/// 详见 docs/tui-output-intelligence-plan.md §3.1。
pub(crate) fn compute_priority(
    tool_name: &str,
    input: &str,
    result: &str,
    is_error: bool,
) -> Priority {
    // 1. 模型显式 emphasis：最高优先级
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
        if let Some(emphasis) = v.get("emphasis").and_then(|e| e.as_str()) {
            match emphasis {
                "high" => return Priority::P0, // 模型明确要求强调
                "low" => return Priority::P3,  // 模型明确要求低调
                _ => {}
            }
        }
    }
    // 2. 错误标记：P0 永不折叠
    if is_error {
        return Priority::P0;
    }
    // 3. bash 专用：returnCodeInterpretation 覆盖行数启发式
    if tool_name == "bash" || tool_name == "Bash" {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(result) {
            if let Some(rc) = v.get("returnCodeInterpretation").and_then(|v| v.as_str()) {
                if rc == "interrupted" {
                    return Priority::P3; // 用户 Ctrl+C 取消
                }
                if matches!(rc, "idle.timeout" | "timeout" | "test.hung") {
                    return Priority::P0; // 超时/挂起，需关注
                }
                if rc.starts_with("exit_code:") && rc != "exit_code:0" {
                    return Priority::P0; // 命令失败
                }
            }
        }
    }

    // 4. 内容语义分类器兜底（P0 修复 2026-08-04）
    // 根因：旧实现统计 pretty JSON 信封行数（3 行 stdout → 38 行 JSON 恒 P2
    // 折叠），且与内容语义无关。现在先提取真实内容，再按工具语义决定默认
    // 展开层级：内容是答案（read_file/grep/测试/错误）→ 展开；
    // 过程噪音（write/edit/glob）→ 折叠单行。
    let body = crate::tui::tool_card::extract_tool_output_body_public(tool_name, result);
    let lines = body.lines().count();
    match tool_name {
        "bash" | "Bash" => {
            if BASH_ERROR_MARKERS.iter().any(|m| body.contains(m)) {
                Priority::P0 // 命令失败/编译错误：内容信号覆盖行数
            } else if body.contains("test result:") {
                Priority::P1 // 测试总结（cargo test 末行）——长输出也展开
            } else if lines > 8 {
                Priority::P2 // 长输出折叠（ls -la 等过程输出）
            } else {
                Priority::P1
            }
        }
        "read_file" | "Read" => {
            // 内容是答案，门槛放宽到 40 行
            if lines > 40 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
        "grep_search" | "Grep" => {
            // 命中即证据，门槛放宽到 50 行
            if lines > 50 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
        "edit_file" | "Edit" | "write_file" | "Write" => {
            if BASH_ERROR_MARKERS.iter().any(|m| body.contains(m)) {
                Priority::P0 // cargo check 编译错误 → 展开显示错误
            } else {
                Priority::P3 // 纯确认 → 单行摘要
            }
        }
        "glob_search" | "Glob" | "Skill" | "TodoWrite" | "ToolSearch" | "benchmark_compare" => {
            Priority::P3 // 过程噪音/确认：单行摘要
        }
        "WebFetch" => {
            if lines > 8 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
        _ => {
            if lines > 8 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
    }
}

/// 结构化输出条目。
#[derive(Debug, Clone)]
pub(crate) enum OutputEntry {
    /// 普通文本流（AI 回复、用户 echo、斜杠命令输出）。
    Text { content: String, timestamp: String },
    /// 工具调用卡片，可折叠/展开。
    ToolCard {
        /// 工具调用 ID（用于匹配 ToolUse 和 ToolResult）。
        tool_id: String,
        /// 工具名称。
        name: String,
        /// 工具输入（JSON 字符串，用于 diff 显示）。
        input: String,
        /// 工具结果（None 表示仍在执行中）。
        result: Option<String>,
        /// 是否为错误结果。
        is_error: bool,
        /// 展现优先级（决定默认折叠行为与视觉突出）。
        priority: Priority,
        /// 当前是否折叠（true=折叠只显示 header，false=展开显示完整结果）。
        collapsed: bool,
        /// 条目创建时的本地时间戳（HH:MM:SS）。
        timestamp: String,
    },
    /// Thinking 块卡片：流式实时显示全文，结束后自动折叠为摘要。
    Thinking {
        /// 完整思考文本（流式增量累积）。
        full_text: String,
        /// 折叠时显示的摘要（如 "\n▶ Thinking (N chars hidden)\n"）。
        summary: String,
        /// 当前是否折叠（true=只显示摘要，false=展开显示全文）。
        collapsed: bool,
        /// 条目创建时的本地时间戳（HH:MM:SS）。
        timestamp: String,
    },
    /// 工具时间线。
    Timeline { summary: String, timestamp: String },
    /// 对等会话消息（Session Bus，设计文档 2026-08-11-session-bus-design.md §2.3）。
    PeerMessage {
        /// 来源会话标签，如 "subagent:api-worker"。
        from: String,
        /// 消息种类，如 "handoff" / "message"。
        kind: String,
        /// 摘要（单行，控制台友好）。
        summary: String,
        /// 条目创建时的本地时间戳（HH:MM:SS）。
        timestamp: String,
    },
}

impl OutputEntry {
    /// 工厂方法：创建 Text 条目，自动填充当前时间戳。
    pub(crate) fn text(content: String) -> Self {
        Self::Text {
            content,
            timestamp: now_timestamp(),
        }
    }

    /// 工厂方法：创建执行中的 ToolCard 条目，自动填充当前时间戳。
    pub(crate) fn tool_card_start(tool_id: String, name: String, input: String) -> Self {
        Self::ToolCard {
            tool_id,
            name,
            input,
            result: None,
            is_error: false,
            priority: Priority::P1, // 执行中默认 P1，complete 时重算
            collapsed: false,
            timestamp: now_timestamp(),
        }
    }

    /// 工厂方法：创建 Thinking 条目，自动填充当前时间戳。
    ///
    /// 默认折叠为摘要（session_replay 等一次性渲染路径使用）；流式路径
    /// 用 [`OutputBuffer::start_thinking`] / [`append_thinking_delta`] 创建展开态。
    pub(crate) fn thinking(summary: String) -> Self {
        Self::Thinking {
            full_text: String::new(),
            summary,
            collapsed: true,
            timestamp: now_timestamp(),
        }
    }

    /// 工厂方法：创建展开态的 Thinking 条目（流式思考开始），
    /// 之后通过 [`OutputBuffer::append_thinking_delta`] 追加全文。
    pub(crate) fn thinking_started() -> Self {
        Self::Thinking {
            full_text: String::new(),
            summary: String::new(),
            collapsed: false,
            timestamp: now_timestamp(),
        }
    }

    /// 工厂方法：创建 Timeline 条目，自动填充当前时间戳。
    pub(crate) fn timeline(summary: String) -> Self {
        Self::Timeline {
            summary,
            timestamp: now_timestamp(),
        }
    }

    /// 工厂方法：创建对等会话消息条目（Session Bus），自动填充当前时间戳。
    ///
    /// 渲染为 `[来自 <from> · <kind>] <summary>`，优先级 P1（折叠档，不抢占焦点）。
    pub(crate) fn peer_message(from: String, kind: String, summary: String) -> Self {
        Self::PeerMessage {
            from,
            kind,
            summary,
            timestamp: now_timestamp(),
        }
    }

    /// 返回此条目在当前折叠状态下渲染出的文本（含 ANSI 转义）。
    /// 每个条目末尾不以换行结束，由调用方负责条目间分隔。
    pub(crate) fn render(&self) -> String {
        match self {
            OutputEntry::Text { content, timestamp } => {
                // 表格行（render.rs 渲染的表格首行以 │ 开头，可能带 ANSI 样式
                // 前缀）不加时间戳前缀：否则表头行被整体右移、与数据行错位
                // （用户报告的"表格第一行错位"bug）。
                if starts_with_table_border(content) {
                    content.clone()
                } else {
                    format!("\x1b[38;5;240m[{timestamp}]\x1b[0m {content}")
                }
            }
            OutputEntry::Thinking {
                full_text,
                summary,
                collapsed,
                timestamp,
            } => {
                if *collapsed {
                    format!("\x1b[38;5;240m[{timestamp}]\x1b[0m{summary}")
                } else {
                    // 展开态：显示完整思考文本。
                    format!(
                        "\x1b[38;5;240m[{timestamp}]\x1b[0m\n▶ Thinking\n{full_text}"
                    )
                }
            }
            OutputEntry::Timeline { summary, timestamp } => {
                format!("\x1b[38;5;240m[{timestamp}]\x1b[0m{summary}")
            }
            OutputEntry::PeerMessage {
                from,
                kind,
                summary,
                timestamp,
            } => {
                format!(
                    "\x1b[38;5;240m[{timestamp}]\x1b[0m \x1b[36m[来自 {from} · {kind}]\x1b[0m {summary}"
                )
            }
            OutputEntry::ToolCard {
                name,
                input,
                result,
                is_error,
                priority,
                collapsed,
                timestamp,
                ..
            } => {
                let summary = crate::tui::tool_card::summarize_tool_input_public(name, input);
                let ts_prefix = format!("\x1b[38;5;240m[{timestamp}]\x1b[0m ");
                if result.is_none() {
                    // 执行中：只显示 header
                    return format!("\n{ts_prefix}┌─ 🔧 {name} {summary} ⏳\n");
                }
                let output = result
                    .as_ref()
                    .expect("result must be Some after executing check");
                // P1 修复：统一委托给 render_tool_result_public，由它根据
                // `collapsed` 参数决定折叠预览（前3行+展开提示）或完整展开。
                let rendered = crate::tui::tool_card::render_tool_result_public(
                    name,
                    output,
                    *is_error,
                    Some(input),
                    *collapsed,
                    *priority,
                );
                // render_tool_result_public 输出以 \n 开头，把时间戳插入到首行
                if let Some(stripped) = rendered.strip_prefix('\n') {
                    format!("\n{ts_prefix}{stripped}")
                } else {
                    format!("{ts_prefix}{rendered}")
                }
            }
        }
    }
}

/// 判断文本是否以表格边框行开头（render.rs 渲染的表格首行以 │ 开头，
/// 且可能带 ANSI 样式前缀）。用于 Text entry 渲染时决定是否跳过时间戳前缀，
/// 保持表格列对齐。
fn starts_with_table_border(text: &str) -> bool {
    let first_line = text.split('\n').next().unwrap_or_default();
    // 跳过前导 ANSI SGR 序列（如边框样式 \x1b[38;5;6m）
    let mut chars = first_line.chars().peekable();
    while chars.peek() == Some(&'\u{1b}') {
        chars.next();
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            break;
        }
    }
    chars.peek() == Some(&'│')
}

/// 线程安全的结构化输出缓冲区（窗口化）。
fn ansi_to_lines(ansi: &str) -> Vec<Line<'static>> {
    if ansi.is_empty() {
        return Vec::new();
    }
    match ansi.into_text() {
        Ok(text) => text.lines,
        Err(_) => ansi
            .lines()
            .map(|l| Line::raw(strip_ansi_for_raw_line(l).to_string()))
            .collect(),
    }
}

/// Strip ALL ANSI escape sequences from a string.
///
/// Safety net for `ansi_to_lines` fallback: when `ansi_to_tui` fails to parse
/// the input, we must NOT pass raw `\x1b[...` sequences to `Line::raw()`.
/// Ratatui's `Print()` writes span content as raw bytes to stdout; the terminal
/// interprets `\x1b[2;1H` (Cursor Position) as a cursor movement command,
/// corrupting the TUI display — text appears at wrong positions (especially
/// the input line), making it look like the input buffer was "auto-filled".
///
/// Handles: CSI (\x1b[...letter), OSC (\x1b]...BEL/ST), standalone ESC,
/// and other ESC+char sequences.
fn strip_ansi_for_raw_line(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() || c == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[derive(Debug)]
pub(crate) struct OutputView {
    inner: Arc<Mutex<OutputBuffer>>,
}

/// 窗口化输出缓冲区。
///
/// 只保留最近 `MAX_WINDOW_ENTRIES` 条结构化条目；窗口滑出即丢弃。
/// 无内存预算、无淘汰策略 —— 数据权威在后端 session JSONL。
#[derive(Debug, Default)]
pub(crate) struct OutputBuffer {
    /// 窗口内条目（结构化，供 ToolCard 更新/折叠/点击/跳转）。
    entries: VecDeque<OutputEntry>,
    /// 单调版本号：每次内容变化递增，供主循环 draw 触发检测。
    version: u64,
    /// 极简渲染缓存：`(version, lines, breaks)`。
    /// draw 间无变化直接复用，避免全量重建；version 不匹配即整体重建
    /// （窗口小 → 毫秒级）。无增量一致性复杂度。
    cached_lines: Option<(u64, Arc<Vec<Line<'static>>>, Vec<usize>)>,
}

impl OutputBuffer {
    /// 内容变化：版本号递增 + 渲染缓存失效。
    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
        self.cached_lines = None;
    }

    /// 入窗口（尾部追加），超容量从头部弹出（滚出窗口即丢弃）。
    fn push_window(&mut self, entry: OutputEntry) {
        self.entries.push_back(entry);
        while self.entries.len() > MAX_WINDOW_ENTRIES {
            self.entries.pop_front();
        }
    }

    /// 追加文本到当前条目。如果最后一个条目是 Text，则合并；
    /// 否则新建一个 Text 条目。
    ///
    /// 段落感知分段：text 含段落分隔（双换行 `\n\n`）时按段落分割为独立
    /// Text entry（配合 J/K 键快速跳转）。最后一段不闭合（可能后续流式追加）。
    pub(crate) fn append(&mut self, text: &str) {
        if text.contains("\n\n") {
            self.append_segmented(text);
            return;
        }
        // 无段落分隔：走原合并逻辑
        if let Some(OutputEntry::Text { content, .. }) = self.entries.back_mut() {
            content.push_str(text);
        } else {
            self.push_window(OutputEntry::text(text.to_string()));
        }
        self.bump();
    }

    /// 追加对等会话消息条目（Session Bus）。独立成条，不与 Text 合并。
    pub(crate) fn push_peer_message(&mut self, from: String, kind: String, summary: String) {
        self.push_window(OutputEntry::peer_message(from, kind, summary));
        self.bump();
    }

    /// 段落感知追加：按双换行分割 text，每段为独立 Text entry。
    /// 最后一段合并到已存在的 trailing Text entry（或新建），支持后续流式追加。
    fn append_segmented(&mut self, text: &str) {
        let segments: Vec<&str> = text.split("\n\n").collect();
        let seg_count = segments.len();
        for (i, seg) in segments.iter().enumerate() {
            if seg.is_empty() {
                continue;
            }
            if i + 1 == seg_count {
                // 最后一段：合并到 trailing Text entry 或新建
                if let Some(OutputEntry::Text { content, .. }) = self.entries.back_mut() {
                    content.push_str(seg);
                } else {
                    self.push_window(OutputEntry::text(seg.to_string()));
                }
            } else {
                // 中间段：闭合为独立 entry（加回 \n\n 保持渲染换行）
                self.push_window(OutputEntry::text(format!("{seg}\n\n")));
            }
        }
        self.bump();
    }

    /// 追加一个结构化条目。
    pub(crate) fn push_entry(&mut self, entry: OutputEntry) {
        self.push_window(entry);
        self.bump();
    }

    /// 流式思考开始：新建一个展开态的 Thinking 条目。
    /// 若最后一个条目已是展开态 Thinking（上一块未结束），则复用。
    pub(crate) fn start_thinking(&mut self) {
        let reuse = matches!(
            self.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: false,
                ..
            })
        );
        if !reuse {
            self.push_window(OutputEntry::thinking_started());
        }
        self.bump();
    }

    /// 追加 thinking 增量文本到当前 Thinking 条目（实时显示）。
    /// 若最后一个条目不是展开态 Thinking，先新建。
    pub(crate) fn append_thinking_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let reuse = matches!(
            self.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: false,
                ..
            })
        );
        if !reuse {
            self.push_window(OutputEntry::thinking_started());
        }
        if let Some(OutputEntry::Thinking { full_text, .. }) = self.entries.back_mut() {
            full_text.push_str(text);
        }
        self.bump();
    }

    /// 思考块结束：把最近一个 Thinking 条目折叠为摘要。
    /// 幂等 —— 已折叠或不存在时无操作。`char_count` 是思考总字符数
    /// （None 表示 provider 隐藏），`redacted` 表示内容被 provider 抹除。
    pub(crate) fn complete_thinking(
        &mut self,
        char_count: Option<usize>,
        redacted: bool,
    ) {
        let summary = if redacted {
            "\n▶ Thinking block hidden by provider\n".to_string()
        } else if let Some(char_count) = char_count {
            format!("\n▶ Thinking ({char_count} chars hidden)\n")
        } else {
            "\n▶ Thinking hidden\n".to_string()
        };
        // 从后往前找最近一个 Thinking 条目（完整块路径刚 append 后即折叠）。
        // 若不存在（如直接收到 done 信号的 RedactedThinking），新建折叠态条目。
        if let Some(idx) = self.entries.iter().enumerate().rev().find_map(|(idx, e)| {
            matches!(e, OutputEntry::Thinking { .. }).then_some(idx)
        }) {
            if let OutputEntry::Thinking {
                summary: s,
                collapsed,
                ..
            } = &mut self.entries[idx]
            {
                *s = summary;
                *collapsed = true;
            }
        } else {
            self.push_window(OutputEntry::thinking(summary));
        }
        self.bump();
    }

    /// 切换最近一个可折叠条目的折叠/展开状态（Thinking 或已完成的 ToolCard）。
    /// 优先 Thinking（流式思考卡片更常需要查看全文），否则 ToolCard。
    /// 返回 true 表示成功切换。
    pub(crate) fn toggle_latest_collapsible(&mut self) -> bool {
        let found_idx = self.entries.iter().enumerate().rev().find_map(|(idx, e)| {
            match e {
                OutputEntry::Thinking { .. } => Some(idx),
                OutputEntry::ToolCard { result: Some(_), .. } => Some(idx),
                _ => None,
            }
        });
        if let Some(idx) = found_idx {
            match &mut self.entries[idx] {
                OutputEntry::Thinking { collapsed, .. } => *collapsed = !*collapsed,
                OutputEntry::ToolCard { collapsed, .. } => *collapsed = !*collapsed,
                _ => unreachable!("idx 已确认是可折叠条目"),
            }
            self.bump();
            return true;
        }
        false
    }

    /// 切换最近一个 Thinking 条目的折叠/展开状态。
    /// 返回 true 表示成功切换。
    pub(crate) fn toggle_latest_thinking(&mut self) -> bool {
        let found_idx = self.entries.iter().enumerate().rev().find_map(|(idx, e)| {
            matches!(e, OutputEntry::Thinking { .. }).then_some(idx)
        });
        if let Some(idx) = found_idx {
            if let OutputEntry::Thinking { collapsed, .. } = &mut self.entries[idx] {
                *collapsed = !*collapsed;
            }
            self.bump();
            return true;
        }
        false
    }

    /// 更新指定 tool_id 的 ToolCard：设置 result 并按优先级决定折叠状态。
    pub(crate) fn complete_tool_card(
        &mut self,
        tool_id: &str,
        result: String,
        is_error: bool,
    ) -> bool {
        // 先查找目标索引，避免在 iter_mut 期间借用冲突。
        let found_idx = self.entries.iter().position(|e| {
            matches!(e, OutputEntry::ToolCard { tool_id: id, result: r, .. } if id == tool_id && r.is_none())
        });
        if let Some(idx) = found_idx {
            // 先读取 name 和 input 用于计算优先级（借用冲突需先 clone）。
            let (name, input) = match &self.entries[idx] {
                OutputEntry::ToolCard { name, input, .. } => (name.clone(), input.clone()),
                _ => unreachable!("idx 已确认是 ToolCard"),
            };
            let priority = compute_priority(&name, &input, &result, is_error);
            let collapsed = priority.default_collapsed();
            if let OutputEntry::ToolCard {
                result: r,
                is_error: e,
                priority: p,
                collapsed: c,
                ..
            } = &mut self.entries[idx]
            {
                *r = Some(result);
                *e = is_error;
                *p = priority;
                *c = collapsed;
            }
            self.bump();
            return true;
        }
        false
    }

    /// 按工具名称匹配最近一个未完成的 ToolCard（用于 tool_display.rs
    /// 无法获取 tool_use_id 时的兜底匹配）。
    /// 返回 true 表示成功匹配并更新。
    pub(crate) fn complete_tool_card_by_name(
        &mut self,
        tool_name: &str,
        result: String,
        is_error: bool,
    ) -> bool {
        let found_idx = self
            .entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, e)| match e {
                OutputEntry::ToolCard {
                    name, result: r, ..
                } if name == tool_name && r.is_none() => Some(idx),
                _ => None,
            });
        if let Some(idx) = found_idx {
            let input = match &self.entries[idx] {
                OutputEntry::ToolCard { input, .. } => input.clone(),
                _ => unreachable!("idx 已确认是 ToolCard"),
            };
            let priority = compute_priority(tool_name, &input, &result, is_error);
            let collapsed = priority.default_collapsed();
            if let OutputEntry::ToolCard {
                result: r,
                is_error: e,
                priority: p,
                collapsed: c,
                ..
            } = &mut self.entries[idx]
            {
                *r = Some(result);
                *e = is_error;
                *p = priority;
                *c = collapsed;
            }
            self.bump();
            return true;
        }
        false
    }

    /// 切换最近一个 ToolCard 的折叠/展开状态。
    /// 返回 true 表示成功切换。
    pub(crate) fn toggle_latest_tool_card(&mut self) -> bool {
        let found_idx = self.entries.iter().enumerate().rev().find_map(|(idx, e)| {
            matches!(
                e,
                OutputEntry::ToolCard {
                    result: Some(_),
                    ..
                }
            )
            .then_some(idx)
        });
        if let Some(idx) = found_idx {
            if let OutputEntry::ToolCard { collapsed, .. } = &mut self.entries[idx] {
                *collapsed = !*collapsed;
            }
            self.bump();
            return true;
        }
        false
    }

    /// 切换指定索引处 ToolCard 的折叠状态。
    pub(crate) fn toggle_tool_card_at(&mut self, index: usize) -> bool {
        let mut count = 0;
        let found_idx = self.entries.iter().enumerate().find_map(|(idx, e)| {
            if matches!(
                e,
                OutputEntry::ToolCard {
                    result: Some(_),
                    ..
                }
            ) {
                if count == index {
                    return Some(idx);
                }
                count += 1;
            }
            None
        });
        if let Some(idx) = found_idx {
            if let OutputEntry::ToolCard { collapsed, .. } = &mut self.entries[idx] {
                *collapsed = !*collapsed;
            }
            self.bump();
            return true;
        }
        false
    }

    /// 返回所有已完成的 ToolCard 数量。
    pub(crate) fn completed_tool_card_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    OutputEntry::ToolCard {
                        result: Some(_),
                        ..
                    }
                )
            })
            .count()
    }

    /// 返回所有 error entry 的索引（is_error 或 priority=P0）。
    /// 供 E 键跳转使用（详见方案 §3.4）。
    pub(crate) fn error_entry_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(idx, e)| match e {
                OutputEntry::ToolCard {
                    is_error, priority, ..
                } if *is_error || *priority == Priority::P0 => Some(idx),
                _ => None,
            })
            .collect()
    }

    /// 返回每个 ToolCard 在 `render_all()` 输出中的**显示行**区间 `[start, end)`。
    ///
    /// 鼠标点击场景：把点击的 row + scroll_y 映射到显示行号，
    /// 然后查这个表找出命中的 ToolCard entry。
    ///
    /// 与 draw 同源：用 `snapshot_lines()` + `snapshot_breaks()` 计算，
    /// 保证区间与屏幕显示完全一致（P0-3 修复：不再用含 ANSI 的原始串估算
    /// 宽度，避免高估折行数导致点击命中错位）。
    pub(crate) fn tool_card_line_ranges(
        &mut self,
        area_width: usize,
    ) -> Vec<(
        usize, /*entry_idx*/
        usize, /*start*/
        usize, /*end*/
    )> {
        let width = area_width.max(1);
        let lines = self.snapshot_lines();
        let breaks = self.snapshot_breaks();
        let mut ranges = Vec::new();
        let mut current_display: usize = 0;
        for i in 0..breaks.len().saturating_sub(1) {
            let start = breaks[i];
            let end = breaks[i + 1];
            let entry_display_lines: usize = if start < end && end <= lines.len() {
                lines[start..end]
                    .iter()
                    .map(|l| crate::tui::app::wrap_line_to_display_lines(l, width).len())
                    .sum()
            } else {
                0
            };
            if let Some(entry) = self.entries.get(i) {
                if matches!(entry, OutputEntry::ToolCard { .. }) {
                    // ToolCard render 以 `\n` 开头（前导空行）→ 卡片体从下一行开始
                    let (card_start, card_end) = if entry_display_lines > 0 {
                        (current_display + 1, current_display + entry_display_lines)
                    } else {
                        (current_display, current_display)
                    };
                    ranges.push((i, card_start, card_end));
                }
            }
            current_display += entry_display_lines;
        }
        ranges
    }

    /// 按显示行号切换命中的 ToolCard 折叠状态。
    /// 返回 true 表示成功切换。用于鼠标点击场景。
    /// `area_width` 是输出区可见宽度（用于计算 wrap 折行后的显示行数）。
    pub(crate) fn toggle_tool_card_at_line(&mut self, line: usize, area_width: usize) -> bool {
        let ranges = self.tool_card_line_ranges(area_width);
        for (entry_idx, start, end) in ranges {
            if line >= start && line < end {
                if let Some(OutputEntry::ToolCard {
                    collapsed,
                    result: Some(_),
                    ..
                }) = self.entries.get_mut(entry_idx)
                {
                    *collapsed = !*collapsed;
                    self.bump();
                    return true;
                }
            }
        }
        false
    }

    /// 返回窗口内全部条目的渲染文本（含 ANSI 转义）。
    pub(crate) fn render_all(&self) -> String {
        self.entries.iter().map(|e| e.render()).collect()
    }

    /// 保留向后兼容：返回渲染后的文本（等价于 render_all）。
    pub(crate) fn buffer(&self) -> String {
        self.render_all()
    }

    /// 只读访问版本号（内容变化即递增，供 draw 触发检测）。
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    /// 重建窗口内渲染行 + 每条目的起始行索引。
    fn rebuild(&mut self) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut breaks: Vec<usize> = Vec::with_capacity(self.entries.len() + 1);
        breaks.push(0);
        for entry in &self.entries {
            let rendered = entry.render();
            let entry_lines = ansi_to_lines(&rendered);
            lines.extend(entry_lines);
            breaks.push(lines.len());
        }
        self.cached_lines = Some((self.version, Arc::new(lines), breaks));
    }

    /// 返回窗口内全部渲染行（Arc 共享，draw 直接使用）。
    pub(crate) fn snapshot_lines(&mut self) -> Arc<Vec<Line<'static>>> {
        if self.cached_lines.as_ref().map(|(v, _, _)| *v) != Some(self.version) {
            self.rebuild();
        }
        Arc::clone(
            &self
                .cached_lines
                .as_ref()
                .expect("cached_lines must be set after rebuild")
                .1,
        )
    }

    /// 快照每个 entry 在渲染行中的起始行号（原始行，未 wrap）。
    /// 长度 = entries.len() + 1，breaks[0]=0，breaks[i+1] = 前 i+1 个 entry 的总行数。
    pub(crate) fn snapshot_breaks(&mut self) -> Vec<usize> {
        if self.cached_lines.as_ref().map(|(v, _, _)| *v) != Some(self.version) {
            self.rebuild();
        }
        self.cached_lines
            .as_ref()
            .expect("cached_lines must be set after rebuild")
            .2
            .clone()
    }

    /// 返回所有 Text 类型 entry 的 display 起始行号（原始行，未 wrap）。
    /// 供 J/K 键跳转 AI 回复锚点使用。
    /// 仅返回 Text entry，跳过 ToolCard/Thinking/Timeline（它们不是 AI 回复）。
    pub(crate) fn text_entry_display_starts(&mut self) -> Vec<usize> {
        self.snapshot_breaks(); // 确保 breaks 已建立
        let breaks = self.snapshot_breaks();
        let mut result = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if matches!(entry, OutputEntry::Text { .. }) {
                if let Some(&start) = breaks.get(i) {
                    result.push(start);
                }
            }
        }
        result
    }

    /// 清空窗口（本地 /clear 或 /new）。
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bump();
    }

    /// 历史回看：把 session JSONL 重放得到的更早条目前置到窗口头部。
    /// 超出窗口容量时从尾部弹出（尾部内容仍保存在后端 session 文件，
    /// 滚动回底部时重新实时追加）。
    pub(crate) fn prepend_history(&mut self, entries: Vec<OutputEntry>) {
        if entries.is_empty() {
            return;
        }
        for e in entries.into_iter().rev() {
            self.entries.push_front(e);
        }
        while self.entries.len() > MAX_WINDOW_ENTRIES {
            self.entries.pop_back();
        }
        self.bump();
    }

    /// 窗口内条目数（诊断用）。
    pub(crate) fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// 取出窗口内全部条目（供 session_replay 收集历史段）。
    /// 取空后窗口清空，版本递增。
    pub(crate) fn drain_entries(&mut self) -> Vec<OutputEntry> {
        let out: Vec<OutputEntry> = self.entries.drain(..).collect();
        self.bump();
        out
    }
}

impl OutputView {
    /// 创建空的输出视图。
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutputBuffer::default())),
        }
    }

    /// 共享底层 buffer 的 Arc 句柄。
    pub(crate) fn shared_handle(&self) -> Arc<Mutex<OutputBuffer>> {
        Arc::clone(&self.inner)
    }

    /// 快照当前渲染后的文本内容（窗口内，克隆）。
    pub(crate) fn snapshot(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .render_all()
    }

    pub(crate) fn snapshot_lines(&self) -> Arc<Vec<Line<'static>>> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.snapshot_lines()
    }

    /// 快照每个 entry 在渲染行中的起始行号(原始行,未 wrap)。
    /// 供 sticky_view 计算粘性头部时定位 entry 边界。
    pub(crate) fn snapshot_breaks(&self) -> Vec<usize> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.snapshot_breaks()
    }

    /// 只读访问版本号（内容变化即递增，供 draw 触发检测）。
    pub(crate) fn version(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .version
    }

    /// 返回所有 Text 类型 entry 的 display 起始行号（原始行，未 wrap）。
    /// 供 J/K 键跳转 AI 回复锚点使用。
    pub(crate) fn text_entry_display_starts(&self) -> Vec<usize> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .text_entry_display_starts()
    }
}

impl Default for OutputView {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for OutputView {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(bytes);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.append(&text);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(tool_id: &str, name: &str, input: &str) -> OutputEntry {
        OutputEntry::ToolCard {
            tool_id: tool_id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
            result: None,
            is_error: false,
            priority: Priority::P1,
            collapsed: false,
            timestamp: String::new(),
        }
    }

    #[test]
    fn write_appends_to_buffer() {
        let mut view = OutputView::new();
        view.write_all(b"hello ").unwrap();
        view.write_all(b"world").unwrap();
        // Text 渲染会带时间戳前缀 [HH:MM:SS]
        let snap = view.snapshot();
        assert!(snap.contains("hello world"));
    }

    /// 回归测试：表格 ANSI 首行以 │ 开头，render() 不得加时间戳前缀，
    /// 否则表头行会被整体右移、与数据行错位（用户报告的"表格第一行错位"）。
    #[test]
    fn table_text_entry_skips_timestamp_prefix_on_first_line() {
        use crate::render::TerminalRenderer;
        let renderer = TerminalRenderer::new();
        let rendered = renderer.markdown_to_ansi("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(rendered.contains('│'), "表格应渲染出边框: {rendered}");
        let entry = OutputEntry::text(rendered);
        let out = entry.render();
        let first_line = out.lines().next().unwrap_or_default();
        // 首行不应带灰色时间戳前缀（否则表头被整体右移）
        assert!(
            !first_line.starts_with("\u{1b}[38;5;240m["),
            "表格首行不应加时间戳前缀: {first_line:?}"
        );
    }

    #[test]
    fn shared_handle_shares_state() {
        let view = OutputView::new();
        let handle = view.shared_handle();
        handle.lock().unwrap().append("shared");
        assert!(view.snapshot().contains("shared"));
    }

    #[test]
    fn flush_is_noop() {
        let mut view = OutputView::new();
        assert!(view.flush().is_ok());
    }

    #[test]
    fn invalid_utf8_is_lossy_converted() {
        let mut view = OutputView::new();
        view.write_all(&[0xff, 0xfe, 0xfd]).unwrap();
        assert!(!view.snapshot().is_empty());
    }

    #[test]
    fn push_entry_creates_distinct_entry() {
        let mut buf = OutputBuffer::default();
        buf.push_entry(OutputEntry::text("text1".to_string()));
        buf.push_entry(card("t1", "bash", "{}"));
        assert_eq!(buf.entry_count(), 2);
        let snap = buf.render_all();
        assert!(snap.contains("text1"));
        assert!(snap.contains("bash"));
    }

    #[test]
    fn text_entries_merge_consecutive_writes() {
        let mut buf = OutputBuffer::default();
        buf.append("text1 ");
        buf.append("text2 ");
        buf.append("text3");
        assert_eq!(buf.entry_count(), 1, "连续 append 应合并为单个 Text entry");
        assert!(buf.render_all().contains("text1 text2 text3"));
    }

    #[test]
    fn complete_tool_card_sets_result() {
        let mut buf = OutputBuffer::default();
        buf.push_entry(OutputEntry::tool_card_start(
            "t1".to_string(),
            "bash".to_string(),
            r#"{"command":"ls"}"#.to_string(),
        ));
        assert!(buf.complete_tool_card("t1", "file1\nfile2".to_string(), false));
        assert_eq!(buf.completed_tool_card_count(), 1);
        let snap = buf.render_all();
        assert!(snap.contains("bash"));
        assert!(snap.contains("2 行"));
    }

    /// 回归测试：complete_tool_card 后长输出应显示折叠预览（前3行 + 展开 hint）。
    #[test]
    fn complete_tool_card_long_output_shows_collapse_preview() {
        let mut buf = OutputBuffer::default();
        buf.push_entry(OutputEntry::tool_card_start(
            "t1".to_string(),
            "bash".to_string(),
            r#"{"command":"cat big.txt"}"#.to_string(),
        ));
        let long_output = (1..=50).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        assert!(buf.complete_tool_card("t1", long_output, false));
        let snap = buf.render_all();
        assert!(snap.contains("50 行"), "应显示总行数: {snap}");
        assert!(snap.contains("折叠"), "应显示折叠标记: {snap}");
        assert!(!snap.contains("[+] 展开"), "单行标题折叠不应有展开提示: {snap}");
    }

    #[test]
    fn toggle_latest_tool_card_switches_collapsed() {
        let mut buf = OutputBuffer::default();
        buf.push_entry(OutputEntry::text("before".to_string()));
        buf.push_entry(OutputEntry::tool_card_start(
            "t1".to_string(),
            "bash".to_string(),
            "{}".to_string(),
        ));
        buf.complete_tool_card("t1", "ok".to_string(), false);
        assert!(buf.toggle_latest_tool_card());
        // 折叠后渲染只显示标题行
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::ToolCard {
                collapsed: true,
                ..
            })
        ));
    }

    /// 流式 thinking：增量 delta 实时追加全文，结束后自动折叠为摘要。
    #[test]
    fn thinking_streaming_append_delta_then_complete_collapses() {
        let mut buf = OutputBuffer::default();
        // 思考开始 + 增量文本
        buf.start_thinking();
        buf.append_thinking_delta("step1 ");
        buf.append_thinking_delta("step2 ");
        buf.append_thinking_delta("step3");
        // 展开态应包含全部思考全文
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: false,
                full_text,
                ..
            }) if full_text == "step1 step2 step3"
        ));
        let expanded = buf.render_all();
        assert!(expanded.contains("step1 step2 step3"), "展开态应显示全文");
        assert!(expanded.contains("▶ Thinking"), "展开态应有 Thinking 标题");

        // 思考块结束 → 自动折叠为摘要
        buf.complete_thinking(Some(16), false);
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: true,
                ..
            })
        ));
        let collapsed = buf.render_all();
        assert!(collapsed.contains("▶ Thinking (16 chars hidden)"));
        assert!(!collapsed.contains("step1 step2 step3"), "折叠后隐藏全文");
    }

    /// 完整块路径：done 信号带全文，先 append 再折叠。
    #[test]
    fn thinking_complete_with_text_appends_then_collapses() {
        let mut buf = OutputBuffer::default();
        buf.append_thinking_delta("full thinking text");
        buf.complete_thinking(Some(17), false);
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: true,
                ..
            })
        ));
        let out = buf.render_all();
        assert!(out.contains("▶ Thinking (17 chars hidden)"));
    }

    /// RedactedThinking：直接 done 信号，无文本，新建折叠态摘要。
    #[test]
    fn thinking_complete_redacted_without_existing_entry() {
        let mut buf = OutputBuffer::default();
        buf.complete_thinking(None, true);
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: true,
                ..
            })
        ));
        let out = buf.render_all();
        assert!(out.contains("▶ Thinking block hidden by provider"));
    }

    /// toggle_latest_collapsible 优先切换 Thinking，否则 ToolCard。
    #[test]
    fn toggle_latest_collapsible_prefers_thinking_then_tool_card() {
        let mut buf = OutputBuffer::default();
        // 只有 ToolCard → 切换 ToolCard
        buf.push_entry(OutputEntry::tool_card_start(
            "t1".to_string(),
            "bash".to_string(),
            "{}".to_string(),
        ));
        buf.complete_tool_card("t1", "ok".to_string(), false);
        assert!(buf.toggle_latest_collapsible());
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::ToolCard {
                collapsed: true,
                ..
            })
        ));
        // 追加 Thinking → 优先切换 Thinking
        buf.append_thinking_delta("think");
        assert!(buf.toggle_latest_collapsible());
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: true,
                ..
            })
        ));
        // 再切换展开
        assert!(buf.toggle_latest_collapsible());
        assert!(matches!(
            buf.entries.back(),
            Some(OutputEntry::Thinking {
                collapsed: false,
                ..
            })
        ));
    }

    /// 展开长 ToolCard 应显示完整输出。
    #[test]
    fn toggle_expand_long_tool_card_shows_full_output() {
        let mut buf = OutputBuffer::default();
        let long_output = (1..=20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        buf.push_entry(OutputEntry::ToolCard {
            tool_id: "t1".to_string(),
            name: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
            result: Some(long_output),
            is_error: false,
            priority: Priority::P2,
            collapsed: true,
            timestamp: String::new(),
        });
        // 初始折叠：只显示标题
        let collapsed0 = buf.render_all();
        assert!(!collapsed0.contains("line1"), "P2 默认折叠应隐藏正文");
        // 切换为展开
        assert!(buf.toggle_latest_tool_card());
        let expanded = buf.render_all();
        assert!(expanded.contains("line1"));
        assert!(expanded.contains("line20"));
        // 再切换回折叠
        assert!(buf.toggle_latest_tool_card());
        let collapsed = buf.render_all();
        assert!(!collapsed.contains("line1"));
    }

    #[test]
    fn completed_tool_card_count_excludes_pending() {
        let mut buf = OutputBuffer::default();
        buf.push_entry(OutputEntry::tool_card_start(
            "t1".to_string(),
            "bash".to_string(),
            "{}".to_string(),
        ));
        assert_eq!(buf.completed_tool_card_count(), 0);
        buf.complete_tool_card("t1", "ok".to_string(), false);
        assert_eq!(buf.completed_tool_card_count(), 1);
    }

    #[test]
    fn error_entry_indices_flags_error_cards() {
        let mut buf = OutputBuffer::default();
        buf.push_entry(OutputEntry::text("reply".to_string()));
        buf.push_entry(OutputEntry::tool_card_start(
            "t1".to_string(),
            "bash".to_string(),
            r#"{"command":"false"}"#.to_string(),
        ));
        buf.complete_tool_card("t1", "boom".to_string(), true);
        let idx = buf.error_entry_indices();
        assert_eq!(idx, vec![1]);
    }

    /// 窗口容量：超出 MAX_WINDOW_ENTRIES 时从头部弹出，内存恒定。
    #[test]
    fn window_drops_oldest_when_capacity_exceeded() {
        let mut buf = OutputBuffer::default();
        for i in 0..MAX_WINDOW_ENTRIES + 50 {
            buf.push_entry(OutputEntry::text(format!("entry {i}")));
        }
        assert_eq!(
            buf.entry_count(),
            MAX_WINDOW_ENTRIES,
            "窗口应保持容量上限"
        );
        // 最旧内容已弹出（数据权威在后端 session 文件，TUI 不保留）
        assert!(!buf.render_all().contains("entry 0"));
        assert!(buf.render_all().contains(&format!("entry {}", MAX_WINDOW_ENTRIES + 49)));
    }

    /// 版本号：每次内容变化递增，供 draw 触发检测。
    #[test]
    fn version_increments_on_change() {
        let mut buf = OutputBuffer::default();
        let v0 = buf.version();
        buf.append("hello");
        assert!(buf.version() > v0);
        buf.push_entry(OutputEntry::text("x".to_string()));
        assert!(buf.version() > v0 + 1);
        let v1 = buf.version();
        buf.complete_tool_card("missing", "x".to_string(), false);
        assert_eq!(buf.version(), v1, "未命中的 complete 不应递增版本");
    }

    /// 渲染快照与 breaks 一致（draw 数据源）。
    #[test]
    fn snapshot_lines_and_breaks_consistent() {
        let mut buf = OutputBuffer::default();
        buf.push_entry(OutputEntry::text("AAA".to_string()));
        buf.push_entry(card("t1", "bash", "{}"));
        buf.complete_tool_card("t1", "ok".to_string(), false);
        buf.append("回复文本\n\n第二段");
        let lines = buf.snapshot_lines();
        let breaks = buf.snapshot_breaks();
        assert_eq!(breaks.len(), buf.entry_count() + 1);
        for (i, &b) in breaks.iter().enumerate() {
            assert!(b <= lines.len(), "break[{i}]={b} 越界");
        }
        assert_eq!(*breaks.last().unwrap(), lines.len());
        let joined: String = lines.iter().map(|l| l.to_string() + "\n").collect();
        assert!(joined.contains("回复文本"));
    }

    /// prepend_history：历史条目前置到窗口头部，最新内容保持尾部。
    #[test]
    fn prepend_history_puts_older_first() {
        let mut buf = OutputBuffer::default();
        buf.append("live-output");
        let history: Vec<OutputEntry> = vec![
            OutputEntry::text("historical-1".to_string()),
            OutputEntry::text("historical-2".to_string()),
        ];
        buf.prepend_history(history);
        assert_eq!(buf.entry_count(), 3);
        let all = buf.render_all();
        let i1 = all.find("historical-1").unwrap();
        let i2 = all.find("historical-2").unwrap();
        let il = all.find("live-output").unwrap();
        assert!(i1 < i2 && i2 < il, "历史应在前、最新在后: {all}");
    }

    /// 段落感知：一次 append 含 3+ 段落时全部保留（P0 回归：AI 回复被吞）。
    #[test]
    fn append_multi_paragraph_single_delta_preserves_all_content() {
        let mut buf = OutputBuffer::default();
        buf.append("Para A\n\nPara B\n\nPara C");
        let snap = buf.render_all();
        assert!(snap.contains("Para A"), "Para A 应保留: {snap}");
        assert!(snap.contains("Para B"), "Para B 应保留: {snap}");
        assert!(snap.contains("Para C"), "Para C 应保留: {snap}");
    }

    /// 流式：多段文本分段追加后累计完整（尾段合并）。
    #[test]
    fn streaming_paragraph_boundary_preserves_accumulated_text() {
        let mut buf = OutputBuffer::default();
        buf.append("Hello world.\n\n");
        buf.append("Second paragraph\n\n");
        buf.append("Final sentence");
        let snap = buf.render_all();
        assert!(snap.contains("Hello world."), "应保留已累积段落: {snap}");
        assert!(snap.contains("Second paragraph"), "第二段应保留: {snap}");
        assert!(snap.contains("Final sentence"), "flush 段应保留: {snap}");
    }

    /// 事故回归：窗口满（大量 ToolCard）+ 最终回复流式 append → 内容完整。
    /// 旧架构在 256KB 预算下 trim 吞掉 AI 回复；窗口化后无预算、无淘汰，
    /// 最新内容天然保留。
    #[test]
    fn repro_session_tail_stream_final_reply_after_trim() {
        use crate::render::{MarkdownStreamState, TerminalRenderer};
        let renderer = TerminalRenderer::shared();
        // 1) 窗口填满 ToolCard（接近上限）
        let mut buf = OutputBuffer::default();
        for i in 0..MAX_WINDOW_ENTRIES - 2 {
            let input = format!(r#"{{"command":"git status --short step{i}"}}"#);
            buf.push_entry(OutputEntry::ToolCard {
                tool_id: format!("t{i}"),
                name: "bash".to_string(),
                input,
                result: Some(format!("{{\"stdout\":\"{}\"}}", "output line\n".repeat(60))),
                is_error: false,
                priority: Priority::P1,
                collapsed: false,
                timestamp: String::new(),
            });
        }
        // 2) 最后一张 bash 卡片完成（result 即事故现场的 1843 字节）
        buf.push_entry(OutputEntry::tool_card_start(
            "last_bash".to_string(),
            "bash".to_string(),
            r#"{"command":"git add macd.py && git commit -m 'perf: test' && git log --oneline -3"}"#
                .to_string(),
        ));
        buf.complete_tool_card("last_bash", "x".repeat(1843), false);
        // 3) 分片流式 append 最终回复（模拟 TextDelta 事件序列 + MessageStop flush）
        let final_reply = "✅ **已提交** `b396231`（分支 `yesterday-full`）\n\n```\nb396231 perf: 切换时间周期/品种数据链路加速 3.6x — MACD 面积扫描二分优化\n  2 files changed, 82 insertions(+), 57 deletions(-)\n```\n\n**提交内容**（纯本次优化，方便精确回滚）：\n- `macd.py`：`scan_macd_area` 二分优化 + 单循环合并（137 行）\n- `data_manager.py`：仅 `MAX_STORES 3→6` 这一个 hunk（用 `git add -p` 拆分出来的）\n\n**留在工作树未提交**：data_manager.py 里 08-10 早间 session 的**缺口检测基准修复**（`prev_confirmed_ts`、`_fill_gap from_ts` 等 9 个 hunk）——它不属于本次优化主题，回滚点更清晰。如需一并提交可以说一声。\n\n回滚命令：`git reset --hard b396231~1` 即可回到优化前（注意会同时丢弃工作树里早间的缺口修复，如需保留先 `git stash`）。\n\n现在可以继续优化了。继续之前的方向——下一步是**渐进式切换**（先拉最近 1000 根秒出图 + 后台补全剩余 4000 根），还是先做**网络层首片优先**？或者你有其他优先级想法？";
        let mut ms = MarkdownStreamState::with_max_width(Some(120));
        // 按字符边界切分，每片 ~80 字符（模拟流式 delta）
        let chars: Vec<char> = final_reply.chars().collect();
        let mut pos = 0;
        let mut appended = 0usize;
        while pos < chars.len() {
            let end = (pos + 80).min(chars.len());
            let delta: String = chars[pos..end].iter().collect();
            if let Some(rendered) = ms.push(&renderer, &delta) {
                buf.append(&rendered);
                appended += rendered.len();
            }
            pos = end;
        }
        if let Some(rendered) = ms.flush(&renderer) {
            buf.append(&rendered);
            appended += rendered.len();
        }
        let snap = buf.render_all();
        assert!(
            snap.contains("已提交"),
            "最终回复应保留在窗口中（appended={appended}）:\n{snap}"
        );
        assert!(
            snap.contains("MACD 面积扫描二分优化"),
            "code block 内容应保留"
        );
        // draw 数据源一致性
        let lines = buf.snapshot_lines();
        let breaks = buf.snapshot_breaks();
        assert_eq!(breaks.len(), buf.entry_count() + 1);
        assert_eq!(*breaks.last().unwrap(), lines.len());
        let joined: String = lines.iter().map(|l| l.to_string() + "\n").collect();
        assert!(joined.contains("已提交"), "snapshot_lines 应含最终回复");
    }

    /// compute_priority：模型 emphasis=high → P0
    #[test]
    fn compute_priority_emphasis_high() {
        let input = r#"{"command":"ls","emphasis":"high"}"#;
        let result = r#"{"stdout":"ok"}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P0);
    }

    /// compute_priority：模型 emphasis=low → P3
    #[test]
    fn compute_priority_emphasis_low() {
        let input = r#"{"command":"ls","emphasis":"low"}"#;
        let result = r#"{"stdout":"ok"}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P3);
    }

    /// compute_priority：is_error=true → P0（非 bash 工具）
    #[test]
    fn compute_priority_is_error() {
        let input = r#"{"path":"foo.rs"}"#;
        assert_eq!(
            compute_priority("read_file", input, "err", true),
            Priority::P0
        );
    }

    /// compute_priority：bash interrupted → P3（用户取消）
    #[test]
    fn compute_priority_bash_interrupted() {
        let input = r#"{"command":"sleep 100"}"#;
        let result = r#"{"returnCodeInterpretation":"interrupted"}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P3);
    }

    /// compute_priority：bash exit_code:1 → P0（命令失败）
    #[test]
    fn compute_priority_bash_exit_nonzero() {
        let input = r#"{"command":"false"}"#;
        let result = r#"{"returnCodeInterpretation":"exit_code:1"}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P0);
    }

    /// compute_priority：bash idle.timeout → P0（挂起需关注）
    #[test]
    fn compute_priority_bash_idle_timeout() {
        let input = r#"{"command":"hang"}"#;
        let result = r#"{"returnCodeInterpretation":"idle.timeout"}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P0);
    }

    /// compute_priority：bash exit_code:0 + 短输出 → P1（默认展开）
    #[test]
    fn compute_priority_bash_ok_short() {
        let input = r#"{"command":"ls"}"#;
        let result = r#"{"returnCodeInterpretation":"exit_code:0","stdout":"file1\nfile2"}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P1);
    }

    /// compute_priority：长输出 >8 行 → P2（折叠为单行标题）
    #[test]
    fn compute_priority_long_output() {
        let input = r#"{"command":"cat big.txt"}"#;
        let result = "line\n".repeat(50);
        assert_eq!(
            compute_priority("bash", input, &result, false),
            Priority::P2
        );
    }

    /// compute_priority：9 行输出 → P2（门槛边界,>8 即折叠）
    #[test]
    fn compute_priority_9_lines_collapses() {
        let input = r#"{"command":"ls"}"#;
        let result = "line\n".repeat(9);
        assert_eq!(
            compute_priority("bash", input, &result, false),
            Priority::P2
        );
    }

    /// compute_priority：8 行输出 → P1（门槛边界,≤8 保持展开）
    #[test]
    fn compute_priority_8_lines_expands() {
        let input = r#"{"command":"ls"}"#;
        let result = "line\n".repeat(8);
        assert_eq!(
            compute_priority("bash", input, &result, false),
            Priority::P1
        );
    }

    /// compute_priority：normal emphasis → P1
    #[test]
    fn compute_priority_emphasis_normal() {
        let input = r#"{"command":"ls","emphasis":"normal"}"#;
        let result = r#"{"stdout":"ok"}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P1);
    }

    /// compute_priority：bash 长输出含 error: → P0（错误信号覆盖行数）
    #[test]
    fn compute_priority_bash_error_marker_is_p0() {
        let input = r#"{"command":"cargo build"}"#;
        let result = format!(
            "{{\"returnCodeInterpretation\":\"exit_code:101\",\"stdout\":\"{}\"}}",
            "error[E0308]: mismatched types\n".repeat(30)
        );
        assert_eq!(
            compute_priority("bash", input, &result, false),
            Priority::P0
        );
    }

    /// compute_priority：bash 长输出含 test result: → P1（测试总结是信号）
    #[test]
    fn compute_priority_bash_test_result_long_output_expands() {
        let input = r#"{"command":"cargo test"}"#;
        let result = format!(
            "{{\"returnCodeInterpretation\":\"exit_code:0\",\"stdout\":\"{}\"}}",
            "running 41 tests\ntest result: ok. 41 passed; 0 failed".to_string()
        );
        assert_eq!(
            compute_priority("bash", input, &result, false),
            Priority::P1
        );
    }

    /// compute_priority：read_file 20 行 → P1（内容是答案，门槛 40 行）
    #[test]
    fn compute_priority_read_file_20_lines_expands() {
        let input = r#"{"path":"foo.rs"}"#;
        let result = (1..=20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        assert_eq!(
            compute_priority("read_file", input, &result, false),
            Priority::P1
        );
    }

    /// compute_priority：write_file 纯确认 → P3
    #[test]
    fn compute_priority_write_file_confirms_p3() {
        let input = r#"{"path":"a.txt"}"#;
        let result = r#"{"ok":true}"#;
        assert_eq!(
            compute_priority("write_file", input, result, false),
            Priority::P3
        );
    }

    /// compute_priority：write_file cargo check 错误 → P0
    #[test]
    fn compute_priority_write_file_cargo_check_error_p0() {
        let input = r#"{"path":"src/main.rs","content":"fn main() {}"}"#;
        let result = format!(
            "{}\n\n--- cargo check ---\nerror[E0308]: mismatched types\n --> src/main.rs:2:23",
            r#"{
    "type": "write",
    "filePath": "src/main.rs",
    "content": "fn main() {}",
    "structuredPatch": [],
    "originalFile": null,
    "gitDiff": null
  }"#
        );
        assert_eq!(
            compute_priority("write_file", input, &result, false),
            Priority::P0
        );
    }

    /// 鼠标点击行号区间与显示 wrap 一致。
    #[test]
    fn tool_card_line_ranges_match_display_wrap() {
        let mut buf = OutputBuffer::default();
        // 前置 Text entry（不折行的短文本）
        buf.push_entry(OutputEntry::text("hello world".to_string()));
        // ToolCard 1：30 行 × 30 字符，宽度 20 下每行折成 2 个显示行
        buf.push_entry(OutputEntry::ToolCard {
            tool_id: "t1".to_string(),
            name: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
            result: Some(
                (1..=30)
                    .map(|i| format!("line{i:02}").repeat(2))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            is_error: false,
            priority: Priority::P2,
            collapsed: true,
            timestamp: String::new(),
        });
        // ToolCard 2：短输出，不折行
        buf.push_entry(OutputEntry::ToolCard {
            tool_id: "t2".to_string(),
            name: "read_file".to_string(),
            input: r#"{"path":"a.rs"}"#.to_string(),
            result: Some("line1\nline2".to_string()),
            is_error: false,
            priority: Priority::P1,
            collapsed: false,
            timestamp: String::new(),
        });
        let width = 20;
        let ranges = buf.tool_card_line_ranges(width);
        assert_eq!(ranges.len(), 2, "应有 2 个 ToolCard 区间: {ranges:?}");
        let (i1, s1, e1) = ranges[0];
        let (i2, s2, e2) = ranges[1];
        assert_eq!((i1, i2), (1, 2), "区间应按 entry 顺序: {ranges:?}");
        // 区间有效且不重叠（旧实现因 ANSI 宽度高估会重叠/错位）
        assert!(s1 < e1, "区间1 应有效 [start<end]: {ranges:?}");
        assert!(s2 < e2, "区间2 应有效 [start<end]: {ranges:?}");
        assert!(e1 <= s2, "区间不应重叠: {ranges:?}");
        // 命中测试：点击区间起点行应切换对应的卡片
        assert!(
            buf.toggle_tool_card_at_line(s1, width),
            "点击区间1起点应命中卡片1"
        );
        assert!(
            matches!(&buf.entries[1], OutputEntry::ToolCard { collapsed: false, .. }),
            "卡片1 应被展开"
        );
        let ranges2 = buf.tool_card_line_ranges(width);
        assert!(
            buf.toggle_tool_card_at_line(ranges2[1].1, width),
            "点击区间2起点应命中卡片2"
        );
        assert!(
            matches!(&buf.entries[2], OutputEntry::ToolCard { collapsed: true, .. }),
            "卡片2 应被折叠"
        );
        // 区间外（前置 Text entry 所在行）不应命中
        assert!(
            !buf.toggle_tool_card_at_line(0, width),
            "Text entry 行不应命中 ToolCard"
        );
    }
}
