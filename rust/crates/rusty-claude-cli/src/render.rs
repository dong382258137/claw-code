use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::sync::OnceLock;

use crossterm::cursor::{MoveToColumn, RestorePosition, SavePosition};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor, Stylize};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
    heading: Color,
    emphasis: Color,
    strong: Color,
    inline_code: Color,
    link: Color,
    quote: Color,
    table_border: Color,
    code_block_border: Color,
    spinner_active: Color,
    spinner_done: Color,
    spinner_failed: Color,
}

impl Default for ColorTheme {
    fn default() -> Self {
        Self {
            heading: Color::Cyan,
            emphasis: Color::Magenta,
            strong: Color::Yellow,
            inline_code: Color::Green,
            link: Color::Blue,
            quote: Color::DarkGrey,
            table_border: Color::DarkCyan,
            code_block_border: Color::DarkGrey,
            spinner_active: Color::Blue,
            spinner_done: Color::Green,
            spinner_failed: Color::Red,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Spinner {
    frame_index: usize,
}

impl Spinner {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        let frame = Self::FRAMES[self.frame_index % Self::FRAMES.len()];
        self.frame_index += 1;
        queue!(
            out,
            SavePosition,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_active),
            Print(format!("{frame} {label}")),
            ResetColor,
            RestorePosition
        )?;
        out.flush()
    }

    pub fn finish(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.frame_index = 0;
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_done),
            Print(format!("✔ {label}\n")),
            ResetColor
        )?;
        out.flush()
    }

    pub fn fail(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.frame_index = 0;
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_failed),
            Print(format!("✘ {label}\n")),
            ResetColor
        )?;
        out.flush()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered { next_index: u64 },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TableState {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_head: bool,
}

impl TableState {
    fn push_cell(&mut self) {
        let cell = self.current_cell.trim().to_string();
        self.current_row.push(cell);
        self.current_cell.clear();
    }

