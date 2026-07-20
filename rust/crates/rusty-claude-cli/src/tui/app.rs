//! TuiApp — main ratatui event loop integrating with LiveCli.
//!
//! Owns the alternate-screen Terminal, InputLine, SlashMenu, OutputView,
//! and shared StatusBarState. Routes keyboard events to InputLine / Menu,
//! submits Enter to `LiveCli::run_turn` (capturing output via OutputView
//! sink + StatusEmitter callback for live status updates).

#![allow(dead_code, unused_imports, unused_variables, unused_assignments, clippy::too_many_lines)]

use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;

// Phase 3.2: TerminalRenderer is used to convert markdown → ANSI; ansi_to_tui
// then converts ANSI → ratatui Text<'static> so Paragraph can render styled
// spans (headings, code blocks, bold/italic, etc.) instead of raw text.
use ansi_to_tui::IntoText;
use crate::render::TerminalRenderer;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

use crate::app::LiveCli;
use crate::tui::input_line::{InputAction, InputLine};
use crate::tui::output_view::OutputView;
use crate::tui::sidebar::{render_sidebar, ToolHistory};
use crate::tui::slash_menu::{format_menu_item, SlashMenu};
use crate::tui::status_bar::{StatusBar, StatusBarState};
// 斜杠命令本地分发：TUI 下 /help 等命令应在本地处理，而非发给 AI。
// 修复"输入 /help 发送给 AI"的 bug。
use commands::SlashCommand;

/// Entry point: run the TUI REPL until user exits.
pub(crate) fn run_tui_repl(cli: LiveCli) -> Result<(), Box<dyn std::error::Error>> {
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

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        let result = run_event_loop(&mut terminal, cli);
        // Restore terminal on exit (inside closure so it runs even on panic via ?).
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste
        )?;
        terminal.show_cursor()?;
        result
    })();

    // If the closure failed after EnterAlternateScreen but before the inner
    // restore, we still need to clean up. Check if we're still in raw mode.
    if result.is_err() {
        let _ = disable_raw_mode();
        // Best-effort: try to leave alternate screen (may already be gone).
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
        let _ = execute!(stdout, crossterm::event::DisableMouseCapture);
        let _ = execute!(stdout, crossterm::event::DisableBracketedPaste);
        let _ = execute!(stdout, crossterm::cursor::Show);
    }

    result
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
        let split_pos = snapshot
            .rfind("\n\n")
            .map(|p| p + 2)
            .unwrap_or(0);

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

/// Result of a turn executed in a background thread.
struct TurnResult {
    cli: LiveCli,
    result: Result<(), String>,
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cli: LiveCli,
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

    // Sidebar: visible by default on wide terminals (>=100 cols), toggleable
    // via F2 / Ctrl+B. Holds a shared tool-history mirror so the sidebar
    // can show live tool-call progress during a streaming turn.
    let mut sidebar_visible: bool = terminal.size().map(|s| s.width >= 100).unwrap_or(false);
    let tool_history_shared: Arc<Mutex<ToolHistory>> = Arc::new(Mutex::new(Vec::new()));

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

    // 鼠标点击支持：把 draw 闭包内的 main_area 和 scroll_y 缓存到 loop 外，
    // 这样 Event::Mouse 分支可以访问它们，把点击坐标映射到逻辑行号。
    // draw 闭包每次渲染后更新这两个值。
    let mut last_main_area: Rect = Rect::default();
    let mut last_scroll_y: u16 = 0;

