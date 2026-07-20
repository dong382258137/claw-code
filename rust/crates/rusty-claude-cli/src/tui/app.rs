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
    execute!(stdout, EnterAlternateScreen)?;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        let result = run_event_loop(&mut terminal, cli);
        // Restore terminal on exit (inside closure so it runs even on panic via ?).
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
        let _ = execute!(stdout, crossterm::cursor::Show);
    }

    result
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

    // Phase 3.2: construct TerminalRenderer once and reuse across renders.
    // Loading SyntaxSet/ThemeSet is expensive (several ms), so we cache it.
    // The renderer converts markdown → ANSI; ansi_to_tui converts ANSI →
    // ratatui Text<'static> with styled spans (headings, code blocks, etc.).
    let renderer = TerminalRenderer::new();

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

    // P1 性能优化：Markdown 渲染缓存。
    // 每次渲染（流式时 50ms 一次）都对 64KB buffer 做 markdown_to_ansi +
    // into_text 转换，长对话时严重卡顿。缓存上次渲染的 hash + Text，
    // snapshot 未变时跳过转换。hash 用 std::hash::DefaultHasher（无需
    // 新增依赖），对 64KB 字符串约 100ns，远快于 pulldown-cmark + syntect
    // 的全量解析（数 ms 到数十 ms）。
    let mut cached_render: Option<(u64, Text<'static>)> = None;

    // `?` toggles a centered keybindings overlay. While visible, most other
    // keybindings are intercepted so the overlay behaves like a modal.
    let mut help_visible: bool = false;

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
            // Phase 3.2: convert the raw markdown snapshot → ANSI via the
            // cached TerminalRenderer, then → ratatui Text<'static> via
            // ansi_to_tui so styled spans (headings, code blocks, bold/
            // italic, syntax highlighting) render correctly. Fall back to
            // raw text if ANSI conversion fails (shouldn't happen in
            // practice, but keeps the TUI usable on any renderer glitch).
            let output_text = output_view.snapshot();
            // P1 性能优化：用 hash 缓存避免重复渲染。流式时每 50ms 触发
            // 一次 draw，但 snapshot 可能在多次 draw 间不变（如等待 API
            // 响应、用户滚动时）。hash 命中时跳过 markdown_to_ansi +
            // into_text 全量转换，将渲染耗时从数十 ms 降到 ~100ns。
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&output_text, &mut hasher);
            let text_hash = std::hash::Hasher::finish(&hasher);
            let output_rendered: Text<'static> =
                if let Some((cached_hash, cached_text)) = &cached_render {
                    if *cached_hash == text_hash {
                        cached_text.clone()
                    } else {
                        let rendered = if output_text.is_empty() {
                            Text::default()
                        } else {
                            let ansi = renderer.markdown_to_ansi(&output_text);
                            match ansi.into_text() {
                                Ok(text) => text,
                                Err(_) => Text::raw(output_text.clone()),
                            }
                        };
                        cached_render = Some((text_hash, rendered.clone()));
                        rendered
                    }
                } else {
                    let rendered = if output_text.is_empty() {
                        Text::default()
                    } else {
                        let ansi = renderer.markdown_to_ansi(&output_text);
                        match ansi.into_text() {
                            Ok(text) => text,
                            Err(_) => Text::raw(output_text.clone()),
                        }
                    };
                    cached_render = Some((text_hash, rendered.clone()));
                    rendered
                };
            // Compute scroll position: when scroll_offset is None (follow
            // mode), the Paragraph scrolls to its bottom automatically via
            // .scroll((max_scroll, 0)). When Some(n), we scroll n lines
            // above the bottom. Paragraph's scroll takes the top-left
            // corner's (row, col); rows beyond the visible area are hidden.
            let visible_height = main_area.height.saturating_sub(1) as usize; // -1 for top border
            let total_lines = output_rendered.lines.len();
            let max_scroll = total_lines.saturating_sub(visible_height);
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
            let input_line = format!("> {}", input.buffer());
            let input_paragraph = Paragraph::new(input_line)
                .block(Block::default().borders(Borders::TOP).title("输入"));
            f.render_widget(input_paragraph, outer[1]);

            // Position the visible cursor inside the input area
            // Layout: border(1px top) + "> "(2 chars) + cursor offset within buffer
            let prompt_prefix_len: usize = 2; // "> "
            // BUG fix: use display *width* (not char count) for cursor column.
            // CJK characters occupy 2 columns each; using char count would
            // place the cursor too far left when typing Chinese / Japanese /
            // Korean / emoji. unicode-width's `UnicodeWidthStr::width`
            // correctly accounts for wide chars and combining marks.
            let cursor_char_idx = prompt_prefix_len + input.cursor_display_width();
            f.set_cursor_position((
                outer[1].x + cursor_char_idx as u16,
                outer[1].y + 1, // +1 for the top border
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
        })?;

        // Poll for events (shorter timeout during streaming for responsive rendering)
        let poll_timeout = if turn_rx.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(200)
        };
        if event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
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
                            let echo_handle = output_view.shared_handle();
                            if let Ok(mut buf) = echo_handle.lock() {
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

    let emitter: StatusEmitter = Arc::new(move |event: StatusEvent| {
        match event {
            StatusEvent::TextDelta(text) => {
                if let Ok(mut buf) = output_handle.lock() {
                    buf.append(&text);
                }
            }
            StatusEvent::ToolUse { id, name, input } => {
                // Render tool call start card
                let card = crate::tui::tool_card::render_tool_call_start(&name, &input);
                if let Ok(mut buf) = output_handle.lock() {
                    buf.append(&card);
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
                // Render collapsible tool result card
                let card = crate::tui::tool_card::render_tool_result(&name, &output, is_error);
                if let Ok(mut buf) = output_handle.lock() {
                    buf.append(&card);
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
                // Reset tool history for new turn
                if let Ok(mut history) = tool_history.lock() {
                    history.clear();
                }
                if let Ok(mut sidebar_history) = tool_history_for_sidebar.lock() {
                    sidebar_history.clear();
                }
            }
            StatusEvent::MessageStop => {
                // Render tool timeline summary if any tools were called
                if let Ok(history) = tool_history.lock() {
                    if !history.is_empty() {
                        let timeline = crate::tui::tool_card::render_tool_timeline(&history);
                        if let Ok(mut buf) = output_for_closure.lock() {
                            buf.append(&timeline);
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
}