    fn finish_row(&mut self) {
        if self.current_row.is_empty() {
            return;
        }
        let row = std::mem::take(&mut self.current_row);
        if self.in_head {
            self.headers = row;
        } else {
            self.rows.push(row);
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RenderState {
    emphasis: usize,
    strong: usize,
    heading_level: Option<u8>,
    quote: usize,
    list_stack: Vec<ListKind>,
    link_stack: Vec<LinkState>,
    table: Option<TableState>,
    /// 表格渲染目标最大宽度（None 时按终端宽度/默认值解析）。
    table_max_width: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkState {
    destination: String,
    text: String,
}

impl RenderState {
    fn style_text(&self, text: &str, theme: &ColorTheme) -> String {
        let mut style = text.stylize();

        if matches!(self.heading_level, Some(1 | 2)) || self.strong > 0 {
            style = style.bold();
        }
        if self.emphasis > 0 {
            style = style.italic();
        }

        if let Some(level) = self.heading_level {
            style = match level {
                1 => style.with(theme.heading),
                2 => style.white(),
                3 => style.with(Color::Blue),
                _ => style.with(Color::Grey),
            };
        } else if self.strong > 0 {
            style = style.with(theme.strong);
        } else if self.emphasis > 0 {
            style = style.with(theme.emphasis);
        }

        if self.quote > 0 {
            style = style.with(theme.quote);
        }

        format!("{style}")
    }

    fn append_raw(&mut self, output: &mut String, text: &str) {
        if let Some(link) = self.link_stack.last_mut() {
            link.text.push_str(text);
        } else if let Some(table) = self.table.as_mut() {
            table.current_cell.push_str(text);
        } else {
            output.push_str(text);
        }
    }

    fn append_styled(&mut self, output: &mut String, text: &str, theme: &ColorTheme) {
        let styled = self.style_text(text, theme);
        self.append_raw(output, &styled);
    }
}

#[derive(Debug)]
pub struct TerminalRenderer {
    syntax_set: SyntaxSet,
    syntax_theme: Theme,
    color_theme: ColorTheme,
}

impl Default for TerminalRenderer {
    fn default() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let syntax_theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_default();
        Self {
            syntax_set,
            syntax_theme,
            color_theme: ColorTheme::default(),
        }
    }
}

/// 进程级共享 `TerminalRenderer` 单例。
///
/// `SyntaxSet::load_defaults_newlines()` + `ThemeSet::load_defaults()` 加载
/// 全套语法与主题表，单次构造可达数十毫秒。原 TUI 的 `tool_card.rs`
/// 在每次 ToolCard 渲染时都 `TerminalRenderer::new()`，导致 cargo test 等
/// 含大量工具调用的场景每个 card 都重复加载语法集，显著拖慢渲染。
///
/// 改为进程级 `OnceLock` 复用单一实例：首次访问时构造，此后零开销。
/// `TerminalRenderer` 的所有方法都是 `&self`，并发只读访问安全。
static SHARED_RENDERER: OnceLock<TerminalRenderer> = OnceLock::new();

impl TerminalRenderer {
    /// 获取进程级共享实例（首次调用时构造，此后零开销返回引用）。
    /// 用于 TUI ToolCard 渲染等高频路径，避免每次重新加载语法集。
    #[must_use]
    pub fn shared() -> &'static Self {
        SHARED_RENDERER.get_or_init(TerminalRenderer::default)
    }
}

impl TerminalRenderer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn color_theme(&self) -> &ColorTheme {
        &self.color_theme
    }

    #[must_use]
    pub fn render_markdown(&self, markdown: &str) -> String {
        self.render_markdown_with_width(markdown, None)
    }

    fn render_markdown_with_width(&self, markdown: &str, max_width: Option<usize>) -> String {
        self.render_markdown_with_width_trim(markdown, max_width, true)
    }

    /// 内部渲染实现。`trim_trailing` 为 true 时剥掉尾部空白（一次性整块渲染）；
    /// false 时保留（流式增量渲染，保证段落/表格与后续内容的换行边界不被吞掉）。
    fn render_markdown_with_width_trim(
        &self,
        markdown: &str,
        max_width: Option<usize>,
        trim_trailing: bool,
    ) -> String {
        let normalized = normalize_nested_fences(markdown);
        let mut output = String::new();
        let mut state = RenderState {
            table_max_width: max_width,
            ..RenderState::default()
        };
        let mut code_language = String::new();
        let mut code_buffer = String::new();
        let mut in_code_block = false;

        for event in Parser::new_ext(&normalized, Options::all()) {
            self.render_event(
                event,
                &mut state,
                &mut output,
                &mut code_buffer,
                &mut code_language,
                &mut in_code_block,
            );
        }

        if trim_trailing {
            output.trim_end().to_string()
        } else {
            output
        }
    }

    #[must_use]
    pub fn markdown_to_ansi(&self, markdown: &str) -> String {
        self.render_markdown_with_width(markdown, None)
    }

    /// 带目标宽度的 markdown → ANSI 渲染。`max_width` 为 Some(>0) 时作为
    /// 表格宽度上限；None 或 0 时按终端宽度（查询失败兜底 100）。
    #[must_use]
    pub fn markdown_to_ansi_with_width(&self, markdown: &str, max_width: Option<usize>) -> String {
        self.render_markdown_with_width(markdown, max_width)
    }

    #[allow(clippy::too_many_lines)]
    fn render_event(
        &self,
        event: Event<'_>,
        state: &mut RenderState,
        output: &mut String,
        code_buffer: &mut String,
        code_language: &mut String,
        in_code_block: &mut bool,
    ) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                Self::start_heading(state, level as u8, output);
            }
            Event::End(TagEnd::Paragraph) => output.push_str("\n\n"),
            Event::Start(Tag::BlockQuote(..)) => self.start_quote(state, output),
            Event::End(TagEnd::BlockQuote(..)) => {
                state.quote = state.quote.saturating_sub(1);
                output.push('\n');
            }
            Event::End(TagEnd::Heading(..)) => {
                state.heading_level = None;
                output.push_str("\n\n");
            }
            Event::End(TagEnd::Item) | Event::SoftBreak | Event::HardBreak => {
                state.append_raw(output, "\n");
            }
            Event::Start(Tag::List(first_item)) => {
                let kind = match first_item {
                    Some(index) => ListKind::Ordered { next_index: index },
                    None => ListKind::Unordered,
                };
                state.list_stack.push(kind);
            }
            Event::End(TagEnd::List(..)) => {
                state.list_stack.pop();
                output.push('\n');
            }
            Event::Start(Tag::Item) => Self::start_item(state, output),
            Event::Start(Tag::CodeBlock(kind)) => {
                *in_code_block = true;
                *code_language = match kind {
                    CodeBlockKind::Indented => String::from("text"),
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                };
                code_buffer.clear();
                self.start_code_block(code_language, output);
            }
            Event::End(TagEnd::CodeBlock) => {
                self.finish_code_block(code_buffer, code_language, output);
                *in_code_block = false;
                code_language.clear();
                code_buffer.clear();
            }
            Event::Start(Tag::Emphasis) => state.emphasis += 1,
            Event::End(TagEnd::Emphasis) => state.emphasis = state.emphasis.saturating_sub(1),
            Event::Start(Tag::Strong) => state.strong += 1,
            Event::End(TagEnd::Strong) => state.strong = state.strong.saturating_sub(1),
            Event::Code(code) => {
                let rendered =
                    format!("{}", format!("`{code}`").with(self.color_theme.inline_code));
                state.append_raw(output, &rendered);
            }
            Event::Rule => output.push_str("---\n"),
            Event::Text(text) => {
                self.push_text(text.as_ref(), state, output, code_buffer, *in_code_block);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                state.append_raw(output, &html);
            }
            Event::FootnoteReference(reference) => {
                state.append_raw(output, &format!("[{reference}]"));
            }
            Event::TaskListMarker(done) => {
                state.append_raw(output, if done { "[x] " } else { "[ ] " });
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                state.append_raw(output, &math);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                state.link_stack.push(LinkState {
                    destination: dest_url.to_string(),
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Link) => {
                if let Some(link) = state.link_stack.pop() {
                    let label = if link.text.is_empty() {
                        link.destination.clone()
                    } else {
                        link.text
                    };
                    let rendered = format!(
                        "{}",
                        format!("[{label}]({})", link.destination)
                            .underlined()
                            .with(self.color_theme.link)
                    );
                    state.append_raw(output, &rendered);
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let rendered = format!(
                    "{}",
                    format!("[image:{dest_url}]").with(self.color_theme.link)
                );
                state.append_raw(output, &rendered);
            }
            Event::Start(Tag::Table(..)) => state.table = Some(TableState::default()),
            Event::End(TagEnd::Table) => {
                if let Some(table) = state.table.take() {
                    output.push_str(&self.render_table(&table, state.table_max_width));
                    output.push_str("\n\n");
                }
            }
            Event::Start(Tag::TableHead) => {
                if let Some(table) = state.table.as_mut() {
                    table.in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(table) = state.table.as_mut() {
                    table.finish_row();
                    table.in_head = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(table) = state.table.as_mut() {
                    table.current_row.clear();
                    table.current_cell.clear();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(table) = state.table.as_mut() {
                    table.finish_row();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = state.table.as_mut() {
                    table.current_cell.clear();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = state.table.as_mut() {
                    table.push_cell();
                }
            }
            Event::Start(Tag::Paragraph | Tag::MetadataBlock(..) | _)
            | Event::End(TagEnd::Image | TagEnd::MetadataBlock(..) | _) => {}
        }
    }

    fn start_heading(state: &mut RenderState, level: u8, output: &mut String) {
        state.heading_level = Some(level);
        if !output.is_empty() {
            output.push('\n');
        }
    }

    fn start_quote(&self, state: &mut RenderState, output: &mut String) {
        state.quote += 1;
        let _ = write!(output, "{}", "│ ".with(self.color_theme.quote));
    }

    fn start_item(state: &mut RenderState, output: &mut String) {
        let depth = state.list_stack.len().saturating_sub(1);
        output.push_str(&"  ".repeat(depth));

        let marker = match state.list_stack.last_mut() {
            Some(ListKind::Ordered { next_index }) => {
                let value = *next_index;
                *next_index += 1;
                format!("{value}. ")
            }
            _ => "• ".to_string(),
        };
        output.push_str(&marker);
    }

    fn start_code_block(&self, code_language: &str, output: &mut String) {
        let label = if code_language.is_empty() {
            "code".to_string()
        } else {
            code_language.to_string()
        };
        let _ = writeln!(
            output,
            "{}",
            format!("╭─ {label}")
                .bold()
                .with(self.color_theme.code_block_border)
        );
    }

    fn finish_code_block(&self, code_buffer: &str, code_language: &str, output: &mut String) {
        output.push_str(&self.highlight_code(code_buffer, code_language));
        let _ = write!(
            output,
            "{}",
            "╰─".bold().with(self.color_theme.code_block_border)
        );
        output.push_str("\n\n");
    }

    fn push_text(
        &self,
        text: &str,
        state: &mut RenderState,
        output: &mut String,
        code_buffer: &mut String,
        in_code_block: bool,
    ) {
        if in_code_block {
            code_buffer.push_str(text);
        } else {
            state.append_styled(output, text, &self.color_theme);
        }
    }

    /// 按目标总宽收缩列宽（比例收缩，下限 MIN_TABLE_COLUMN_WIDTH）。
    /// 返回每列最终显示宽度。
    fn fit_column_widths(natural: &[usize], target_width: usize) -> Vec<usize> {
        let n = natural.len();
        if n == 0 {
            return Vec::new();
        }
        // 表格总宽 = Σ(w_i) + 3n + 1：每列内容 + 左右 padding 2、列间 ┼ 1、两端 │ 2
        let base = 3 * n + 1;
        let total: usize = natural.iter().sum::<usize>() + base;
        if total <= target_width {
            return natural.to_vec();
        }
        let available = target_width.saturating_sub(base);
        let sum_natural: usize = natural.iter().sum::<usize>().max(1);
        let mut widths: Vec<usize> = natural
            .iter()
            .map(|w| (w.saturating_mul(available) / sum_natural).max(MIN_TABLE_COLUMN_WIDTH))
            .collect();
        // 迭代收缩直到总和 ≤ available（每列下限 MIN_TABLE_COLUMN_WIDTH）
        loop {
            let current_total: usize = widths.iter().sum();
            if current_total <= available {
                break;
            }
            let excess = current_total - available;
            let capacity_sum: usize = widths
                .iter()
                .map(|w| w.saturating_sub(MIN_TABLE_COLUMN_WIDTH))
                .sum();
            if capacity_sum == 0 {
                break; // 全部已到下限，接受溢出（TUI 裁剪兜底）
            }
            let mut remaining = excess.min(capacity_sum);
            let mut changed = false;
            for w in widths.iter_mut() {
                if remaining == 0 {
                    break;
                }
                let capacity = w.saturating_sub(MIN_TABLE_COLUMN_WIDTH);
                if capacity > 0 {
                    let take = capacity.min(remaining);
                    *w -= take;
                    remaining -= take;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        widths
    }

    /// 单元格按 `\n` 分段后各段的可见宽度最大值（列宽计算用）。
    fn max_visible_line_width(cell: &str) -> usize {
        cell.split('\n').map(visible_width).max().unwrap_or(0)
    }

    fn render_table(&self, table: &TableState, max_width: Option<usize>) -> String {
        let mut rows = Vec::new();
        if !table.headers.is_empty() {
            rows.push(table.headers.clone());
        }
        rows.extend(table.rows.iter().cloned());

        if rows.is_empty() {
            return String::new();
        }

        let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
        let target_width = resolve_table_max_width(max_width);

        let natural_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                rows.iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| Self::max_visible_line_width(cell))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let widths = Self::fit_column_widths(&natural_widths, target_width);

        let border = format!("{}", "│".with(self.color_theme.table_border));
        let separator = widths
            .iter()
            .map(|width| "─".repeat(*width + 2))
            .collect::<Vec<_>>()
            .join(&format!("{}", "┼".with(self.color_theme.table_border)));
        let separator = format!("{border}{separator}{border}");

        let mut output = String::new();
        if !table.headers.is_empty() {
            output.push_str(&self.render_table_row(&table.headers, &widths, true));
            output.push('\n');
            output.push_str(&separator);
            if !table.rows.is_empty() {
                output.push('\n');
            }
        }

        for (index, row) in table.rows.iter().enumerate() {
            output.push_str(&self.render_table_row(row, &widths, false));
            if index + 1 < table.rows.len() {
                output.push('\n');
            }
        }

        output
    }

    /// 渲染一行（支持单元格按列宽折行成多物理行，所有物理行边框对齐）。
    fn render_table_row(&self, row: &[String], widths: &[usize], is_header: bool) -> String {
        let border = format!("{}", "│".with(self.color_theme.table_border));
        // 每列折行（含 `\n` 分段与 ANSI 样式保留）
        let cell_lines: Vec<Vec<String>> = widths
            .iter()
            .enumerate()
            .map(|(index, width)| {
                let cell = row.get(index).map_or("", String::as_str);
                wrap_cell_lines(cell, *width)
            })
            .collect();
        let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1).max(1);

        let mut output = String::new();
        for line_index in 0..height {
            let mut line = border.clone();
            for (index, width) in widths.iter().enumerate() {
                let cell_line = cell_lines
                    .get(index)
                    .and_then(|lines| lines.get(line_index))
                    .map_or("", String::as_str);
                line.push(' ');
                if is_header {
                    let _ = write!(line, "{}", cell_line.bold().with(self.color_theme.heading));
                } else {
                    line.push_str(cell_line);
                }
                let padding = width.saturating_sub(visible_width(cell_line));
                line.push_str(&" ".repeat(padding + 1));
                line.push_str(&border);
            }
            output.push_str(&line);
            if line_index + 1 < height {
                output.push('\n');
            }
        }
        output
    }
    #[must_use]
    pub fn highlight_code(&self, code: &str, language: &str) -> String {
        // P0 修复:bash 输出(如 `cargo test --workspace` 带 --color=always)可能
        // 包含原始 ANSI 颜色序列。syntect 的 highlight_line 不会识别这些序列,
        // 会把它们当作字面量重新高亮,导致输出的 ANSI 序列密度翻倍。
        // 这些密集序列经 crossterm 反射为键盘事件(ESC + 参数字符)会污染 InputLine,
        // 在系统繁忙时 peek-ahead 超时还会把它们当作普通字符插入 buffer。
        // 先剥离输入中的所有 ANSI 序列,再交给 syntect 重新高亮。
        let code = strip_ansi(code);
        let syntax = self
            .syntax_set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let mut syntax_highlighter = HighlightLines::new(syntax, &self.syntax_theme);
        let mut colored_output = String::new();

        for line in LinesWithEndings::from(&code) {
            match syntax_highlighter.highlight_line(line, &self.syntax_set) {
                Ok(ranges) => {
                    let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                    colored_output.push_str(&apply_code_block_background(&escaped));
                }
                Err(_) => colored_output.push_str(&apply_code_block_background(line)),
            }
        }

        colored_output
    }

    pub fn stream_markdown(&self, markdown: &str, out: &mut impl Write) -> io::Result<()> {
        let rendered_markdown = self.markdown_to_ansi(markdown);
        write!(out, "{rendered_markdown}")?;
        if !rendered_markdown.ends_with('\n') {
            writeln!(out)?;
        }
        out.flush()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MarkdownStreamState {
    pending: String,
    /// 表格渲染目标宽度（None → 终端宽度/默认值）。
    max_width: Option<usize>,
}

impl MarkdownStreamState {
    /// 携带初始目标宽度的构造器。
    #[must_use]
    pub fn with_max_width(max_width: Option<usize>) -> Self {
        Self {
            pending: String::new(),
            max_width,
        }
    }

    /// 更新目标宽度（TUI draw 循环每帧刷新内容区宽度后由 emitter 调用）。
    pub fn set_max_width(&mut self, max_width: Option<usize>) {
        self.max_width = max_width;
    }

    #[must_use]
    pub fn push(&mut self, renderer: &TerminalRenderer, delta: &str) -> Option<String> {
        self.pending.push_str(delta);
        let split = find_stream_safe_boundary(&self.pending)?;
        let ready = self.pending[..split].to_string();
        self.pending.drain(..split);
        // 保留尾部换行：ready 以空行结尾，段落/表格与后续内容的换行边界
        // 必须原样保留，否则跨 flush 的内容（如下一个表格）会粘连错位。
        Some(
            renderer
                .render_markdown_with_width_trim(&ready, self.max_width, false),
        )
    }

    #[must_use]
    pub fn flush(&mut self, renderer: &TerminalRenderer) -> Option<String> {
        if self.pending.trim().is_empty() {
            self.pending.clear();
            None
        } else {
            let pending = std::mem::take(&mut self.pending);
            Some(renderer.markdown_to_ansi_with_width(&pending, self.max_width))
        }
    }
}

fn apply_code_block_background(line: &str) -> String {
    let trimmed = line.trim_end_matches('\n');
    let trailing_newline = if trimmed.len() == line.len() {
        ""
    } else {
        "\n"
    };
    let with_background = trimmed.replace("\u{1b}[0m", "\u{1b}[0;48;5;236m");
    format!("\u{1b}[48;5;236m{with_background}\u{1b}[0m{trailing_newline}")
}

/// Pre-process raw markdown so that fenced code blocks whose body contains
/// fence markers of equal or greater length are wrapped with a longer fence.
///
/// LLMs frequently emit triple-backtick code blocks that contain triple-backtick
/// examples.  `CommonMark` (and pulldown-cmark) treats the inner marker as the
/// closing fence, breaking the render.  This function detects the situation and
/// upgrades the outer fence to use enough backticks (or tildes) that the inner
/// markers become ordinary content.
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::manual_repeat_n,
    clippy::manual_str_repeat
)]
fn normalize_nested_fences(markdown: &str) -> String {
    // A fence line is either "labeled" (has an info string ⇒ always an opener)
    // or "bare" (no info string ⇒ could be opener or closer).
    #[derive(Debug, Clone)]
    struct FenceLine {
        char: char,
        len: usize,
        has_info: bool,
        indent: usize,
    }

    fn parse_fence_line(line: &str) -> Option<FenceLine> {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        let indent = trimmed.chars().take_while(|c| *c == ' ').count();
        if indent > 3 {
            return None;
        }
        let rest = &trimmed[indent..];
        let ch = rest.chars().next()?;
        if ch != '`' && ch != '~' {
            return None;
        }
        let len = rest.chars().take_while(|c| *c == ch).count();
        if len < 3 {
            return None;
        }
        let after = &rest[len..];
        if ch == '`' && after.contains('`') {
            return None;
        }
        let has_info = !after.trim().is_empty();
        Some(FenceLine {
            char: ch,
            len,
            has_info,
            indent,
        })
    }

    let lines: Vec<&str> = markdown.split_inclusive('\n').collect();
    // Handle final line that may lack trailing newline.
    // split_inclusive already keeps the original chunks, including a
    // final chunk without '\n' if the input doesn't end with one.

    // First pass: classify every line.
    let fence_info: Vec<Option<FenceLine>> = lines.iter().map(|l| parse_fence_line(l)).collect();

    // Second pass: pair openers with closers using a stack, recording
    // (opener_idx, closer_idx) pairs plus the max fence length found between
    // them.
    struct StackEntry {
        line_idx: usize,
        fence: FenceLine,
    }

    let mut stack: Vec<StackEntry> = Vec::new();
    // Paired blocks: (opener_line, closer_line, max_inner_fence_len)
    let mut pairs: Vec<(usize, usize, usize)> = Vec::new();

    for (i, fi) in fence_info.iter().enumerate() {
        let Some(fl) = fi else { continue };

        if fl.has_info {
            // Labeled fence ⇒ always an opener.
            stack.push(StackEntry {
                line_idx: i,
                fence: fl.clone(),
            });
        } else {
            // Bare fence ⇒ try to close the top of the stack if compatible.
            let closes_top = stack
                .last()
                .is_some_and(|top| top.fence.char == fl.char && fl.len >= top.fence.len);
            if closes_top {
                let opener = stack
                    .pop()
                    .expect("stack non-empty after last().is_some_and check");
                // Find max fence length of any fence line strictly between
                // opener and closer (these are the nested fences).
                let inner_max = fence_info[opener.line_idx + 1..i]
                    .iter()
                    .filter_map(|fi| fi.as_ref().map(|f| f.len))
                    .max()
                    .unwrap_or(0);
                pairs.push((opener.line_idx, i, inner_max));
            } else {
                // Treat as opener.
                stack.push(StackEntry {
                    line_idx: i,
                    fence: fl.clone(),
                });
            }
        }
    }

    // Determine which lines need rewriting.  A pair needs rewriting when
    // its opener length <= max inner fence length.
    struct Rewrite {
        char: char,
        new_len: usize,
        indent: usize,
    }
    let mut rewrites: std::collections::HashMap<usize, Rewrite> = std::collections::HashMap::new();

    for (opener_idx, closer_idx, inner_max) in &pairs {
        let opener_fl = fence_info[*opener_idx]
            .as_ref()
            .expect("opener fence must be in fence_info");
        if opener_fl.len <= *inner_max {
            let new_len = inner_max + 1;
            let info_part = {
                let trimmed = lines[*opener_idx]
                    .trim_end_matches('\n')
                    .trim_end_matches('\r');
                let rest = &trimmed[opener_fl.indent..];
                rest[opener_fl.len..].to_string()
            };
            rewrites.insert(
                *opener_idx,
                Rewrite {
                    char: opener_fl.char,
                    new_len,
                    indent: opener_fl.indent,
                },
            );
            let closer_fl = fence_info[*closer_idx]
                .as_ref()
                .expect("closer fence must be in fence_info");
            rewrites.insert(
                *closer_idx,
                Rewrite {
                    char: closer_fl.char,
                    new_len,
                    indent: closer_fl.indent,
                },
            );
            // Store info string only in the opener; closer keeps the trailing
            // portion which is already handled through the original line.
            // Actually, we rebuild both lines from scratch below, including
            // the info string for the opener.
            let _ = info_part; // consumed in rebuild
        }
    }

    if rewrites.is_empty() {
        return markdown.to_string();
    }

    // Rebuild.
    let mut out = String::with_capacity(markdown.len() + rewrites.len() * 4);
    for (i, line) in lines.iter().enumerate() {
        if let Some(rw) = rewrites.get(&i) {
            let fence_str: String = std::iter::repeat(rw.char).take(rw.new_len).collect();
            let indent_str: String = std::iter::repeat(' ').take(rw.indent).collect();
            // Recover the original info string (if any) and trailing newline.
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            let fi = fence_info[i]
                .as_ref()
                .expect("rewrite entry must have fence_info");
            let info = &trimmed[fi.indent + fi.len..];
            let trailing = &line[trimmed.len()..];
            out.push_str(&indent_str);
            out.push_str(&fence_str);
            out.push_str(info);
            out.push_str(trailing);
        } else {
            out.push_str(line);
        }
    }
    out
}

fn find_stream_safe_boundary(markdown: &str) -> Option<usize> {
    let mut open_fence: Option<FenceMarker> = None;
    let mut last_boundary = None;

    for (offset, line) in markdown.split_inclusive('\n').scan(0usize, |cursor, line| {
        let start = *cursor;
        *cursor += line.len();
        Some((start, line))
    }) {
        let line_without_newline = line.trim_end_matches('\n');
        if let Some(opener) = open_fence {
            if line_closes_fence(line_without_newline, opener) {
                open_fence = None;
                last_boundary = Some(offset + line.len());
            }
            continue;
        }

        if let Some(opener) = parse_fence_opener(line_without_newline) {
            open_fence = Some(opener);
            continue;
        }

        if line_without_newline.trim().is_empty() {
            last_boundary = Some(offset + line.len());
        }
    }

    last_boundary
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FenceMarker {
    character: char,
    length: usize,
}

fn parse_fence_opener(line: &str) -> Option<FenceMarker> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let character = rest.chars().next()?;
    if character != '`' && character != '~' {
        return None;
    }
    let length = rest.chars().take_while(|c| *c == character).count();
    if length < 3 {
        return None;
    }
    let info_string = &rest[length..];
    if character == '`' && info_string.contains('`') {
        return None;
    }
    Some(FenceMarker { character, length })
}

fn line_closes_fence(line: &str, opener: FenceMarker) -> bool {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let length = rest.chars().take_while(|c| *c == opener.character).count();
    if length < opener.length {
        return false;
    }
    rest[length..].chars().all(|c| c == ' ' || c == '\t')
}

fn visible_width(input: &str) -> usize {
    // BUG fix: 之前用 `chars().count()` 按 Unicode code point 计数，
    // 导致 CJK / emoji 等宽字符在表格列对齐时 padding 计算不足，边框错位。
    // 改用 unicode-width 按实际显示列宽计算，与 TUI 其它路径
    // (input_line.rs / status_bar.rs / output_view.rs / app.rs wrap) 保持一致。
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(strip_ansi(input).as_str())
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            output.push(ch);
        }
    }

    output
}

/// 表格渲染默认最大总宽度（终端尺寸查询失败时的兜底）。
const DEFAULT_TABLE_MAX_WIDTH: usize = 100;
/// 列宽收缩时单列最小显示宽度。
const MIN_TABLE_COLUMN_WIDTH: usize = 8;

/// 解析表格渲染目标宽度：显式指定 > 0 优先，否则查终端宽度，最后兜底默认值。
fn resolve_table_max_width(requested: Option<usize>) -> usize {
    if let Some(width) = requested {
        if width > 0 {
            return width;
        }
    }
    crossterm::terminal::size()
        .ok()
        .and_then(|(width, _)| (width > 0).then_some(width as usize))
        .unwrap_or(DEFAULT_TABLE_MAX_WIDTH)
}

/// ANSI 字符串解析单元：字符 + 从干净状态渲染它所需的前缀 + 该字符是否带样式。
struct AnsiUnit {
    /// 渲染该字符前必须输出的转义前缀（`\x1b[0m` + 激活的 SGR 序列）。
    prefix: String,
    ch: char,
    /// 该字符是否处于非默认样式（用于行尾是否需要补 reset）。
    styled: bool,
}

/// 解析 ANSI SGR 字符串为 (前缀, 字符) 单元序列。
///
/// 前缀规则（保证任意断点处新行从干净状态正确渲染）：
/// - 带样式字符：`\x1b[0m` + 当前激活的全部 SGR 序列（自愈式重建）；
/// - 紧跟在带样式字符后的无样式字符：`\x1b[0m`（清除泄漏的样式）；
/// - 其余：空字符串。
///
/// 非 SGR 转义（OSC 等）作为零宽透传，追加到后续字符前缀。
fn parse_ansi_units(input: &str) -> Vec<AnsiUnit> {
    let mut units: Vec<AnsiUnit> = Vec::new();
    let mut active: Vec<String> = Vec::new();
    let mut prev_styled = false;
    let mut pending_escape = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            let mut seq = String::from("\u{1b}");
            if chars.peek() == Some(&'[') {
                chars.next();
                seq.push('[');
                for next in chars.by_ref() {
                    seq.push(next);
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                if seq.ends_with('m') {
                    let params = &seq[2..seq.len() - 1];
                    if params == "0" {
                        active.clear();
                    } else {
                        active.push(seq.clone());
                    }
                } else {
                    // 非 SGR（如 cursor 移动）:按零宽透传
                    pending_escape.push_str(&seq);
                    continue;
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                seq.push(']');
                for next in chars.by_ref() {
                    seq.push(next);
                    if next == '\u{07}' {
                        break;
                    }
                }
                pending_escape.push_str(&seq);
                continue;
            } else if let Some(&next) = chars.peek() {
                seq.push(next);
                chars.next();
                pending_escape.push_str(&seq);
                continue;
            }
            // SGR 更新 active 后不产出字符单元
            continue;
        }
        let styled = !active.is_empty();
        let prefix = if styled {
            format!("\u{1b}[0m{}", active.concat())
        } else if prev_styled {
            String::from("\u{1b}[0m")
        } else {
            String::new()
        };
        let prefix = std::mem::take(&mut pending_escape) + &prefix;
        pending_escape.clear();
        units.push(AnsiUnit { prefix, ch, styled });
        prev_styled = styled;
    }
    units
}

/// 单元格文本（可能含 ANSI 样式与 `\n`）按指定显示宽度折行，返回样式保留的显示行。
///
/// - `\n` 强制分段；空格处优先断行；单个 token 超过列宽才硬拆；
/// - CJK/emoji 按显示宽度计数；
/// - `width == 0` 时仅按 `\n` 分段，不折行。
fn wrap_cell_lines(cell: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return cell.split('\n').map(str::to_string).collect();
    }
    let units = parse_ansi_units(cell);
    let mut segments: Vec<Vec<AnsiUnit>> = vec![Vec::new()];
    for unit in units {
        if unit.ch == '\n' {
            segments.push(Vec::new());
        } else if let Some(last) = segments.last_mut() {
            last.push(unit);
        }
    }
    let mut lines = Vec::new();
    for segment in &segments {
        wrap_segment_lines(segment, width, &mut lines);
    }
    lines
}

fn char_display_width(ch: char) -> usize {
    unicode_width::UnicodeWidthStr::width(ch.to_string().as_str())
}

/// 把当前行 flush 到输出，行尾若残留样式则补 `\x1b[0m`（保证续行从干净状态开始）。
fn flush_ansi_line(
    current: &mut String,
    current_width: &mut usize,
    last_styled: &mut bool,
    out: &mut Vec<String>,
) {
    if *last_styled {
        current.push_str("\u{1b}[0m");
    }
    if !current.is_empty() {
        out.push(std::mem::take(current));
    }
    *current_width = 0;
    *last_styled = false;
}

fn emit_ansi_units(
    units: &[&AnsiUnit],
    target: &mut String,
    width: &mut usize,
    last_styled: &mut bool,
) {
    for unit in units {
        if !unit.prefix.is_empty() {
            target.push_str(&unit.prefix);
        }
        target.push(unit.ch);
        *width += char_display_width(unit.ch);
        *last_styled = unit.styled;
    }
}

/// 词边界折行一个文本段（已按 `\n` 分段）。
fn wrap_segment_lines(segment: &[AnsiUnit], width: usize, out: &mut Vec<String>) {
    if segment.is_empty() {
        out.push(String::new());
        return;
    }
    // tokenize:word(非空白) / ws(空白) 交替
    struct Token<'a> {
        units: Vec<&'a AnsiUnit>,
        is_ws: bool,
    }
    let mut tokens: Vec<Token> = Vec::new();
    for unit in segment {
        let is_ws = unit.ch.is_whitespace();
        if let Some(last) = tokens.last_mut() {
            if last.is_ws == is_ws {
                last.units.push(unit);
                continue;
            }
        }
        tokens.push(Token {
            units: vec![unit],
            is_ws,
        });
    }

    let mut current = String::new();
    let mut current_width = 0usize;
    let mut last_styled = false;
    let mut pending_ws: Vec<&AnsiUnit> = Vec::new();

    for token in &tokens {
        if token.is_ws {
            pending_ws.extend(token.units.iter().copied());
            continue;
        }
        let word_width: usize = token.units.iter().map(|u| char_display_width(u.ch)).sum();
        let ws_width: usize = pending_ws.iter().map(|u| char_display_width(u.ch)).sum();
        if current_width + ws_width + word_width <= width {
            emit_ansi_units(
                &pending_ws,
                &mut current,
                &mut current_width,
                &mut last_styled,
            );
            pending_ws.clear();
            emit_ansi_units(
                &token.units,
                &mut current,
                &mut current_width,
                &mut last_styled,
            );
        } else if word_width <= width {
            flush_ansi_line(&mut current, &mut current_width, &mut last_styled, out);
            pending_ws.clear();
            emit_ansi_units(
                &token.units,
                &mut current,
                &mut current_width,
                &mut last_styled,
            );
        } else {
            // 单词本身超宽:flush 当前行后硬拆
            flush_ansi_line(&mut current, &mut current_width, &mut last_styled, out);
            pending_ws.clear();
            for unit in &token.units {
                let w = char_display_width(unit.ch);
                if w == 0 {
                    if !unit.prefix.is_empty() {
                        current.push_str(&unit.prefix);
                    }
                    current.push(unit.ch);
                    last_styled = unit.styled;
                    continue;
                }
                if current_width + w > width && current_width > 0 {
                    flush_ansi_line(&mut current, &mut current_width, &mut last_styled, out);
                }
                if !unit.prefix.is_empty() {
                    current.push_str(&unit.prefix);
                }
                current.push(unit.ch);
                current_width += w;
                last_styled = unit.styled;
            }
        }
    }
    flush_ansi_line(&mut current, &mut current_width, &mut last_styled, out);
}
///
/// 通过 `/output-style [style]` 斜杠命令切换，或由 `CliToolExecutor` 在
/// `format_tool_result` / `show_tool_results` / `show_tool_errors` 路径上消费。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputVerbosity {
    /// 完整输出：工具调用、结果、错误全部打印（带截断常量）。
    #[default]
    Full,
    /// 紧凑输出：工具结果折叠为一行成功标记，仅错误打印详情。
    Compact,
    /// 静默输出：不打印任何工具结果与错误，仅保留流式助手文本。
    Silent,
    /// 最小输出：仅打印关键工具（read/write/edit/bash）的一行摘要。
    Minimal,
}

impl OutputVerbosity {
    /// 从字符串参数解析冗度级别。接受 `full`/`compact`/`silent`/`minimal`，
    /// 大小写不敏感；其他输入返回 `None` 由调用方回退到当前级别或打印帮助。
    pub fn from_style_arg(arg: &str) -> Option<Self> {
        match arg.trim().to_ascii_lowercase().as_str() {
            "full" => Some(OutputVerbosity::Full),
            "compact" => Some(OutputVerbosity::Compact),
            "silent" => Some(OutputVerbosity::Silent),
            "minimal" => Some(OutputVerbosity::Minimal),
            _ => None,
        }
    }

    /// 返回人类可读的级别标签，用于 `/output-style` 命令回显。
    pub fn label(&self) -> &'static str {
        match self {
            OutputVerbosity::Full => "full",
            OutputVerbosity::Compact => "compact",
            OutputVerbosity::Silent => "silent",
            OutputVerbosity::Minimal => "minimal",
        }
    }

    /// 是否打印工具成功结果的完整 Markdown 渲染。
    /// `Full` 放行；`Compact`/`Minimal` 走折叠分支；`Silent` 全部抑制。
    pub fn show_tool_results(&self) -> bool {
        matches!(self, OutputVerbosity::Full)
    }

    /// 是否打印工具错误结果，或紧凑模式下的成功标记。
    /// `Silent` 完全静默；其他级别都允许错误输出。
    pub fn show_tool_errors(&self) -> bool {
        !matches!(self, OutputVerbosity::Silent)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        strip_ansi, visible_width, wrap_cell_lines, MarkdownStreamState, Spinner, TerminalRenderer,
    };

    #[test]
    fn renders_markdown_with_styling_and_lists() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output = terminal_renderer
            .render_markdown("# Heading\n\nThis is **bold** and *italic*.\n\n- item\n\n`code`");

        assert!(markdown_output.contains("Heading"));
        assert!(markdown_output.contains("• item"));
        assert!(markdown_output.contains("code"));
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn renders_links_as_colored_markdown_labels() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.render_markdown("See [Claw](https://example.com/docs) now.");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("[Claw](https://example.com/docs)"));
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn highlights_fenced_code_blocks() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.markdown_to_ansi("```rust\nfn hi() { println!(\"hi\"); }\n```");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("╭─ rust"));
        assert!(plain_text.contains("fn hi"));
        assert!(markdown_output.contains('\u{1b}'));
        assert!(markdown_output.contains("[48;5;236m"));
    }

    #[test]
    fn renders_ordered_and_nested_lists() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.render_markdown("1. first\n2. second\n   - nested\n   - child");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("1. first"));
        assert!(plain_text.contains("2. second"));
        assert!(plain_text.contains("  • nested"));
        assert!(plain_text.contains("  • child"));
    }

    #[test]
    fn renders_tables_with_alignment() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output = terminal_renderer
            .render_markdown("| Name | Value |\n| ---- | ----- |\n| alpha | 1 |\n| beta | 22 |");
        let plain_text = strip_ansi(&markdown_output);
        let lines = plain_text.lines().collect::<Vec<_>>();

        assert_eq!(lines[0], "│ Name  │ Value │");
        assert_eq!(lines[1], "│───────┼───────│");
        assert_eq!(lines[2], "│ alpha │ 1     │");
        assert_eq!(lines[3], "│ beta  │ 22    │");
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn streaming_state_waits_for_complete_blocks() {
        let renderer = TerminalRenderer::new();
        let mut state = MarkdownStreamState::default();

        assert_eq!(state.push(&renderer, "# Heading"), None);
        let flushed = state
            .push(&renderer, "\n\nParagraph\n\n")
            .expect("completed block");
        let plain_text = strip_ansi(&flushed);
        assert!(plain_text.contains("Heading"));
        assert!(plain_text.contains("Paragraph"));

        assert_eq!(state.push(&renderer, "```rust\nfn main() {}\n"), None);
        let code = state
            .push(&renderer, "```\n")
            .expect("closed code fence flushes");
        assert!(strip_ansi(&code).contains("fn main()"));
    }

    #[test]
    fn streaming_state_holds_outer_fence_with_nested_inner_fence() {
        let renderer = TerminalRenderer::new();
        let mut state = MarkdownStreamState::default();

        assert_eq!(
            state.push(&renderer, "````markdown\n```rust\nfn inner() {}\n"),
            None,
            "inner triple backticks must not close the outer four-backtick fence"
        );
        assert_eq!(
            state.push(&renderer, "```\n"),
            None,
            "closing the inner fence must not flush the outer fence"
        );
        let flushed = state
            .push(&renderer, "````\n")
            .expect("closing the outer four-backtick fence flushes the buffered block");
        let plain_text = strip_ansi(&flushed);
        assert!(plain_text.contains("fn inner()"));
        assert!(plain_text.contains("```rust"));
    }

    #[test]
    fn streaming_state_distinguishes_backtick_and_tilde_fences() {
        let renderer = TerminalRenderer::new();
        let mut state = MarkdownStreamState::default();

        assert_eq!(state.push(&renderer, "~~~text\n"), None);
        assert_eq!(
            state.push(&renderer, "```\nstill inside tilde fence\n"),
            None,
            "a backtick fence cannot close a tilde-opened fence"
        );
        assert_eq!(state.push(&renderer, "```\n"), None);
        let flushed = state
            .push(&renderer, "~~~\n")
            .expect("matching tilde marker closes the fence");
        let plain_text = strip_ansi(&flushed);
        assert!(plain_text.contains("still inside tilde fence"));
    }

    #[test]
    fn renders_nested_fenced_code_block_preserves_inner_markers() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.markdown_to_ansi("````markdown\n```rust\nfn nested() {}\n```\n````");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("╭─ markdown"));
        assert!(plain_text.contains("```rust"));
        assert!(plain_text.contains("fn nested()"));
    }

    #[test]
    fn spinner_advances_frames() {
        let terminal_renderer = TerminalRenderer::new();
        let mut spinner = Spinner::new();
        let mut out = Vec::new();
        spinner
            .tick("Working", terminal_renderer.color_theme(), &mut out)
            .expect("tick succeeds");
        spinner
            .tick("Working", terminal_renderer.color_theme(), &mut out)
            .expect("tick succeeds");

        let output = String::from_utf8_lossy(&out);
        assert!(output.contains("Working"));
    }
}

