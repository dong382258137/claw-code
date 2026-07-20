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

/// 最大保留字节数（Text 条目的总文本长度上限）。
const MAX_BUFFER_BYTES: usize = 64 * 1024;

/// 结构化输出条目。
#[derive(Debug, Clone)]
pub(crate) enum OutputEntry {
    /// 普通文本流（AI 回复、用户 echo、斜杠命令输出）。
    Text { content: String },
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
    },
    /// Thinking 块摘要。
    Thinking { summary: String },
    /// 工具时间线。
    Timeline { summary: String },
}

impl OutputEntry {
    /// 返回此条目在当前折叠状态下渲染出的文本（含 ANSI 转义）。
    /// 每个条目末尾不以换行结束，由调用方负责条目间分隔。
    pub(crate) fn render(&self) -> String {
        match self {
            OutputEntry::Text { content } => content.clone(),
            OutputEntry::Thinking { summary } => summary.clone(),
            OutputEntry::Timeline { summary } => summary.clone(),
            OutputEntry::ToolCard {
                name,
                input,
                result,
                is_error,
                collapsed,
                ..
            } => {
                let summary = crate::tui::tool_card::summarize_tool_input_public(name, input);
                if result.is_none() {
                    // 执行中：只显示 header
                    return format!("\n┌─ 🔧 {name} {summary} ⏳\n");
                }
                let output = result.as_ref().unwrap();
                if *collapsed {
                    // 折叠状态：只显示 header + 结果摘要
                    let icon = if *is_error { "❌" } else { "✅" };
                    let line_count = output.lines().count();
                    if line_count == 0 {
                        format!("\n┌─ 🔧 {name} {summary}\n├─ {icon} {name} (空)\n└─\n")
                    } else {
                        format!(
                            "\n┌─ 🔧 {name} {summary}\n├─ {icon} {name} ({line_count} 行，已折叠)\n└─\n"
                        )
                    }
                } else {
                    // 展开状态：显示完整卡片（含 diff 和结果）
                    crate::tui::tool_card::render_tool_result_public(
                        name,
                        output,
                        *is_error,
                        Some(input),
                    )
                }
            }
        }
    }
}

/// 线程安全的结构化输出缓冲区。
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
}

impl OutputBuffer {
    /// 追加文本到当前条目。如果最后一个条目是 Text，则合并；
    /// 否则新建一个 Text 条目。
    pub(crate) fn append(&mut self, text: &str) {
        self.total_written += text.len() as u64;
        // 尝试合并到上一个 Text 条目
        if let Some(OutputEntry::Text { content }) = self.entries.last_mut() {
            content.push_str(text);
            self.text_total_bytes += text.len();
        } else {
            self.text_total_bytes += text.len();
            self.entries.push(OutputEntry::Text {
                content: text.to_string(),
            });
        }
        self.trim_if_needed();
    }

    /// 追加一个结构化条目。
    pub(crate) fn push_entry(&mut self, entry: OutputEntry) {
        if let OutputEntry::Text { content } = &entry {
            self.text_total_bytes += content.len();
        }
        self.entries.push(entry);
        self.trim_if_needed();
    }

    /// 更新指定 tool_id 的 ToolCard：设置 result 并切换为折叠状态。
    pub(crate) fn complete_tool_card(
        &mut self,
        tool_id: &str,
        result: String,
        is_error: bool,
    ) -> bool {
        for entry in self.entries.iter_mut() {
            if let OutputEntry::ToolCard {
                tool_id: id,
                result: r,
                is_error: e,
                ..
            } = entry
            {
                if id == tool_id && r.is_none() {
                    *r = Some(result);
                    *e = is_error;
                    // 结果到达后默认折叠
                    if let OutputEntry::ToolCard { collapsed, .. } = entry {
                        *collapsed = true;
                    }
                    return true;
                }
            }
        }
        false
    }

