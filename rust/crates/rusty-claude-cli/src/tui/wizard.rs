//! First-run configuration wizard for the Claw Plus TUI.
//!
//! When the bootstrapped sentinel file is missing, this module presents a
//! two-step ratatui screen that lets the user select a provider and enter an
//! API key before proceeding to the main chat interface.

#![cfg(feature = "full-tui")]
#![allow(dead_code, unused_imports, unused_variables)]

use std::io::{self, Write};
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;

use runtime::{save_wizard_settings, WizardSettings};

// ── Environment key probes ────────────────────────────────────────────────

/// Description of a provider the wizard can detect.
struct ProviderInfo {
    /// Display name in the selection list.
    label: &'static str,
    /// Env var to probe for automatic detection.
    env_key: &'static str,
    /// Provider slug stored to settings ("anthropic", "openai", etc.).
    slug: &'static str,
    /// Default model alias for this provider.
    default_model: &'static str,
    /// Base URL env var (for display purposes).
    base_url_env: &'static str,
    /// One-line description shown in the list.
    description: &'static str,
}

const KNOWN_PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        label: "Anthropic",
        env_key: "ANTHROPIC_API_KEY",
        slug: "anthropic",
        default_model: "sonnet",
        base_url_env: "ANTHROPIC_BASE_URL",
        description: "Claude models (Sonnet, Opus, Haiku)",
    },
    ProviderInfo {
        label: "OpenAI",
        env_key: "OPENAI_API_KEY",
        slug: "openai",
        default_model: "openai/gpt-4.1-mini",
        base_url_env: "OPENAI_BASE_URL",
        description: "GPT-4.1, GPT-4o, o3, o4-mini",
    },
    ProviderInfo {
        label: "xAI (Grok)",
        env_key: "XAI_API_KEY",
        slug: "xai",
        default_model: "grok",
        base_url_env: "XAI_BASE_URL",
        description: "Grok-3, Grok-3-mini",
    },
    ProviderInfo {
        label: "DashScope (Kimi / Qwen)",
        env_key: "DASHSCOPE_API_KEY",
        slug: "dashscope",
        default_model: "kimi",
        base_url_env: "DASHSCOPE_BASE_URL",
        description: "Kimi K2.5, Qwen models via Alibaba Cloud",
    },
];

/// Result of scanning environment variables for pre-existing keys.
struct DetectedKey {
    provider_index: usize,
    masked_key: String,
    raw_key: String,
}

fn scan_detected_keys() -> Vec<DetectedKey> {
    let mut detected = Vec::new();
    for (i, info) in KNOWN_PROVIDERS.iter().enumerate() {
        if let Ok(key) = std::env::var(info.env_key) {
            if !key.is_empty() {
                let masked = mask_key(&key);
                detected.push(DetectedKey {
                    provider_index: i,
                    masked_key: masked,
                    raw_key: key,
                });
            }
        }
    }
    detected
}

fn mask_key(key: &str) -> String {
    if key.len() <= 10 {
        return "***".to_string();
    }
    let prefix = &key[..4];
    let suffix = &key[key.len() - 4..];
    format!("{prefix}...{suffix}")
}

// ── Wizard result ──────────────────────────────────────────────────────────

/// What the wizard produced (or that the user quit).
enum WizardOutcome {
    /// User completed configuration.
    Configured {
        provider_slug: String,
        api_key: String,
    },
    /// User pressed Esc / Ctrl+C — exit the application.
    Quit,
}

// ── Public entry point ─────────────────────────────────────────────────────

