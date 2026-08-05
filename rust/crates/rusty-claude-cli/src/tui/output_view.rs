#![cfg(feature = "full-tui")]

//! 结构化输出视图 — 支持交互式折叠/展开的工具卡片。
//!
//! P1 重构：从纯文本 ring buffer 改为结构化条目存储。
//! - `OutputEntry::Text` — 普通文本流（AI 回复、用户 echo）
//! - `OutputEntry::ToolCard` — 工具调用卡片，可折叠/展开
//! - `OutputEntry::Thinking` — Thinking 块摘要
//! - `OutputEntry::Timeline` — 工具时间线
//!
//! 渲染时根据每个 entry 的 `collapsed` 状态动态生成可见行。
//! `Tab` 键切换最近一个 ToolCard 的折叠状态。

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ansi_to_tui::IntoText;
use ratatui::text::Line;

/// 最大保留字节数（Text 条目的总文本长度上限）。
/// 调大到 256KB 以支持长会话（100+ 工具调用）。
const MAX_BUFFER_BYTES: usize = 256 * 1024;

/// trim_if_needed 的最大迭代次数，防止意外死循环。
const MAX_TRIM_ITERS: usize = 1000;

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
    // 1. 模型 emphasis（最高优先级，模型明确表达意图）
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
        if let Some(emp) = v.get("emphasis").and_then(|e| e.as_str()) {
            return match emp {
                "high" => Priority::P0,
                "low" => Priority::P3,
                _ => Priority::P1, // "normal"
            };
        }
    }

    // 2. is_error（对非 bash 工具有效；bash 成功执行时 is_error 永远 false）
    if is_error {
        return Priority::P0;
    }

    // 3. bash returnCodeInterpretation 启发式（解析输出 JSON 中的字段）
    if tool_name == "bash" {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(result) {
            if let Some(rc) = v.get("returnCodeInterpretation").and_then(|e| e.as_str()) {
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
    /// Thinking 块摘要。
    Thinking { summary: String, timestamp: String },
    /// 工具时间线。
    Timeline { summary: String, timestamp: String },
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
    pub(crate) fn thinking(summary: String) -> Self {
        Self::Thinking {
            summary,
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

    /// 返回此条目在当前折叠状态下渲染出的文本（含 ANSI 转义）。
    /// 每个条目末尾不以换行结束，由调用方负责条目间分隔。
    pub(crate) fn render(&self) -> String {
        match self {
            OutputEntry::Text { content, timestamp } => {
                format!("\x1b[38;5;240m[{timestamp}]\x1b[0m {content}")
            }
            OutputEntry::Thinking { summary, timestamp } => {
                format!("\x1b[38;5;240m[{timestamp}]\x1b[0m{summary}")
            }
            OutputEntry::Timeline { summary, timestamp } => {
                format!("\x1b[38;5;240m[{timestamp}]\x1b[0m{summary}")
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
                // 之前 collapsed==true 走独立分支只显示一行摘要，导致
                // render_tool_result 的折叠预览优化永远不生效。
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

/// 线程安全的结构化输出缓冲区。
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
#[derive(Debug, Default)]
pub(crate) struct OutputBuffer {
    /// 结构化条目列表（按追加顺序）。
    entries: Vec<OutputEntry>,
    /// Text 条目的总字节数（用于 MAX_BUFFER_BYTES 限制）。
    text_total_bytes: usize,
    /// 总写入字节数（含已淘汰的，用于诊断）。
    total_written: u64,
    /// 是否发生过淘汰。
    truncated: bool,
    /// 每个条目在 cached_snapshot 中对应的文本长度。与 entries 等长，
    /// 用于 recompute_snapshot_tail 增量更新时定位截断点。
    rendered_lengths: Vec<usize>,
    /// 渲染后的完整文本缓存。由 recompute_snapshot_tail 增量维护，
    /// snapshot() 直接 clone 此字段，持锁时间 O(n) 纯 memcpy 无业务逻辑。
    cached_snapshot: String,
    cached_lines: Option<Arc<Vec<Line<'static>>>>,
    /// 每个 entry 在 cached_lines 中的起始行号。
    /// 长度 = entries.len() + 1，breaks[0]=0，breaks[i+1] = 前 i+1 个 entry 的总行数。
    /// 由 recompute_snapshot_tail 增量维护，用于增量更新 cached_lines 时定位截断点。
    cached_lines_breaks: Vec<usize>,
}

impl OutputBuffer {
    fn invalidate_lines_cache(&mut self) {
        self.cached_lines = None;
        self.cached_lines_breaks.clear();
    }

    fn snapshot_lines(&mut self) -> Arc<Vec<Line<'static>>> {
        if self.cached_lines.is_none() {
            // 全量重建：逐 entry 解析（而非 ansi_to_lines 整个 cached_snapshot），
            // 同时建立 cached_lines_breaks 索引，供后续 recompute 增量更新。
            let mut lines: Vec<Line<'static>> = Vec::new();
            let mut breaks: Vec<usize> = Vec::with_capacity(self.entries.len() + 1);
            breaks.push(0);
            for entry in &self.entries {
                let rendered = entry.render();
                let entry_lines = ansi_to_lines(&rendered);
                lines.extend(entry_lines);
                breaks.push(lines.len());
            }
            self.cached_lines = Some(Arc::new(lines));
            self.cached_lines_breaks = breaks;
        }
        Arc::clone(
            self.cached_lines
                .as_ref()
                .expect("cached_lines must be Some after is_none check"),
        )
    }
}

impl OutputBuffer {
    /// 追加文本到当前条目。如果最后一个条目是 Text，则合并；
    /// 否则新建一个 Text 条目。
    ///
    /// P0 改进(2026-08-01):段落感知分段。当 text 包含段落分隔(双换行 `\n\n`)时,
    /// 按段落分割为独立 Text entry。这样每段 AI 回复都是独立可寻址的条目,
    /// 配合 J/K 键可快速跳转。最后一段不闭合(可能后续还有流式数据继续追加)。
    pub(crate) fn append(&mut self, text: &str) {
        self.total_written += text.len() as u64;

        // 段落感知分段:检测 text 中的双换行分隔符
        // 只对流式 AI 回复有意义(非工具输出)。工具结果走 push_entry,不会经过 append。
        if text.contains("\n\n") {
            self.append_segmented(text);
            return;
        }

        // 无段落分隔:走原合并逻辑
        let from_idx = if let Some(OutputEntry::Text { content, .. }) = self.entries.last_mut() {
            content.push_str(text);
            self.text_total_bytes += text.len();
            self.entries.len() - 1
        } else {
            self.text_total_bytes += text.len();
            self.entries.push(OutputEntry::text(text.to_string()));
            self.entries.len() - 1
        };
        self.recompute_snapshot_tail(from_idx);
        self.trim_if_needed();
    }

    /// 段落感知追加:按双换行分割 text,每段为独立 Text entry。
    /// 最后一段合并到已存在的 trailing Text entry(或新建),支持后续流式追加。
    fn append_segmented(&mut self, text: &str) {
        // 按 "\n\n" 分割,保留非空段。最后一段单独处理(可能未结束)。
        let segments: Vec<&str> = text.split("\n\n").collect();
        let seg_count = segments.len();
        let mut from_idx = self.entries.len();

        for (i, seg) in segments.iter().enumerate() {
            if seg.is_empty() {
                continue;
            }
            if i + 1 == seg_count {
                // 最后一段:合并到 trailing Text entry 或新建
                from_idx = if let Some(OutputEntry::Text { content, .. }) = self.entries.last_mut()
                {
                    content.push_str(seg);
                    self.text_total_bytes += seg.len();
                    self.entries.len() - 1
                } else {
                    self.text_total_bytes += seg.len();
                    self.entries.push(OutputEntry::text(seg.to_string()));
                    self.entries.len() - 1
                };
            } else {
                // 中间段:闭合为独立 entry(加回 \n\n 保持渲染换行)
                let seg_text = format!("{seg}\n\n");
                self.text_total_bytes += seg_text.len();
                self.entries.push(OutputEntry::text(seg_text));
                from_idx = self.entries.len() - 1;
            }
        }
        self.recompute_snapshot_tail(from_idx);
        self.trim_if_needed();
    }

    /// 追加一个结构化条目。
    pub(crate) fn push_entry(&mut self, entry: OutputEntry) {
        // Bug L8 修复：ToolCard 的 input 字节数也计入 text_total_bytes，
        // 防止大量工具调用 input（如长 bash 命令、大文件 write 内容）无限积累。
        // result 到达时由 complete_tool_card 单独计入。
        // timestamp 字段长度不计入（恒为 8 字节，可忽略）。
        if let OutputEntry::Text { content, .. } = &entry {
            self.text_total_bytes += content.len();
        } else if let OutputEntry::ToolCard {
            input,
            result: Some(r),
            ..
        } = &entry
        {
            self.text_total_bytes += input.len() + r.len();
        } else if let OutputEntry::ToolCard {
            input,
            result: None,
            ..
        } = &entry
        {
            self.text_total_bytes += input.len();
        } else if let OutputEntry::Thinking { summary, .. } = &entry {
            self.text_total_bytes += summary.len();
        } else if let OutputEntry::Timeline { summary, .. } = &entry {
            self.text_total_bytes += summary.len();
        }
        self.entries.push(entry);
        // 增量更新 cached_snapshot：只重渲染新增的最后一个条目。
        let from_idx = self.entries.len() - 1;
        self.recompute_snapshot_tail(from_idx);
        self.trim_if_needed();
    }

    /// 更新指定 tool_id 的 ToolCard：设置 result 并按优先级决定折叠状态。
    pub(crate) fn complete_tool_card(
        &mut self,
        tool_id: &str,
        result: String,
        is_error: bool,
    ) -> bool {
        // 先查找目标索引，避免在 iter_mut 期间调用 recompute_snapshot_tail 的借用冲突。
        let found_idx = self.entries.iter().position(|e| {
            matches!(e, OutputEntry::ToolCard { tool_id: id, result: r, .. } if id == tool_id && r.is_none())
        });
        if let Some(idx) = found_idx {
            // 工具结果可能很大（read 大文件、bash 大量输出），
            // 不计入会导致内存无限制增长。
            self.text_total_bytes += result.len();
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
            // 增量更新 cached_snapshot：从 idx 开始重渲染。
            self.recompute_snapshot_tail(idx);
            self.trim_if_needed();
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
            self.text_total_bytes += result.len();
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
            self.recompute_snapshot_tail(idx);
            self.trim_if_needed();
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
            self.recompute_snapshot_tail(idx);
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
            self.recompute_snapshot_tail(idx);
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
    /// **行号计算规则**（与 `Paragraph::scroll` 单位一致，考虑 `Wrap`）：
    /// - 对每个 entry 的 `render()` 输出，按 `area_width` 计算显示行数
    ///   （每个逻辑行若超过 area_width 会折成多行显示行）
    /// - 累计得到 entry 在显示行空间的 `[start, end)` 区间
    /// - 区间命中条件：`start <= line < end`
    ///
    /// **P1-2 修复**：之前用 `rendered.lines().count()` 计算逻辑行，
    /// 但 `last_scroll_y` 是显示行单位（Paragraph::scroll 基于 Wrap 后的显示行），
    /// 两者单位不一致导致长行场景下点击坐标偏移到错误 ToolCard。
    /// 现在按显示行计算，与 `last_scroll_y` 单位一致。
    pub(crate) fn tool_card_line_ranges(
        &self,
        area_width: usize,
    ) -> Vec<(
        usize, /*entry_idx*/
        usize, /*start*/
        usize, /*end*/
    )> {
        let mut ranges = Vec::new();
        let mut current_line: usize = 0;
        let width = area_width.max(1);
        for (i, entry) in self.entries.iter().enumerate() {
            let rendered = entry.render();
            // P1-2 修复：按显示行计算而非逻辑行。
            // 每个逻辑行的显示行数 = max(1, ceil(line_visual_width / width))
            // 其中 line_visual_width 用 UnicodeWidthStr 计算（处理 CJK/emoji 宽字符）。
            let line_count: usize = rendered
                .lines()
                .map(|line| {
                    let w = unicode_width::UnicodeWidthStr::width(line);
                    if w == 0 {
                        1
                    } else {
                        w.div_ceil(width)
                    }
                })
                .sum();
            // 多数 ToolCard render 以 `\n` 开头（前导空行），把空行算作上一个 entry 的尾部
            let (start, end) = if rendered.starts_with('\n') {
                let body_lines = line_count.saturating_sub(1);
                (current_line + 1, current_line + 1 + body_lines)
            } else {
                (current_line, current_line + line_count)
            };
            if matches!(entry, OutputEntry::ToolCard { .. }) {
                ranges.push((i, start, end));
            }
            current_line += line_count;
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
                    // 增量更新 cached_snapshot：从 entry_idx 开始重渲染。
                    self.recompute_snapshot_tail(entry_idx);
                    return true;
                }
            }
        }
        false
    }

    /// 从 from_idx 开始重新渲染条目，增量更新 cached_snapshot 和 rendered_lengths。
    /// 用于 entries 被修改后的增量更新：只重渲染受影响的尾部，避免全量遍历。
    ///
    /// 复杂度：O(entries.len() - from_idx) 的 render 调用 + O(cached_snapshot.len())
    /// 的 truncate。对于高频的 append 合并（from_idx = len-1），只重渲染 1 个条目。
    fn recompute_snapshot_tail(&mut self, from_idx: usize) {
        // 计算 from_idx 之前所有条目的 render 长度之和（cached_snapshot 中的起始字节位置）
        let start_byte: usize = self.rendered_lengths.iter().take(from_idx).sum();
        // 截断 cached_snapshot 到 start_byte
        self.cached_snapshot.truncate(start_byte);
        // 截断 rendered_lengths 到 from_idx
        self.rendered_lengths.truncate(from_idx);
        // 重新渲染 from_idx 之后的条目，同时更新 cached_snapshot
        for i in from_idx..self.entries.len() {
            let rendered = self.entries[i].render();
            let len = rendered.len();
            self.cached_snapshot.push_str(&rendered);
            self.rendered_lengths.push(len);
        }
        // P0-2 修复：增量维护 cached_lines，而非全量 invalidate。
        //
        // 增量方案优先用 Arc::try_unwrap（strong_count==1 时零拷贝取出 Vec），
        // 失败时（主线程 draw 闭包仍持有 snapshot_lines 返回的 Arc clone）改为
        // clone 现有 Vec 后增量修改 —— 比 invalidate + 全量 ansi_to_lines 重建快 10-50 倍。
        //
        // 根因（渲染卡住恶性循环）：
        //   1. 主线程 draw 闭包持 Arc clone → strong_count=2
        //   2. worker 线程 append → try_unwrap 失败 → invalidate
        //   3. 下次 draw → snapshot_lines 全量重建（5-10ms 持锁）
        //   4. 全量重建期间 worker append 阻塞 → total_written 不更新 → content_changed=false
        //   5. 只有 elapsed_changed（每秒一次）触发 draw → "卡住"现象
        //
        // clone 开销分析：256KB buffer ≈ 3000 行 ≈ 9000 个 Span → ~0.3ms
        // 全量重建开销：ansi_to_lines 解析 256KB → 5-10ms
        // clone 方案在 streaming 高频 append 下总开销 ~10ms/s，可接受。
        if let Some(lines_arc) = self.cached_lines.take() {
            match Arc::try_unwrap(lines_arc) {
                Ok(mut lines) => {
                    // 快速路径：strong_count==1，零拷贝取出 Vec
                    self.cached_lines_breaks.truncate(from_idx + 1);
                    let line_start = self
                        .cached_lines_breaks
                        .get(from_idx)
                        .copied()
                        .expect("breaks[from_idx] must exist after truncate to from_idx+1");
                    lines.truncate(line_start);
                    for i in from_idx..self.entries.len() {
                        let rendered = self.entries[i].render();
                        let entry_lines = ansi_to_lines(&rendered);
                        lines.extend(entry_lines);
                        self.cached_lines_breaks.push(lines.len());
                    }
                    self.cached_lines = Some(Arc::new(lines));
                    return;
                }
                Err(arc) => {
                    // strong_count > 1（主线程仍持 Arc clone）：
                    // clone 现有 Vec，增量修改后创建新 Arc。
                    // 避免 invalidate → 全量 ansi_to_lines 重建（5-10ms 持锁）。
                    let mut lines: Vec<Line<'static>> = (*arc).clone();
                    self.cached_lines_breaks.truncate(from_idx + 1);
                    let line_start = self
                        .cached_lines_breaks
                        .get(from_idx)
                        .copied()
                        .expect("breaks[from_idx] must exist after truncate to from_idx+1");
                    lines.truncate(line_start);
                    for i in from_idx..self.entries.len() {
                        let rendered = self.entries[i].render();
                        let entry_lines = ansi_to_lines(&rendered);
                        lines.extend(entry_lines);
                        self.cached_lines_breaks.push(lines.len());
                    }
                    self.cached_lines = Some(Arc::new(lines));
                    return;
                }
            }
        }
        // 首次或被 clear：全量重建交给下次 snapshot_lines() 调用。
        self.invalidate_lines_cache();
    }

    /// 返回 cached_snapshot 的 clone。
    /// 增量维护模式下 cached_snapshot 始终最新，无需全量遍历。
    pub(crate) fn render_all(&self) -> String {
        self.cached_snapshot.clone()
    }

    /// 保留向后兼容：返回渲染后的文本（等价于 render_all）。
    pub(crate) fn buffer(&self) -> String {
        self.render_all()
    }

    /// 只读访问总写入字节数。
    pub(crate) fn total_written(&self) -> u64 {
        self.total_written
    }

    /// 只读访问 truncated 标志。
    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    /// 当 Text 总字节数超限时，淘汰最早的 Text 条目；
    /// 若无 Text 条目可淘汰，则裁剪最早的 ToolCard 的 result。
    ///
    /// **Bug L8 修复**：原实现只淘汰 Text，ToolCard 的 result（可能几 MB）
    /// 完全不计入限制，导致内存爆炸。现在：
    /// - push_entry / complete_tool_card 把 ToolCard 的 input + result 字节数
    ///   都计入 `text_total_bytes`。
    /// - trim 时若仍超限且无 Text 可淘汰，把最早的 ToolCard 的 result 替换为
    ///   `[trimmed: N bytes]` 占位符（保留 header 和 input 以维持工具调用历史，
    ///   仅裁剪可能极大的 result 文本），并相应减少 text_total_bytes。
    ///
    /// **卡死修复**：原 trim 在 ToolCard result 被替换为占位符后，下一轮迭代
    /// 仍命中同一 entry（占位符非空），导致无限循环并持锁死锁主渲染线程。
    /// 修复策略：
    /// 1. 跳过已是 `[trimmed:` 开头的占位符，遍历到下一个未裁剪过的 ToolCard
    /// 2. 增加 MAX_TRIM_ITERS 兜底，防止未来类似问题
    /// 3. 把占位符字节数从 text_total_bytes 中扣除（占位符属于元信息，不计入预算）
    fn trim_if_needed(&mut self) {
        let mut iter_count = 0;
        while self.text_total_bytes > MAX_BUFFER_BYTES {
            iter_count += 1;
            if iter_count > MAX_TRIM_ITERS {
                // 防御性兜底：超出迭代上限仍未收敛，记录 truncated 并退出。
                // 比死锁主线程好得多。
                self.truncated = true;
                break;
            }
            // 优先淘汰最早的 Text 条目
            let first_text_idx = self
                .entries
                .iter()
                .position(|e| matches!(e, OutputEntry::Text { .. }));
            if let Some(idx) = first_text_idx {
                if let OutputEntry::Text { content, .. } = &self.entries[idx] {
                    self.text_total_bytes = self.text_total_bytes.saturating_sub(content.len());
                }
                self.entries.remove(idx);
                self.truncated = true;
                // 删除条目后增量更新 cached_snapshot：从 idx 开始重渲染。
                self.recompute_snapshot_tail(idx);
                continue;
            }
            // Bug L8 修复：无 Text 可淘汰时，裁剪最早的 ToolCard 的 result。
            // 卡死修复：跳过已是占位符的 entry，避免无限裁剪同一 entry。
            // 方案 §3.4：error/P0 entry 不被 trim 淘汰（用户需看到错误）。
            let first_card_idx = self.entries.iter().position(|e| {
                matches!(e, OutputEntry::ToolCard { result: Some(r), is_error, priority, .. }
                    if !r.is_empty() && !r.starts_with("[trimmed:")
                    && !*is_error && *priority != Priority::P0)
            });
            if let Some(idx) = first_card_idx {
                if let OutputEntry::ToolCard { result, .. } = &mut self.entries[idx] {
                    if let Some(r) = result.take() {
                        let trimmed_len = r.len();
                        let placeholder = format!("[trimmed: {} bytes]", trimmed_len);
                        // 占位符自身从 text_total_bytes 中扣除，避免占位符
                        // 又被下一轮迭代命中（占位符不算业务文本）。
                        self.text_total_bytes = self.text_total_bytes.saturating_sub(trimmed_len);
                        *result = Some(placeholder);
                        self.truncated = true;
                    }
                }
                // result 被替换为占位符后增量更新 cached_snapshot。
                self.recompute_snapshot_tail(idx);
            } else {
                // 既无 Text 也无可裁剪的 ToolCard，停止以避免死循环。
                break;
            }
        }
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

    /// 快照当前渲染后的文本内容（克隆）。
    /// 增量维护模式下 cached_snapshot 始终最新，持锁期间只做 String clone。
    pub(crate) fn snapshot(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cached_snapshot
            .clone()
    }

    pub(crate) fn snapshot_lines(&self) -> Arc<Vec<Line<'static>>> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.snapshot_lines()
    }

    /// 快照每个 entry 在 cached_lines 中的起始行号(原始行,未 wrap)。
    /// 长度 = entries.len() + 1,breaks[0]=0,breaks[i+1] = 前 i+1 个 entry 的总行数。
    /// 供 sticky_view 计算粘性头部时定位 entry 边界(调用方需在 wrap 后映射到 display 行)。
    pub(crate) fn snapshot_breaks(&self) -> Vec<usize> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // 确保 cached_lines_breaks 已建立(同 snapshot_lines 的惰性初始化)
        guard.snapshot_lines();
        guard.cached_lines_breaks.clone()
    }

    /// 返回所有 Text 类型 entry 的 display 起始行号(原始行,未 wrap)。
    /// 供 J/K 键跳转 AI 回复锚点使用(P0 改进)。
    /// 仅返回 Text entry,跳过 ToolCard/Thinking/Timeline(它们不是 AI 回复)。
    pub(crate) fn text_entry_display_starts(&self) -> Vec<usize> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.snapshot_lines(); // 确保 cached_lines_breaks 已建立
        let breaks = &guard.cached_lines_breaks;
        let entries = &guard.entries;
        let mut result = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            if matches!(entry, OutputEntry::Text { .. }) {
                if let Some(&start) = breaks.get(i) {
                    result.push(start);
                }
            }
        }
        result
    }

    /// 清空所有条目。
    pub(crate) fn clear(&mut self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.entries.clear();
        guard.text_total_bytes = 0;
        guard.truncated = false;
        guard.rendered_lengths.clear();
        guard.cached_snapshot.clear();
        guard.cached_lines = None;
        guard.cached_lines_breaks.clear();
    }

    /// 总写入字节数。
    pub(crate) fn total_written(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .total_written
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

    #[test]
    fn write_appends_to_buffer() {
        let mut view = OutputView::new();
        view.write_all(b"hello ").unwrap();
        view.write_all(b"world").unwrap();
        // Text 渲染会带时间戳前缀 [HH:MM:SS]
        let snap = view.snapshot();
        assert!(snap.contains("hello world"));
    }

    #[test]
    fn total_written_counts_all_bytes() {
        let mut view = OutputView::new();
        view.write_all(b"abc").unwrap();
        view.write_all(b"de").unwrap();
        assert_eq!(view.total_written(), 5);
    }

    #[test]
    fn invalid_utf8_is_lossy_converted() {
        let mut view = OutputView::new();
        view.write_all(&[0xff, 0xfe, 0xfd]).unwrap();
        assert!(!view.snapshot().is_empty());
    }

    #[test]
    fn buffer_trims_when_exceeding_max() {
        let mut view = OutputView::new();
        let big_chunk = "x".repeat(MAX_BUFFER_BYTES + 100);
        view.write_all(big_chunk.as_bytes()).unwrap();
        let snap = view.snapshot();
        // 渲染后长度可能略大于 MAX_BUFFER_BYTES（因时间戳前缀），但应远小于写入总量
        assert!(snap.len() < MAX_BUFFER_BYTES + 100);
        assert!(view.total_written() as usize >= MAX_BUFFER_BYTES);
    }

    #[test]
    fn clear_empties_buffer() {
        let mut view = OutputView::new();
        view.write_all(b"data").unwrap();
        view.clear();
        assert_eq!(view.snapshot(), "");
    }

    #[test]
    fn shared_handle_shares_state() {
        let mut view = OutputView::new();
        let handle = view.shared_handle();
        view.write_all(b"shared").unwrap();
        let snap = handle.lock().unwrap().render_all();
        // Text 渲染会带时间戳前缀
        assert!(snap.contains("shared"));
    }

    #[test]
    fn flush_is_noop() {
        let mut view = OutputView::new();
        assert!(view.flush().is_ok());
    }

    #[test]
    fn push_entry_creates_distinct_entry() {
        let mut view = OutputView::new();
        view.write_all(b"text1").unwrap();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::thinking("\n▶ Thinking hidden\n".to_string()));
        }
        view.write_all(b"text2").unwrap();
        let snap = view.snapshot();
        assert!(snap.contains("text1"));
        assert!(snap.contains("text2"));
        assert!(snap.contains("Thinking hidden"));
    }

    #[test]
    fn complete_tool_card_sets_result() {
        let view = OutputView::new();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::tool_card_start(
                "t1".to_string(),
                "bash".to_string(),
                r#"{"command":"ls"}"#.to_string(),
            ));
        }
        // 完成工具调用
        {
            let mut guard = view.inner.lock().unwrap();
            assert!(guard.complete_tool_card("t1", "file1\nfile2".to_string(), false));
        }
        // 渲染应包含结果
        let snap = view.snapshot();
        assert!(snap.contains("bash"));
        assert!(snap.contains("2 行"));
    }

    /// 回归测试：complete_tool_card 后长输出应显示折叠预览（前3行 + 展开 hint）。
    ///
    /// 这是用户报告的核心问题：之前 complete_tool_card 把 collapsed 设为 true，
    /// 但 render() 在 collapsed==true 分支只显示一行摘要 "(N 行，已折叠)"，
    /// 完全跳过了 render_tool_result 中的折叠预览逻辑（前3行 + [+] 展开）。
    /// 修复后 render() 统一委托给 render_tool_result(..., collapsed)，
    /// collapsed==true + 长输出 → 前3行预览 + [+] 展开提示。
    #[test]
    fn complete_tool_card_long_output_shows_collapse_preview() {
        let view = OutputView::new();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::tool_card_start(
                "t1".to_string(),
                "bash".to_string(),
                r#"{"command":"ls"}"#.to_string(),
            ));
        }
        // 50 行输出，超过 P2 阈值(40) → 默认折叠
        let long_output = (1..=50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        {
            let mut guard = view.inner.lock().unwrap();
            assert!(guard.complete_tool_card("t1", long_output, false));
        }
        let snap = view.snapshot();
        // P0 改进(2026-08-01):折叠时只显示单行标题,不再显示 3 行预览。
        // 新行为:标题(含行数+折叠标记) + 尾行,不显示 [+] 展开提示和预览行。
        assert!(snap.contains("50 行"), "应显示总行数: {snap}");
        assert!(snap.contains("折叠"), "应显示折叠标记: {snap}");
        assert!(!snap.contains("[+] 展开"), "不应显示展开提示: {snap}");
        assert!(!snap.contains("│ line1"), "不应显示预览行: {snap}");
        assert!(!snap.contains("│ line3"), "不应显示预览行: {snap}");
    }

    /// 回归测试：toggle 展开 ToolCard 后长输出应显示完整内容。
    #[test]
    fn toggle_expand_long_tool_card_shows_full_output() {
        let view = OutputView::new();
        let long_output = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t1".to_string(),
                name: "bash".to_string(),
                input: r#"{"command":"ls"}"#.to_string(),
                result: Some(long_output),
                is_error: false,
                priority: Priority::P2,
                collapsed: true,
                timestamp: String::new(),
            });
        }
        // 切换为展开
        {
            let mut guard = view.inner.lock().unwrap();
            assert!(guard.toggle_latest_tool_card());
        }
        let snap = view.snapshot();
        // 展开后不应有折叠提示
        assert!(!snap.contains("[+] 展开"), "展开状态不应有折叠提示: {snap}");
        // 应包含所有行
        assert!(snap.contains("line1"));
        assert!(snap.contains("line20"));
    }

    #[test]
    fn toggle_latest_tool_card_switches_collapsed() {
        let view = OutputView::new();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t1".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
                result: Some("output".to_string()),
                is_error: false,
                priority: Priority::P2,
                collapsed: true,
                timestamp: String::new(),
            });
        }
        // 切换折叠
        {
            let mut guard = view.inner.lock().unwrap();
            assert!(guard.toggle_latest_tool_card());
        }
        // 渲染应显示完整内容（展开状态）
        let snap = view.snapshot();
        assert!(snap.contains("output"));
    }

    #[test]
    fn completed_tool_card_count_excludes_pending() {
        let view = OutputView::new();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t1".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
                result: Some("out".to_string()),
                is_error: false,
                priority: Priority::P2,
                collapsed: true,
                timestamp: String::new(),
            });
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t2".to_string(),
                name: "read".to_string(),
                input: "{}".to_string(),
                result: None,
                is_error: false,
                priority: Priority::P1,
                collapsed: false,
                timestamp: String::new(),
            });
        }
        let count = view.inner.lock().unwrap().completed_tool_card_count();
        assert_eq!(count, 1);
    }

    #[test]
    fn text_entries_merge_consecutive_writes() {
        let mut view = OutputView::new();
        view.write_all(b"hello ").unwrap();
        view.write_all(b"world").unwrap();
        let guard = view.inner.lock().unwrap();
        // 应该只有 1 个 Text 条目（合并）
        let text_count = guard
            .entries
            .iter()
            .filter(|e| matches!(e, OutputEntry::Text { .. }))
            .count();
        assert_eq!(text_count, 1);
    }

    /// 卡死回归测试：模拟 100+ 工具调用导致 text_total_bytes 超 MAX_BUFFER_BYTES，
    /// 验证 trim_if_needed 不会陷入无限循环。
    #[test]
    fn trim_if_needed_terminates_with_many_tool_cards() {
        let mut buf = OutputBuffer::default();
        // 制造 200 个 ToolCard，每个 result 1KB → 总 200KB < 256KB（不会触发 trim）
        // 再追加一个 300KB 的 Text → 触发 trim
        for i in 0..200 {
            buf.push_entry(OutputEntry::ToolCard {
                tool_id: format!("t{i}"),
                name: "bash".to_string(),
                input: "{}".to_string(),
                result: Some("x".repeat(1024)),
                is_error: false,
                priority: Priority::P2,
                collapsed: true,
                timestamp: String::new(),
            });
        }
        // 此时 text_total_bytes ≈ 200KB，再追加 100KB Text 触发 trim
        buf.append(&"y".repeat(100 * 1024));
        // 如果 trim_if_needed 死循环，这里永远不会返回（测试会超时）
        let snap = buf.render_all();
        // 应该有被裁剪的占位符
        assert!(snap.contains("[trimmed:") || buf.truncated);
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

    // ---------- 语义分类器测试（P0 修复 2026-08-04） ----------

    /// 核心回归：bash 3 行 stdout 的 pretty JSON 信封（18 行）应展开（P1）。
    /// 旧实现统计信封行数 → 恒 P2 折叠；新实现提取 stdout（3 行 ≤ 8）→ P1。
    #[test]
    fn compute_priority_pretty_json_short_stdout_expands() {
        let input = r#"{"command":"echo hi"}"#;
        let result = r#"{
  "stdout": "line1\nline2\nline3",
  "stderr": "",
  "interrupted": false,
  "isImage": false,
  "backgroundTaskId": null,
  "backgroundedByUser": false,
  "assistantAutoBackgrounded": false,
  "dangerouslyDisableSandbox": false,
  "returnCodeInterpretation": "exit_code:0",
  "noOutputExpected": false,
  "structuredContent": null,
  "persistedOutputPath": null,
  "persistedOutputSize": null,
  "sandboxStatus": {
    "enabled": true,
    "supported": true,
    "active": false
  }
}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P1);
    }

    /// bash stdout 含错误标记（rc 为 0 时也能命中）→ P0，内容信号覆盖行数
    #[test]
    fn compute_priority_bash_error_marker_is_p0() {
        let input = r#"{"command":"cargo build"}"#;
        let result = r#"{
  "stdout": "error: could not compile `demo`",
  "returnCodeInterpretation": "exit_code:0"
}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P0);
    }

    /// bash 长输出含 test result:（41 行全过测试）→ P1 展开（测试总结是信号）
    #[test]
    fn compute_priority_bash_test_result_long_output_expands() {
        let input = r#"{"command":"cargo test"}"#;
        let mut stdout = String::new();
        for i in 0..40 {
            stdout.push_str(&format!("test tests::case{i} ... ok\n"));
        }
        stdout.push_str("test result: ok. 40 passed");
        let result = format!(
            "{{\n  \"stdout\": \"{}\",\n  \"returnCodeInterpretation\": \"exit_code:0\"\n}}",
            stdout.replace('\n', "\\n")
        );
        assert_eq!(
            compute_priority("bash", input, &result, false),
            Priority::P1
        );
    }

    /// read_file 20 行内容（信封 10 行）→ P1 展开（内容是答案，门槛放宽到 40）
    #[test]
    fn compute_priority_read_file_20_lines_expands() {
        let input = r#"{"path":"src/main.rs"}"#;
        let content = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let result = format!(
            "{{\n  \"type\": \"file\",\n  \"file\": {{\n    \"filePath\": \"src/main.rs\",\n    \"content\": \"{content}\",\n    \"numLines\": 20,\n    \"startLine\": 1,\n    \"totalLines\": 20\n  }}\n}}"
        );
        assert_eq!(
            compute_priority("read_file", input, &result, false),
            Priority::P1
        );
    }

    /// write_file 纯确认 → P3 单行摘要（过程噪音折叠）
    #[test]
    fn compute_priority_write_file_confirms_p3() {
        let input = r#"{"path":"a.txt","content":"hi"}"#;
        let result = r#"{
  "type": "write",
  "filePath": "a.txt",
  "content": "hi",
  "structuredPatch": [],
  "originalFile": null,
  "gitDiff": null
}"#;
        assert_eq!(
            compute_priority("write_file", input, result, false),
            Priority::P3
        );
    }

    /// write_file 带 cargo check 编译错误 → P0（错误是信号）
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

    /// trim 保护 error entry：error/P0 ToolCard 的 result 不被 trim 淘汰（方案 §3.4）。
    #[test]
    fn trim_protects_error_entries() {
        let mut buf = OutputBuffer::default();
        // 1 个 error ToolCard（大 result）+ 1 个普通 ToolCard（大 result）
        // + 超大 Text 触发 trim。验证 error entry 的 result 保持完整。
        buf.push_entry(OutputEntry::ToolCard {
            tool_id: "err1".to_string(),
            name: "bash".to_string(),
            input: "{}".to_string(),
            result: Some("E".repeat(100 * 1024)),
            is_error: true,
            priority: Priority::P0,
            collapsed: false,
            timestamp: String::new(),
        });
        buf.push_entry(OutputEntry::ToolCard {
            tool_id: "ok1".to_string(),
            name: "bash".to_string(),
            input: "{}".to_string(),
            result: Some("O".repeat(100 * 1024)),
            is_error: false,
            priority: Priority::P2,
            collapsed: true,
            timestamp: String::new(),
        });
        // 追加 200KB Text → text_total_bytes 远超 256KB → 触发 trim
        buf.append(&"T".repeat(200 * 1024));
        // 验证 error entry 的 result 未被裁剪
        if let OutputEntry::ToolCard { result, .. } = &buf.entries[0] {
            let r = result.as_ref().expect("error result should exist");
            assert!(
                !r.starts_with("[trimmed:"),
                "error entry must not be trimmed"
            );
            assert_eq!(r.len(), 100 * 1024, "error entry result must be intact");
        } else {
            panic!("entries[0] should be the error ToolCard");
        }
    }
}