    loop {
        // Check if a running turn has completed
        if let Some(ref rx) = turn_rx {
            match rx.try_recv() {
                Ok(turn_result) => {
                    if let Err(e) = turn_result.result {
                        let handle = output_view.shared_handle();
                        if let Ok(mut buf) = handle.lock() {
                            buf.append(&format!("\n[error] {e}\n"));
                        };
                    }
                    cli_holder = Some(turn_result.cli);
                    turn_rx = None;
                    turn_start = None;
                    if let Some(ref cli) = cli_holder {
                        sync_status_from_cli(&status_state, cli);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Turn still running, continue rendering
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Thread panicked; cli is lost, reset streaming state
                    turn_rx = None;
                    turn_start = None;
                    if let Ok(mut guard) = status_state.lock() {
                        if guard.streaming {
                            guard.finish_turn();
                        }
                    }
                }
            }
        }

        // Render
        terminal.draw(|f| {
            // Top-level vertical layout: main row (output+input) + status bar.
            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),     // main row (output + optional sidebar)
                    Constraint::Length(3),   // input + popup area
                    Constraint::Length(1),   // status bar
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
                    let guard = status_state.lock().expect("StatusBarState poisoned");
                    guard.clone()
                };
                let history_snapshot = tool_history_shared
                    .lock()
                    .map(|h| h.clone())
                    .unwrap_or_default();
                let sidebar_buf = f.buffer_mut();
                render_sidebar(cols[1], sidebar_buf, &state_snapshot, &history_snapshot);
                cols[0]
            } else {
                outer[0]
            };

            // Output area
            // 增量渲染：已完成段落永久缓存，只有最后一个未完成段落重新
            // 渲染。流式时单次 draw 从 20-100ms 降到 < 2ms。
            let output_text = output_view.snapshot();
            let output_rendered: Text<'static> = incremental.render(&output_text);
            // Compute scroll position: when scroll_offset is None (follow
            // mode), the Paragraph scrolls to its bottom automatically via
            // .scroll((max_scroll, 0)). When Some(n), we scroll n lines
            // above the bottom. Paragraph's scroll takes the top-left
            // corner's (row, col); rows beyond the visible area are hidden.
            //
            // BUG fix：旧实现用 `output_rendered.lines.len()`（逻辑行数）算
            // max_scroll，但启用了 `Wrap { trim: false }` 后长行会折成多个
            // 显示行。若按逻辑行算 max_scroll，follow mode 时 scroll_y 不
            // 足以滚到真正的底部，Paragraph 顶部渲染会把最后几显示行裁掉，
            // 看起来像"最后一行被输入框盖住"。这里按显示行计算：
            //   display_lines(line) = max(1, ceil(line_width / area_width))
            // 累加得到总显示行数，再算 max_scroll。
            let visible_height = main_area.height.saturating_sub(1) as usize; // -1 for top border
            let area_width = main_area.width as usize;
            use unicode_width::UnicodeWidthStr;
            let total_display_lines: usize = output_rendered
                .lines
                .iter()
                .map(|line| {
                    // ratatui 的 Line 是 Vec<Span>，需要累加所有 span 的 width。
                    // Span::content 是 Cow<str>，用 UnicodeWidthStr::width 计算视觉宽度。
                    let w: usize = line
                        .spans
                        .iter()
                        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                        .sum();
                    if w == 0 || area_width == 0 {
                        1
                    } else {
                        ((w + area_width - 1) / area_width).max(1)
                    }
                })
                .sum();
            let max_scroll = total_display_lines.saturating_sub(visible_height);
            let scroll_y = match scroll_offset {
                None => max_scroll,
                Some(offset) => max_scroll.saturating_sub(offset),
            };
            let scroll_label = match scroll_offset {
                None => String::new(),
                Some(offset) => format!(" [scroll -{offset}]"),
            };
            let output_paragraph = Paragraph::new(output_rendered)
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .title(format!("输出{scroll_label}")),
                )
                .scroll((scroll_y as u16, 0))
                .wrap(Wrap { trim: false });
            f.render_widget(output_paragraph, main_area);

            // Input area
            // 多行 buffer 支持：把整个 buffer（含 \n）交给 Paragraph 渲染。
            // 输入区高度为 3 行（outer[1]），减去 1 行 top border 后还有 2 行
            // 内容区。若 buffer 超过 2 行，超出部分不可见，但 Enter 仍能提交
            // 完整内容。这是 MVP 取舍：保证粘贴/多行编辑功能正确，渲染可见性
            // 后续可改为动态扩展输入区高度。
            let input_line = format!("> {}", input.buffer());
            let input_paragraph = Paragraph::new(input_line)
                .block(Block::default().borders(Borders::TOP).title("输入"));
            f.render_widget(input_paragraph, outer[1]);

