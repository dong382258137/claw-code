#![cfg(feature = "full-tui")]

//! Slash command popup menu with fuzzy filtering.
//!
//! When the user types a `/`-prefixed query, `SlashMenu` filters the
//! available `SlashCommandSpec` list and tracks the currently selected
//! item. Up/Down arrow keys move the selection; Enter submits the
//! selected command; Esc closes the menu.

use std::borrow::Cow;

use commands::{slash_command_specs, SlashCommandSpec};

/// Maximum items shown at once in the popup.
const MAX_VISIBLE_ITEMS: usize = 10;

/// A slash command menu with fuzzy-filtered items.
#[derive(Debug, Clone)]
pub(crate) struct SlashMenu {
    /// All candidate commands (loaded once from `slash_command_specs()`).
    all_items: Vec<&'static SlashCommandSpec>,
    /// Current filter query (text after the `/`).
    query: String,
    /// Currently selected index into `filtered()`, or None if no selection.
    selected: Option<usize>,
    /// Scroll offset for the visible window.
    scroll: usize,
}

impl SlashMenu {
    /// Build a menu from the static `slash_command_specs()` list.
    #[must_use]
    pub(crate) fn new() -> Self {
        let all_items = slash_command_specs().iter().collect::<Vec<_>>();
        let selected = if all_items.is_empty() { None } else { Some(0) };
        Self {
            all_items,
            query: String::new(),
            selected,
            scroll: 0,
        }
    }

    /// Update the filter query (text typed after `/`). Resets selection
    /// to the first item. Empty query shows all commands.
    pub(crate) fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.selected = if self.filtered().is_empty() { None } else { Some(0) };
        self.scroll = 0;
    }

    /// Move selection up by one (wraps to bottom).
    pub(crate) fn move_up(&mut self) {
        if let Some(idx) = self.selected {
            let len = self.filtered().len();
            if len == 0 {
                return;
            }
            let new_idx = if idx == 0 { len - 1 } else { idx - 1 };
            self.selected = Some(new_idx);
            self.adjust_scroll();
        }
    }

    /// Move selection down by one (wraps to top).
    pub(crate) fn move_down(&mut self) {
        if let Some(idx) = self.selected {
            let len = self.filtered().len();
            if len == 0 {
                return;
            }
            let new_idx = if idx + 1 >= len { 0 } else { idx + 1 };
            self.selected = Some(new_idx);
            self.adjust_scroll();
        }
    }

    /// Currently selected command spec, or None.
    pub(crate) fn selected_spec(&self) -> Option<&'static SlashCommandSpec> {
        let idx = self.selected?;
        self.filtered().get(idx).copied()
    }

    /// Reset to initial state (clear query, select first).
    pub(crate) fn reset(&mut self) {
        self.query.clear();
        self.selected = if self.all_items.is_empty() { None } else { Some(0) };
        self.scroll = 0;
    }

    /// Filtered command list based on current query.
    /// Empty query → all commands. Non-empty query → commands whose name
    /// OR aliases OR summary contains the query (case-insensitive).
    pub(crate) fn filtered(&self) -> Vec<&'static SlashCommandSpec> {
        if self.query.is_empty() {
            return self.all_items.clone();
        }
        let q = self.query.to_ascii_lowercase();
        self.all_items
            .iter()
            .filter(|spec| {
                let name = spec.name.to_ascii_lowercase();
                let summary = spec.summary.to_ascii_lowercase();
                let aliases_match = spec.aliases.iter().any(|a| a.to_ascii_lowercase().contains(&q));
                name.contains(&q) || summary.contains(&q) || aliases_match
            })
            .copied()
            .collect()
    }

    /// Visible window of items (paginated by `MAX_VISIBLE_ITEMS`).
    pub(crate) fn visible_window(&self) -> Vec<&'static SlashCommandSpec> {
        let filtered = self.filtered();
        let start = self.scroll.min(filtered.len().saturating_sub(1));
        let end = (start + MAX_VISIBLE_ITEMS).min(filtered.len());
        filtered[start..end].to_vec()
    }

    /// Current scroll offset (for rendering scroll indicators).
    pub(crate) fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Total filtered count (for rendering "N of M").
    pub(crate) fn total_count(&self) -> usize {
        self.filtered().len()
    }

    /// Currently selected index (None if nothing selected).
    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// Index within the visible window (None if out of view).
    pub(crate) fn visible_index(&self) -> Option<usize> {
        let idx = self.selected?;
        let visible = idx.saturating_sub(self.scroll);
        if visible < MAX_VISIBLE_ITEMS {
            Some(visible)
        } else {
            None
        }
    }

    /// Total number of all candidate commands (ignoring filter).
    pub(crate) fn all_items_count(&self) -> usize {
        self.all_items.len()
    }

    fn adjust_scroll(&mut self) {
        if let Some(idx) = self.selected {
            if idx < self.scroll {
                self.scroll = idx;
            } else if idx >= self.scroll + MAX_VISIBLE_ITEMS {
                self.scroll = idx + 1 - MAX_VISIBLE_ITEMS;
            }
        }
    }
}