/// Run the first-run wizard inside a ratatui alternate screen.
///
/// Returns `Ok(())` when the wizard finished successfully (settings saved,
/// env vars injected for the current process).  Returns an error if terminal
/// setup fails.
pub(crate) fn run_first_run_wizard() -> Result<(), Box<dyn std::error::Error>> {
    let detected = scan_detected_keys();

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;

    // Drop guard for terminal cleanup on any exit path.
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                LeaveAlternateScreen,
                crossterm::event::DisableBracketedPaste,
                crossterm::cursor::Show
            );
        }
    }
    let _guard = Guard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let outcome = run_wizard_loop(&mut terminal, &detected);

    let finalise = |provider_slug: &str, api_key: &str| -> Result<(), Box<dyn std::error::Error>> {
        let settings = WizardSettings {
            provider: provider_slug.to_string(),
            api_key: api_key.to_string(),
        };
        save_wizard_settings(&settings)?;
        inject_api_key(provider_slug, api_key);
        runtime::mark_bootstrapped()?;
        Ok(())
    };

    match outcome {
        Ok(WizardOutcome::Configured {
            provider_slug,
            api_key,
        }) => match finalise(&provider_slug, &api_key) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Render the error on the alternate screen so the user can
                // read it before the terminal is restored and the process exits.
                render_error_screen(&mut terminal, &e.to_string());
                Err(e)
            }
        },
        Ok(WizardOutcome::Quit) => Err("Configuration cancelled by user.".into()),
        Err(e) => {
            // If the wizard loop itself errored (crossterm I/O), also show
            // the error on-screen before teardown.
            render_error_screen(&mut terminal, &e.to_string());
            Err(e)
        }
    }
}

fn inject_api_key(provider_slug: &str, api_key: &str) {
    match provider_slug {
        "anthropic" => {
            std::env::set_var("ANTHROPIC_API_KEY", api_key);
        }
        "openai" => {
            std::env::set_var("OPENAI_API_KEY", api_key);
        }
        "xai" => {
            std::env::set_var("XAI_API_KEY", api_key);
        }
        "dashscope" => {
            std::env::set_var("DASHSCOPE_API_KEY", api_key);
        }
        _ => {}
    }
}

// ── Error screen (rendered before terminal teardown) ───────────────────────

/// Render an error message on the alternate screen and wait for a keypress.
///
/// This gives the user time to read the error before the terminal is restored
/// and the process exits. Without this, on Windows the console window closes
/// immediately after printing the error to stderr, making it look like a
/// "flash crash".
fn render_error_screen(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, message: &str) {
    let _ = terminal.draw(|f| {
        let area = centered_rect(64, 10, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Error ")
            .style(Style::default().fg(Color::Red));
        f.render_widget(
            Paragraph::new(format!(
                "Configuration failed:\n\n{}\n\nPress any key to exit...",
                message
            ))
            .block(block)
            .style(Style::default().fg(Color::White))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
            area,
        );
    });
    // Drain any buffered events, then wait for a keypress.
    while event::poll(std::time::Duration::from_millis(10)).unwrap_or(false) {
        let _ = event::read();
    }
    loop {
        match event::read() {
            Ok(Event::Key(_)) => break,
            Err(_) => break, // stdin broken — don't loop forever
            _ => {}          // non-key event — keep waiting
        }
    }
}

// ── Main wizard event loop ─────────────────────────────────────────────────

fn run_wizard_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    detected: &[DetectedKey],
) -> Result<WizardOutcome, Box<dyn std::error::Error>> {
    // Step 0: figure out which step we're on.
    // If keys are detected, start at the selection screen.
    // Otherwise jump straight to manual input.

    let step = if detected.is_empty() {
        WizardStep::ManualInput {
            provider_idx: 0,
            input_buffer: String::new(),
            error: None,
        }
    } else {
        WizardStep::SelectDetected { selected: 0 }
    };

    let mut step = step;

    loop {
        // Draw
        terminal.draw(|f| {
            let area = f.area();
            render_wizard(f, area, &step, detected);
        })?;

        // Read input
        if !event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }
        let ev = event::read()?;
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => return Ok(WizardOutcome::Quit),
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    return Ok(WizardOutcome::Quit);
                }
                other => {
                    step = handle_key(&step, other, detected);
                    if let WizardStep::Done {
                        provider_slug,
                        api_key,
                    } = step
                    {
                        return Ok(WizardOutcome::Configured {
                            provider_slug,
                            api_key,
                        });
                    }
                }
            },
            _ => {}
        }
    }
}

// ── Wizard step state machine ──────────────────────────────────────────────

#[derive(Clone)]
enum WizardStep {
    /// Display detected keys + "Manual config..." as a selectable list.
    SelectDetected { selected: usize },
    /// User chose "Manual config..." or no keys were detected.
    ManualInput {
        provider_idx: usize,
        input_buffer: String,
        error: Option<String>,
    },
    /// User confirmed — extract result.
    Done {
        provider_slug: String,
        api_key: String,
    },
}

