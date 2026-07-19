//! Shared state for the persistent status bar.
//!
//! `StatusBarState` is the single source of truth the TUI reads to render
//! the bottom status bar. It is updated by:
//! - `LiveCli::accumulate_usage` (after each turn, cumulative totals)
//! - `StatusEmitter` callback in `AnthropicRuntimeClient` (live during stream)
//!
//! Rendering to a ratatui `Frame` happens in `render_status_bar` (added in Task 4).

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
    pub(crate) fn reset_turn(&mut self) {
        self.turn_usage = TokenUsage::default();
        self.turn_elapsed_ms = 0;
        self.streaming = true;
    }

    /// Mark the turn as finished.
    pub(crate) fn finish_turn(&mut self) {
        self.streaming = false;
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
}