    /// 切换最近一个 ToolCard 的折叠/展开状态。
    /// 返回 true 表示成功切换。
    pub(crate) fn toggle_latest_tool_card(&mut self) -> bool {
        for entry in self.entries.iter_mut().rev() {
            if let OutputEntry::ToolCard {
                collapsed,
                result: Some(_),
                ..
            } = entry
            {
                *collapsed = !*collapsed;
                return true;
            }
        }
        false
    }

    /// 切换指定索引处 ToolCard 的折叠状态。
    pub(crate) fn toggle_tool_card_at(&mut self, index: usize) -> bool {
        let mut count = 0;
        for entry in self.entries.iter_mut() {
            if let OutputEntry::ToolCard {
                collapsed,
                result: Some(_),
                ..
            } = entry
            {
                if count == index {
                    *collapsed = !*collapsed;
                    return true;
                }
                count += 1;
            }
        }
        false
    }

    /// 返回所有已完成的 ToolCard 数量。
    pub(crate) fn completed_tool_card_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, OutputEntry::ToolCard { result: Some(_), .. }))
            .count()
    }

    /// 渲染所有条目为单个字符串。
    pub(crate) fn render_all(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&entry.render());
        }
        out
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

    /// 当 Text 总字节数超限时，淘汰最早的 Text 条目。
    fn trim_if_needed(&mut self) {
        while self.text_total_bytes > MAX_BUFFER_BYTES {
            // 找到第一个 Text 条目并移除
            let first_text_idx = self.entries.iter().position(|e| matches!(e, OutputEntry::Text { .. }));
            if let Some(idx) = first_text_idx {
                if let OutputEntry::Text { content } = &self.entries[idx] {
                    self.text_total_bytes = self.text_total_bytes.saturating_sub(content.len());
                }
                self.entries.remove(idx);
                self.truncated = true;
            } else {
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
    pub(crate) fn snapshot(&self) -> String {
        self.inner
            .lock()
            .expect("OutputBuffer mutex poisoned")
            .render_all()
    }

    /// 清空所有条目。
    pub(crate) fn clear(&mut self) {
        let mut guard = self.inner.lock().expect("OutputBuffer mutex poisoned");
        guard.entries.clear();
        guard.text_total_bytes = 0;
        guard.truncated = false;
    }

    /// 总写入字节数。
    pub(crate) fn total_written(&self) -> u64 {
        self.inner
            .lock()
            .expect("OutputBuffer mutex poisoned")
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
        let mut guard = self.inner.lock().expect("OutputBuffer mutex poisoned");
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
        assert_eq!(view.snapshot(), "hello world");
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
        // 渲染后长度应 <= MAX_BUFFER_BYTES（可能因合并而略小）
        assert!(snap.len() <= MAX_BUFFER_BYTES);
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
        assert_eq!(snap, "shared");
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
            guard.push_entry(OutputEntry::Thinking {
                summary: "\n▶ Thinking hidden\n".to_string(),
            });
        }
        view.write_all(b"text2").unwrap();
        let snap = view.snapshot();
        assert!(snap.contains("text1"));
        assert!(snap.contains("text2"));
        assert!(snap.contains("Thinking hidden"));
    }

    #[test]
    fn complete_tool_card_sets_result() {
        let mut view = OutputView::new();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t1".to_string(),
                name: "bash".to_string(),
                input: r#"{"command":"ls"}"#.to_string(),
                result: None,
                is_error: false,
                collapsed: false,
            });
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
        let mut view = OutputView::new();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t1".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
                result: Some("output".to_string()),
                is_error: false,
                collapsed: true,
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
        let mut view = OutputView::new();
        {
            let mut guard = view.inner.lock().unwrap();
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t1".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
                result: Some("out".to_string()),
                is_error: false,
                collapsed: true,
            });
            guard.push_entry(OutputEntry::ToolCard {
                tool_id: "t2".to_string(),
                name: "read".to_string(),
                input: "{}".to_string(),
                result: None,
                is_error: false,
                collapsed: false,
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
}