fn handle_key(step: &WizardStep, key: KeyCode, detected: &[DetectedKey]) -> WizardStep {
    match step {
        WizardStep::SelectDetected { selected } => {
            // Items: each detected key + "Manual config..."
            let item_count = detected.len() + 1;
            match key {
                KeyCode::Up => WizardStep::SelectDetected {
                    selected: selected.saturating_sub(1),
                },
                KeyCode::Down => WizardStep::SelectDetected {
                    selected: (*selected + 1).min(item_count.saturating_sub(1)),
                },
                KeyCode::Enter => {
                    if *selected < detected.len() {
                        // Use detected key
                        let dk = &detected[*selected];
                        let info = &KNOWN_PROVIDERS[dk.provider_index];
                        WizardStep::Done {
                            provider_slug: info.slug.to_string(),
                            api_key: dk.raw_key.clone(),
                        }
                    } else {
                        // "Manual config..." — go to manual input
                        WizardStep::ManualInput {
                            provider_idx: 0,
                            input_buffer: String::new(),
                            error: None,
                        }
                    }
                }
                _ => WizardStep::SelectDetected {
                    selected: *selected,
                },
            }
        }
        WizardStep::ManualInput {
            provider_idx,
            input_buffer,
            error: _,
        } => {
            let provider_count = KNOWN_PROVIDERS.len();
            match key {
                KeyCode::Up => WizardStep::ManualInput {
                    provider_idx: provider_idx.saturating_sub(1),
                    input_buffer: input_buffer.clone(),
                    error: None,
                },
                KeyCode::Down => WizardStep::ManualInput {
                    provider_idx: (*provider_idx + 1).min(provider_count.saturating_sub(1)),
                    input_buffer: input_buffer.clone(),
                    error: None,
                },
                KeyCode::Char(c) => {
                    let mut buf = input_buffer.clone();
                    buf.push(c);
                    WizardStep::ManualInput {
                        provider_idx: *provider_idx,
                        input_buffer: buf,
                        error: None,
                    }
                }
                KeyCode::Backspace => {
                    let mut buf = input_buffer.clone();
                    buf.pop();
                    WizardStep::ManualInput {
                        provider_idx: *provider_idx,
                        input_buffer: buf,
                        error: None,
                    }
                }
                KeyCode::Enter => {
                    let trimmed = input_buffer.trim();
                    if trimmed.is_empty() {
                        WizardStep::ManualInput {
                            provider_idx: *provider_idx,
                            input_buffer: input_buffer.clone(),
                            error: Some("API key cannot be empty.".to_string()),
                        }
                    } else {
                        let info = &KNOWN_PROVIDERS[*provider_idx];
                        WizardStep::Done {
                            provider_slug: info.slug.to_string(),
                            api_key: trimmed.to_string(),
                        }
                    }
                }
                _ => WizardStep::ManualInput {
                    provider_idx: *provider_idx,
                    input_buffer: input_buffer.clone(),
                    error: None,
                },
            }
        }
        WizardStep::Done { .. } => step.clone(),
    }
}

// ── Rendering ──────────────────────────────────────────────────────────────

const HELP_TEXT: &str = "\u{2191}/\u{2193} Navigate   Enter Confirm   Esc Quit";

fn render_wizard(f: &mut ratatui::Frame, area: Rect, step: &WizardStep, detected: &[DetectedKey]) {
    match step {
        WizardStep::SelectDetected { selected } => {
            render_detected_screen(f, area, *selected, detected);
        }
        WizardStep::ManualInput {
            provider_idx,
            input_buffer,
            error,
        } => {
            render_manual_input(f, area, *provider_idx, input_buffer, error.as_deref());
        }
        WizardStep::Done { .. } => {}
    }
}

