//! TuiApp — main ratatui event loop integrating with LiveCli.
//!
//! Owns the alternate-screen Terminal, InputLine, SlashMenu, OutputView,
//! and shared StatusBarState. Routes keyboard events to InputLine / Menu,
//! submits Enter to `LiveCli::run_turn` (capturing output via OutputView
//! sink + StatusEmitter callback for live status updates).

#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    unused_assignments,
    clippy::too_many_lines
)]

use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, StyledGrapheme, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
// Styled trait 提供 `line.styled_graphemes(style)` 方法，用于按 grapheme
// 迭代 Line 并保留样式信息（自己 wrap 时需要）。
use ratatui::style::Styled;

// Phase 3.2: TerminalRenderer is used to convert markdown → ANSI; ansi_to_tui
// then converts ANSI → ratatui Text<'static> so Paragraph can render styled
// spans (headings, code blocks, bold/italic, etc.) instead of raw text.
use crate::render::TerminalRenderer;
use ansi_to_tui::IntoText;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
// UnicodeWidthStr 用于按显示宽度计算 wrap 和光标定位（CJK 字符宽度为 2）。
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::app::LiveCli;
use crate::tui::input_line::{InputAction, InputLine};
use crate::tui::output_view::OutputView;
use crate::tui::sidebar::{render_sidebar, ToolHistory};
use crate::tui::slash_menu::{format_menu_item, SlashMenu};
use crate::tui::status_bar::{StatusBar, StatusBarState};
// 斜杠命令本地分发：TUI 下 /help 等命令应在本地处理，而非发给 AI。
// 修复"输入 /help 发送给 AI"的 bug。
use commands::SlashCommand;
// 多行粘贴兜底：当终端不支持 bracketed paste（如 conhost）或 Ctrl+V
// 被终端拦截逐行发送时，用 try_auto_expand_clipboard 检测剪贴板内容。
// 参考 CLI 路径 app.rs 的处理逻辑。
use crate::paste::{
    fold_pasted_input, paste_diag_log, try_auto_expand_clipboard, write_clipboard_to_temp_file,
};

/// Entry point: run the TUI REPL until user exits.
pub(crate) fn run_tui_repl(cli: LiveCli) -> Result<(), Box<dyn std::error::Error>> {
    // 静默 paste.rs 中的 [paste-dbg] eprintln 日志，避免污染 alternate screen。
    // 退出时恢复 false（用 drop guard 确保异常退出也恢复）。
    struct TuiSilentGuard;
    impl Drop for TuiSilentGuard {
        fn drop(&mut self) {
            crate::paste::set_tui_silent(false);
        }
    }
    let _silence_guard = TuiSilentGuard;
    crate::paste::set_tui_silent(true);

    // 注册 TUI 模式下的 AskUserQuestion handler。
    //
    // 修复 BUG：worker 线程内 `run_ask_user_question` 原本用 io::stdout/stdin
    // 阻塞式 I/O。但 TUI 模式下 stdout 处于 alternate screen（writeln 破坏渲染
    // 且用户看不到），stdin 处于 raw mode 被 crossterm event loop 拥有
    // （read_line 永远拿不到输入）。结果就是用户看到 AI 输出的 "Enter choice"
    // 提示但无法输入数字或选择选项。
    //
    // 解决方案：handler 通过 channel 把请求投递给主循环，主循环把问题显示到
    // OutputView 并设 pending_ask，下一次 Submit 时把 InputLine 内容作为答案
    // 通过 resp_tx 回传给 worker 线程，handler 返回，run_ask_user_question 继续。
    let (ask_tx, ask_rx) = mpsc::channel::<AskRequest>();
    let ask_tx_for_handler = ask_tx;
    tools::set_ask_user_question_handler(Some(Arc::new(
        move |req: tools::AskUserQuestionRequest| {
            let (resp_tx, resp_rx) = mpsc::channel::<String>();
            ask_tx_for_handler
                .send(AskRequest {
                    question: req.question,
                    options: req.options,
                    resp_tx,
                })
                .map_err(|_| "TUI 主循环已退出，AskUserQuestion 无法投递".to_string())?;
            resp_rx.recv().map_err(|_| "TUI 响应通道关闭".to_string())
        },
    )));

    struct AskHandlerGuard;
    impl Drop for AskHandlerGuard {
        fn drop(&mut self) {
            tools::set_ask_user_question_handler(None);
        }
    }
    let _ask_handler_guard = AskHandlerGuard;

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    // 启用鼠标捕获（左键点击切换工具卡片折叠状态）和 bracketed paste
    // mode（DECSET 2004：终端用 \x1b[200~ ... \x1b[201~ 包裹粘贴内容，
    // 整段作为一个 Event::Paste 事件投递，而不是逐字符触发 Event::Key，
    // 避免多行粘贴时 \n 被当作 Enter 立即提交）。
    // 参考 CLI 路径 input.rs 的 `.bracketed_paste(true)`，TUI 路径此前
    // 完全没有启用此模式，导致多行粘贴体验糟糕。
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )?;

    // Bug L10 修复：用 TerminalGuard Drop 确保终端状态恢复。
    // 旧实现用 closure + `?` 传播 Err，但 panic 会直接展开栈跳过 closure
    // 和 `result.is_err()` 块，导致 raw mode / alternate screen / mouse
    // capture / bracketed paste 残留，shell 不可用。
    // Drop guard 在任何退出路径（正常返回、Err、panic）都会执行。
    struct TerminalGuard;
    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture,
                crossterm::event::DisableBracketedPaste,
                crossterm::cursor::Show
            );
        }
    }
    let _terminal_guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = run_event_loop(&mut terminal, cli, ask_rx);
    // Drop guard 会恢复终端状态，这里直接返回结果。
    result
}

/// TUI 主循环内 AskUserQuestion 请求载荷。
///
/// 由 worker 线程通过 ask handler 投递，主循环消费后通过 `resp_tx`
/// 把用户输入的答案回传给 worker 线程。
struct AskRequest {
    question: String,
    options: Option<Vec<String>>,
    resp_tx: mpsc::Sender<String>,
}

/// 快速字符串 hash（无需新依赖，对 64KB 字符串 ~100ns）。
fn fast_hash(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(s, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

/// 增量 markdown 渲染器 — 解决流式输出性能瓶颈。
///
/// **核心思路**：markdown 段落以 `\n\n` 为安全边界。已完成的段落
/// （最后一个 `\n\n` 之前的部分）只渲染一次并缓存；未完成的段落
/// （最后一个 `\n\n` 之后的部分）每次重新渲染，但通常很短（< 1KB），
/// 渲染耗时 < 1ms。
///
/// **性能对比**（64KB buffer，每 50ms 一次 draw）：
/// - 旧方案：每次全量 pulldown-cmark + syntect 高亮，单次 ~20-100ms
/// - 新方案：增量路径 O(1) 命中缓存 / 未完成段落 < 1ms，单次 < 2ms
///
/// **正确性**：
/// - 段落边界推进时，新增段落渲染并 append 到 completed_lines
/// - pending 部分每次重新渲染（短），并做 hash 缓存避免短时间重复
/// - buffer 被 trim/清空时（snapshot.len() < completed_bytes 或
///   split_pos < completed_bytes）自动重置
/// - 整个 snapshot 未变时（hash 命中）直接返回缓存的 Text
struct IncrementalRenderer {
    renderer: TerminalRenderer,
    /// 已渲染完成的段落行（已处理到 `completed_bytes` 字节位置）。
    completed_lines: Vec<Line<'static>>,
    /// 已处理的字节位置（指向最后一个 `\n\n` 之后）。
    completed_bytes: usize,
    /// 未完成段落的 hash 缓存，避免短时间重复渲染。
    pending_cache: Option<(u64, Vec<Line<'static>>)>,
    /// 上次完整渲染的 hash + Text。整个 snapshot 未变时直接 clone 返回。
    full_cache: Option<(u64, Text<'static>)>,
}

impl IncrementalRenderer {
    fn new() -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            completed_lines: Vec::new(),
            completed_bytes: 0,
            pending_cache: None,
            full_cache: None,
        }
    }

    /// 渲染当前 snapshot 为 ratatui Text。
    fn render(&mut self, snapshot: &str) -> Text<'static> {
        // 空快照：重置并返回空 Text。
        if snapshot.is_empty() {
            self.reset();
            return Text::default();
        }

        // 快速路径：整个 snapshot hash 相同 → 返回完整缓存。
        // 流式等待 API 响应、用户滚动时 snapshot 不变，此路径命中。
        let total_hash = fast_hash(snapshot);
        if let Some((h, ref text)) = self.full_cache {
            if h == total_hash {
                return text.clone();
            }
        }

        // 找到最后一个段落分隔符 `\n\n` 的位置（+2 跳过 \n\n）。
        // 这是 markdown 的"安全边界"——之前的段落已完整，可以缓存。
        let split_pos = snapshot.rfind("\n\n").map(|p| p + 2).unwrap_or(0);

        // 检测 buffer 被裁剪/清空：
        // - snapshot.len() < completed_bytes：buffer 缩短（trim 触发）
        // - split_pos < completed_bytes：段落边界回退（buffer 头部被改）
        if snapshot.len() < self.completed_bytes || split_pos < self.completed_bytes {
            self.reset();
        }

        // 处理新增的完成段落：从 completed_bytes 到 split_pos。
        // 这部分文本已经完整（以 \n\n 结尾），渲染一次后永久缓存。
        if split_pos > self.completed_bytes {
            let new_completed = &snapshot[self.completed_bytes..split_pos];
            if !new_completed.is_empty() {
                let ansi = self.renderer.markdown_to_ansi(new_completed);
                match ansi.into_text() {
                    Ok(text) => self.completed_lines.extend(text.lines),
                    Err(_) => self
                        .completed_lines
                        .push(Line::raw(new_completed.to_string())),
                }
            }
            self.completed_bytes = split_pos;
            // 段落边界推进，pending 缓存失效。
            self.pending_cache = None;
        }

        // 限制 completed 行数（防止超长对话导致无限增长）。
        // 超过上限时从头部淘汰（旧对话历史）。
        const MAX_COMPLETED_LINES: usize = 2000;
        if self.completed_lines.len() > MAX_COMPLETED_LINES {
            let drain = self.completed_lines.len() - MAX_COMPLETED_LINES;
            self.completed_lines.drain(0..drain);
        }

        // 渲染未完成段落（split_pos 之后的部分，通常 < 1KB）。
        let pending = &snapshot[self.completed_bytes..];
        let pending_lines: Vec<Line<'static>> = if pending.is_empty() {
            Vec::new()
        } else {
            let pending_hash = fast_hash(pending);
            let need_render = match &self.pending_cache {
                Some((h, _)) => *h != pending_hash,
                None => true,
            };
            if need_render {
                let lines = self.render_to_lines(pending);
                self.pending_cache = Some((pending_hash, lines.clone()));
                lines
            } else {
                self.pending_cache.as_ref().unwrap().1.clone()
            }
        };

        // 合并 completed + pending。
        let mut all_lines = self.completed_lines.clone();
        all_lines.extend(pending_lines);
        let result = Text::from(all_lines);

        // 缓存完整结果（下次 snapshot 未变时直接 clone 返回）。
        self.full_cache = Some((total_hash, result.clone()));

        result
    }

    /// markdown 字符串 → ratatui Vec<Line>（失败时降级为纯文本）。
    fn render_to_lines(&self, text: &str) -> Vec<Line<'static>> {
        if text.is_empty() {
            return Vec::new();
        }
        let ansi = self.renderer.markdown_to_ansi(text);
        match ansi.into_text() {
            Ok(text) => text.lines,
            Err(_) => vec![Line::raw(text.to_string())],
        }
    }

    /// 重置所有缓存（buffer 被裁剪/清空时调用）。
    fn reset(&mut self) {
        self.completed_lines.clear();
        self.completed_bytes = 0;
        self.pending_cache = None;
        self.full_cache = None;
    }
}

/// 按字符宽度 wrap 一个 `Line` 到多个显示行（保留 span 样式边界）。
///
/// **背景**：ratatui 的 `Paragraph` 在 `.wrap(Wrap { trim: false })` 模式下
/// 用内部的 `WordWrapper` 按 word 边界折行（遇到空格才断），与简单的
/// `ceil(line_width / area_width)` 字符 wrap 不一致。这导致我们用字符 wrap
/// 估算的 `total_display_lines` 偏小，`scroll_y` 偏小，最后一行被裁掉。
///
/// 此函数自己按字符 wrap，确保与后续 `Paragraph`（不启用 `.wrap()`）的
/// 渲染 100% 一致。每个 grapheme 按其 `UnicodeWidthStr::width` 计算宽度，
/// 累加超过 `area_width` 时换行。样式信息通过 `StyledGrapheme` 保留，
/// 相邻同 style 的 grapheme 合并为一个 `Span`，减少 span 数量。
///
/// **边界情况**：
/// - `area_width == 0`：返回原始 line（无法 wrap）
/// - line 总宽度 <= area_width：返回原始 line（不需 wrap）
/// - 零宽字符（如组合字符）：不触发换行，追加到当前 span
/// - 单个字符宽度 > area_width：无法分割，独占一行（会超出 area_width，
///   Paragraph 会截断，但至少不会丢行）
fn wrap_line_to_display_lines(line: &Line<'static>, area_width: usize) -> Vec<Line<'static>> {
    if area_width == 0 {
        return vec![line.clone()];
    }
    // 用 styled_graphemes 迭代，保留每个 grapheme 的样式。
    // graphemes 借用 line 的内容，最终通过 to_string() 转 'static。
    let graphemes: Vec<StyledGrapheme<'_>> = line.styled_graphemes(Style::default()).collect();
    let total_width: usize = graphemes
        .iter()
        .map(|g| unicode_width::UnicodeWidthStr::width(g.symbol))
        .sum();
    if total_width <= area_width {
        return vec![line.clone()];
    }

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_span_str = String::new();
    let mut current_span_style = Style::default();
    let mut has_span = false;
    let mut current_width: usize = 0;

    // 把当前累积的 span 推入 current_spans
    macro_rules! flush_span {
        () => {
            if has_span && !current_span_str.is_empty() {
                current_spans.push(Span::styled(
                    std::mem::take(&mut current_span_str),
                    current_span_style,
                ));
                has_span = false;
            }
        };
    }
    // 把 current_spans 推入 result，开始新行
    macro_rules! flush_line {
        () => {
            flush_span!();
            if !current_spans.is_empty() {
                let new_line = Line {
                    spans: std::mem::take(&mut current_spans),
                    style: line.style,
                    alignment: line.alignment,
                };
                result.push(new_line);
            }
            current_width = 0;
        };
    }

    for g in &graphemes {
        let gw = unicode_width::UnicodeWidthStr::width(g.symbol);
        if gw == 0 {
            // 零宽字符：追加到当前 span（不触发换行）
            if has_span && current_span_style == g.style {
                current_span_str.push_str(g.symbol);
            } else {
                flush_span!();
                current_span_str = g.symbol.to_string();
                current_span_style = g.style;
                has_span = true;
            }
            continue;
        }
        // 超过 area_width 且当前行非空：换行
        if current_width + gw > area_width && current_width > 0 {
            flush_line!();
        }
        // 追加 grapheme 到当前 span（style 相同则合并，不同则新建 span）
        if has_span && current_span_style == g.style {
            current_span_str.push_str(g.symbol);
        } else {
            flush_span!();
            current_span_str = g.symbol.to_string();
            current_span_style = g.style;
            has_span = true;
        }
        current_width += gw;
    }
    // flush 最后一行
    flush_line!();

    if result.is_empty() {
        // 安全兜底：不应触发（total_width > area_width 保证至少一行）
        vec![line.clone()]
    } else {
        result
    }
}

/// 按字符宽度 wrap 纯文本字符串到多个显示行。
///
/// 与 ratatui 的 WordWrapper 不同，此函数按字符宽度严格折行，
/// 确保光标位置计算与渲染 100% 一致。"\n" 字符直接触发换行。
///
/// 边界情况：
/// - `width == 0`：返回原始文本（不 wrap）
/// - 零宽字符：不触发换行，追加到当前行
/// - 单个字符宽度 > width：独占一行（会超出 width）
fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut current_width: usize = 0;

    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut current_line));
            current_width = 0;
            continue;
        }
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if ch_width == 0 {
            current_line.push(ch);
            continue;
        }
        if current_width + ch_width > width && current_width > 0 {
            lines.push(std::mem::take(&mut current_line));
            current_width = 0;
        }
        current_line.push(ch);
        current_width += ch_width;
    }
    // Push the last line (empty line if text ends with \n)
    lines.push(current_line);

    lines
}

