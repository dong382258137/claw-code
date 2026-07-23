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

use crate::render::TerminalRenderer;

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
                let output = result.as_ref().unwrap();
                if *collapsed {
                    // 折叠状态：只显示 header + 结果摘要
                    let icon = if *is_error { "❌" } else { "✅" };
                    let line_count = output.lines().count();
                    if line_count == 0 {
                        format!("\n{ts_prefix}┌─ 🔧 {name} {summary}\n{ts_prefix}├─ {icon} {name} (空)\n{ts_prefix}└─\n")
                    } else {
                        format!(
                            "\n{ts_prefix}┌─ 🔧 {name} {summary}\n{ts_prefix}├─ {icon} {name} ({line_count} 行，已折叠)\n{ts_prefix}└─\n"
                        )
                    }
                } else {
                    // 展开状态：显示完整卡片（含 diff 和结果）
                    // 时间戳前缀加在 header 行前
                    let rendered = crate::tui::tool_card::render_tool_result_public(
                        name,
                        output,
                        *is_error,
                        Some(input),
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
}

/// 线程安全的结构化输出缓冲区。
fn ansi_to_lines(ansi: &str) -> Vec<Line<'static>> {
    if ansi.is_empty() {
        return Vec::new();
    }
    match ansi.into_text() {
        Ok(text) => text.lines,
        Err(_) => ansi.lines().map(|l| Line::raw(l.to_string())).collect(),
    }
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
    renderer: TerminalRenderer,
}

impl OutputBuffer {
    fn invalidate_lines_cache(&mut self) {
        self.cached_lines = None;
    }

    fn snapshot_lines(&mut self) -> Arc<Vec<Line<'static>>> {
        if self.cached_lines.is_none() {
            let lines = ansi_to_lines(&self.cached_snapshot);
            self.cached_lines = Some(Arc::new(lines));
        }
        Arc::clone(self.cached_lines.as_ref().unwrap())
    }
}

impl OutputBuffer {
    /// 追加文本到当前条目。如果最后一个条目是 Text，则合并；
    /// 否则新建一个 Text 条目。
    pub(crate) fn append(&mut self, text: &str) {
        self.total_written += text.len() as u64;
        // 尝试合并到上一个 Text 条目
        let from_idx = if let Some(OutputEntry::Text { content, .. }) = self.entries.last_mut() {
            content.push_str(text);
            self.text_total_bytes += text.len();
            self.entries.len() - 1
        } else {
            self.text_total_bytes += text.len();
            self.entries.push(OutputEntry::text(text.to_string()));
            self.entries.len() - 1
        };
        // 增量更新 cached_snapshot：只重渲染受影响的最后一个条目。
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

    /// 更新指定 tool_id 的 ToolCard：设置 result 并切换为折叠状态。
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
            if let OutputEntry::ToolCard {
                result: r,
                is_error: e,
                collapsed,
                ..
            } = &mut self.entries[idx]
            {
                *r = Some(result);
                *e = is_error;
                *collapsed = true;
            }
            // 增量更新 cached_snapshot：从 idx 开始重渲染。
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
            matches!(e, OutputEntry::ToolCard { result: Some(_), .. }).then_some(idx)
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
            if matches!(e, OutputEntry::ToolCard { result: Some(_), .. }) {
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
        let start_byte: usize = self
            .rendered_lengths
            .iter()
            .take(from_idx)
            .sum();
        // 截断 cached_snapshot 到 start_byte
        self.cached_snapshot.truncate(start_byte);
        // 截断 rendered_lengths 到 from_idx
        self.rendered_lengths.truncate(from_idx);
        // 重新渲染 from_idx 之后的条目
        for i in from_idx..self.entries.len() {
            let rendered = self.entries[i].render();
            let len = rendered.len();
            self.cached_snapshot.push_str(&rendered);
            self.rendered_lengths.push(len);
        }
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
            let first_card_idx = self.entries.iter().position(|e| {
                matches!(e, OutputEntry::ToolCard { result: Some(r), .. }
                    if !r.is_empty() && !r.starts_with("[trimmed:"))
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

    /// 清空所有条目。
    pub(crate) fn clear(&mut self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.entries.clear();
        guard.text_total_bytes = 0;
        guard.truncated = false;
        guard.rendered_lengths.clear();
        guard.cached_snapshot.clear();
        guard.cached_lines = None;
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
                collapsed: true,
                timestamp: String::new(),
            });
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t2".to_string(),
                name: "read".to_string(),
                input: "{}".to_string(),
                result: None,
                is_error: false,
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
}