            // Position the visible cursor inside the input area.
            // 多行 buffer 光标定位：Y 坐标按光标所在行号（capped to visible area），
            // X 坐标按当前行内光标左侧文本的显示宽度（不累加其他行）。
            // 旧实现用 `cursor_display_width()` 累加所有行宽度，对 "hello\nworld"
            // 返回 10，导致光标被定位到第 1 行第 12 列（超出可见区域）。
            let prompt_prefix_len: usize = 2; // "> "
            let (line_idx, _byte_offset_in_line, line_content_before_cursor) =
                input.cursor_line_and_column();
            let line_display_width = UnicodeWidthStr::width(line_content_before_cursor);
            let cursor_x = prompt_prefix_len + line_display_width;
            // 输入区可见内容行数 = outer[1].height - 1（top border）。一般 = 2。
            let input_content_height = outer[1].height.saturating_sub(1) as usize;
            // 把 line_idx 限制在可见区域内：超过则光标停在最后一行可见行。
            let visible_line_idx = line_idx.min(input_content_height.saturating_sub(1));
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
                    if let Some(query) = input.menu_query() {
                        menu.set_query(&query);
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
        if event::poll(poll_timeout)? {
            let ev = event::read()?;
            if let Event::Key(key) = ev {
                let action = route_key(&mut input, key);
                match action {
                    InputAction::Exit => break,
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
                        // Only submit if idle (no turn currently running)
                        if cli_holder.is_some() && turn_rx.is_none() {
                            // Re-enter follow mode so the user sees new output.
                            scroll_offset = None;
                            turn_start = Some(Instant::now());

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
                            let echo_handle = output_view.shared_handle();
                            if let Ok(mut buf) = echo_handle.lock() {
                                let current = buf.buffer();
                                if !current.is_empty() && !current.ends_with('\n') {
                                    buf.append("\n\n");
                                }
                                buf.append(&format!("> {line}\n\n"));
                            }

                            let mut cli = cli_holder.take().unwrap();
                            let output_handle = output_view.shared_handle();
                            let status_handle = Arc::clone(&status_state);
                            let tool_history_handle = Arc::clone(&tool_history_shared);

                            // 斜杠命令本地分发：先尝试解析为 SlashCommand。
                            // 如果是斜杠命令，本地执行 handle_repl_command
                            // （如 /help 显示命令列表、/clear 清会话等），
                            // 而不是当作普通输入发给 AI。
                            // 修复"输入 /help 发送给 AI"的 bug。
                            let slash_parsed = SlashCommand::parse(line.trim());

                            let (tx, rx) = mpsc::channel();
                            match slash_parsed {
                                Ok(Some(command)) => {
                                    // 本地命令：设置 tui_output 捕获 println，
                                    // 在 worker 线程执行 handle_repl_command。
                                    cli.set_tui_output(Arc::clone(&output_handle));
                                    std::thread::spawn(move || {
                                        let result = execute_slash_command(cli, command);
                                        let _ = tx.send(result);
                                    });
                                }
                                Ok(None) | Err(_) => {
                                    // 普通对话：发给 AI。清空工具历史。
                                    if let Ok(mut h) = tool_history_shared.lock() {
                                        h.clear();
                                    }
                                    std::thread::spawn(move || {
                                        let result = execute_turn(
                                            cli,
                                            &line,
                                            &output_handle,
                                            &status_handle,
                                            &tool_history_handle,
                                        );
                                        let _ = tx.send(result);
                                    });
                                }
                            }
                            turn_rx = Some(rx);
                        }
                    }
                    InputAction::MenuUp => menu.move_up(),
                    InputAction::MenuDown => menu.move_down(),
                    InputAction::MenuAccept => {
                        // Fill the input buffer with the selected command, then
                        // close the menu so the next Enter submits it. This
                        // gives the "select → Enter fills → Enter sends" UX:
                        // the user can review the completed command before
                        // sending, or edit it (e.g., add args to /search).
                        if let Some(spec) = menu.selected_spec() {
                            let completion = format!("/{}", spec.name);
                            input.accept_menu_completion(&completion);
                        }
                    }
                    InputAction::CloseMenu => {
                        // menu state already updated in input.handle_key
                    }
                    InputAction::Continue | InputAction::Ignore => {}
                }
            } else if let Event::Mouse(mouse) = ev {
                // 鼠标左键点击：若命中输出区，切换该行所在的 ToolCard 折叠状态。
                // 坐标映射：mouse.row 是终端绝对行号，需减去 main_area.y + 1
                // （+1 为顶部 border）得到相对行号，再加 scroll_y 得到逻辑行号。
                use crossterm::event::{MouseButton, MouseEventKind};
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && !help_visible
                    && last_main_area.height > 0
                {
                    let content_top = last_main_area.y + 1; // +1 for top border
                    let content_bottom = last_main_area.y + last_main_area.height;
                    if mouse.row >= content_top && mouse.row < content_bottom {
                        let relative_row = (mouse.row - content_top) as usize;
                        let logical_row = relative_row + last_scroll_y as usize;
                        let handle = output_view.shared_handle();
                        if let Ok(mut buf) = handle.lock() {
                            buf.toggle_tool_card_at_line(logical_row);
                        };
                    }
                }
            } else if let Event::Paste(text) = ev {
                // Bracketed paste：整段粘贴作为一个事件投递。
                // 参考 CLI 路径 input.rs 的 `.bracketed_paste(true)` 行为：
                // 把粘贴内容原子地插入到当前光标位置，保留所有 \n，不触发 Submit。
                // 修复"多行粘贴被切成多次 Submit"的 bug。
                if !help_visible {
                    input.insert_paste(&text);
                }
            }
        }