#[test]
fn wraps_cell_with_ansi_styles_at_word_boundaries() {
    // 带 \x1b[1m 粗体样式的单元格,宽度 6 词边界折行
    let styled = "\u{1b}[1mhello world\u{1b}[0m";
    let lines = wrap_cell_lines(styled, 6);
    assert_eq!(lines.len(), 2, "应折成 2 行");
    assert_eq!(strip_ansi(&lines[0]), "hello");
    assert_eq!(strip_ansi(&lines[1]), "world");
    // 样式保留:每行都应包含粗体序列
    assert!(lines[0].contains("\u{1b}[1m"));
    assert!(lines[1].contains("\u{1b}[1m"));
    // 每行可见宽度 ≤ 6
    for line in &lines {
        assert!(visible_width(line) <= 6, "行超宽: {line:?}");
    }
}

#[test]
fn wraps_cell_splits_overwide_single_token() {
    // 无空格超宽 token:硬拆为 5/5/2
    let lines = wrap_cell_lines("ABCDEFGHIJKL", 5);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["ABCDE", "FGHIJ", "KL"]);
}

#[test]
fn wraps_cell_breaks_on_newlines() {
    // 换行强制分段
    let lines = wrap_cell_lines("aaa\nbbb ccc", 6);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["aaa", "bbb", "ccc"]);
}