impl Default for SlashMenu {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a single slash command spec as a display string for the popup.
/// Format: `/name [aliases]  summary`
pub(crate) fn format_menu_item(spec: &SlashCommandSpec) -> Cow<'static, str> {
    let mut s = String::new();
    s.push('/');
    s.push_str(spec.name);
    if !spec.aliases.is_empty() {
        s.push_str(", /");
        s.push_str(&spec.aliases.join(", /"));
    }
    s.push_str("  ");
    s.push_str(spec.summary);
    Cow::Owned(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_menu_has_all_specs_and_first_selected() {
        let menu = SlashMenu::new();
        assert!(!menu.all_items.is_empty(), "slash_command_specs should return commands");
        assert_eq!(menu.selected, Some(0));
        assert_eq!(menu.scroll, 0);
    }

    #[test]
    fn empty_query_shows_all() {
        let menu = SlashMenu::new();
        let all = menu.all_items.len();
        assert_eq!(menu.filtered().len(), all);
    }

    #[test]
    fn query_filters_by_name_substring() {
        let mut menu = SlashMenu::new();
        menu.set_query("hel");
        let filtered = menu.filtered();
        assert!(filtered.iter().any(|s| s.name == "help"), "should find 'help'");
        for spec in &filtered {
            let name_lower = spec.name.to_ascii_lowercase();
            let summary_lower = spec.summary.to_ascii_lowercase();
            let alias_match = spec.aliases.iter().any(|a| a.to_ascii_lowercase().contains("hel"));
            assert!(
                name_lower.contains("hel") || summary_lower.contains("hel") || alias_match,
                "filtered item '{}' should match query 'hel'",
                spec.name
            );
        }
    }

    #[test]
    fn query_matches_aliases() {
        let mut menu = SlashMenu::new();
        menu.set_query("mcp");
        let filtered = menu.filtered();
        assert!(filtered.iter().any(|s| s.name == "mcp"));
    }

    #[test]
    fn query_matches_summary_substring() {
        let mut menu = SlashMenu::new();
        menu.set_query("session");
        let filtered = menu.filtered();
        assert!(filtered.iter().any(|s| s.name == "status"));
    }

    #[test]
    fn move_down_wraps_to_top() {
        let mut menu = SlashMenu::new();
        let last_idx = menu.filtered().len() - 1;
        menu.selected = Some(last_idx);
        menu.move_down();
        assert_eq!(menu.selected, Some(0), "should wrap to top");
    }

    #[test]
    fn move_up_wraps_to_bottom() {
        let mut menu = SlashMenu::new();
        menu.selected = Some(0);
        let last_idx = menu.filtered().len() - 1;
        menu.move_up();
        assert_eq!(menu.selected, Some(last_idx), "should wrap to bottom");
    }

    #[test]
    fn selected_spec_returns_current() {
        let mut menu = SlashMenu::new();
        menu.selected = Some(0);
        let spec = menu.selected_spec();
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().name, menu.all_items[0].name);
    }

    #[test]
    fn set_query_resets_selection_to_first() {
        let mut menu = SlashMenu::new();
        menu.selected = Some(5);
        menu.set_query("hel");
        assert_eq!(menu.selected, Some(0));
        assert_eq!(menu.scroll, 0);
    }

    #[test]
    fn set_query_with_no_matches_clears_selection() {
        let mut menu = SlashMenu::new();
        menu.set_query("zzz_nomatch_zzz");
        assert_eq!(menu.filtered().len(), 0);
        assert_eq!(menu.selected, None);
    }

    #[test]
    fn scroll_adjusts_when_moving_past_bottom_of_window() {
        let mut menu = SlashMenu::new();
        let big_idx = 15.min(menu.all_items.len().saturating_sub(1));
        if big_idx >= MAX_VISIBLE_ITEMS {
            menu.selected = Some(big_idx);
            menu.adjust_scroll();
            assert!(
                menu.scroll + MAX_VISIBLE_ITEMS > big_idx,
                "scroll should make selected visible"
            );
            assert!(menu.visible_index().is_some(), "selected should be in visible window");
        }
    }

    #[test]
    fn visible_window_returns_at_most_max_items() {
        let menu = SlashMenu::new();
        let visible = menu.visible_window();
        assert!(visible.len() <= MAX_VISIBLE_ITEMS);
    }

    #[test]
    fn reset_clears_query_and_selects_first() {
        let mut menu = SlashMenu::new();
        menu.set_query("hel");
        menu.reset();
        assert!(menu.query.is_empty());
        assert_eq!(menu.filtered().len(), menu.all_items.len());
        assert_eq!(menu.selected, Some(0));
    }

    #[test]
    fn format_menu_item_includes_name_and_summary() {
        let menu = SlashMenu::new();
        let first = menu.all_items[0];
        let s = format_menu_item(first);
        assert!(s.starts_with('/'));
        assert!(s.contains(first.name));
        assert!(s.contains(first.summary));
    }

    #[test]
    fn all_items_count_matches_static_specs() {
        let menu = SlashMenu::new();
        assert_eq!(menu.all_items_count(), slash_command_specs().len());
    }
}