fn render_detected_screen(
    f: &mut ratatui::Frame,
    area: Rect,
    selected: usize,
    detected: &[DetectedKey],
) {
    // Center a ~72-wide block
    let centered = centered_rect(72, 18, area);
    f.render_widget(Clear, centered);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // spacer
            Constraint::Length(1), // subtitle
            Constraint::Length(1), // spacer
            Constraint::Min(3),    // list
            Constraint::Length(1), // spacer
            Constraint::Length(1), // help
        ])
        .split(centered);

    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let subtitle_style = Style::default().fg(Color::Gray);
    let help_style = Style::default().fg(Color::DarkGray);

    // Title
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Claw Plus",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" \u{2014} First Run Setup"),
        ]))
        .alignment(Alignment::Center),
        chunks[0],
    );

    // Subtitle
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(
            "Welcome! Detected API keys from your environment:",
        )]))
        .style(subtitle_style)
        .alignment(Alignment::Center),
        chunks[2],
    );

    // List items
    let mut items: Vec<ListItem> = Vec::new();

    for dk in detected {
        let info = &KNOWN_PROVIDERS[dk.provider_index];
        let line = Line::from(vec![
            Span::raw(format!("  {}  ", info.label)),
            Span::styled(dk.masked_key.as_str(), Style::default().fg(Color::Green)),
            Span::raw(format!("  ({})", info.description)),
        ]);
        items.push(ListItem::new(line));
    }

    // Separator + "Manual config..."
    items.push(ListItem::new(Line::from(Span::styled(
        "  \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
        Style::default().fg(Color::DarkGray),
    ))));
    items.push(ListItem::new(Line::from(vec![
        Span::raw("  \u{2699}  "),
        Span::styled("Manual config...", Style::default().fg(Color::White)),
        Span::raw("  (enter a key for any provider)"),
    ])));

    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol(" \u{25B6}");

    f.render_stateful_widget(list, chunks[4], &mut list_state(selected));

    // Help
    f.render_widget(
        Paragraph::new(HELP_TEXT)
            .style(help_style)
            .alignment(Alignment::Center),
        chunks[6],
    );

    // Draw the border after everything else so it renders on top
    let border_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(border_block, centered);
}

fn render_manual_input(
    f: &mut ratatui::Frame,
    area: Rect,
    provider_idx: usize,
    input_buffer: &str,
    error: Option<&str>,
) {
    let centered = centered_rect(70, 16, area);
    f.render_widget(Clear, centered);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // spacer
            Constraint::Length(1), // provider selector label
            Constraint::Length(1), // provider list
            Constraint::Length(1), // spacer
            Constraint::Length(1), // key label
            Constraint::Length(3), // key input
            Constraint::Length(1), // error
            Constraint::Length(1), // help
        ])
        .split(centered);

    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Gray);
    let help_style = Style::default().fg(Color::DarkGray);
    let error_style = Style::default().fg(Color::Red);

    // Title
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Claw Plus",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" \u{2014} API Key Configuration"),
        ]))
        .alignment(Alignment::Center),
        chunks[0],
    );

    // Provider label
    f.render_widget(
        Paragraph::new("Provider (\u{2191}/\u{2193} to switch):").style(label_style),
        chunks[2],
    );

    // Provider list — horizontal inline
    let mut spans = Vec::new();
    for (i, info) in KNOWN_PROVIDERS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if i == provider_idx {
            spans.push(Span::styled(
                format!("[ {} ]", info.label),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(info.label, Style::default().fg(Color::Gray)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), chunks[3]);

    // Key label
    let info = &KNOWN_PROVIDERS[provider_idx];
    f.render_widget(
        Paragraph::new(format!("API Key (env: {}):", info.env_key)).style(label_style),
        chunks[5],
    );

    // Key input with mask
    let masked = "*".repeat(input_buffer.len());
    let display = if masked.is_empty() {
        Span::styled(
            "<paste your key and press Enter>",
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::styled(masked, Style::default().fg(Color::Green))
    };

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let input_area = chunks[6];
    f.render_widget(input_block, input_area);
    let inner = Layout::default()
        .margin(1)
        .constraints([Constraint::Min(1)])
        .split(input_area);
    f.render_widget(Paragraph::new(Line::from(display)), inner[0]);

    // Error
    if let Some(msg) = error {
        f.render_widget(
            Paragraph::new(msg)
                .style(error_style)
                .alignment(Alignment::Center),
            chunks[7],
        );
    }

    // Help
    f.render_widget(
        Paragraph::new("Keys are stored in ~/.claw/settings.json")
            .style(help_style)
            .alignment(Alignment::Center),
        chunks[8],
    );

    let border_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(border_block, centered);
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Compute a centered rectangle of the given dimensions within `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

/// Build a simple ListState for the given selected index.
fn list_state(selected: usize) -> ratatui::widgets::ListState {
    let mut state = ratatui::widgets::ListState::default();
    if selected < 100 {
        state.select(Some(selected));
    }
    state
}