#[test]
fn wraps_cell_handles_plain_text_and_cjk() {
    let lines = wrap_cell_lines("这是很长的一段中文文本", 8);
    for line in &lines {
        assert!(visible_width(line) <= 8, "CJK 行超宽: {line:?}");
    }
    assert!(lines.len() >= 2, "8 列宽应容纳不下整句");
    let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(joined, "这是很长的一段中文文本", "折行不应丢字符");
}

#[test]
fn renders_tables_wrap_long_cells_and_stay_aligned() {
    let terminal_renderer = TerminalRenderer::new();
    let md = "\
| 工具 | 说明 |
| ---- | ---- |
| read_file | 读取 https://raw.githubusercontent.com/example/very/long/path/to/a/documentation/file.md |
| write_file | 写入文件 |
";
    // 目标宽度 40:列收缩 + 单元格折行
    let markdown_output = terminal_renderer.markdown_to_ansi_with_width(md, Some(40));
    let plain_text = strip_ansi(&markdown_output);
    let lines = plain_text.lines().collect::<Vec<_>>();
    assert!(lines.len() >= 4, "长表格应折成多行: {}", lines.len());
    let widths: std::collections::BTreeSet<usize> =
        lines.iter().map(|l| visible_width(l)).collect();
    assert_eq!(widths.len(), 1, "所有表格行宽度应一致(对齐): {widths:?}");
    for line in &lines {
        assert!(line.starts_with('│'), "应以边框开头: {line:?}");
        assert!(line.ends_with('│'), "应以边框结尾: {line:?}");
        assert!(visible_width(line) <= 40, "不应超过目标宽度: {line:?}");
    }
}