/// Result of a turn executed in a background thread.
struct TurnResult {
    cli: LiveCli,
    result: Result<(), String>,
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cli: LiveCli,
    ask_rx: mpsc::Receiver<AskRequest>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = InputLine::new();
    let mut menu = SlashMenu::new();
    let output_view = OutputView::new();
    let status_state = StatusBarState::shared();
    // Initialize status fields from cli state
    initialize_status(&status_state, &cli);

    let mut turn_start: Option<Instant> = None;
    // cli_holder: Some when idle, None when a turn is running in a thread
    let mut cli_holder: Option<LiveCli> = Some(cli);
    // Turn completion channel: Some when a turn is running
    let mut turn_rx: Option<mpsc::Receiver<TurnResult>> = None;

    // TUI 中断支持：当前 turn 的 abort signal handle（TUI 主线程保留的 clone）。
    // spawn worker thread 前创建，设置到 cli（通过 set_external_abort_signal），
    // Ctrl+C（busy 时）通过此 handle 取消当前 turn。
    let mut current_abort_signal: Option<runtime::HookAbortSignal> = None;

    // TUI 插话支持：busy 时用户输入的待处理队列。
    // turn 完成（含中断）后自动检查并提交，实现"任务间隙插话"。
    let mut pending_input: Option<String> = None;

    // P0-4 修复：标记 worker 线程已因 Disconnected 崩溃。
    // 一旦置 true，后续 Submit 不再静默丢弃输入，而是向 OutputView 反馈。
    let mut fatal_error: bool = false;

    // Sidebar: visible by default on wide terminals (>=100 cols), toggleable
    // via F2 / Ctrl+B. Holds a shared tool-history mirror so the sidebar
    // can show live tool-call progress during a streaming turn.
    let mut sidebar_visible: bool = terminal.size().map(|s| s.width >= 100).unwrap_or(false);
    let tool_history_shared: Arc<Mutex<ToolHistory>> = Arc::new(Mutex::new(Vec::new()));

    // 侧栏工具历史滚动状态：None=跟随底部（显示最新），Some(n)=从底部往上偏移 n 行。
    // Alt+Up 增加 n（看更早），Alt+Down 减少 n（回到最新），新工具到来时自动归零。
    let mut sidebar_tools_scroll: Option<usize> = None;

    // TUI 原生会话选择器状态：
    // - Some：/session pick 打开了选择器，键盘事件被拦截路由到选择器
    // - None：正常对话/命令模式
    let mut session_picker: Option<SessionPickerState> = None;

    // Output view scroll state. `None` means "follow bottom" (auto-scroll on
    // new output). `Some(n)` means "manual scroll n lines above the bottom";
    // new output does NOT auto-scroll while the user is in manual mode.
    // Any ScrollDown that brings n back to 0 re-enters follow mode.
    let mut scroll_offset: Option<usize> = None;

    // 性能优化：增量 markdown 渲染器。
    // 旧方案每次 draw 都对整个 64KB buffer 做全量 pulldown-cmark + syntect
    // 解析，长对话时单次渲染 20-100ms，严重卡顿。增量渲染器以 `\n\n` 为
    // 段落安全边界，已完成段落永久缓存，只有最后一个未完成段落重新渲染
    // （通常 < 1KB，< 1ms）。详见 `IncrementalRenderer` 文档注释。
    let mut incremental = IncrementalRenderer::new();

    // `?` toggles a centered keybindings overlay. While visible, most other
    // keybindings are intercepted so the overlay behaves like a modal.
    let mut help_visible: bool = false;

    // 多行粘贴兜底所需 state：
    // - paste_id_gen：本会话自增的 paste id（用于 paste-cache 文件名）
    // - pending_paste_lines：conhost 逐行发送时待丢弃的行（TUI 路径用不到，
    //   但 try_auto_expand_clipboard 签名需要）
    // - pending_paste_last_line：conhost 粘贴最后一行（不带 \n）的残留内容，
    //   用于清理 InputLine buffer。详见 main loop 中的清理逻辑。
    // - conhost_paste_intercepted：conhost 多行粘贴方案 C 标志，true 表示
    //   已写文件，待 conhost 注入完所有行后填充 @路径到 buffer。
    // - pending_at_path：方案 C 待填充的 @路径。方案 C 触发时不立即
    //   insert_paste（避免 conhost 后续注入的字符与 @路径拼接），
    //   而是保存到这个变量，等 pending_paste_lines 为空（conhost 注入完毕）
    //   后再 insert_paste 到 buffer。
    let mut paste_id_gen: u32 = 0;
    let mut pending_paste_lines: Vec<String> = Vec::new();
    let mut pending_paste_last_line: Option<String> = None;
    let mut conhost_paste_intercepted: bool = false;
    let mut pending_at_path: Option<String> = None;
    let mut conhost_suppress_input: bool = false;

    // AskUserQuestion 协作状态：
    // - `Some` 表示 worker 线程正在等待用户回答（通过 AskUserQuestion 工具），
    //   InputLine 的下一次 Submit 会把内容作为答案通过 resp_tx 回传。
    // - `None` 表示没有待回答的问题，Submit 走正常 AI 对话流程。
    // 主循环每次 iteration 从 ask_rx try_recv 新请求，收到时把问题显示到
    // OutputView 并设 pending_ask = Some(...)。
    let mut pending_ask: Option<AskRequest> = None;

    // 鼠标点击支持：把 draw 闭包内的 main_area 和 scroll_y 缓存到 loop 外，
    // 这样 Event::Mouse 分支可以访问它们，把点击坐标映射到逻辑行号。
    // draw 闭包每次渲染后更新这两个值。
    let mut last_main_area: Rect = Rect::default();
    let mut last_scroll_y: u16 = 0;

    'main_loop: loop {
        // 处理 AskUserQuestion 请求：worker 线程通过 ask handler 投递的待回答问题。
        //
        // 只在 pending_ask 为空（没有正在等待回答的问题）时 try_recv 新请求。
        // 一次 turn 内最多有一个 pending ask（worker 线程同步阻塞等答案，
        // 不会并发投递多个问题）。
        if pending_ask.is_none() {
            if let Ok(req) = ask_rx.try_recv() {
                // 把问题渲染到 OutputView，让用户能看到 AI 在问什么。
                let mut prompt = String::from("\n[Question] ");
                prompt.push_str(&req.question);
                prompt.push('\n');
                if let Some(ref opts) = req.options {
                    for (i, opt) in opts.iter().enumerate() {
                        prompt.push_str(&format!("  {}. {}\n", i + 1, opt));
                    }
                    prompt.push_str(&format!(
                        "请输入选项编号 (1-{}) 或自定义文本后回车：\n",
                        opts.len()
                    ));
                } else {
                    prompt.push_str("请输入回答后回车：\n");
                }
                if let Ok(mut buf) = output_view.shared_handle().lock() {
                    buf.append(&prompt);
                }
                pending_ask = Some(req);
            }
        }

