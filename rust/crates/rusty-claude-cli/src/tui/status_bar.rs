//! Shared state for the persistent status bar.
//!
//! `StatusBarState` is the single source of truth the TUI reads to render
//! the bottom status bar. It is updated by:
//! - `LiveCli::accumulate_usage` (after each turn, cumulative totals)
//! - `StatusEmitter` callback in `AnthropicRuntimeClient` (live during stream)
//!
//! Rendering to a ratatui `Frame` happens in `render_status_bar` (added in Task 4).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use std::sync::{Arc, Mutex};

use runtime::TokenUsage;

/// Snapshot of everything the status bar displays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StatusBarState {
    /// Resolved model name (e.g. `claude-opus-4-6`).
    pub model: String,
    /// Provider label (e.g. `Anthropic`, `OpenAI`, `xAI`).
    pub provider: String,
    /// Short cwd path (e.g. `~/projects/claw`).
    pub cwd: String,
    /// Current git branch, or empty if not in a repo.
    pub git_branch: String,
    /// Active permission mode label.
    pub permission_mode: String,
    /// Session id.
    pub session_id: String,
    /// Cumulative token usage across all turns in this session.
    pub cumulative_usage: TokenUsage,
    /// Delta usage observed *during* the current streaming turn (resets per turn).
    pub turn_usage: TokenUsage,
    /// Elapsed millis since the current turn started (0 when idle).
    pub turn_elapsed_ms: u64,
    /// True when a streaming turn is in progress.
    pub streaming: bool,
    /// Goal badge text (e.g. `🎯 goal` / `⚠ goal (1/3)`), or empty when paused/no goal.
    pub goal_badge: String,
    /// Poor-mode active flag.
    pub poor_mode: bool,
    /// 当前 reasoning effort 设置（None=默认，Some("low"/"medium"/"high")=已设置）。
    /// 由 /effort 命令或 --reasoning-effort CLI flag 设置，侧栏会显示。
    pub reasoning_effort: Option<String>,
    /// 累计 AI 思考轮次（每个 turn +1）。由 sync_status_from_cli_inner
    /// 从 `LiveCli::turns_snapshot()` 同步。
    pub turn_count: u32,
    /// 标记当前 turn 是否已开始（用于多轮工具调用中避免重复 reset）。
    /// 由 reset_turn 置 true，finish_turn 置 false。
    pub turn_in_progress: bool,
}

impl StatusBarState {
    /// Create a shared, thread-safe handle suitable for passing to
    /// `StatusEmitter` callbacks and the TUI render loop.
    pub(crate) fn shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    /// Total tokens (cumulative + current turn delta).
    pub(crate) fn total_tokens(&self) -> u128 {
        let cumulative = self.cumulative_usage.total_tokens() as u128;
        let turn = self.turn_usage.total_tokens() as u128;
        cumulative + turn
    }

    /// Reset turn-scoped fields at the start of each turn.
    ///
    /// **缓存命中率修复**：多轮工具调用中，每个 agent loop 迭代都会 emit
    /// `StreamStart`。如果每次都 reset，会清空前几轮迭代累积的 cache 数据，
    /// 导致缓存命中率计算失效（hit/miss 被清零）。现在用 `turn_in_progress`
    /// 标志确保只在本 turn 首次 StreamStart 时 reset，后续 StreamStart 只
    /// 刷新 streaming 状态。
    pub(crate) fn reset_turn(&mut self) {
        if self.turn_in_progress {
            // 本 turn 已开始（多轮工具调用的后续迭代），只刷新 streaming 状态，
            // 不清空 turn_usage，保留前几轮累积的 cache 数据。
            self.streaming = true;
            return;
        }
        self.turn_usage = TokenUsage::default();
        self.turn_elapsed_ms = 0;
        self.streaming = true;
        self.turn_in_progress = true;
    }

    /// Mark the turn as finished.
    pub(crate) fn finish_turn(&mut self) {
        self.streaming = false;
        self.turn_in_progress = false;
        // Fold turn delta into cumulative.
        self.cumulative_usage.input_tokens += self.turn_usage.input_tokens;
        self.cumulative_usage.output_tokens += self.turn_usage.output_tokens;
        self.cumulative_usage.cache_creation_input_tokens +=
            self.turn_usage.cache_creation_input_tokens;
        self.cumulative_usage.cache_read_input_tokens += self.turn_usage.cache_read_input_tokens;
        self.turn_usage = TokenUsage::default();
        self.turn_elapsed_ms = 0;
    }
}

/// Ratatui widget that renders the persistent status bar.
///
/// Renders a single line at the bottom of the terminal showing:
/// `│ model via provider │ 📁 cwd │ 🌿 branch │ 🔢 tokens │ 💰 cost │ 🎯 goal │`
pub(crate) struct StatusBar<'a> {
    pub state: &'a StatusBarState,
}

