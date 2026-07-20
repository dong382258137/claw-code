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
    /// User pressed Up arrow (menu closed) to scroll output up one line.
    ScrollUpLine,
    /// User pressed Down arrow (menu closed) to scroll output down one line.
    ScrollDownLine,
    /// User pressed Tab to accept the selected menu item as completion.
    MenuAccept,
    /// User pressed F2 (or Ctrl+B) to toggle the right-hand sidebar.
    ToggleSidebar,
    /// User pressed Ctrl+T to toggle the latest tool card's collapse state.
    ToggleToolCard,
    /// User pressed PgUp to scroll the output view up one screen.
    ScrollUp,
    /// User pressed PgDn to scroll the output view down one screen.
    ScrollDown,
    /// User pressed `?` (with empty input) to toggle the keybindings overlay.
    ToggleHelp,
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

    /// Cursor position as character offset (for display positioning).
    pub(crate) fn cursor_char_offset(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
    }

    /// Cursor position as **display column width** (for terminal cursor
    /// positioning). This is different from `cursor_char_offset` for CJK /
    /// wide characters: a Chinese character occupies 2 columns but counts
    /// as 1 char. `set_cursor_position` expects a visual column, so this is
    /// the correct value to use for cursor placement.
    ///
    /// BUG fix: previously the TUI used `cursor_char_offset()` for cursor
    /// positioning, which caused the cursor to lag behind the actual text
    /// when typing CJK characters (e.g., typing "好的好的" placed the cursor
    /// 4 columns too far left, because each char is 2 columns wide).
    pub(crate) fn cursor_display_width(&self) -> usize {
        use unicode_width::UnicodeWidthStr;
        UnicodeWidthStr::width(&self.buffer[..self.cursor])
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
    /// "Backspace", "Left", "Right", "Tab", "Newline").
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

        // BUG 3 fix: Shift+Enter / Ctrl+J inserts a newline for multi-line
        // input. Routed here by `route_key` in app.rs which detects
        // `KeyModifiers::SHIFT | CONTROL` on `KeyCode::Enter` / `Char('j')`.
        // We must check this *before* the "Enter submits" branch below.
        if key == "Newline" {
            self.buffer.insert(self.cursor, '\n');
            self.cursor += 1; // '\n' is 1 byte
            self.update_menu_state();
            return InputAction::Continue;
        }

        if key == "Enter" {
            if self.menu_open {
                // BUG fix: when the slash menu is open, Enter must accept the
                // selected completion (not submit the half-typed query).
                // Previously this branch had a "query non-empty → submit
                // current buffer" shortcut that caused pressing Enter after
                // navigating the menu with Up/Down to send the half-typed
                // query (e.g., `/he` instead of `/help`). The event loop in
                // app.rs handles MenuAccept by replacing the buffer with the
                // selected command and then auto-submitting, so the user
                // gets the expected "Enter = accept + send" behavior.
                return InputAction::MenuAccept;
            }
            let submitted = self.buffer.clone();
            if self.buffer.trim().is_empty() {
                return InputAction::Continue;
            }
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
            debug_assert!(
                self.buffer.is_char_boundary(self.cursor),
                "cursor {} is not a char boundary in buffer of len {}",
                self.cursor,
                self.buffer.len()
            );
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

    /// 在当前光标位置插入粘贴的文本（支持多行）。
    ///
    /// **设计动机**：参考 CLI 路径（`input.rs` 的 `rustyline` + `.bracketed_paste(true)`）
    /// 把整段粘贴作为一个原子操作插入缓冲区，而不是逐字符触发 `Event::Key` ——
    /// 后者会让粘贴的 `\n` 被当作 `Enter` 立即提交，导致一段多行文本被切成多次 Submit。
    ///
    /// TUI 路径此前完全没有处理 `Event::Paste`，多行粘贴只能靠 `Ctrl+J` 手动逐行
    /// 重组，体验极差。配合 `crossterm::event::EnableBracketedPaste`（在
    /// `run_tui_repl` 中启用），整段粘贴会作为单个 `Event::Paste(String)` 投递，
    /// 这里一次性插入到 buffer，保留所有原始换行符。
    ///
    /// 插入后光标移动到粘贴文本末尾，菜单状态按新 buffer 重新计算。
    pub(crate) fn insert_paste(&mut self, text: &str) {
        // 粘贴内容可能含任意字符（包括 `\n`、`\t`、CJK、emoji），
        // 全部按原样插入。debug_assert 检查光标在字符边界。
        debug_assert!(
            self.buffer.is_char_boundary(self.cursor),
            "cursor {} is not a char boundary in buffer of len {}",
            self.cursor,
            self.buffer.len()
        );
        self.buffer.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.update_menu_state();
    }

    /// 返回光标所在的"当前行"内容及在该行中的字节偏移。
    ///
    /// 用于多行 buffer 的光标定位：渲染时只显示当前行，光标 Y 坐标
    /// 固定在输入框第一行，X 坐标按当前行左侧文本的显示宽度计算。
    /// 这样多行粘贴或 `Ctrl+J` 多行编辑时光标位置始终正确。
    pub(crate) fn cursor_line_and_column(&self) -> (usize /*line_idx*/, usize /*byte_offset_in_line*/, &str /*line_content_before_cursor*/) {
        let left = &self.buffer[..self.cursor];
        // 计算光标前有多少个 \n，决定当前是第几行
        let line_idx = left.matches('\n').count();
        // 当前行的起点：最后一个 \n 之后
        let line_start = left.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let byte_offset_in_line = self.cursor - line_start;
        let line_content_before_cursor = &left[line_start..self.cursor];
        (line_idx, byte_offset_in_line, line_content_before_cursor)
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
    fn enter_when_menu_open_with_query_returns_menu_accept() {
        // BUG fix: when the slash menu is open, Enter must always return
        // MenuAccept (not Submit the half-typed query). The event loop in
        // app.rs handles MenuAccept by replacing the buffer with the selected
        // command and then auto-submitting. Previously Enter would submit
        // the half-typed query (e.g., `/he` instead of `/help`), which was
        // incorrect — the user's menu selection was ignored.
        let mut line = InputLine::new();
        for ch in "/he".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(line.handle_key(None, "Enter"), InputAction::MenuAccept);
        // Buffer should still be "/he" — MenuAccept only changes buffer via
        // accept_menu_completion, which is called by the event loop.
        assert_eq!(line.buffer(), "/he");
    }

    #[test]
    fn enter_when_menu_open_with_bare_slash_accepts_selection() {
        // When only "/" is typed (no query), Enter should accept the menu selection
        let mut line = InputLine::new();
        let (c, k) = char_key('/');
        line.handle_key(c, k);
        assert_eq!(line.handle_key(None, "Enter"), InputAction::MenuAccept);
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

    // BUG 3 regression tests: Shift+Enter / Ctrl+J must insert a newline
    // instead of submitting, enabling multi-line input.

    #[test]
    fn newline_key_inserts_newline_into_buffer() {
        let mut line = InputLine::new();
        for ch in "hello".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(line.handle_key(None, "Newline"), InputAction::Continue);
        for ch in "world".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(line.buffer(), "hello\nworld");
        assert_eq!(line.cursor(), "hello\nworld".len());
    }

    #[test]
    fn newline_key_does_not_submit_even_with_content() {
        // Ensure Newline is handled before the Enter/submit branch.
        let mut line = InputLine::new();
        for ch in "foo".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        let action = line.handle_key(None, "Newline");
        assert_eq!(action, InputAction::Continue);
        assert_eq!(line.buffer(), "foo\n");
    }

    #[test]
    fn newline_key_advances_cursor_past_inserted_newline() {
        let mut line = InputLine::new();
        line.handle_key(Some('a'), "");
        line.handle_key(None, "Newline");
        // cursor must be after the '\n' so subsequent typing goes to line 2
        assert_eq!(line.cursor(), 2);
        line.handle_key(Some('b'), "");
        assert_eq!(line.buffer(), "a\nb");
        assert_eq!(line.cursor_char_offset(), 3);
    }

    #[test]
    fn cursor_display_width_accounts_for_cjk_wide_chars() {
        // BUG regression test: cursor_display_width must return the visual
        // column width (2 per CJK char), not the char count (1 per char).
        // Typing "好的好的" → 4 chars, 8 display columns.
        let mut line = InputLine::new();
        for ch in "好的好的".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.buffer(), "好的好的");
        assert_eq!(line.cursor_char_offset(), 4); // char count
        assert_eq!(line.cursor_display_width(), 8); // visual columns
    }

    #[test]
    fn cursor_display_width_mixed_ascii_and_cjk() {
        // Mixed content: "a你b好" → 4 chars, a/b = 1 col each, 你/好 = 2 cols
        // each → total 6 display columns.
        let mut line = InputLine::new();
        for ch in "a你b好".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.cursor_char_offset(), 4);
        assert_eq!(line.cursor_display_width(), 6);
    }

    #[test]
    fn cursor_display_width_after_backspace_in_cjk() {
        // After typing "你好" and backspace, buffer is "你", cursor at byte 3.
        // Display width should be 2 (one CJK char).
        let mut line = InputLine::new();
        for ch in "你好".chars() {
            line.handle_key(Some(ch), "");
        }
        line.handle_key(None, "Backspace");
        assert_eq!(line.buffer(), "你");
        assert_eq!(line.cursor_display_width(), 2);
    }

    #[test]
    fn cursor_display_width_mid_buffer_uses_left_slice_only() {
        // Move cursor to middle of "ab你好" (after "ab你", before "好").
        // Left slice "ab你" = 1 + 1 + 2 = 4 display columns.
        let mut line = InputLine::new();
        for ch in "ab你好".chars() {
            line.handle_key(Some(ch), "");
        }
        // Move cursor left twice → cursor sits between "你" and "好"
        line.handle_key(None, "Left");
        assert_eq!(line.cursor_display_width(), 4);
    }

    #[test]
    fn enter_still_submits_when_newline_key_not_used() {
        // Regression guard: Enter must still submit a single-line buffer.
        let mut line = InputLine::new();
        for ch in "submit me".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(
            line.handle_key(None, "Enter"),
            InputAction::Submit("submit me".to_string())
        );
    }

    #[test]
    fn multi_line_buffer_submits_with_embedded_newline() {
        let mut line = InputLine::new();
        for ch in "line1".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        line.handle_key(None, "Newline");
        for ch in "line2".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        let action = line.handle_key(None, "Enter");
        assert_eq!(action, InputAction::Submit("line1\nline2".to_string()));
        assert_eq!(line.buffer(), "");
    }
}
