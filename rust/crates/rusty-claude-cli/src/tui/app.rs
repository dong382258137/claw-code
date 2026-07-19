//! TuiApp — main ratatui event loop integrating with LiveCli.
//!
//! Owns the alternate-screen Terminal, InputLine, SlashMenu, OutputView,
//! and shared StatusBarState. Routes keyboard events to InputLine / Menu,
//! submits Enter to `LiveCli::run_turn` (capturing output via OutputView
//! sink + StatusEmitter callback for live status updates).

#![allow(dead_code, unused_imports, unused_variables, unused_assignments, clippy::too_many_lines)]

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

use crate::app::LiveCli;
use crate::tui::input_line::{InputAction, InputLine};
use crate::tui::output_view::OutputView;
use crate::tui::slash_menu::{format_menu_item, SlashMenu};
use crate::tui::status_bar::{StatusBar, StatusBarState};

/// Entry point: run the TUI REPL until user exits.
pub(crate) fn run_tui_repl(cli: LiveCli) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, cli);

    // Restore terminal on exit.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut cli: LiveCli,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = InputLine::new();
    let mut menu = SlashMenu::new();
    let mut output_view = OutputView::new();
    let status_state = StatusBarState::shared();
    // Initialize status fields from cli state
    initialize_status(&status_state, &cli);

    let mut turn_start: Option<Instant> = None;

    loop {
        // Render
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),     // output area
                    Constraint::Length(3),   // input + popup area
                    Constraint::Length(1),   // status bar
                ])
                .split(f.area());

            // Output area
            let output_text = output_view.snapshot();
            let output_paragraph = Paragraph::new(output_text)
                .block(Block::default().borders(Borders::TOP).title("Output"))
                .wrap(Wrap { trim: false });
            f.render_widget(output_paragraph, chunks[0]);

            // Input area
            let input_line = format!("> {}", input.buffer());
            let input_paragraph = Paragraph::new(input_line)
                .block(Block::default().borders(Borders::TOP).title("Input"));
            f.render_widget(input_paragraph, chunks[1]);

            // Slash menu popup (overlays below input line)
            if input.menu_open() {
                let below_input_y = chunks[1].y.saturating_add(chunks[1].height);
                let available = f
                    .area()
                    .height
                    .saturating_sub(below_input_y)
                    .saturating_sub(1);
                let menu_height = 12u16.min(available);
                let menu_area = Rect {
                    x: chunks[1].x,
                    y: below_input_y,
                    width: chunks[1].width,
                    height: menu_height,
                };
                if let Some(query) = input.menu_query() {
                    menu.set_query(&query);
                }
                render_menu(&mut menu, f, menu_area);
            }

            // Status bar
            let state_snapshot = {
                let guard = status_state.lock().expect("StatusBarState poisoned");
                guard.clone()
            };
            let status_widget = StatusBar { state: &state_snapshot };
            f.render_widget(status_widget, chunks[2]);
        })?;

        // Poll for events (200ms timeout for status refresh)
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                let action = route_key(&mut input, key);
                match action {
                    InputAction::Exit => break,
                    InputAction::Submit(line) => {
                        turn_start = Some(Instant::now());
                        handle_submit(&mut cli, &line, &mut output_view, &status_state)?;
                        turn_start = None;
                    }
                    InputAction::MenuUp => menu.move_up(),
                    InputAction::MenuDown => menu.move_down(),
                    InputAction::MenuAccept => {
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
    let modifiers_name = if key.modifiers.contains(KeyModifiers::CONTROL) {
        "Ctrl"
    } else {
        ""
    };

    // Ctrl+C / Ctrl+D — handle before char dispatch
    if modifiers_name == "Ctrl" {
        if let KeyCode::Char(c) = key.code {
            let lower = c.to_ascii_lowercase();
            if lower == 'c' || lower == 'd' {
                return input.handle_key(None, "CtrlC");
            }
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
        .title(format!("Commands ({}/{})", menu.total_count(), menu.all_items_count()));
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn handle_submit(
    cli: &mut LiveCli,
    line: &str,
    output_view: &mut OutputView,
    status_state: &Arc<Mutex<StatusBarState>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::streaming::{StatusEmitter, StatusEvent};

    // Phase 2: Construct a StatusEmitter that updates OutputView + StatusBarState
    // in real time as streaming events arrive. The emitter is injected into LiveCli
    // via set_status_emitter, and prepare_turn_runtime will forward it to the
    // freshly-built AnthropicRuntimeClient.
    let output_handle = output_view.shared_handle();
    let status_handle = Arc::clone(status_state);
    let emitter: StatusEmitter = Arc::new(move |event: StatusEvent| {
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
            StatusEvent::ToolUse { .. } => {
                // Tool use events don't directly update the status bar or output view;
                // the rendered tool call display is written to stdout by consume_stream,
                // which TUI mode suppresses via set_tui_mode(true). For MVP, TUI shows
                // only TextDelta content; tool call cards are a future enhancement.
            }
        }
    });
    cli.set_status_emitter(emitter);
    cli.set_tui_mode(true);

    // Call the existing run_turn path. StatusEmitter callback will fire
    // during streaming, updating output_view and status_state in real time.
    // set_tui_mode(true) makes prepare_turn_runtime use emit_output=false,
    // so consume_stream writes to io::sink instead of stdout — preventing
    // duplicate output under TUI's alternate screen.
    let result = cli.run_turn(line);

    // Detach emitter and reset TUI mode so next turn starts clean
    cli.clear_status_emitter();
    cli.set_tui_mode(false);

    // After turn, sync the authoritative cumulative_usage from cli (the
    // emitter only saw turn_usage deltas; cumulative is still tracked by LiveCli).
    sync_status_from_cli(status_state, cli);

    result?;
    Ok(())
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
    guard.cumulative_usage = cli.cumulative_usage_snapshot();
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
            }
        })
    }

    #[test]
    fn emitter_textdelta_appends_to_output_view() {
        let mut output_view = OutputView::new();
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
}