impl<'a> Widget for StatusBar<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let style_dim = Style::default().fg(Color::DarkGray);
        let style_version = Style::default().fg(Color::DarkGray);
        let style_streaming = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);

        // Build sections in priority order. Each section is a Vec<Span>.
        // We add sections until we exceed the available width, then stop.
        //
        // 去重策略：底栏只显示侧栏没有的信息（cwd、版本号）+ 执行状态指示（streaming timer）。
        // model/provider/tokens/cost/branch/goal/poor mode 已在侧栏详细显示，此处不再重复。
        let width = area.width as usize;

        let mut sections: Vec<Vec<Span>> = Vec::new();

        // P1: Cwd (侧栏不显示)
        sections.push(vec![
            Span::styled("│ ", style_dim),
            Span::styled("📁 ", style_dim),
            Span::styled(&self.state.cwd, style_dim),
        ]);

        // P2: Version (侧栏不显示，新增)
        sections.push(vec![
            Span::styled(" │ ", style_dim),
            Span::styled(format!("v{}", crate::VERSION), style_version),
        ]);

        // P3: Streaming timer (执行状态指示，侧栏虽有但底栏需要快速判断是否在执行)
        if self.state.streaming {
            let elapsed_s = self.state.turn_elapsed_ms / 1000;
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled(format!("⏳ {elapsed_s}s"), style_streaming),
            ]);
        }

        // Flatten sections up to available width
        let mut spans: Vec<Span> = Vec::new();
        let mut used: usize = 0;
        for section in &sections {
            // P2-3 修复：用 UnicodeWidthStr 计算视觉宽度，而不是字节长度。
            // 之前用 .len() 会高估含中文/emoji 的 section 实际占用宽度，
            // 导致低优先级 section（cwd / git branch / streaming timer /
            // goal badge / poor mode）在窄终端被错误跳过。
            let section_width: usize = section
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            if used + section_width > width && !spans.is_empty() {
                break; // skip low-priority sections that don't fit
            }
            used += section_width;
            spans.extend(section.iter().cloned());
        }

        // Closing delimiter
        spans.push(Span::styled(" │", style_dim));

        let line = Line::from(spans);
        Widget::render(line, area, buf);
    }
}

/// Cost estimate helper — delegates to runtime's pricing logic.
/// For TUI display only; the authoritative cost calc lives in `format_status_bar`.
fn estimate_cost(usage: &TokenUsage, model: &str) -> f64 {
    let pricing = runtime::pricing_for_model(model);
    pricing.map_or_else(
        || usage.estimate_cost_usd().total_cost_usd(),
        |p| usage.estimate_cost_usd_with_pricing(p).total_cost_usd(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_idle() {
        let state = StatusBarState::default();
        assert!(!state.streaming);
        assert_eq!(state.total_tokens(), 0);
    }

    #[test]
    fn reset_turn_marks_streaming() {
        let mut state = StatusBarState::default();
        state.reset_turn();
        assert!(state.streaming);
        assert_eq!(state.turn_usage.total_tokens(), 0);
    }

    #[test]
    fn finish_turn_folds_delta_into_cumulative() {
        let mut state = StatusBarState::default();
        state.reset_turn();
        state.turn_usage.input_tokens = 100;
        state.turn_usage.output_tokens = 50;
        state.finish_turn();
        assert!(!state.streaming);
        assert_eq!(state.cumulative_usage.input_tokens, 100);
        assert_eq!(state.cumulative_usage.output_tokens, 50);
        assert_eq!(state.turn_usage.total_tokens(), 0);
    }

    #[test]
    fn total_tokens_sums_cumulative_and_turn() {
        let mut state = StatusBarState::default();
        state.cumulative_usage.input_tokens = 1000;
        state.turn_usage.input_tokens = 200;
        assert_eq!(state.total_tokens(), 1200);
    }

    #[test]
    fn status_bar_renders_without_panic() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "claude-opus-4-6".to_string(),
            provider: "Anthropic".to_string(),
            cwd: "~/claw".to_string(),
            git_branch: "main".to_string(),
            cumulative_usage: TokenUsage {
                input_tokens: 1000,
                output_tokens: 500,
                ..Default::default()
            },
            goal_badge: "🎯 goal".to_string(),
            ..Default::default()
        };

        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        // Verify the buffer contains the model name somewhere
        let content = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("claude-opus-4-6"));
        assert!(content.contains("Anthropic"));
        assert!(content.contains("~/claw"));
        assert!(content.contains("main"));
    }

    #[test]
    fn status_bar_shows_streaming_indicator_when_streaming() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "test-model".to_string(),
            streaming: true,
            turn_elapsed_ms: 5000,
            ..Default::default()
        };

        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        let content = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("⏱"));
        assert!(content.contains("5s"));
    }
}