        // Refresh status: update turn_elapsed_ms if streaming
        {
            let mut guard = status_state.lock().expect("StatusBarState poisoned");
            if guard.streaming {
                if let Some(start) = turn_start {
                    guard.turn_elapsed_ms = start.elapsed().as_millis() as u64;
                }
            }
        }
    }

    Ok(())
}

fn route_key(input: &mut InputLine, key: KeyEvent) -> InputAction {
    // Windows crossterm quirk: by default it emits *two* KeyEvents per key
    // press — one `Press` and one `Release`. Without filtering, every char
    // gets inserted twice (e.g., typing "你好" yields "你你好好"). Only handle
    // `Press` (and `Repeat` for key-hold auto-repeat) events; ignore `Release`.
    // On Unix/macOS crossterm always emits `Press`, so this filter is a no-op
    // there. This is the documented crossterm 0.28 behavior on Windows.
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
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

fn render_menu(
    menu: &mut SlashMenu,
    f: &mut ratatui::Frame,
    area: Rect,
) {
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

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("命令 ({}/{})", menu.total_count(), menu.all_items_count()));
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
        ("Ctrl+T", "折叠 / 展开最近一个工具卡片"),
        ("鼠标左键", "点击工具卡片切换折叠 / 展开"),
        ("粘贴 (Ctrl+V)", "整段粘贴（含多行）作为一个块插入，不立即提交"),
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

/// Execute a turn in a background thread. Returns the cli (for reuse) and the result.
/// This runs on a worker thread so the main event loop can continue rendering
/// and processing keyboard events (e.g., Ctrl+C to exit) during streaming.
fn execute_turn(
    mut cli: LiveCli,
    line: &str,
    output_handle: &Arc<Mutex<crate::tui::output_view::OutputBuffer>>,
    status_state: &Arc<Mutex<StatusBarState>>,
    tool_history_shared: &Arc<Mutex<ToolHistory>>,
) -> TurnResult {
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
                if let Ok(mut buf) = output_handle.lock() {
                    buf.push_entry(crate::tui::output_view::OutputEntry::ToolCard {
                        tool_id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        result: None,
                        is_error: false,
                        collapsed: false,
                    });
                }
            }
            StatusEvent::ToolResult { id, name, output, is_error } => {
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
                    guard.turn_usage.cache_read_input_tokens +=
                        usage.cache_read_input_tokens;
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
                            buf.push_entry(crate::tui::output_view::OutputEntry::Timeline {
                                summary: timeline,
                            });
                        }
                    }
                }
                if let Ok(mut guard) = status_handle.lock() {
                    if guard.streaming {
                        guard.finish_turn();
                    }
                }
            }
            StatusEvent::Thinking { char_count, redacted } => {
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
        }
    });

    cli.set_status_emitter(emitter);
    cli.set_tui_mode(true);

    let result = cli.run_turn(line);

    cli.clear_status_emitter();
    cli.set_tui_mode(false);

    // Ensure streaming is marked as finished even on error
    if let Ok(mut guard) = status_state.lock() {
        if guard.streaming {
            guard.finish_turn();
        }
    }

    TurnResult {
        cli,
        result: result.map_err(|e| format!("{e}")),
    }
}

/// 在 worker 线程执行本地斜杠命令（如 /help, /clear, /status）。
///
/// 与 `execute_turn` 不同，此函数调用 `LiveCli::handle_repl_command` 在本地
/// 处理命令，不会把输入发给 AI。命令的 println 输出通过 `tui_output`
/// 捕获到 OutputBuffer（在调用前已由 Submit 分支设置）。
///
/// 执行完成后清除 `tui_output`，避免后续轮次误捕获。
fn execute_slash_command(mut cli: LiveCli, command: SlashCommand) -> TurnResult {
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
    TurnResult { cli, result }
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
                        guard.turn_usage.cache_read_input_tokens +=
                            usage.cache_read_input_tokens;
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
                StatusEvent::Thinking { char_count, redacted } => {
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

        assert_eq!(output_view.snapshot(), "Hello world!");
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
        let mut output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::Thinking { char_count: None, redacted: false });

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
        let mut output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::Thinking { char_count: Some(42), redacted: false });

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
        let mut output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::Thinking { char_count: None, redacted: true });

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
        let markdown = "# Heading\n\nSome **bold** text and a code block:\n\n```rust\nfn main() {}\n```";
        let renderer = TerminalRenderer::new();
        let ansi = renderer.markdown_to_ansi(markdown);
        let text = ansi.into_text().expect("ansi-to-tui conversion should succeed");
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
        assert_eq!(flat1, flat2, "unchanged snapshot should yield identical text");
    }
}