#[test]
fn renders_tables_shrink_columns_proportionally() {
    let terminal_renderer = TerminalRenderer::new();
    let long = "x".repeat(50);
    let md = format!("| a | b |\n| - | - |\n| {long} | 1 |");
    let markdown_output = terminal_renderer.markdown_to_ansi_with_width(&md, Some(40));
    let plain_text = strip_ansi(&markdown_output);
    for line in plain_text.lines() {
        assert!(visible_width(line) <= 40, "收缩后仍超宽: {line:?}");
        assert!(line.starts_with('│') && line.ends_with('│'));
    }
}

#[test]
fn renders_tables_cjk_cells_stay_aligned() {
    let terminal_renderer = TerminalRenderer::new();
    let md = "| 名称 | 数量 |\n| ---- | ---- |\n| 苹果 | 10 |\n| 香蕉香蕉香蕉 | 3 |";
    let markdown_output = terminal_renderer.markdown_to_ansi_with_width(md, Some(24));
    let plain_text = strip_ansi(&markdown_output);
    let widths: std::collections::BTreeSet<usize> = plain_text.lines().map(visible_width).collect();
    assert_eq!(widths.len(), 1, "CJK 表格行应对齐: {widths:?}");
}

#[test]
fn table_after_paragraph_across_flush_keeps_header_aligned() {
    let renderer = TerminalRenderer::new();
    // 用户报告的真实场景：段落（含 →、inline code）后空行、表格紧随其后，
    // 流式分块使段落与表格分别在不同 push 中 flush。
    let para = "02:34，而源码修复在 8/9 23:29。`web/static/dist/` 被 gitignore，git 部署不会更新产物 → 浏览器永远加载旧代码。已执行的修复：";
    let mut state = MarkdownStreamState::with_max_width(Some(60));
    let mut chunks = Vec::new();
    for delta in [
        // 段落 + 空行：单独 flush，输出必须以换行结尾，不能与后续表格粘连
        format!("{para}\n\n"),
        "| 步骤 | 结果 |\n".to_string(),
        "| --- | --- |\n".to_string(),
        "| 01 | 已执行 |\n".to_string(),
        "| 02 | 已执行 |\n".to_string(),
        "| 03 | 已执行 |\n\n".to_string(), // 表格结束空行触发整表 flush
        "后续文本".to_string(),
    ] {
        if let Some(rendered) = state.push(&renderer, &delta) {
            chunks.push(rendered);
        }
    }
    if let Some(rendered) = state.flush(&renderer) {
        chunks.push(rendered);
    }
    let plain = strip_ansi(&chunks.concat());
    println!("=== stream output ===\n{plain}");
    let lines: Vec<&str> = plain.lines().collect();
    assert!(!lines.is_empty(), "应有渲染输出");
    // 1) 表格表头行必须以 │ 开头且独占一行（不与段落文本粘连）
    let header_idx = lines.iter().position(|l| l.trim_start().starts_with('│'));
    assert!(header_idx.is_some(), "应渲染出表格表头行");
    // 2) 表头行之前的段落行不得混入表格边框
    for l in &lines[..header_idx.unwrap()] {
        if !l.trim().is_empty() {
            assert!(!l.contains('│'), "段落行不应混入表格边框: {l:?}");
        }
    }
    // 3) 表格所有物理行宽度一致（对齐）
    let table_lines: Vec<&str> = lines
        .iter()
        .filter(|l| l.trim_start().starts_with('│'))
        .copied()
        .collect();
    let widths: std::collections::BTreeSet<usize> =
        table_lines.iter().map(|l| visible_width(l)).collect();
    assert_eq!(widths.len(), 1, "表格行应对齐: {widths:?}");
    // 4) 表格与后续文本不粘连
    let last_table = table_lines.last().expect("有表格行");
    assert!(plain.contains(&format!("{last_table}\n")), "表格后应有换行");
}

#[test]
fn streaming_state_applies_max_width_to_tables() {
    let renderer = TerminalRenderer::new();
    let long = "x".repeat(50);
    let md = format!("| a | b |\n| - | - |\n| {long} | 1 |\n\n");
    let mut state = MarkdownStreamState::with_max_width(Some(40));
    let flushed = state.push(&renderer, &md).expect("blank line flushes");
    for line in strip_ansi(&flushed).lines() {
        assert!(visible_width(line) <= 40, "流式渲染未应用宽度: {line:?}");
    }
    // 无宽度（默认）时按终端宽度/100 解析，不应 panic
    let mut default_state = MarkdownStreamState::default();
    assert!(default_state.push(&renderer, &md).is_some());
}