        // Check if a running turn has completed
        if let Some(ref rx) = turn_rx {
            match rx.try_recv() {
                Ok(turn_result) => {
                    let elapsed = turn_start.map(|s| s.elapsed().as_millis()).unwrap_or(0);
                    let status = if turn_result.result.is_ok() { "ok" } else { "err" };
                    crate::diag_log(&format!(
                        "[turn-end] result={status} elapsed_ms={elapsed} pending_input={}",
                        pending_input.is_some()
                    ));
                    if let Err(e) = &turn_result.result {
                        let handle = output_view.shared_handle();
                        if let Ok(mut buf) = handle.lock() {
                            // 区分用户中断和真实错误
                            if e.contains("turn interrupted by user") {
                                buf.append("\n[interrupt] 任务已取消。\n");
                            } else {
                                buf.append(&format!("\n[error] {e}\n"));
                            }
                        };
                    }
                    cli_holder = Some(turn_result.cli);
                    turn_rx = None;
                    turn_start = None;
                    current_abort_signal = None;
                    if let Some(ref cli) = cli_holder {
                        sync_status_from_cli(&status_state, cli);
                    }
                    // TUI 插话支持：turn 完成（含中断）后检查 pending_input。
                    // 如果有排队输入，自动提交（不等待用户再次按 Enter），
                    // 实现"任务间隙插话后无缝继续对话"。
                    if let Some(pending) = pending_input.take() {
                        // 回显排队消息到 OutputView（保持对话连贯性）。
                        {
                            if let Ok(mut buf) = output_view.shared_handle().lock() {
                                let current = buf.buffer();
                                if !current.is_empty() && !current.ends_with('\n') {
                                    buf.append("\n\n");
                                }
                                buf.push_entry(
                                    crate::tui::output_view::OutputEntry::text(format!(
                                        "> {pending}\n\n"
                                    )),
                                );
                            }
                        }

                        if let Some(mut cli) = cli_holder.take() {
                            crate::diag_log(&format!(
                                "[turn-start] auto_submit pending_input len={}",
                                pending.len()
                            ));
                            // 清空上一轮的工具历史，避免污染。
                            if let Ok(mut h) = tool_history_shared.lock() {
                                h.clear();
                            }

                            // 设置状态栏为 streaming（turn 开始）。
                            if let Ok(mut guard) = status_state.lock() {
                                guard.reset_turn();
                            }

                            let output_handle = output_view.shared_handle();
                            let status_handle = Arc::clone(&status_state);
                            let tool_history_handle = Arc::clone(&tool_history_shared);

                            turn_start = Some(Instant::now());

                            let (tx, rx) = mpsc::channel();
                            let abort_signal = runtime::HookAbortSignal::new();
                            cli.set_external_abort_signal(abort_signal.clone());
                            current_abort_signal = Some(abort_signal);
                            let status_handle_for_panic = Arc::clone(&status_handle);

                            std::thread::spawn(move || {
                                use std::panic::{catch_unwind, AssertUnwindSafe};
                                let mut cli = cli;
                                let cli_ref = &mut cli;
                                let result = catch_unwind(AssertUnwindSafe(move || {
                                    execute_turn(
                                        cli_ref,
                                        &pending,
                                        &output_handle,
                                        &status_handle,
                                        &tool_history_handle,
                                    )
                                }));
                                let turn_result = match result {
                                    Ok(r) => TurnResult { cli, result: r },
                                    Err(payload) => {
                                        let msg = payload
                                            .downcast_ref::<String>()
                                            .cloned()
                                            .or_else(|| {
                                                payload
                                                    .downcast_ref::<&str>()
                                                    .map(|s| s.to_string())
                                            })
                                            .unwrap_or_else(|| "<unknown panic>".to_string());
                                        if let Ok(mut guard) = status_handle_for_panic.lock() {
                                            if guard.streaming {
                                                guard.finish_turn();
                                            }
                                        }
                                        TurnResult {
                                            cli,
                                            result: Err(format!(
                                                "worker thread panicked: {msg}"
                                            )),
                                        }
                                    }
                                };
                                let _ = tx.send(turn_result);
                            });

                            turn_rx = Some(rx);
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Turn still running, continue rendering
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Thread panicked; cli is lost, reset streaming state.
                    //
                    // **P0-4 修复**：之前 Disconnected 分支只清理 rx/start/streaming，
                    // 没有恢复 cli_holder（cli 已随 panic 线程 Drop），也没有向用户反馈。
                    // 后续 Submit 检查 `cli_holder.is_some() && turn_rx.is_none()` 永远 false，
                    // Enter 键无任何反应，TUI 看似活着但无法对话。
                    // 现在向 OutputView 追加错误提示让用户知晓，并标记 fatal_error 让
                    // Submit 分支能给出反馈。
                    crate::diag_log("[turn-panic] worker thread disconnected (panic)");
                    turn_rx = None;
                    turn_start = None;
                    if let Ok(mut guard) = status_state.lock() {
                        if guard.streaming {
                            guard.finish_turn();
                        }
                    }
                    // 向 OutputView 追加致命错误提示，让用户知道需要重启 TUI。
                    if let Ok(mut buf) = output_view.shared_handle().lock() {
                        buf.append(
                            "\n[error] 对话线程已崩溃，无法继续对话。请退出并重启 TUI（Ctrl+C 或 Ctrl+D）。\n",
                        );
                    }
                    // 标记致命错误：Submit 分支据此给出反馈而非静默丢弃输入。
                    fatal_error = true;
                }
            }
        }

        // Render
        terminal.draw(|f| {
            // Top-level vertical layout: main row (output+input) + status bar.
            // 动态输入区高度：根据当前 buffer 的显示行数调整。
            // - 最少 3 行（1 border + 至少 2 内容行）
            // - 最多 8 行（避免输入区挤占输出区过多空间）
            // - 内容行数 = buffer 中所有行的显示行数（考虑 wrap）总和
            //   每行显示行数 = max(1, ceil(line_width / area_width))
            // 这样长输入或多行粘贴时输入区会自动扩展，不会出现"看不全"的问题。
            let input_area_width = f.area().width as usize;
            let input_content_lines: usize = {
                let buf_str = input.buffer();
                buf_str
                    .split('\n')
                    .enumerate()
                    .map(|(i, line)| {
                        let mut w = UnicodeWidthStr::width(line);
                        // 第 0 行有 "> " 前缀（2 列显示宽度）
                        if i == 0 {
                            w += 2;
                        }
                        if w == 0 || input_area_width == 0 {
                            1
                        } else {
                            w.div_ceil(input_area_width)
                        }
                    })
                    .sum()
            };
            // +1 for top border, +1 for safety margin（避免光标在最后一行被裁）
            let input_height = (input_content_lines + 2).clamp(3, 8) as u16;

            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),                // main row (output + optional sidebar)
                    Constraint::Length(input_height),   // input + popup area (动态)
                    Constraint::Length(1),              // status bar
                ])
                .split(f.area());

            // Within the main row, split horizontally into output + sidebar
            // when the sidebar is visible.
            let main_area = if sidebar_visible {
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Min(40),    // output
                        Constraint::Length(36),  // sidebar
                    ])
                    .split(outer[0]);
                // Render sidebar using the latest state + tool history.
                let state_snapshot = {
                    // Bug L9 修复：mutex 毒化时容错访问，避免 draw 闭包 panic。
                    // worker 线程持锁时 panic 会中毒 mutex，旧实现 expect 直接
                    // 让 draw 闭包 panic → TUI 崩溃无恢复。改为访问中毒数据。
                    let guard = status_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.clone()
                };
                let history_snapshot = tool_history_shared
                    .lock()
                    .map(|h| h.clone())
                    .unwrap_or_default();
                let sidebar_buf = f.buffer_mut();
                render_sidebar(cols[1], sidebar_buf, &state_snapshot, &history_snapshot, sidebar_tools_scroll);
                cols[0]
            } else {
                outer[0]
            };

            // Output area
            // 增量渲染：已完成段落永久缓存，只有最后一个未完成段落重新
            // 渲染。流式时单次 draw 从 20-100ms 降到 < 2ms。
            let output_text = output_view.snapshot();
            let output_rendered: Text<'static> = incremental.render(&output_text);

            // Bug fix（输出最后一行被输入框覆盖）：
            // 旧实现用 `Paragraph::new(text).scroll((y,0)).wrap(Wrap{...})`，
            // 自己用 `ceil(line_width / area_width)` 按字符 wrap 计算显示行数。
            // 但 ratatui 的 WordWrapper 按 word 边界折行（遇到空格才断），
            // 英文长段落中 word 放不下时换行留下行尾空白，实际显示行数比
            // 字符 wrap 更多 → 我们的 total_display_lines 偏小 → max_scroll
            // 偏小 → follow mode 下 scroll_y 不足以滚到真正底部，Paragraph
            // 顶部渲染把最后几显示行裁掉，看起来像"最后一行被输入框盖住"。
            //
            // 徹底修复：不再依赖 Paragraph 的 .wrap() + .scroll()，改为
            // 自己按字符 wrap（保留 span 样式边界）+ 自己裁剪要显示的行，
            // 传给 Paragraph 时不用 .wrap() 和 .scroll()。这样显示行数计算
            // 与实际渲染 100% 一致，scroll_y 永远准确。
            //
            // 性能：按字符 wrap 比按 word wrap 略快（不需查 word 边界），
            // 且只需遍历一次 graphemes。对 64KB buffer（~1000 行）单次 < 2ms。
            let visible_height = main_area.height.saturating_sub(2) as usize; // -1 border, -1 safety margin
            let content_width = main_area.width as usize; // Block 只有 TOP border，无左右 border
            let wrapped_lines: Vec<Line<'static>> = output_rendered
                .lines
                .iter()
                .flat_map(|line| wrap_line_to_display_lines(line, content_width))
                .collect();
            let total_display_lines = wrapped_lines.len();
            let max_scroll = total_display_lines.saturating_sub(visible_height);
            let scroll_y = match scroll_offset {
                None => max_scroll,
                Some(offset) => max_scroll.saturating_sub(offset),
            };
            let scroll_label = match scroll_offset {
                None => String::new(),
                Some(offset) => format!(" [scroll -{offset}]"),
            };
            // 裁剪要显示的行：从 scroll_y 开始取 visible_height 行。
            // scroll_y 可能等于 total_display_lines（空 buffer），此时 start == end，渲染空。
            let start = scroll_y.min(total_display_lines);
            let end = (start + visible_height).min(total_display_lines);
            let visible_lines: Vec<Line<'static>> = if start < end {
                wrapped_lines[start..end].to_vec()
            } else {
                Vec::new()
            };
            let output_paragraph = Paragraph::new(Text::from(visible_lines))
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .title(format!("输出{scroll_label}")),
                );
            // 不用 .scroll() 和 .wrap()：已自己 wrap + 裁剪。
            f.render_widget(output_paragraph, main_area);

            // Input area
            // Bug fix（输入换行后光标位置不正确）：
            // 旧实现用 Paragraph::new(input_line).wrap(Wrap{...}) 让 ratatui
            // 的 WordWrapper 按 word 边界折行，但光标定位用字符级 wrap 计算。
            // 两种折行策略不一致导致文本实际显示位置与光标计算位置不同：
            // 输入换行后光标跑到错误的行/列。
            //
            // 彻底修复（与输出区修复一致）：自己按字符 wrap + 裁剪显示行，
            // 传给 Paragraph 时不用 .wrap()。这样渲染与光标定位使用完全相同的
            // wrap 策略，光标永远准确。
            let input_text = format!("> {}", input.buffer());
            let input_width = outer[1].width as usize;
            let input_wrapped: Vec<String> = wrap_plain_text(&input_text, input_width);
            // 裁剪：只取可见高度内的行（输入区可能不够高）
            let input_content_height = outer[1].height.saturating_sub(1) as usize;
            let visible_input_lines: Vec<Line<'static>> = input_wrapped
                .iter()
                .take(input_content_height)
                .map(|s| Line::raw(s.clone()))
                .collect();
            let input_paragraph = Paragraph::new(Text::from(visible_input_lines))
                .block(Block::default().borders(Borders::TOP).title("输入"));
            f.render_widget(input_paragraph, outer[1]);

            // Cursor positioning：基于预折行结果计算，与渲染 100% 一致。
            //
            // 用 "> " + buffer 的完整文本中光标之前的子串做 wrap，
            // 行数 - 1 即光标所在显示行号，最后一行的显示宽度即 X。
            let prompt_prefix_len: usize = 2; // "> "
            let cursor_byte = prompt_prefix_len + input.cursor();
            let cursor_before = &input_text[..cursor_byte.min(input_text.len())];
            let cursor_wrapped = wrap_plain_text(cursor_before, input_width);
            let display_row = cursor_wrapped.len().saturating_sub(1);
            let cursor_x =
                UnicodeWidthStr::width(cursor_wrapped.last().map(String::as_str).unwrap_or(""));
            // 把 display_row 限制在可见区域内
            let visible_line_idx = display_row.min(input_content_height.saturating_sub(1));
            // 诊断日志：只在 buffer 非空且包含多行或长行时记录（排查 wrap 光标 BUG）
            if !input.buffer().is_empty()
                && (input.buffer().contains('\n')
                    || UnicodeWidthStr::width(input.buffer()) + prompt_prefix_len > input_width)
            {
                paste_diag_log(&format!(
                    "光标计算: buf_len={} cursor={} display_row={} cursor_x={} visible_idx={} input_w={}",
                    input.buffer().len(),
                    input.cursor(),
                    display_row,
                    cursor_x,
                    visible_line_idx,
                    input_width,
                ));
            }
            f.set_cursor_position((
                outer[1].x + cursor_x as u16,
                outer[1].y + 1 + visible_line_idx as u16, // +1 for the top border
            ));

            // Slash menu popup (overlays above input line, into the output area)
            if input.menu_open() {
                let menu_height: u16 = 12;
                let available_above = outer[1]
                    .y
                    .saturating_sub(outer[0].y)
                    .saturating_sub(1);
                let actual_height = menu_height.min(available_above);
                if actual_height > 0 {
                    // 根据层级选 query：Sub 用 sub_menu_query（空格后内容）
                    let query = match menu.level() {
                        crate::tui::slash_menu::MenuLevel::Sub => input.sub_menu_query(),
                        crate::tui::slash_menu::MenuLevel::Top => input.menu_query(),
                    };
                    if let Some(q) = query {
                        menu.set_query(&q);
                    }
                    let menu_area = Rect {
                        x: outer[1].x,
                        y: outer[1].y.saturating_sub(actual_height),
                        width: main_area.width,
                        height: actual_height,
                    };
                    render_menu(&mut menu, f, menu_area);
                }
            }

            // Status bar
            let state_snapshot = {
                let guard = status_state.lock().expect("StatusBarState poisoned");
                guard.clone()
            };
            let status_widget = StatusBar { state: &state_snapshot };
            f.render_widget(status_widget, outer[2]);

            // Help overlay (centered modal). Drawn last so it sits on top.
            if help_visible {
                render_help_overlay(f, f.area());
            }

            // SessionPicker overlay（/session pick 触发）。
            // 在 help overlay 之上渲染，确保选择器可见。
            if let Some(ref picker) = session_picker {
                render_session_picker(picker, f, main_area);
            }

            // 缓存 main_area 和 scroll_y 到 loop 外变量，供 Event::Mouse 分支使用。
            last_main_area = main_area;
            last_scroll_y = scroll_y as u16;
        })?;

        // Poll for events (shorter timeout during streaming for responsive rendering)
        let poll_timeout = if turn_rx.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(200)
        };
        // conhost drain phase: after all Enter events consumed, wait for
        // remaining key events (last line chars) to be delivered.
        // Use short poll timeout to detect when input stream goes idle.
        //
        // 两种进入条件：
        // 1. pending_paste_lines 为空：所有 Enter 已消费，等待残留字符排空后填 @路径
        // 2. pending_paste_lines 非空：最后一行不带 \n，字符被 suppress 后无 Enter
        //    触发消费。靠 poll 超时判定 conhost 注入完毕，强制清空并填 @路径。
        if conhost_suppress_input {
            if event::poll(Duration::from_millis(50))? {
                // 仍在接收事件 — 丢弃并等待
                let _ = event::read()?;
                continue;
            }
            // 事件流空闲 — conhost 注入完毕
            paste_diag_log(&format!(
                "  conhost drain phase: poll timeout, injection complete (pending_paste_lines.len()={})",
                pending_paste_lines.len()
            ));
            conhost_suppress_input = false;
            conhost_paste_intercepted = false;
            pending_paste_lines.clear();
            pending_paste_last_line = None;
            if let Some(at_path) = pending_at_path.take() {
                paste_diag_log(&format!("  drain done: insert @path={:?}", at_path));
                input.insert_paste(&at_path);
            }
        }

        if event::poll(poll_timeout)? {
            let ev = event::read()?;
            // 诊断日志：记录所有收到的 Event 类型（特别是 KeyEvent 和 Paste），
            // 用于确认 Windows Terminal 中 Ctrl+V 粘贴时 crossterm 收到的事件序列。
            // 只记录关键事件（ESC、Ctrl+V、Enter、Char），避免日志过大。
            match &ev {
                Event::Key(k) => {
                    let key_desc = match k.code {
                        KeyCode::Char(c) => {
                            let mods = if k.modifiers.contains(KeyModifiers::CONTROL) {
                                "Ctrl+"
                            } else {
                                ""
                            };
                            format!("{mods}{c:?}")
                        }
                        KeyCode::Enter => "Enter".to_string(),
                        KeyCode::Esc => "Esc".to_string(),
                        _ => format!("{:?}", k.code),
                    };
                    // 只记录关键事件，避免日志过大
                    if matches!(k.code, KeyCode::Esc | KeyCode::Enter)
                        || (matches!(k.code, KeyCode::Char(_))
                            && (k.modifiers.contains(KeyModifiers::CONTROL)
                                || k.code == KeyCode::Char('\u{1b}')))
                    {
                        paste_diag_log(&format!(
                            "KeyEvent 收到: kind={:?} code={} mods={:?}",
                            k.kind, key_desc, k.modifiers
                        ));
                    }
                }
                Event::Paste(text) => {
                    paste_diag_log(&format!(
                        "Event::Paste 收到: {} 字节, {} 行",
                        text.len(),
                        text.lines().count()
                    ));
                }
                _ => {}
            }
            // conhost_suppress_input: suppress non-Enter key events to
            // prevent clipboard characters from appearing in InputLine
            // during conhost multi-line paste.
            if conhost_suppress_input {
                if let Event::Key(key) = &ev {
                    if !matches!(key.code, KeyCode::Enter) || key.kind != KeyEventKind::Press {
                        continue;
                    }
                }
            }

            if let Event::Key(key) = ev {
                // SessionPicker 打开时拦截所有键盘事件
                if let Some(ref mut picker) = session_picker {
                    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                        continue;
                    }
                    match key.code {
                        KeyCode::Up => picker.move_up(),
                        KeyCode::Down => picker.move_down(),
                        KeyCode::Enter => {
                            // 确认切换：取出选中会话，执行 switch
                            let picked_id = picker
                                .selected_session()
                                .map(|s| s.id.clone());
                            session_picker = None; // 关闭选择器
                            if let Some(target_id) = picked_id {
                                // 在主线程执行 switch（需要 cli）
                                if let Some(mut cli) = cli_holder.take() {
                                    let output_handle = output_view.shared_handle();
                                    cli.set_tui_output(Arc::clone(&output_handle));
                                    let result = cli.handle_repl_command(
                                        commands::SlashCommand::Session {
                                            action: Some("switch".to_string()),
                                            target: Some(target_id),
                                        },
                                    );
                                    cli.clear_tui_output();
                                    // 刷新 status bar
                                    sync_status_from_cli(&status_state, &cli);
                                    cli_holder = Some(cli);
                                    if let Err(e) = result {
                                        if let Ok(mut buf) = output_handle.lock() {
                                            buf.push_entry(
                                                crate::tui::output_view::OutputEntry::text(
                                                    format!("[error] 切换会话失败: {e}\n\n"),
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        KeyCode::Esc => {
                            // 取消选择
                            session_picker = None;
                            if let Ok(mut buf) = output_view.shared_handle().lock() {
                                buf.push_entry(
                                    crate::tui::output_view::OutputEntry::text(
                                        "[info] 会话选择已取消。\n\n".to_string(),
                                    ),
                                );
                            }
                        }
                        _ => {} // 忽略其他键
                    }
                    continue;
                }

                let busy = turn_rx.is_some();
                let action = route_key(&mut input, key, help_visible, busy);

                // 方案 C drain phase 修复：
                // conhost_suppress_input 抑制字符输入后 buffer 始终为空，
                // Enter 被 handle_key 返回 Continue 而非 Submit，
                // 导致 pending_paste_lines 永远不消费、@路径永远不填充。
                // 在 drain 模式下对空 buffer 的 Enter 手动消费 pending_paste_lines。
                if matches!(action, InputAction::Continue)
                    && conhost_suppress_input
                    && matches!(key.code, KeyCode::Enter)
                    && key.kind == KeyEventKind::Press
                {
                    if !pending_paste_lines.is_empty() {
                        paste_diag_log(&format!(
                            "  conhost drain Continue→Enter: 手动消费 pending[0]={:?}, 剩余 {} 行",
                            pending_paste_lines[0],
                            pending_paste_lines.len() - 1
                        ));
                        pending_paste_lines.remove(0);
                        if pending_paste_lines.is_empty() {
                            conhost_paste_intercepted = false;
                            pending_paste_last_line = None;
                            paste_diag_log("  conhost drain Continue→Enter: pending 清空，conhost_paste_intercepted=false，等待 drain phase 填充 @路径");
                        }
                    }
                    continue 'main_loop;
                }

                // conhost 多行粘贴最后一行残留清理：
                //
                // BUG 现象：conhost 不支持 bracketed paste，Ctrl+V 粘贴多行文本时，
                // crossterm 把剪贴板内容作为普通字符序列处理，每行 \n 触发 Submit。
                // try_auto_expand_clipboard 兜底机制能正确处理前 N-1 行（首行触发替换，
                // 后续行 skip_submit 丢弃），但最后一行（不带 \n）作为普通字符插入
                // InputLine buffer，导致"发送后输入框填充最后一行内容"。
                //
                // 修复：每次处理完键盘事件后，如果 pending_paste_last_line 非空且
                // InputLine buffer normalize 后等于 pending_paste_last_line normalize，
                // 主动清空 buffer。normalize 是为了忽略 Tab 等空白差异。
                if pending_paste_last_line.is_some() {
                    // BUG-3 修复：normalize_whitespace 原本过滤所有空白，导致
                    // "fn main()" 与 "fnmain()" 判等，误清空 buffer。改回 trim + 去 \r，
                    // 仅处理 conhost 行尾差异（\r\n vs \n），保留内部空白。
                    let normalize_whitespace = |s: &str| -> String { s.trim().replace('\r', "") };
                    let normalized_buffer = normalize_whitespace(input.buffer());
                    let normalized_last =
                        normalize_whitespace(pending_paste_last_line.as_deref().unwrap_or(""));
                    if !normalized_buffer.is_empty() && normalized_buffer == normalized_last {
                        paste_diag_log(&format!(
                            "  清理最后一行残留: buffer={:?} == pending_paste_last_line, 清空 buffer",
                            input.buffer()
                        ));
                        input.reset();
                        pending_paste_last_line = None;
                        // 方案 C：最后一行被清理意味着 conhost 注入完毕，
                        // 清空 pending_paste_lines 并填充 @路径到 buffer
                        if conhost_paste_intercepted {
                            pending_paste_lines.clear();
                            conhost_paste_intercepted = false;
                            paste_diag_log("  conhost 注入完毕（最后一行清理触发），重置 conhost_paste_intercepted=false");
                            if let Some(at_path) = pending_at_path.take() {
                                paste_diag_log(&format!(
                                    "  conhost 注入完毕，填充 @路径到 buffer={:?}",
                                    at_path
                                ));
                                input.insert_paste(&at_path);
                            }
                        }
                    }
                }
                match action {
                    InputAction::Exit => break,
                    InputAction::InterruptTurn => {
                        // TUI 中断支持：Ctrl+C 在 busy 时取消当前 turn。
                        // abort signal 让 agent loop 在下一次迭代顶部退出。
                        // 正在进行的 API 流式请求无法中断（阻塞 IO），但可以阻止
                        // 下一轮迭代（不再发起新请求、不再执行新工具）。
                        crate::diag_log("[turn-abort] Ctrl+C interrupt signal sent");
                        if let Some(ref signal) = current_abort_signal {
                            signal.abort();
                            if let Ok(mut buf) = output_view.shared_handle().lock() {
                                buf.append("\n[interrupt] 正在取消当前任务...\n");
                            }
                        }
                    }
                    InputAction::ToggleHelp => {
                        help_visible = !help_visible;
                    }
                    InputAction::CloseMenu if help_visible => {
                        // Esc closes the help overlay first.
                        help_visible = false;
                    }
                    _ if help_visible => {
                        // While help is visible, swallow everything except
                        // the toggle and Esc (handled above). This makes the
                        // overlay modal so background typing is ignored.
                        // Exception: Exit must still break the loop.
                        if matches!(action, InputAction::Exit) {
                            break;
                        }
                    }
                    InputAction::ToggleSidebar => {
                        sidebar_visible = !sidebar_visible;
                    }
                    InputAction::SidebarScrollUp => {
                        // Alt+Up：侧栏工具历史往上滚（看更早的记录）。
                        // 每次 +3 行，进入手动模式。新工具到来时会归零。
                        let cur = sidebar_tools_scroll.unwrap_or(0);
                        sidebar_tools_scroll = Some(cur.saturating_add(3));
                    }
                    InputAction::SidebarScrollDown => {
                        // Alt+Down：侧栏工具历史往下滚（回到最新）。
                        // 偏移归零后进入跟随模式（None），新工具自动显示。
                        match sidebar_tools_scroll {
                            None => {}
                            Some(0) => sidebar_tools_scroll = None,
                            Some(n) => sidebar_tools_scroll = Some(n.saturating_sub(3)),
                        }
                    }
                    InputAction::ToggleToolCard => {
                        // P1 重构：交互式折叠/展开最近一个工具卡片。
                        // 配合结构化 OutputView，按 Ctrl+T 切换最新 ToolCard
                        // 的 collapsed 字段，下次渲染时动态生成可见行。
                        let handle = output_view.shared_handle();
                        if let Ok(mut buf) = handle.lock() {
                            buf.toggle_latest_tool_card();
                        };
                    }
                    InputAction::ScrollUp => {
                        // PgUp: enter manual-scroll mode and move up by ~half
                        // the visible height (or at least 5 lines). Don't
                        // disturb the slash menu when it's open.
                        if !input.menu_open() {
                            let page = terminal
                                .size()
                                .map(|s| (s.height as usize / 2).max(5))
                                .unwrap_or(10);
                            let current = scroll_offset.unwrap_or(0);
                            scroll_offset = Some(current + page);
                        }
                    }
                    InputAction::ScrollDown => {
                        // PgDn: move toward the bottom; reaching 0 re-enters
                        // follow mode.
                        if !input.menu_open() {
                            if let Some(offset) = scroll_offset {
                                let page = terminal
                                    .size()
                                    .map(|s| (s.height as usize / 2).max(5))
                                    .unwrap_or(10);
                                if offset <= page {
                                    scroll_offset = None; // back to follow mode
                                } else {
                                    scroll_offset = Some(offset - page);
                                }
                            }
                        }
                    }
                    InputAction::ScrollUpLine => {
                        // Up arrow (menu closed): scroll output up one line.
                        let current = scroll_offset.unwrap_or(0);
                        scroll_offset = Some(current + 1);
                    }
                    InputAction::ScrollDownLine => {
                        // Down arrow (menu closed): scroll down one line.
                        // Reaching 0 re-enters follow mode.
                        if let Some(offset) = scroll_offset {
                            if offset <= 1 {
                                scroll_offset = None;
                            } else {
                                scroll_offset = Some(offset - 1);
                            }
                        }
                    }
                    InputAction::Submit(line) => {
                        // 重置 conhost_paste_intercepted 标志（每次 Submit 入口）
                        // 注意：如果上次设置了 conhost_paste_intercepted，后续的
                        // pending_paste_lines 仍需被丢弃，所以这里不能简单重置。
                        // 真正的重置在 pending_paste_lines 清空后。
                        // 诊断日志：记录每次 Submit 入口，用于排查 conhost 多行粘贴 BUG。
                        paste_diag_log(&format!(
                            "Submit 入口: line={:?} ({} 字节), pending_paste_lines.len()={}, cli_holder.is_some()={}, turn_rx.is_some()={}",
                            line,
                            line.len(),
                            pending_paste_lines.len(),
                            cli_holder.is_some(),
                            turn_rx.is_some()
                        ));
                        if !pending_paste_lines.is_empty() {
                            paste_diag_log(&format!(
                                "  pending_paste_lines[0]={:?}, line.trim()={:?}",
                                pending_paste_lines[0],
                                line.trim()
                            ));
                        }
                        // conhost 多行粘贴后续行丢弃：
                        // try_auto_expand_clipboard 触发时会填充 pending_paste_lines
                        // （剪贴板第 2 行到最后一行）。conhost 不支持 bracketed paste，
                        // 粘贴会逐行触发 Submit，这里检查并丢弃后续行，避免每行被当作
                        // 独立消息发送。
                        //
                        // 匹配规则：line（normalize 后）== pending_paste_lines[0]（normalize 后）。
                        // normalize = 去除所有空白字符（包括 Tab、空格、\r 等），因为
                        // conhost 可能把 Tab 解释为 Tab 键事件而非字面字符，导致
                        // InputLine 收到的内容与剪贴板原始内容不匹配。
                        //
                        // 特殊情况：conhost_paste_intercepted=true 时（方案 C 已写文件 + 填充 @路径），
                        // 后续所有 Submit 都应丢弃（包括 @路径本身），因为 conhost 还在
                        // 逐行发送剪贴板内容。此时不匹配 pending_paste_lines[0] 也丢弃，
                        // 但仍消费 pending_paste_lines 以维持计数。
                        //
                        // 匹配 → 丢弃该 Submit，从 pending_paste_lines 移除该行。
                        // 不匹配且 !conhost_paste_intercepted → 粘贴已完成，清空并正常处理。
                        // 不匹配且 conhost_paste_intercepted → 仍丢弃（@路径或残留行）。
                        // BUG-3 修复：normalize 改为 trim + 去 \r，保留内部空白。
                        let normalize_whitespace =
                            |s: &str| -> String { s.trim().replace('\r', "") };
                        // AskUserQuestion 模式下强制 skip_submit=false，
                        // 避免 conhost paste 残留状态误把 ask 答案丢弃。
                        let in_ask_mode = pending_ask.is_some();
                        let skip_submit = if in_ask_mode {
                            false
                        } else if conhost_paste_intercepted {
                            // 方案 C 触发后，Windows Terminal 会把剪贴板内容作为字符流注入 stdin
                            // （不是 Event::Paste），每行 \n 触发 Enter 事件。
                            // 这里需要 skip 所有这些 Submit，直到 pending_paste_lines 为空。
                            //
                            // - @路径 Submit：我们插入的，skip 但不移除 pending_paste_lines
                            // - 匹配 pending_paste_lines[0]：skip 并移除
                            // - 不匹配且不以 @ 开头：可能是剩余行被 conhost 修改了编码，
                            //   保守 skip 并移除 pending_paste_lines[0]
                            // - pending_paste_lines 为空：重置 conhost_paste_intercepted
                            if line.trim().starts_with('@') {
                                paste_diag_log("  skip_submit=true (conhost 拦截后的 @路径，保留 pending_paste_lines)");
                                true
                            } else if !pending_paste_lines.is_empty() {
                                let normalize_whitespace =
                                    |s: &str| -> String { s.trim().replace('\r', "") };
                                let normalized_line = normalize_whitespace(&line);
                                let normalized_expected =
                                    normalize_whitespace(&pending_paste_lines[0]);
                                if !normalized_line.is_empty()
                                    && normalized_line == normalized_expected
                                {
                                    paste_diag_log("  skip_submit=true (conhost 模式，匹配 pending_paste_lines[0]，移除)");
                                } else {
                                    paste_diag_log(&format!(
                                        "  skip_submit=true (conhost 模式，不匹配但保守丢弃 line={:?})",
                                        line.trim()
                                    ));
                                }
                                pending_paste_lines.remove(0);
                                // BUG-4 修复：弹出最后一个元素时同步清理 pending_paste_last_line，
                                // 防止残留状态导致后续用户输入被误清空 buffer。
                                if pending_paste_lines.is_empty() {
                                    pending_paste_last_line = None;
                                }
                                true
                            } else {
                                paste_diag_log("  skip_submit=true (conhost 模式，pending_paste_lines 已空，最后兜底)");
                                true
                            }
                        } else if !pending_paste_lines.is_empty() {
                            let normalized_line = normalize_whitespace(&line);
                            let normalized_expected = normalize_whitespace(&pending_paste_lines[0]);
                            if !normalized_line.is_empty() && normalized_line == normalized_expected
                            {
                                pending_paste_lines.remove(0);
                                // BUG-4 修复：同上，弹空时同步清理 pending_paste_last_line。
                                if pending_paste_lines.is_empty() {
                                    pending_paste_last_line = None;
                                }
                                paste_diag_log("  skip_submit=true (normalize 后匹配 pending_paste_lines[0]，丢弃)");
                                true
                            } else {
                                pending_paste_lines.clear();
                                // BUG-4 修复：clear 时也清理 pending_paste_last_line。
                                pending_paste_last_line = None;
                                paste_diag_log(&format!(
                                    "  skip_submit=false (不匹配 normalized_line={:?} normalized_expected={:?})",
                                    normalized_line, normalized_expected
                                ));
                                false
                            }
                        } else {
                            false
                        };

                        if skip_submit {
                            // 丢弃该 Submit，等待下一行。
                            // InputLine::handle_key 在返回 Submit 前已 reset()，buffer 为空。
                            // conhost_paste_intercepted 只有在 pending_paste_lines 空时才重置。
                            if pending_paste_lines.is_empty() && conhost_paste_intercepted {
                                conhost_paste_intercepted = false;
                                // Keep conhost_suppress_input = true
                                // (drain phase will handle @path insertion)
                                // BUG-1 修复：conhost 模式通过"最后一行带 \n"路径结束时，
                                // pending_paste_last_line 仍保留旧值，后续用户输入匹配旧值
                                // 会被误清空 buffer。在此同步清理。
                                pending_paste_last_line = None;
                                paste_diag_log("  pending_paste_lines 清空，重置 conhost_paste_intercepted=false, keep suppress_input=true");
                                // @路径延后到 drain phase 填充
                            }
                        } else if let Some(ask) = pending_ask.take() {
                            // AskUserQuestion 协作路径：worker 线程正在等待用户回答。
                            // 把 InputLine 的内容作为答案通过 resp_tx 回传，
                            // **不**触发 execute_turn。worker 线程拿到答案后，
                            // run_ask_user_question 会返回，整个 turn 继续执行。
                            //
                            // 输入处理：
                            // - 选项模式：用户输入数字 → 解析为对应选项文本回传；
                            //   若超出范围或非数字 → 原样回传（让 LLM 看到用户的自由文本）。
                            // - 自由文本模式：原样回传。
                            let answer = if let Some(ref opts) = ask.options {
                                let trimmed = line.trim();
                                if let Ok(idx) = trimmed.parse::<usize>() {
                                    if idx >= 1 && idx <= opts.len() {
                                        opts[idx - 1].clone()
                                    } else {
                                        line.clone()
                                    }
                                } else {
                                    line.clone()
                                }
                            } else {
                                line.clone()
                            };
                            // 把用户答案作为 echo 显示到 OutputView，保持对话上下文清晰。
                            // 用 push_entry 而非 append，让 echo 成为独立 Text entry
                            // 并获得自己的时间戳（与上一条 AI 回复分开）。
                            if let Ok(mut buf) = output_view.shared_handle().lock() {
                                buf.push_entry(crate::tui::output_view::OutputEntry::text(
                                    format!("> {answer}\n\n"),
                                ));
                            }
                            // 回传答案。失败说明 worker 线程已退出（极少见），忽略。
                            let _ = ask.resp_tx.send(answer);
                            // pending_ask 已被 take() 清空，下一次 Submit 走正常对话流程。
                        } else if cli_holder.is_some() && turn_rx.is_none() {
                            // Re-enter follow mode so the user sees new output.
                            scroll_offset = None;
                            turn_start = Some(Instant::now());

                            // P2-4 修复：Submit 后立即调用 reset_turn（内部会设
                            // streaming=true 并清零 turn 计时），避免 worker 线程真正
                            // 启动前（数百 ms ~ 数秒网络延迟）状态栏仍显示"空闲"，
                            // 用户以为没按上。reset_turn 内部已设 streaming=true。
                            if let Ok(mut guard) = status_state.lock() {
                                guard.reset_turn();
                            }

                            // 多行粘贴兜底 + 折叠处理：
                            // - 如果 line 不含 \n 且不以 / 开头，调用 try_auto_expand_clipboard
                            //   检测剪贴板是否有多行内容且第一行匹配 line。如果匹配，用完整
                            //   剪贴板内容替换 line（修复 conhost 不支持 bracketed paste 时
                            //   多行粘贴被切成多次 Submit 的 bug）。
                            // - 否则（line 已含 \n，说明 bracketed paste 已生效）直接 fold。
                            // - fold_pasted_input 处理超长粘贴：超过阈值时存到 paste-cache，
                            //   用占位符 [Pasted text #N +M lines] 替换 display。
                            // - display 用于回显到 OutputView，expanded 用于发给 AI。
                            // - slash 命令（以 / 开头）跳过所有处理，原样发送。
                            let trimmed = line.trim();
                            let session_id = cli_holder
                                .as_ref()
                                .map(|c| c.session_id_snapshot().to_string())
                                .unwrap_or_default();
                            let (display, expanded) = if trimmed.is_empty() {
                                paste_diag_log("  分支: trimmed.is_empty() → 原样发送");
                                (line.clone(), line.clone())
                            } else if trimmed.starts_with('/') {
                                paste_diag_log("  分支: trimmed.starts_with('/') → 原样发送");
                                (line.clone(), line.clone())
                            } else if !line.contains('\n') && pending_paste_lines.is_empty() {
                                // 单行输入：尝试剪贴板检测
                                paste_diag_log(&format!(
                                    "  分支: 单行输入 → 调用 try_auto_expand_clipboard (trimmed={:?})",
                                    trimmed
                                ));
                                let result = try_auto_expand_clipboard(
                                    trimmed,
                                    &session_id,
                                    &mut paste_id_gen,
                                    &mut pending_paste_lines,
                                );
                                paste_diag_log(&format!(
                                    "  try_auto_expand_clipboard 返回: {}",
                                    if result.is_some() {
                                        "Some(触发替换)"
                                    } else {
                                        "None(未触发)"
                                    }
                                ));
                                paste_diag_log(&format!(
                                    "  调用后 pending_paste_lines.len()={}",
                                    pending_paste_lines.len()
                                ));

                                // conhost 多行粘贴新方案（方案 C）：
                                // 如果 try_auto_expand_clipboard 触发（说明 conhost 多行粘贴），
                                // 不直接发送给 AI，而是把完整剪贴板内容写到临时文件，
                                // 在 InputLine buffer 填充 `@<路径>`，让用户决定是否发送。
                                // 这样用户可以编辑后再发送，避免"粘贴后直接发送出去"。
                                //
                                // **关键修复**：不立即 insert_paste @路径到 buffer，因为
                                // conhost 还会继续注入剩余行字符，会与 @路径拼接成
                                // "@路径第二行内容"。而是把 @路径保存到 pending_at_path，
                                // 等 pending_paste_lines 为空（conhost 注入完毕）后再
                                // insert_paste 到 buffer。
                                if result.is_some() && !pending_paste_lines.is_empty() {
                                    // 读取完整剪贴板内容（再读一次，因为 try_auto_expand 没返回）
                                    match crate::paste::read_clipboard_text() {
                                        Ok(clipboard_content) => {
                                            // 写入临时文件，返回 @<路径>
                                            if let Some(at_path) = write_clipboard_to_temp_file(
                                                &clipboard_content,
                                                &session_id,
                                            ) {
                                                // 提取用户前缀文字：
                                                // Submit 内容 = "前缀" + 剪贴板首行
                                                // drain phase 填充 buffer 时需要保留前缀，
                                                // 否则用户输入的引导文字会丢失。
                                                let clip_first = clipboard_content
                                                    .lines()
                                                    .next()
                                                    .unwrap_or("")
                                                    .trim();
                                                let prefix = if !clip_first.is_empty()
                                                    && trimmed.ends_with(clip_first)
                                                {
                                                    trimmed[..trimmed.len() - clip_first.len()]
                                                        .trim()
                                                        .to_string()
                                                } else {
                                                    String::new()
                                                };
                                                let composed = if prefix.is_empty() {
                                                    at_path
                                                } else {
                                                    format!("{prefix} {at_path}")
                                                };
                                                paste_diag_log(&format!(
                                                    "  conhost 方案 C: 写文件成功，@路径暂存 prefix={:?} composed={:?}",
                                                    prefix, composed
                                                ));
                                                pending_at_path = Some(composed.clone());
                                                conhost_suppress_input = true;
                                                pending_paste_last_line = Some(
                                                    pending_paste_lines.last().unwrap().clone(),
                                                );
                                                conhost_paste_intercepted = true;
                                                (composed, String::new())
                                            } else {
                                                paste_diag_log("  写文件失败，回退到原行为");
                                                result.unwrap_or_else(|| {
                                                    fold_pasted_input(
                                                        &line,
                                                        &session_id,
                                                        &mut paste_id_gen,
                                                    )
                                                })
                                            }
                                        }
                                        Err(e) => {
                                            paste_diag_log(&format!(
                                                "  读取剪贴板失败: {e}，回退到原行为"
                                            ));
                                            result.unwrap_or_else(|| {
                                                fold_pasted_input(
                                                    &line,
                                                    &session_id,
                                                    &mut paste_id_gen,
                                                )
                                            })
                                        }
                                    }
                                } else {
                                    // 未触发或触发但 pending 为空，走原逻辑
                                    result.unwrap_or_else(|| {
                                        paste_diag_log("  fallback 到 fold_pasted_input (单行)");
                                        fold_pasted_input(&line, &session_id, &mut paste_id_gen)
                                    })
                                }
                            } else {
                                // 多行输入（bracketed paste 已触发）：直接 fold
                                paste_diag_log(&format!(
                                    "  分支: 多行输入 (含\\n={}) → 直接 fold_pasted_input",
                                    line.contains('\n')
                                ));
                                fold_pasted_input(&line, &session_id, &mut paste_id_gen)
                            };

                            // conhost 方案 C：如果 conhost 多行粘贴被拦截（写文件 + 填充 @路径），
                            // 不发送给 AI，直接跳过 run_turn。用户看到 InputLine buffer 中的
                            // @<路径> 后，可以编辑或直接按 Enter 发送。
                            //
                            // **注意**：方案 C 触发时不 echo display 到输出区，因为
                            // @路径还未填充到 buffer（等 conhost 注入完毕后才填充），
                            // 此时 echo 会显示一个孤立的 @路径，造成混淆。
                            if conhost_paste_intercepted {
                                paste_diag_log(
                                    "  conhost_paste_intercepted=true，跳过 echo 和 run_turn",
                                );
                                continue 'main_loop;
                            }

                            // Echo the user's message into the output view so
                            // the conversation history shows both sides (user
                            // + assistant). Without this the output area only
                            // contained assistant TextDelta events, making it
                            // impossible to tell what the user asked.
                            //
                            // P1 修复：从第二次发送开始，buffer 末尾可能不以
                            // `\n` 结尾（如 MessageStop 已追加 `\n\n`，但若
                            // 上次 turn 异常退出未触发 MessageStop，buffer 末尾
                            // 会残留 AI 文本）。echo 前检查 buffer 末尾，若非空
                            // 且不以 `\n` 结尾，先追加 `\n\n` 作为分隔。
                            //
                            // 回显用 display（折叠后的占位符），不用 expanded
                            // （完整内容可能很长，污染输出区）。
                            let echo_handle = output_view.shared_handle();
                            if let Ok(mut buf) = echo_handle.lock() {
                                let current = buf.buffer();
                                if !current.is_empty() && !current.ends_with('\n') {
                                    buf.append("\n\n");
                                }
                                // 用 push_entry 让用户 echo 成为独立 Text entry
                                // 并获得自己的时间戳（与 AI 回复分开）。
                                buf.push_entry(crate::tui::output_view::OutputEntry::text(
                                    format!("> {display}\n\n"),
                                ));
                            }

                            let output_handle = output_view.shared_handle();
                            let status_handle = Arc::clone(&status_state);
                            let tool_history_handle = Arc::clone(&tool_history_shared);

                            let mut cli = cli_holder.take().unwrap();

                            // TUI 原生选择列表拦截：/session pick 在 TUI 下不走 worker 线程，
                            // 而是打开 SessionPicker overlay，用上下键选中会话后 Enter 确认。
                            // 选中后直接在主线程执行 switch（需要 &mut cli）。
                            let trimmed_expanded = expanded.trim();
                            if trimmed_expanded == "/session pick"
                                || trimmed_expanded == "/session"
                            {
                                // 尝试加载会话列表
                                match crate::session_mgr::list_managed_sessions() {
                                    Ok(mut sessions) => {
                                        // 按修改时间倒序（最新在前）
                                        sessions.sort_by_key(|s| {
                                            std::cmp::Reverse(s.modified_epoch_millis)
                                        });
                                        if sessions.is_empty() {
                                            if let Ok(mut buf) = output_view.shared_handle().lock() {
                                                buf.push_entry(
                                                    crate::tui::output_view::OutputEntry::text(
                                                        "[info] 暂无受管会话。\n\n".to_string(),
                                                    ),
                                                );
                                            }
                                        } else {
                                            // 打开 SessionPicker overlay
                                            session_picker = Some(SessionPickerState::new(
                                                sessions,
                                                cli.session_id_snapshot().to_string(),
                                            ));
                                            // cli 放回 holder，不执行后续逻辑
                                            cli_holder = Some(cli);
                                            continue 'main_loop;
                                        }
                                    }
                                    Err(e) => {
                                        if let Ok(mut buf) = output_view.shared_handle().lock() {
                                            buf.push_entry(
                                                crate::tui::output_view::OutputEntry::text(
                                                    format!("[error] 加载会话列表失败: {e}\n\n"),
                                                ),
                                            );
                                        }
                                    }
                                }
                                cli_holder = Some(cli);
                                continue 'main_loop;
                            }

                            // 斜杠命令本地分发：先尝试解析为 SlashCommand。
                            // 如果是斜杠命令，本地执行 handle_repl_command
                            // （如 /help 显示命令列表、/clear 清会话等），
                            // 而不是当作普通输入发给 AI。
                            // 修复"输入 /help 发送给 AI"的 bug。
                            //
                            // 注意：用 expanded 而非原始 line 来解析，因为
                            // 多行粘贴可能以非 / 开头但包含 / 命令（罕见但可能）。
                            let slash_parsed = SlashCommand::parse(expanded.trim());

                            let (tx, rx) = mpsc::channel();
                            match slash_parsed {
                                Ok(Some(command)) => {
                                    // 本地命令：设置 tui_output 捕获 println，
                                    // 在 worker 线程执行 handle_repl_command。
                                    cli.set_tui_output(Arc::clone(&output_handle));
                                    let status_handle_for_panic = Arc::clone(&status_handle);
                                    std::thread::spawn(move || {
                                        // Bug L3 修复：用 catch_unwind 包裹，panic 时
                                        // 仍通过 channel 返回 cli，避免 cli 永久丢失。
                                        use std::panic::{catch_unwind, AssertUnwindSafe};
                                        let mut cli = cli;
                                        let cli_ref = &mut cli;
                                        let result = catch_unwind(AssertUnwindSafe(move || {
                                            execute_slash_command(cli_ref, command)
                                        }));
                                        let turn_result = match result {
                                            Ok(r) => TurnResult { cli, result: r },
                                            Err(payload) => {
                                                let msg = payload
                                                    .downcast_ref::<String>()
                                                    .cloned()
                                                    .or_else(|| {
                                                        payload
                                                            .downcast_ref::<&str>()
                                                            .map(|s| s.to_string())
                                                    })
                                                    .unwrap_or_else(|| {
                                                        "<unknown panic>".to_string()
                                                    });
                                                if let Ok(mut guard) =
                                                    status_handle_for_panic.lock()
                                                {
                                                    if guard.streaming {
                                                        guard.finish_turn();
                                                    }
                                                }
                                                TurnResult {
                                                    cli,
                                                    result: Err(format!(
                                                        "worker thread panicked: {msg}"
                                                    )),
                                                }
                                            }
                                        };
                                        let _ = tx.send(turn_result);
                                    });
                                }
                                Ok(None) | Err(_) => {
                                    // 普通对话：发给 AI。清空工具历史。
                                    // 用 expanded（含完整粘贴内容）发送，而非 display（占位符）。
                                    if let Ok(mut h) = tool_history_shared.lock() {
                                        h.clear();
                                    }
                                    // TUI 中断支持：创建 abort signal，设置到 cli，
                                    // 保留 clone 用于 Ctrl+C 取消。
                                    let abort_signal = runtime::HookAbortSignal::new();
                                    cli.set_external_abort_signal(abort_signal.clone());
                                    current_abort_signal = Some(abort_signal);
                                    crate::diag_log(&format!(
                                        "[turn-start] normal len={}",
                                        expanded.len()
                                    ));
                                    let status_handle_for_panic = Arc::clone(&status_handle);
                                    std::thread::spawn(move || {
                                        // Bug L3 修复：同上，catch_unwind 包裹。
                                        use std::panic::{catch_unwind, AssertUnwindSafe};
                                        let mut cli = cli;
                                        let cli_ref = &mut cli;
                                        let result = catch_unwind(AssertUnwindSafe(move || {
                                            execute_turn(
                                                cli_ref,
                                                &expanded,
                                                &output_handle,
                                                &status_handle,
                                                &tool_history_handle,
                                            )
                                        }));
                                        let turn_result = match result {
                                            Ok(r) => TurnResult { cli, result: r },
                                            Err(payload) => {
                                                let msg = payload
                                                    .downcast_ref::<String>()
                                                    .cloned()
                                                    .or_else(|| {
                                                        payload
                                                            .downcast_ref::<&str>()
                                                            .map(|s| s.to_string())
                                                    })
                                                    .unwrap_or_else(|| {
                                                        "<unknown panic>".to_string()
                                                    });
                                                if let Ok(mut guard) =
                                                    status_handle_for_panic.lock()
                                                {
                                                    if guard.streaming {
                                                        guard.finish_turn();
                                                    }
                                                }
                                                TurnResult {
                                                    cli,
                                                    result: Err(format!(
                                                        "worker thread panicked: {msg}"
                                                    )),
                                                }
                                            }
                                        };
                                        let _ = tx.send(turn_result);
                                    });
                                }
                            }
                            turn_rx = Some(rx);
                            // 注意：此处不再清空 pending_paste_lines。
                            //
                            // 原 Bug L2 修复（清空 pending_paste_lines）的注释假设
                            // "TUI 路径下 bracketed paste + Event::Paste 不会被切 Submit"，
                            // 但这仅在支持 bracketed paste 的终端（如 Windows Terminal）成立。
                            // conhost 不支持 bracketed paste，粘贴会逐行触发 Submit，
                            // 需要 pending_paste_lines 来识别并丢弃后续行。
                            //
                            // 新逻辑（见 Submit 分支开头）：每次 Submit 检查 line 是否匹配
                            // pending_paste_lines[0]，匹配则丢弃并移除，不匹配则清空。
                            // 因此这里不需要也不应该清空——清空会破坏 conhost 多行粘贴兜底。
                        } else if fatal_error {
                            // P0-4 修复：worker 线程已崩溃（Disconnected），
                            // cli_holder 已永久丢失。之前此分支静默丢弃输入，
                            // 用户敲 Enter 无任何反馈。现在向 OutputView 反馈。
                            input.restore_input(line);
                            if let Ok(mut buf) = output_view.shared_handle().lock() {
                                buf.append(
                                    "\n[error] 对话线程已崩溃，无法继续对话。请退出并重启 TUI（Ctrl+C 或 Ctrl+D）。\n",
                                );
                            }
                        } else {
                            // TUI 插话支持：turn 正在运行时用户敲 Enter，输入进入
                            // pending_input 队列。turn 完成（含中断）后自动提交，
                            // 实现"任务间隙插话"。InputLine 已 reset() 清空 buffer，
                            // 用户可以继续输入新的内容。
                            pending_input = Some(line);
                            if let Ok(mut buf) = output_view.shared_handle().lock() {
                                buf.append("\n[queued] 输入已排队，当前任务结束后自动发送。\n");
                            }
                        }
                    }
                    InputAction::MenuUp => menu.move_up(),
                    InputAction::MenuDown => menu.move_down(),
                    InputAction::MenuAccept => {
                        // 二级菜单支持：根据当前层级分叉
                        match menu.level() {
                            crate::tui::slash_menu::MenuLevel::Sub => {
                                // Sub 层级：选中子选项，拼成 `父+子` 填入 buffer
                                if let (Some(parent), Some(sub)) =
                                    (menu.parent_name(), menu.selected_sub_option())
                                {
                                    // 子选项需要额外参数：填到空格后等用户手敲
                                    // 不需要参数：填入后等用户 Enter 提交
                                    let completion = if sub.requires_arg {
                                        format!("/{parent} {} ", sub.value)
                                    } else {
                                        format!("/{parent} {}", sub.value)
                                    };
                                    input.accept_menu_completion(&completion);
                                    menu.exit_sub_menu();
                                }
                            }
                            crate::tui::slash_menu::MenuLevel::Top => {
                                // Top 层级：选中父命令
                                // 若有子选项 → 进入二级菜单
                                // 若无子选项 → 填入 buffer 等用户 Enter
                                if let Some(spec) = menu.selected_spec() {
                                    let name_static: &'static str = spec.name;
                                    if menu.enter_sub_menu(name_static) {
                                        // 进入二级菜单：菜单保持打开显示子选项。
                                        // buffer 设为 `/parent `，用户可直接输入参数
                                        // 或用上下键选子选项后 Enter。
                                        // 不调用 accept_menu_completion（那会 menu_locked），
                                        // 而是手动设 buffer+关闭一级菜单显示。
                                        input.set_buffer_for_sub_menu(&format!("/{name_static} "));
                                        // 关键：menu_open 保持 true，渲染层会根据
                                        // menu.level()=Sub 显示二级菜单而非一级。
                                    } else {
                                        // 无子选项：填入命令名，等用户 Enter
                                        let completion = format!("/{}", spec.name);
                                        input.accept_menu_completion(&completion);
                                    }
                                }
                            }
                        }
                    }
                    InputAction::CloseMenu => {
                        // menu state already updated in input.handle_key
                        // Sub 层级下 Esc 返回 Top，不直接关闭菜单
                        if menu.level() == crate::tui::slash_menu::MenuLevel::Sub {
                            menu.exit_sub_menu();
                            // 同时清空 buffer 中的 `/parent ` 让用户重新选
                            input.reset();
                        }
                    }
                    InputAction::Continue | InputAction::Ignore => {}
                }
            } else if let Event::Mouse(mouse) = ev {
                // 鼠标事件分发：
                // - 左键单击：命中输出区时切换该行所在 ToolCard 的折叠状态
                // - 滚轮上/下滚：调整 scroll_offset，复用 InputAction::ScrollUpLine /
                //   ScrollDownLine 的语义，每次滚动 3 行（典型鼠标滚轮手感）
                //
                // 坐标映射（仅左键点击需要）：mouse.row 是终端绝对行号，需减去
                // main_area.y + 1（+1 为顶部 border）得到相对行号，再加 scroll_y
                // 得到显示行号。
                //
                // **P1-2 修复**：之前注释写"逻辑行号"，但 last_scroll_y 是显示行单位
                // （Paragraph::scroll 按 Wrap 后的显示行计算），两者不一致导致长行
                // 场景下点击坐标偏移。现在 toggle_tool_card_at_line 接收 area_width
                // 参数，内部按显示行计算 [start, end) 区间，与 scroll 单位一致。
                use crossterm::event::{MouseButton, MouseEventKind};
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if !help_visible && last_main_area.height > 0 =>
                    {
                        let content_top = last_main_area.y + 1; // +1 for top border
                        let content_bottom = last_main_area.y + last_main_area.height;
                        if mouse.row >= content_top && mouse.row < content_bottom {
                            let relative_row = (mouse.row - content_top) as usize;
                            let logical_row = relative_row + last_scroll_y as usize;
                            // 输出区可见宽度 = area.width - 2（左右 border 各 1）
                            let area_width = last_main_area.width.saturating_sub(2) as usize;
                            let handle = output_view.shared_handle();
                            if let Ok(mut buf) = handle.lock() {
                                buf.toggle_tool_card_at_line(logical_row, area_width);
                            };
                        }
                    }
                    // 鼠标滚轮上滚：进入 manual-scroll 模式，每次上滚 3 行。
                    // 与 InputAction::ScrollUpLine 行为一致（仅步长不同）。
                    MouseEventKind::ScrollUp if !help_visible => {
                        let current = scroll_offset.unwrap_or(0);
                        scroll_offset = Some(current.saturating_add(3));
                    }
                    // 鼠标滚轮下滚：在 manual-scroll 模式下每次下滚 3 行；
                    // 到 0 时回到 follow 模式。处于 follow 模式（None）时忽略。
                    MouseEventKind::ScrollDown if !help_visible => {
                        if let Some(offset) = scroll_offset {
                            if offset <= 3 {
                                scroll_offset = None; // back to follow mode
                            } else {
                                scroll_offset = Some(offset - 3);
                            }
                        }
                    }
                    _ => {}
                }
            } else if let Event::Paste(text) = ev {
                // Bracketed paste：整段粘贴作为一个事件投递。
                // 参考 CLI 路径 input.rs 的 `.bracketed_paste(true)` 行为：
                // 把粘贴内容原子地插入到当前光标位置，保留所有 \n，不触发 Submit。
                // 修复"多行粘贴被切成多次 Submit"的 bug。
                paste_diag_log(&format!(
                    "Event::Paste 收到: {} 字节, {} 行, help_visible={}",
                    text.len(),
                    text.lines().count(),
                    help_visible
                ));
                if !help_visible {
                    input.insert_paste(&text);
                    paste_diag_log(&format!(
                        "  insert_paste 后 buffer={:?} ({} 字节)",
                        input.buffer(),
                        input.buffer().len()
                    ));
                }
            }
        }

        // Refresh status: update turn_elapsed_ms if streaming
        {
            // Bug L9 修复：mutex 毒化时容错访问。
            let mut guard = status_state.lock().unwrap_or_else(|e| e.into_inner());
            if guard.streaming {
                if let Some(start) = turn_start {
                    guard.turn_elapsed_ms = start.elapsed().as_millis() as u64;
                }
            }
        }
    }

    Ok(())
}

fn route_key(input: &mut InputLine, key: KeyEvent, help_visible: bool, busy: bool) -> InputAction {
    // Windows crossterm quirk: by default it emits *two* KeyEvents per key
    // press — one `Press` and one `Release`. Without filtering, every char
    // gets inserted twice (e.g., typing "你好" yields "你你好好"). Only handle
    // `Press` (and `Repeat` for key-hold auto-repeat) events; ignore `Release`.
    // On Unix/macOS crossterm always emits `Press`, so this filter is a no-op
    // there. This is the documented crossterm 0.28 behavior on Windows.
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return InputAction::Ignore;
    }

    // Bug L4 修复：help 浮层可见时，只允许少数键（?, Esc, Ctrl+C, Ctrl+D）
    // 走特殊分支，其他键直接返回 Ignore，**不调用 input.handle_key**，
    // 避免字符泄漏到 buffer（用户关掉浮层后会发现 buffer 里多了几个字符）。
    // 原 bug：route_key 先调用 input.handle_key 处理 Char('a')，字符进了
    // buffer，然后 main loop 的 `_ if help_visible` 分支吞掉 action —
    // 但字符已经泄漏，无法挽回。
    if help_visible {
        // Ctrl+C / Ctrl+D → Exit（保留退出能力）
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                let lower = c.to_ascii_lowercase();
                if lower == 'c' || lower == 'd' {
                    return InputAction::Exit;
                }
            }
        }
        // Esc → 关闭浮层（复用 CloseMenu action，main loop 的
        // `InputAction::CloseMenu if help_visible` 分支会处理）
        if matches!(key.code, KeyCode::Esc) {
            return InputAction::CloseMenu;
        }
        // '?' → ToggleHelp（关闭浮层）
        if let KeyCode::Char('?') = key.code {
            return InputAction::ToggleHelp;
        }
        // 其他键（包括字母、数字、Enter、Backspace 等）全部吞掉
        return InputAction::Ignore;
    }

    let modifiers_name = if key.modifiers.contains(KeyModifiers::CONTROL) {
        "Ctrl"
    } else {
        ""
    };

    // BUG 3 fix: Shift+Enter / Ctrl+J → insert newline for multi-line input.
    // Terminal quirks: most terminals do not distinguish Shift+Enter from
    // Enter (both send `\r`), so in practice Ctrl+J (which sends `\n`) is the
    // reliable binding. We also detect Shift+Enter for terminals/Kitty
    // keyboard protocol that *do* send the modifier. The logical "Newline"
    // key is handled by InputLine::handle_key before the submit branch.
    if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Enter) {
        return input.handle_key(None, "Newline");
    }

    // Ctrl+C / Ctrl+D — handle before char dispatch
    if modifiers_name == "Ctrl" {
        if let KeyCode::Char(c) = key.code {
            let lower = c.to_ascii_lowercase();
            if lower == 'c' || lower == 'd' {
                // TUI 中断支持：busy 时 Ctrl+C 取消当前 turn（不退出 TUI）；
                // idle 时 Ctrl+C 退出 TUI（原行为）。
                if busy && lower == 'c' {
                    return InputAction::InterruptTurn;
                }
                return input.handle_key(None, "CtrlC");
            }
            // Ctrl+B → toggle sidebar (tmux convention)
            if lower == 'b' {
                return InputAction::ToggleSidebar;
            }
            // Ctrl+T → toggle latest tool card collapse state.
            // P1 重构：交互式折叠/展开，配合结构化 OutputView。
            if lower == 't' {
                return InputAction::ToggleToolCard;
            }
            // Ctrl+J → newline (multi-line input)
            if lower == 'j' {
                return input.handle_key(None, "Newline");
            }
            // Bug L5 修复：Ctrl+V → 主动粘贴剪贴板内容。
            // 在 conhost（Windows Console Host）下 bracketed paste
            // (DECSET 2004) 不生效，Ctrl+V 被 crossterm 当作普通键事件，
            // route_key 默认走 `KeyCode::Char('v')` 分支插入字面 'v'。
            // 这里拦截 Ctrl+V，主动读剪贴板（PowerShell Get-Clipboard，
            // ~100ms 开销，用户主动操作可接受）并 insert_paste 把整段
            // 内容（含 \n）原子插入 buffer，避免多行被切成多次 Submit。
            // Windows Terminal 等支持 bracketed paste 的终端会先触发
            // Event::Paste（在 main loop 中处理），不会走到这里；
            // 此分支仅作 conhost 兜底。
            if lower == 'v' {
                paste_diag_log("Ctrl+V 按键事件触发，读取剪贴板");
                if let Ok(text) = crate::paste::read_clipboard_text() {
                    paste_diag_log(&format!(
                        "  剪贴板读取成功: {} 字节, {} 行",
                        text.len(),
                        text.lines().count()
                    ));
                    if !text.is_empty() {
                        input.insert_paste(&text);
                        paste_diag_log(&format!(
                            "  insert_paste 后 buffer={} 字节",
                            input.buffer().len()
                        ));
                    } else {
                        paste_diag_log("  剪贴板为空，不插入");
                    }
                } else {
                    paste_diag_log("  剪贴板读取失败");
                }
                return InputAction::Continue;
            }
        }
    }

    // F2 → toggle sidebar (also)
    if let KeyCode::F(2) = key.code {
        return InputAction::ToggleSidebar;
    }

    // PgUp / PgDn → scroll output view (when slash menu is closed so we
    // don't steal navigation from menu browsing).
    if matches!(key.code, KeyCode::PageUp) {
        return InputAction::ScrollUp;
    }
    if matches!(key.code, KeyCode::PageDown) {
        return InputAction::ScrollDown;
    }

    // Alt+Up / Alt+Down → 滚动侧栏工具历史。
    // 与 output_view 的 Up/Down 单行滚动区分：Alt 修饰符专属 sidebar。
    // 让用户能在工具调用很多时回看更早的记录，松开 Alt 后自动跟随底部。
    if key.modifiers.contains(KeyModifiers::ALT) {
        if matches!(key.code, KeyCode::Up) {
            return InputAction::SidebarScrollUp;
        }
        if matches!(key.code, KeyCode::Down) {
            return InputAction::SidebarScrollDown;
        }
    }

    // `?` (Shift+/) when the input buffer is empty → toggle help overlay.
    // We check `input.buffer()` so users can still type `?` inside a prompt
    // they've already started composing.
    if let KeyCode::Char('?') = key.code {
        if input.buffer().is_empty() {
            return InputAction::ToggleHelp;
        }
    }

    // Up/Down: when the slash menu is closed, scroll the output view one
    // line at a time (more natural than forcing the user to use PgUp/PgDn).
    // When the menu is open, Up/Down navigate the menu (handled inside
    // InputLine::handle_key → MenuUp/MenuDown).
    if !input.menu_open() {
        if matches!(key.code, KeyCode::Up) {
            return InputAction::ScrollUpLine;
        }
        if matches!(key.code, KeyCode::Down) {
            return InputAction::ScrollDownLine;
        }
    }

    // Map KeyCode to logical name expected by InputLine::handle_key
    let logical = match key.code {
        KeyCode::Char(c) => return input.handle_key(Some(c), ""),
        KeyCode::Enter => "Enter",
        KeyCode::Esc => "Esc",
        KeyCode::BackTab => "Tab",
        KeyCode::Backspace => "Backspace",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Tab => "Tab",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        _ => return InputAction::Ignore,
    };
    input.handle_key(None, logical)
}

