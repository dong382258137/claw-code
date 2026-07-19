#![cfg(feature = "full-tui")]

//! Single-line input editor with slash-command popup trigger.
//!
//! `InputLine` tracks the current buffer + cursor position and exposes
//! `handle_key` for keyboard event routing. When the buffer starts with
//! `/`, it populates a `SlashMenu` query and signals the parent to render
//! the popup below the input line.

/// Result of handling a key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputAction {
    /// Key was consumed (buffer or cursor changed); parent should re-render.
    Continue,
    /// User submitted the current line.
    Submit(String),
    /// User pressed Ctrl+C / Ctrl+D / Esc with empty input.
    Exit,
    /// User pressed Esc to close the slash menu (only when menu is open).
    CloseMenu,
    /// User pressed Up arrow to navigate the slash menu.
    MenuUp,
    /// User pressed Down arrow to navigate the slash menu.
    MenuDown,
    /// User pressed Tab to accept the selected menu item as completion.
    MenuAccept,
    /// No-op (key not handled).
    Ignore,
}

/// Single-line input state.
#[derive(Debug, Clone)]
pub(crate) struct InputLine {
    buffer: String,
    cursor: usize,
    /// True when slash menu is currently shown (buffer starts with `/`).
    menu_open: bool,
}

impl InputLine {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            menu_open: false,
        }
    }

    /// Current buffer content.
    pub(crate) fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Cursor position (byte offset).
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// True if slash menu should be rendered.
    pub(crate) fn menu_open(&self) -> bool {
        self.menu_open
    }

    /// Reset to empty state.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.menu_open = false;
    }

    /// Sync the slash menu's query with the current buffer (if menu is open).
    /// Returns the query to pass to `SlashMenu::set_query`.
    pub(crate) fn menu_query(&self) -> Option<String> {
        if !self.menu_open {
            return None;
        }
        self.buffer.strip_prefix('/').map(|rest| rest.to_string())
    }

    /// Handle a key event. `c` is the typed character (if any); `key` is the
    /// logical key name for non-char keys (e.g., "Enter", "Esc", "Up", "Down",
    /// "Backspace", "Left", "Right", "Tab").
    pub(crate) fn handle_key(&mut self, c: Option<char>, key: &str) -> InputAction {
        if self.menu_open {
            match key {
                "Up" => return InputAction::MenuUp,
                "Down" => return InputAction::MenuDown,
                "Tab" => return InputAction::MenuAccept,
                "Esc" => {
                    self.menu_open = false;
                    return InputAction::CloseMenu;
                }
                _ => {}
            }
        }

        if key == "Enter" {
            if self.menu_open {
                return InputAction::MenuAccept;
            }
            if self.buffer.trim().is_empty() {
                return InputAction::Continue;
            }
            let submitted = self.buffer.clone();
            self.reset();
            return InputAction::Submit(submitted);
        }

        if key == "Esc" {
            if self.buffer.is_empty() {
                return InputAction::Exit;
            }
            self.reset();
            return InputAction::Continue;
        }

        if key == "CtrlC" || key == "CtrlD" {
            return InputAction::Exit;
        }

        if key == "Backspace" {
            if self.cursor > 0 {
                let prev = self.buffer[..self.cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.buffer.replace_range(prev..self.cursor, "");
                self.cursor = prev;
                self.update_menu_state();
            }
            return InputAction::Continue;
        }

        if key == "Left" {
            if self.cursor > 0 {
                let prev = self.buffer[..self.cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.cursor = prev;
            }
            return InputAction::Continue;
        }

        if key == "Right" {
            if self.cursor < self.buffer.len() {
                let next = self.buffer[self.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| self.cursor + i)
                    .unwrap_or(self.buffer.len());
                self.cursor = next;
            }
            return InputAction::Continue;
        }

        if let Some(ch) = c {
            self.buffer.insert(self.cursor, ch);
            self.cursor += ch.len_utf8();
            self.update_menu_state();
            return InputAction::Continue;
        }

        InputAction::Ignore
    }

    /// Accept a menu selection: replace buffer with the selected command
    /// (e.g., `/help`), position cursor at end, close menu.
    pub(crate) fn accept_menu_completion(&mut self, completion: &str) {
        self.buffer.clear();
        self.buffer.push_str(completion);
        self.cursor = self.buffer.len();
        self.menu_open = false;
    }

    fn update_menu_state(&mut self) {
        self.menu_open = self.buffer.starts_with('/');
    }
}

impl Default for InputLine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_key(c: char) -> (Option<char>, &'static str) {
        (Some(c), "")
    }

    #[test]
    fn new_input_is_empty() {
        let line = InputLine::new();
        assert_eq!(line.buffer(), "");
        assert_eq!(line.cursor(), 0);
        assert!(!line.menu_open());
    }

    #[test]
    fn typing_chars_appends_to_buffer() {
        let mut line = InputLine::new();
        let (c, k) = char_key('h');
        assert_eq!(line.handle_key(c, k), InputAction::Continue);
        let (c, k) = char_key('i');
        assert_eq!(line.handle_key(c, k), InputAction::Continue);
        assert_eq!(line.buffer(), "hi");
        assert_eq!(line.cursor(), 2);
    }

    #[test]
    fn typing_slash_opens_menu() {
        let mut line = InputLine::new();
        let (c, k) = char_key('/');
        line.handle_key(c, k);
        assert!(line.menu_open());
        assert_eq!(line.menu_query(), Some(String::new()));
    }

    #[test]
    fn typing_after_slash_updates_query() {
        let mut line = InputLine::new();
        for ch in "/help".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert!(line.menu_open());
        assert_eq!(line.menu_query().as_deref(), Some("help"));
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut line = InputLine::new();
        for ch in "abc".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        line.handle_key(None, "Backspace");
        assert_eq!(line.buffer(), "ab");
        assert_eq!(line.cursor(), 2);
    }

    #[test]
    fn backspace_on_slash_closes_menu() {
        let mut line = InputLine::new();
        let (c, k) = char_key('/');
        line.handle_key(c, k);
        assert!(line.menu_open());
        line.handle_key(None, "Backspace");
        assert!(!line.menu_open());
    }

    #[test]
    fn enter_submits_when_menu_closed() {
        let mut line = InputLine::new();
        for ch in "hello".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(line.handle_key(None, "Enter"), InputAction::Submit("hello".to_string()));
        assert_eq!(line.buffer(), "");
    }

    #[test]
    fn enter_when_menu_open_accepts_selection() {
        let mut line = InputLine::new();
        for ch in "/he".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(line.handle_key(None, "Enter"), InputAction::MenuAccept);
        assert_eq!(line.buffer(), "/he");
    }

    #[test]
    fn up_down_routed_to_menu_when_open() {
        let mut line = InputLine::new();
        let (c, k) = char_key('/');
        line.handle_key(c, k);
        assert_eq!(line.handle_key(None, "Up"), InputAction::MenuUp);
        assert_eq!(line.handle_key(None, "Down"), InputAction::MenuDown);
    }

    #[test]
    fn tab_routed_to_menu_when_open() {
        let mut line = InputLine::new();
        let (c, k) = char_key('/');
        line.handle_key(c, k);
        assert_eq!(line.handle_key(None, "Tab"), InputAction::MenuAccept);
    }

    #[test]
    fn esc_closes_menu_when_open() {
        let mut line = InputLine::new();
        let (c, k) = char_key('/');
        line.handle_key(c, k);
        assert_eq!(line.handle_key(None, "Esc"), InputAction::CloseMenu);
        assert!(!line.menu_open());
    }

    #[test]
    fn esc_exits_when_buffer_empty() {
        let mut line = InputLine::new();
        assert_eq!(line.handle_key(None, "Esc"), InputAction::Exit);
    }

    #[test]
    fn esc_clears_when_buffer_nonempty() {
        let mut line = InputLine::new();
        for ch in "hello".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(line.handle_key(None, "Esc"), InputAction::Continue);
        assert_eq!(line.buffer(), "");
    }

    #[test]
    fn ctrl_c_exits() {
        let mut line = InputLine::new();
        assert_eq!(line.handle_key(None, "CtrlC"), InputAction::Exit);
    }

    #[test]
    fn left_right_move_cursor() {
        let mut line = InputLine::new();
        for ch in "abc".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        line.handle_key(None, "Left");
        assert_eq!(line.cursor(), 2);
        line.handle_key(None, "Left");
        assert_eq!(line.cursor(), 1);
        line.handle_key(None, "Right");
        assert_eq!(line.cursor(), 2);
    }

    #[test]
    fn accept_menu_completion_replaces_buffer() {
        let mut line = InputLine::new();
        for ch in "/he".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        line.accept_menu_completion("/help");
        assert_eq!(line.buffer(), "/help");
        assert_eq!(line.cursor(), 5);
        assert!(!line.menu_open());
    }

    #[test]
    fn submit_empty_line_returns_continue() {
        let mut line = InputLine::new();
        assert_eq!(line.handle_key(None, "Enter"), InputAction::Continue);
    }

    #[test]
    fn unicode_chars_handled_correctly() {
        let mut line = InputLine::new();
        let (c, k) = char_key('🦀');
        line.handle_key(c, k);
        assert_eq!(line.buffer(), "🦀");
        assert_eq!(line.cursor(), 4);
        line.handle_key(None, "Backspace");
        assert_eq!(line.buffer(), "");
        assert_eq!(line.cursor(), 0);
    }
}