fn render_menu(menu: &mut SlashMenu, f: &mut ratatui::Frame, area: Rect) {
    match menu.level() {
        crate::tui::slash_menu::MenuLevel::Sub => render_sub_menu(menu, f, area),
        crate::tui::slash_menu::MenuLevel::Top => render_top_menu(menu, f, area),
    }
}

fn render_top_menu(menu: &mut SlashMenu, f: &mut ratatui::Frame, area: Rect) {
    let visible = menu.visible_window();
    let selected_idx = menu.selected_index();
    let scroll = menu.scroll_offset();

    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let abs_idx = scroll + i;
            let is_selected = Some(abs_idx) == selected_idx;
            let text = format_menu_item(spec);
            if is_selected {
                Line::from(Span::styled(
                    text,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(text)
            }
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title(format!(
        "命令 ({}/{})",
        menu.total_count(),
        menu.all_items_count()
    ));
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

/// 二级菜单渲染：显示父命令的子选项列表。
/// 每行格式：`  值  中文标签  [参数提示]`
fn render_sub_menu(menu: &mut SlashMenu, f: &mut ratatui::Frame, area: Rect) {
    let visible = menu.sub_visible_window();
    let selected_idx = menu.sub_selected_index();
    let scroll = menu.sub_scroll_offset();
    let parent = menu.parent_name().unwrap_or("");

    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let abs_idx = scroll + i;
            let is_selected = Some(abs_idx) == selected_idx;
            // 格式：`  list  列出所有...  [需参数: 服务器名]`
            let mut spans = vec![Span::raw("  ")];
            spans.push(Span::styled(
                format!("{:<12}", opt.value),
                Style::default().fg(Color::Yellow),
            ));
            spans.push(Span::raw("  "));
            spans.push(Span::raw(opt.label));
            if opt.requires_arg {
                if let Some(hint) = opt.arg_hint {
                    spans.push(Span::styled(
                        format!("  [需参数: {hint}]"),
                        Style::default().fg(Color::Magenta),
                    ));
                } else {
                    spans.push(Span::styled(
                        "  [需参数]",
                        Style::default().fg(Color::Magenta),
                    ));
                }
            }
            let line = Line::from(spans);
            if is_selected {
                Line::from(vec![Span::styled(
                    line.to_string(),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )])
            } else {
                line
            }
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title(format!(
        " /{parent} 子选项 ({}) — Esc 返回 ",
        menu.sub_total_count()
    ));
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

/// Keybindings overlay shown when the user presses `?`.
///
/// Renders a centered modal listing all TUI keybindings. The modal is purely
/// informational — closing it (via `?`, Esc, or any other key when the
/// overlay is modal) returns the user to the previous state.
fn render_help_overlay(f: &mut ratatui::Frame, area: Rect) {
    // Modal size: 50% width, ~70% height, centered.
    let modal_w = (area.width / 2).clamp(50, 80);
    let modal_h = (area.height * 7 / 10).clamp(20, 30);
    let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect {
        x: modal_x,
        y: modal_y,
        width: modal_w,
        height: modal_h,
    };

    // Dim the background slightly by rendering a transparent overlay over
    // the full area first. ratatui doesn't have native "dim", so we just
    // render the modal block with a strong border to make it pop.
    let entries: &[(&str, &str)] = &[
        ("Enter", "提交当前输入"),
        ("Shift+Enter / Ctrl+J", "插入换行（多行输入）"),
        ("Ctrl+C / Ctrl+D", "退出 TUI（或取消当前轮次）"),
        ("Esc", "关闭菜单 / 浮层 / 清空输入"),
        ("Tab", "接受选中的斜杠命令补全"),
        ("Up / Down", "滚动输出（菜单开启时用于导航菜单）"),
        ("PgUp / PgDn", "滚动输出视图 上 / 下 一屏"),
        ("/", "打开斜杠命令菜单（模糊过滤）"),
        ("F2 / Ctrl+B", "切换右侧侧栏"),
        ("Alt+Up / Alt+Down", "滚动侧栏工具历史（看更早 / 回最新）"),
        ("Ctrl+T", "折叠 / 展开最近一个工具卡片"),
        ("鼠标左键", "点击工具卡片切换折叠 / 展开"),
        ("Shift+鼠标拖拽", "终端原生选中复制（绕过程序鼠标捕获）"),
        (
            "粘贴 (Ctrl+V)",
            "整段粘贴（含多行）作为一个块插入，不立即提交",
        ),
        ("?", "切换此快捷键浮层"),
        ("/help", "在输出区显示完整帮助"),
        ("/session pick", "交互式会话选择器"),
        ("/search <query>", "搜索对话历史"),
        ("/undo", "撤销最近一次文件编辑"),
        ("/diff", "显示 git diff（彩色分页）"),
    ];

    let lines: Vec<Line> = entries
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("  {:<22}", key),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(desc.to_string()),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " 快捷键（按 ? 或 Esc 关闭） ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Yellow));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left);
    f.render_widget(paragraph, modal_area);
}

/// Execute a turn in a background thread. Returns the result.
/// This runs on a worker thread so the main event loop can continue rendering
/// and processing keyboard events (e.g., Ctrl+C to exit) during streaming.
///
/// **Bug L3 修复**：函数接收 `&mut LiveCli` 而非 own `LiveCli`，调用方
/// （worker 线程闭包）保留 cli 的所有权。配合 `catch_unwind` 包裹调用，
/// panic 时 cli 仍在 worker 线程的局部变量中，可以通过 channel 返回主线程，
/// 避免每次 panic 都丢失 cli 导致 TUI 卡死在"turn 运行中"状态。
fn execute_turn(
    cli: &mut LiveCli,
    line: &str,
    output_handle: &Arc<Mutex<crate::tui::output_view::OutputBuffer>>,
    status_state: &Arc<Mutex<StatusBarState>>,
    tool_history_shared: &Arc<Mutex<ToolHistory>>,
) -> Result<(), String> {
    use crate::streaming::{StatusEmitter, StatusEvent};

    let output_handle = Arc::clone(output_handle);
    let status_handle = Arc::clone(status_state);
    let tool_history_shared = Arc::clone(tool_history_shared);
    // Track tool calls during this turn for the timeline summary
    let tool_history: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));

    let tool_history_for_closure = Arc::clone(&tool_history);
    let tool_history_for_sidebar = Arc::clone(&tool_history_shared);
    let output_for_closure = Arc::clone(&output_handle);
    // P1 修复：tool input 缓存，供 ToolResult 时取回用于 edit_file diff 显示。
    // key = tool_use_id, value = tool input json string
    let tool_input_cache: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let tool_input_cache_for_closure = Arc::clone(&tool_input_cache);

    let emitter: StatusEmitter = Arc::new(move |event: StatusEvent| {
        match event {
            StatusEvent::TextDelta(text) => {
                if let Ok(mut buf) = output_handle.lock() {
                    buf.append(&text);
                }
            }
            StatusEvent::ToolUse { id, name, input } => {
                // P1 修复：缓存 tool input，供 ToolResult 时取回用于 diff 显示。
                if let Ok(mut cache) = tool_input_cache_for_closure.lock() {
                    cache.insert(id.clone(), input.clone());
                }
                // P1 重构：用结构化 ToolCard entry 替代纯文本 append。
                // ToolCard 默认 collapsed=false（执行中），result 到达后
                // 由 complete_tool_card 设置 result 并切换为 collapsed=true。
                // timestamp 由工厂方法自动填充（消息时间戳）。
                if let Ok(mut buf) = output_handle.lock() {
                    buf.push_entry(crate::tui::output_view::OutputEntry::tool_card_start(
                        id.clone(),
                        name.clone(),
                        input.clone(),
                    ));
                }
            }
            StatusEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => {
                // Track for timeline
                if let Ok(mut history) = tool_history_for_closure.lock() {
                    history.push((name.clone(), is_error));
                }
                // Mirror to shared sidebar state so it can render live progress
                if let Ok(mut sidebar_history) = tool_history_for_sidebar.lock() {
                    sidebar_history.push((name.clone(), is_error));
                }
                // P1 重构：用 complete_tool_card 更新已存在的 ToolCard entry，
                // 设置 result 并切换为折叠状态。渲染时根据 collapsed 状态
                // 动态生成可见行，支持 Tab 键交互式折叠/展开。
                if let Ok(mut buf) = output_handle.lock() {
                    buf.complete_tool_card(&id, output.clone(), is_error);
                }
            }
            StatusEvent::Usage(usage) => {
                if let Ok(mut guard) = status_handle.lock() {
                    guard.turn_usage.input_tokens += usage.input_tokens;
                    guard.turn_usage.output_tokens += usage.output_tokens;
                    guard.turn_usage.cache_creation_input_tokens +=
                        usage.cache_creation_input_tokens;
                    guard.turn_usage.cache_read_input_tokens += usage.cache_read_input_tokens;
                }
            }
            StatusEvent::StreamStart => {
                if let Ok(mut guard) = status_handle.lock() {
                    guard.reset_turn();
                }
                // Reset tool history for new turn (used for timeline summary)
                if let Ok(mut history) = tool_history.lock() {
                    history.clear();
                }
                // P1 修复：不再在 StreamStart 清空 sidebar 历史。
                // 原因：StreamStart 在每个 turn 开始时触发，清空 sidebar 历史
                // 导致用户看不到工具调用记录。sidebar 历史应在 Submit 新 turn
                // 时清空（已在 Submit 分支 L454 处理），让用户在 turn 进行中
                // 和结束后都能看到本次 turn 的工具调用记录。
            }
            StatusEvent::MessageStop => {
                // P1 修复：AI 回复末尾追加换行分隔符，避免下次 Submit echo
                // 的 `> {line}` 紧贴 AI 回复末尾。原 TextDelta 流式 append
                // 没有 `\n` 结尾，导致从第二次发送开始用户消息与 AI 回复
                // 混在一起没有分行。
                if let Ok(mut buf) = output_for_closure.lock() {
                    buf.append("\n\n");
                }
                // Render tool timeline summary if any tools were called
                // P1 重构：用 Timeline entry 替代纯文本 append
                if let Ok(history) = tool_history.lock() {
                    if !history.is_empty() {
                        let timeline = crate::tui::tool_card::render_tool_timeline(&history);
                        if let Ok(mut buf) = output_for_closure.lock() {
                            buf.push_entry(crate::tui::output_view::OutputEntry::timeline(
                                timeline,
                            ));
                        }
                    }
                }
                if let Ok(mut guard) = status_handle.lock() {
                    if guard.streaming {
                        guard.finish_turn();
                    }
                }
            }
            StatusEvent::Thinking {
                char_count,
                redacted,
            } => {
                // Phase 3: render a short thinking-block summary into the
                // output view, mirroring the stdout path in streaming.rs.
                let summary = if redacted {
                    "\n▶ Thinking block hidden by provider\n".to_string()
                } else if let Some(char_count) = char_count {
                    format!("\n▶ Thinking ({char_count} chars hidden)\n")
                } else {
                    "\n▶ Thinking hidden\n".to_string()
                };
                if let Ok(mut buf) = output_handle.lock() {
                    buf.append(&summary);
                }
            }
            StatusEvent::StreamError {
                message,
                recoverable,
            } => {
                // P0-1 修复：consume_stream 内 9 处错误路径 emit 的事件。
                // 之前所有错误路径都不 emit，TUI 收不到信号导致 streaming=true
                // 永久保留，UI 假死。现在收到此事件立即：
                // 1. 向 OutputView 追加错误提示（区分可重试/致命）
                // 2. 调用 finish_turn() 退出 streaming 状态
                let banner = if recoverable {
                    format!("\n[error] 流式错误（可重试）：{message}\n")
                } else {
                    format!("\n[error] 流式错误：{message}\n")
                };
                if let Ok(mut buf) = output_for_closure.lock() {
                    buf.append(&banner);
                }
                if let Ok(mut guard) = status_handle.lock() {
                    if guard.streaming {
                        guard.finish_turn();
                    }
                }
            }
        }
    });

    cli.set_status_emitter(emitter);
    cli.set_tui_mode(true);

    // 细粒度诊断：注入 diag callback，在 run_turn 关键路径埋点写入 claw-diag.log。
    cli.set_diag_callback(Box::new(|msg| {
        crate::diag_log(&msg);
    }));

    let result = cli.run_turn(line);

    cli.clear_diag_callback();
    cli.clear_status_emitter();
    cli.set_tui_mode(false);
    // Ensure streaming is marked as finished even on error
    if let Ok(mut guard) = status_state.lock() {
        if guard.streaming {
            guard.finish_turn();
        }
    }

    result.map_err(|e| format!("{e}"))
}

/// 在 worker 线程执行本地斜杠命令（如 /help, /clear, /status）。
///
/// 与 `execute_turn` 不同，此函数调用 `LiveCli::handle_repl_command` 在本地
/// 处理命令，不会把输入发给 AI。命令的 println 输出通过 `tui_output`
/// 捕获到 OutputBuffer（在调用前已由 Submit 分支设置）。
///
/// 执行完成后清除 `tui_output`，避免后续轮次误捕获。
///
/// **Bug L3 修复**：接收 `&mut LiveCli` 而非 own，配合 `catch_unwind`
/// 保证 panic 时 cli 仍可恢复。
fn execute_slash_command(cli: &mut LiveCli, command: SlashCommand) -> Result<(), String> {
    let result = cli
        .handle_repl_command(command)
        .map(|should_persist| {
            if should_persist {
                let _ = cli.persist_session();
            }
        })
        .map_err(|e| format!("{e}"));
    // 清除 tui_output，避免后续 AI 对话轮次误捕获 println
    cli.clear_tui_output();
    result
}

/// TUI 原生会话选择器状态。
///
/// 由 `/session pick` 触发：从 list_managed_sessions 加载会话列表，
/// 在主区域上 overlay 一个 ratatui List widget，用户用上下键选中、
/// Enter 确认切换、Esc 取消。选中后通过 `pending_switch_target` 传回
/// 主循环执行 switch 逻辑。
struct SessionPickerState {
    /// 全部候选会话（按修改时间倒序）。
    sessions: Vec<crate::session_mgr::ManagedSessionSummary>,
    /// 当前活动会话 ID（用于高亮标记）。
    active_session_id: String,
    /// 当前选中的索引。
    selected: usize,
    /// 滚动偏移。
    scroll: usize,
}

impl SessionPickerState {
    fn new(
        sessions: Vec<crate::session_mgr::ManagedSessionSummary>,
        active_session_id: String,
    ) -> Self {
        // 默认选中活动会话（若存在），否则选第一个
        let selected = sessions
            .iter()
            .position(|s| s.id == active_session_id)
            .unwrap_or(0);
        Self {
            sessions,
            active_session_id,
            selected,
            scroll: 0,
        }
    }

    fn move_up(&mut self) {
        if self.selected == 0 {
            self.selected = self.sessions.len().saturating_sub(1);
        } else {
            self.selected -= 1;
        }
        self.adjust_scroll();
    }

    fn move_down(&mut self) {
        if self.selected + 1 >= self.sessions.len() {
            self.selected = 0;
        } else {
            self.selected += 1;
        }
        self.adjust_scroll();
    }

    fn adjust_scroll(&mut self) {
        const MAX_VISIBLE: usize = 12;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_VISIBLE {
            self.scroll = self.selected + 1 - MAX_VISIBLE;
        }
    }

    /// 当前选中的会话（用于 Enter 确认后取出执行 switch）。
    fn selected_session(&self) -> Option<&crate::session_mgr::ManagedSessionSummary> {
        self.sessions.get(self.selected)
    }
}

/// 渲染会话选择器 overlay。
fn render_session_picker(
    state: &SessionPickerState,
    f: &mut ratatui::Frame,
    area: Rect,
) {
    use ratatui::widgets::{List, ListItem, ListState};

    // 居中显示，占主区域 80% 宽、最多 20 行高
    let popup_width = (area.width as usize * 8 / 10).max(40) as u16;
    let popup_height = area.height.min(20);
    let popup_x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: popup_height,
    };

    let items: Vec<ListItem> = state
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let is_active = s.id == state.active_session_id;
            let marker = if is_active { "*" } else { " " };
            let modified_age = crate::session_mgr::format_session_modified_age(
                s.modified_epoch_millis,
            );
            let branch = s
                .branch_name
                .as_deref()
                .unwrap_or("-");
            let line = format!(
                " {marker} [{i:>3}] {:<24} msgs={:<4} modified={modified_age} branch={branch}",
                s.id, s.message_count
            );
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 选择会话 — ↑/↓ 选中，Enter 切换，Esc 取消 "),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    let mut list_state = ListState::default();
    list_state.select(Some(state.selected));
    f.render_stateful_widget(list, popup_area, &mut list_state);
}

fn initialize_status(state: &Arc<Mutex<StatusBarState>>, cli: &LiveCli) {
    let mut guard = state.lock().expect("StatusBarState poisoned");
    guard.model = cli.model_snapshot().to_string();
    guard.permission_mode = cli.permission_mode_label().to_string();
    guard.session_id = cli.session_id_snapshot().to_string();
    sync_status_from_cli_inner(&mut guard, cli);
}

fn sync_status_from_cli(state: &Arc<Mutex<StatusBarState>>, cli: &LiveCli) {
    let mut guard = state.lock().expect("StatusBarState poisoned");
    sync_status_from_cli_inner(&mut guard, cli);
}

fn sync_status_from_cli_inner(guard: &mut StatusBarState, cli: &LiveCli) {
    // BUG 4 fix: do NOT overwrite `cumulative_usage` from
    // `cli.cumulative_usage_snapshot()` here. In TUI mode the StatusEmitter
    // is the single source of truth for usage: `StatusEvent::Usage` events
    // accumulate into `turn_usage`, and `MessageStop` folds `turn_usage` into
    // `cumulative_usage` (see execute_turn's emitter closure). If we overwrote
    // cumulative_usage with `cli.cumulative_usage` (which itself was bumped by
    // `LiveCli::accumulate_usage(summary.usage)` on the success path), the
    // same usage delta would be counted twice — once via the emitter path and
    // once via the cli snapshot. Leaving cumulative_usage untouched here keeps
    // the emitter as the sole authority.
    if let Ok(cwd) = std::env::current_dir() {
        guard.cwd = format!("{}", cwd.display());
    }
    if let Some(branch) = cli.git_branch_snapshot() {
        guard.git_branch = branch;
    }
    if let Some(badge) = cli.goal_badge_snapshot() {
        guard.goal_badge = badge;
    } else {
        guard.goal_badge.clear();
    }
    guard.poor_mode = runtime::poor_mode::is_active();
    guard.provider =
        crate::provider_label(api::detect_provider_kind(cli.model_snapshot())).to_string();
    guard.reasoning_effort = cli.reasoning_effort();
    guard.turn_count = cli.turns_snapshot();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::{StatusEmitter, StatusEvent};
    use crate::tui::output_view::{OutputBuffer, OutputView};
    use crate::tui::status_bar::StatusBarState;
    use runtime::TokenUsage;
    use std::sync::{Arc, Mutex};

    /// Build an emitter identical to the one handle_submit constructs, for
    /// direct testing without spinning up a full LiveCli.
    fn build_test_emitter(
        output_handle: Arc<Mutex<OutputBuffer>>,
        status_handle: Arc<Mutex<StatusBarState>>,
    ) -> StatusEmitter {
        Arc::new(move |event: StatusEvent| {
            match event {
                StatusEvent::TextDelta(text) => {
                    if let Ok(mut buf) = output_handle.lock() {
                        buf.append(&text);
                    }
                }
                StatusEvent::Usage(usage) => {
                    if let Ok(mut guard) = status_handle.lock() {
                        guard.turn_usage.input_tokens += usage.input_tokens;
                        guard.turn_usage.output_tokens += usage.output_tokens;
                        guard.turn_usage.cache_creation_input_tokens +=
                            usage.cache_creation_input_tokens;
                        guard.turn_usage.cache_read_input_tokens += usage.cache_read_input_tokens;
                    }
                }
                StatusEvent::StreamStart => {
                    if let Ok(mut guard) = status_handle.lock() {
                        guard.reset_turn();
                    }
                }
                StatusEvent::MessageStop => {
                    if let Ok(mut guard) = status_handle.lock() {
                        if guard.streaming {
                            guard.finish_turn();
                        }
                    }
                }
                StatusEvent::ToolUse { .. } => {}
                StatusEvent::ToolResult { .. } => {}
                StatusEvent::Thinking {
                    char_count,
                    redacted,
                } => {
                    let summary = if redacted {
                        "\n▶ Thinking block hidden by provider\n".to_string()
                    } else if let Some(char_count) = char_count {
                        format!("\n▶ Thinking ({char_count} chars hidden)\n")
                    } else {
                        "\n▶ Thinking hidden\n".to_string()
                    };
                    if let Ok(mut buf) = output_handle.lock() {
                        buf.append(&summary);
                    }
                }
                StatusEvent::StreamError {
                    message,
                    recoverable,
                } => {
                    // P0-1 修复：测试 emitter 同步增加 StreamError 处理分支
                    let banner = if recoverable {
                        format!("\n[error] 流式错误（可重试）：{message}\n")
                    } else {
                        format!("\n[error] 流式错误：{message}\n")
                    };
                    if let Ok(mut buf) = output_handle.lock() {
                        buf.append(&banner);
                    }
                    if let Ok(mut guard) = status_handle.lock() {
                        if guard.streaming {
                            guard.finish_turn();
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn emitter_textdelta_appends_to_output_view() {
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, Arc::clone(&status));

        emitter(StatusEvent::TextDelta("Hello ".to_string()));
        emitter(StatusEvent::TextDelta("world!".to_string()));

        // Text 渲染会带时间戳前缀 [HH:MM:SS]
        let snap = output_view.snapshot();
        assert!(snap.contains("Hello world!"));
    }

    #[test]
    fn emitter_usage_accumulates_into_turn_usage() {
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, Arc::clone(&status));

        let usage1 = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        let usage2 = TokenUsage {
            input_tokens: 200,
            output_tokens: 75,
            ..Default::default()
        };
        emitter(StatusEvent::Usage(usage1));
        emitter(StatusEvent::Usage(usage2));

        let guard = status.lock().unwrap();
        assert_eq!(guard.turn_usage.input_tokens, 300);
        assert_eq!(guard.turn_usage.output_tokens, 125);
    }

    #[test]
    fn emitter_streamstart_then_messagestop_folds_turn_into_cumulative() {
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, Arc::clone(&status));

        emitter(StatusEvent::StreamStart);
        {
            let guard = status.lock().unwrap();
            assert!(guard.streaming);
        }

        let usage = TokenUsage {
            input_tokens: 500,
            output_tokens: 250,
            ..Default::default()
        };
        emitter(StatusEvent::Usage(usage));

        emitter(StatusEvent::MessageStop);
        {
            let guard = status.lock().unwrap();
            assert!(!guard.streaming);
            assert_eq!(guard.cumulative_usage.input_tokens, 500);
            assert_eq!(guard.cumulative_usage.output_tokens, 250);
            assert_eq!(guard.turn_usage.total_tokens(), 0);
        }
    }

    #[test]
    fn emitter_does_not_panic_under_normal_usage() {
        // Verify the emitter doesn't panic when called without lock contention.
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::StreamStart);
        emitter(StatusEvent::TextDelta("safe".to_string()));
        emitter(StatusEvent::MessageStop);
    }

    #[test]
    fn emitter_thinking_hidden_renders_summary_without_char_count() {
        // Phase 3: streaming ThinkingDelta carries no char_count — the
        // emitter should render the "hidden" variant of the summary.
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::Thinking {
            char_count: None,
            redacted: false,
        });

        let snapshot = output_view.snapshot();
        assert!(
            snapshot.contains("▶ Thinking hidden"),
            "expected '▶ Thinking hidden' in snapshot, got: {snapshot:?}"
        );
        assert!(
            !snapshot.contains("chars hidden"),
            "should not contain char count when None, got: {snapshot:?}"
        );
    }

    #[test]
    fn emitter_thinking_with_char_count_renders_counted_summary() {
        // Phase 3: non-streaming Thinking block carries a concrete char_count.
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::Thinking {
            char_count: Some(42),
            redacted: false,
        });

        let snapshot = output_view.snapshot();
        assert!(
            snapshot.contains("▶ Thinking (42 chars hidden)"),
            "expected '▶ Thinking (42 chars hidden)' in snapshot, got: {snapshot:?}"
        );
    }

    #[test]
    fn emitter_thinking_redacted_renders_provider_redacted_summary() {
        // Phase 3: RedactedThinking blocks should surface the provider-side
        // redaction so users know why content is missing.
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::Thinking {
            char_count: None,
            redacted: true,
        });

        let snapshot = output_view.snapshot();
        assert!(
            snapshot.contains("▶ Thinking block hidden by provider"),
            "expected provider-redacted summary, got: {snapshot:?}"
        );
    }

    #[test]
    fn markdown_to_ansi_to_text_conversion_preserves_content() {
        // Phase 3.2: verify the rendering pipeline used by run_event_loop:
        //   snapshot (raw markdown) → TerminalRenderer::markdown_to_ansi →
        //   ansi_to_tui::IntoText::into_text → ratatui Text<'static>.
        // The conversion must not drop plain text content and must produce
        // at least one styled span for markdown constructs (e.g. headings).
        let markdown =
            "# Heading\n\nSome **bold** text and a code block:\n\n```rust\nfn main() {}\n```";
        let renderer = TerminalRenderer::new();
        let ansi = renderer.markdown_to_ansi(markdown);
        let text = ansi
            .into_text()
            .expect("ansi-to-tui conversion should succeed");
        // Flatten all spans into a single string for content checks.
        let flattened: String = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            flattened.contains("Heading"),
            "heading text should survive conversion, got: {flattened:?}"
        );
        assert!(
            flattened.contains("bold"),
            "bold text should survive conversion, got: {flattened:?}"
        );
        assert!(
            flattened.contains("fn main()"),
            "code block content should survive conversion, got: {flattened:?}"
        );
    }

    #[test]
    fn markdown_to_ansi_to_text_empty_input_yields_empty_text() {
        // Phase 3.2: empty input should produce empty Text (or at least not
        // panic) — run_event_loop guards against this with is_empty() but
        // the conversion itself should also be safe.
        let renderer = TerminalRenderer::new();
        let ansi = renderer.markdown_to_ansi("");
        let text = ansi.into_text().expect("empty ansi should convert cleanly");
        assert!(
            text.lines.is_empty() || text.lines.iter().all(|l| l.spans.is_empty()),
            "empty markdown should yield empty text, got: {text:?}"
        );
    }

    // ===== IncrementalRenderer 单元测试 =====
    //
    // 验证增量渲染的边界条件：
    // - 空快照、hash 命中、段落边界推进、buffer 裁剪重置
    // - 增量渲染结果与全量渲染结果内容一致

    fn flatten_text(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn incremental_renderer_empty_snapshot_yields_empty_text() {
        let mut r = IncrementalRenderer::new();
        let text = r.render("");
        assert!(
            text.lines.is_empty(),
            "empty snapshot should yield empty Text, got: {text:?}"
        );
    }

    #[test]
    fn incremental_renderer_hash_hit_returns_cached_text() {
        // 同一 snapshot 渲染两次：第二次应命中 hash 缓存，返回相同内容。
        let mut r = IncrementalRenderer::new();
        let snapshot = "# Heading\n\nSome **bold** text.\n\n";
        let first = r.render(snapshot);
        let second = r.render(snapshot);
        let first_flat = flatten_text(&first);
        let second_flat = flatten_text(&second);
        assert_eq!(
            first_flat, second_flat,
            "hash hit should return equivalent content"
        );
        assert!(first_flat.contains("Heading"));
        assert!(first_flat.contains("bold"));
    }

    #[test]
    fn incremental_renderer_appends_new_paragraph_incrementally() {
        // 第一次渲染完整段落，第二次追加新段落：已完成段落应被缓存，
        // 只有新增段落触发渲染。最终内容应等于全量渲染。
        let mut r = IncrementalRenderer::new();
        let part1 = "# First Heading\n\nFirst paragraph.\n\n";
        let _ = r.render(part1);

        // 追加第二个段落（模拟流式 TextDelta 到达）。
        let part2 = format!("{part1}## Second Heading\n\nSecond paragraph.\n\n");
        let incremental_result = r.render(&part2);

        // 全量渲染作为对照基准。
        let renderer = TerminalRenderer::new();
        let full_text = renderer
            .markdown_to_ansi(&part2)
            .into_text()
            .expect("full render should succeed");

        let inc_flat = flatten_text(&incremental_result);
        let full_flat = flatten_text(&full_text);
        assert!(
            inc_flat.contains("First Heading"),
            "incremental should contain first heading: {inc_flat:?}"
        );
        assert!(
            inc_flat.contains("Second Heading"),
            "incremental should contain second heading: {inc_flat:?}"
        );
        assert_eq!(
            inc_flat, full_flat,
            "incremental render should match full render content"
        );
    }

    #[test]
    fn incremental_renderer_pending_segment_is_rendered_each_time() {
        // 没有 \n\n 结尾的未完成段落，每次都重新渲染。
        // 模拟流式输出：先 "Hello"，再 "Hello world"。
        let mut r = IncrementalRenderer::new();
        let _ = r.render("Hello");
        let result = r.render("Hello world");
        let flat = flatten_text(&result);
        assert!(
            flat.contains("Hello world"),
            "pending segment should reflect latest content: {flat:?}"
        );
    }

    #[test]
    fn incremental_renderer_resets_when_buffer_shrinks() {
        // buffer 被 trim 后缩短：completed_bytes > snapshot.len()，应重置。
        let mut r = IncrementalRenderer::new();
        let _ = r.render("# Long Heading\n\nLong paragraph.\n\n");
        // 模拟 trim 后 buffer 缩短到只有 "Short"
        let result = r.render("Short");
        let flat = flatten_text(&result);
        assert!(
            flat.contains("Short"),
            "after reset, should render new short content: {flat:?}"
        );
        assert!(
            !flat.contains("Long Heading"),
            "after reset, stale completed content should be gone: {flat:?}"
        );
    }

    #[test]
    fn incremental_renderer_pending_cache_avoids_re_render_on_unchanged_pending() {
        // 当 snapshot 整体未变时（hash 命中），直接返回缓存。
        // 当只有 pending 部分未变但 completed 推进时，pending_cache 应命中。
        let mut r = IncrementalRenderer::new();
        // 第一次：未完成段落 "pending text"
        let _ = r.render("pending text");
        // 第二次：仍为 "pending text"（整体 hash 命中）
        let result1 = r.render("pending text");
        // 第三次：仍然不变
        let result2 = r.render("pending text");
        let flat1 = flatten_text(&result1);
        let flat2 = flatten_text(&result2);
        assert_eq!(
            flat1, flat2,
            "unchanged snapshot should yield identical text"
        );
    }
}
