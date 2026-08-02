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
    /// User pressed Ctrl+C while a turn is running (interrupt, not exit).
    InterruptTurn,
    /// User pressed Alt+Up to scroll the sidebar tool history up (earlier).
    SidebarScrollUp,
    /// User pressed Alt+Down to scroll the sidebar tool history down (newer).
    SidebarScrollDown,
    /// User pressed End to jump back to bottom (follow mode) + clear new output counter.
    JumpToBottom,
    /// User pressed E (with empty buffer) to jump to the next error entry.
    JumpToNextError,
    /// No-op (key not handled).
    Ignore,
}

/// Single-line input state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CsiState {
    /// Normal mode: inserting characters into buffer.
    Normal,
    /// Just saw \\x1b, waiting for `[` or `]` to enter CSI/OSC consumption.
    ExpectingCsi,
    /// Inside a CSI sequence (\\x1b[...), consuming all chars until terminator.
    ConsumingCsi,
    /// Inside an OSC sequence (\\x1b]...), consuming all chars until ST or BEL.
    ConsumingOsc,
}

/// Single-line input state.
#[derive(Debug, Clone)]
pub(crate) struct InputLine {
    buffer: String,
    cursor: usize,
    /// True when slash menu is currently shown (buffer starts with `/`).
    menu_open: bool,
    /// Accept 后锁定菜单自动重开，避免 Backspace 编辑已确认命令时菜单反复弹出。
    /// 只有用户主动输入新的 `/` 才解锁。
    /// false=正常状态；true=刚 accept 完，编辑不应触发菜单。
    menu_locked: bool,
    /// CSI 消费状态机：防止逐字符路径（无 bracketed paste 终端）中
    /// ANSI 转义序列字符污染 input buffer。见 handle_key 中的消费逻辑。
    csi_state: CsiState,
}

impl InputLine {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            menu_open: false,
            menu_locked: false,
            csi_state: CsiState::Normal,
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

    /// True if the CSI state machine is actively consuming an ANSI escape
    /// sequence (i.e., `csi_state != Normal`).
    ///
    /// Used by `route_key` to decide whether to feed `\x1b` from a
    /// `KeyCode::Esc` event directly into the state machine (when already
    /// consuming) or to do a peek-ahead to distinguish genuine Esc keypress
    /// from ANSI ESC.
    pub(crate) fn is_consuming_ansi(&self) -> bool {
        !matches!(self.csi_state, CsiState::Normal)
    }

    /// Reset to empty state.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.menu_open = false;
        self.menu_locked = false;
        self.csi_state = CsiState::Normal;
    }

    /// Bug L1 修复：恢复 buffer 内容（用于 Submit 后发现不能提交时回填）。
    /// InputLine::handle_key 在返回 Submit 前已 reset，但 app.rs 可能因
    /// turn 正在运行而拒绝提交。此时用此方法把内容放回 buffer，避免丢失。
    /// 光标位置设到末尾（用户刚敲完 Enter 的自然位置）。
    pub(crate) fn restore_input(&mut self, content: String) {
        self.buffer = content;
        self.cursor = self.buffer.len();
        // 不恢复 menu_open 状态：用户刚按 Enter 是要提交，不是要开菜单。
        self.menu_open = false;
        self.menu_locked = false;
        self.csi_state = CsiState::Normal;
    }

    /// Sync the slash menu's query with the current buffer (if menu is open).
    /// Returns the query to pass to `SlashMenu::set_query`.
    ///
    /// 注意：Sub 层级下 buffer 形如 `/mcp `，`/` 后的内容是 `mcp `，
    /// 这不是子选项的过滤 query。调用方（app.rs）应根据 menu.level() 判断
    /// 是否使用此 query。Sub 层级下通常传空 query 显示全部子选项。
    pub(crate) fn menu_query(&self) -> Option<String> {
        if !self.menu_open {
            return None;
        }
        self.buffer.strip_prefix('/').map(|rest| rest.to_string())
    }

    /// 二级菜单专用 query：返回最后一个空格之后的内容作为子选项过滤词。
    /// 如 `/mcp li` → query=`li`（过滤出 list）；`/mcp ` → query=空（显示全部）。
    pub(crate) fn sub_menu_query(&self) -> Option<String> {
        if !self.menu_open {
            return None;
        }
        let after_slash = self.buffer.strip_prefix('/')?;
        let last_space = after_slash.rfind(' ');
        Some(
            last_space
                .map(|p| after_slash[p + 1..].to_string())
                .unwrap_or_default(),
        )
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
            // BUG 修复：逐字符路径的 ANSI 转义序列过滤（状态机方案）。
            //
            // 当终端不支持 bracketed paste 时，粘贴的文本（含 ANSI 转义序列
            // 如 \x1b[2;3H）会作为普通 KeyCode::Char 事件逐字符投递。
            // 单纯过滤 \x1b 不够——CSI 序列的其余字符（[, 数字, ;, H 等）
            // 是普通可打印字符，仍会进入 buffer 造成污染和渲染崩溃。
            //
            // 这里用 CsiState 状态机追踪 ANSI 序列的消费：
            //   Normal → \x1b → ExpectingCsi → [ → ConsumingCsi → H → Normal
            //                                → ] → ConsumingOsc → BEL → Normal
            //
            // insert_paste 路径（Event::Paste / Ctrl+V）已有
            // strip_ansi_and_control 过滤，此修复补上逐字符路径的缺口。
            match self.csi_state {
                CsiState::ConsumingCsi => {
                    // 在 CSI 序列中：消费所有参数字符直到终止字母或 ~。
                    // 参数包括数字、分号、问号、空格、!、#、$、" 等中间字符。
                    if ch.is_ascii_alphabetic() || ch == '~' {
                        self.csi_state = CsiState::Normal; // 序列结束
                    }
                    // 中间字符或终止符：都不插入 buffer
                }
                CsiState::ConsumingOsc => {
                    // 在 OSC 序列中：\\x1b]...\\a 或 \\x1b]...\\x1b\\
                    if ch == '\x07' {
                        self.csi_state = CsiState::Normal; // BEL 终止
                    } else if ch == '\x1b' {
                        self.csi_state = CsiState::ExpectingCsi; // 等 \\ 确认 ST
                    }
                    // 其他字符：消费，不插入 buffer
                }
                CsiState::ExpectingCsi => {
                    // 刚看到 \\x1b，判断是 CSI（[）、OSC（]）还是孤立的 ESC
                    if ch == '[' {
                        self.csi_state = CsiState::ConsumingCsi;
                    } else if ch == ']' {
                        self.csi_state = CsiState::ConsumingOsc;
                    } else if ch == '\x1b' {
                        // 连续 ESC：第二个 \\x1b 在等待 [ 时到达。
                        // 保持 ExpectingCsi，重新等待 [。
                        // 修复 crossterm 把 \\x1b[A\\x1b[A 拆解为事件流时
                        // 第二个 \\x1b 在 ExpectingCsi 状态到达导致退回 Normal、
                        // 后续 [ 和 A/B 作为普通字符插入 buffer（输入框出现
                        // [A[B 残留）的根因 3 bug。
                        self.csi_state = CsiState::ExpectingCsi;
                    } else {
                        // 孤立的 \\x1b（如误触 Esc 键），回到 Normal
                        self.csi_state = CsiState::Normal;
                        // 如果这个非 [ / ] 字符是正常字符，需要插入 buffer
                        // 但通常 ESC 来自键盘而后续字符来自粘贴，重叠概率极低
                        // 保守处理：不插入，避免把 CSI 参数字符误插入
                    }
                }
                CsiState::Normal => {
                    // 正常模式：如果是 \\x1b（ESC 字符），进入状态机
                    if ch == '\x1b' {
                        self.csi_state = CsiState::ExpectingCsi;
                    } else {
                        // 过滤其他 C0 控制字符（0x00-0x1F）和 DEL（0x7F）
                        let cu = ch as u32;
                        if cu < 0x20 || cu == 0x7F {
                            // \\n / \\t / \\r 在 crossterm 中映射为 KeyCode::Enter / Tab，
                            // 理论上不会走 Some(ch) 分支，防御性保留
                            if ch == '\n' || ch == '\t' || ch == '\r' {
                                self.buffer.insert(self.cursor, ch);
                                self.cursor += ch.len_utf8();
                                self.update_menu_state();
                            }
                            // 其他 C0 字符：吐掉
                        } else {
                            debug_assert!(
                                self.buffer.is_char_boundary(self.cursor),
                                "cursor {} is not a char boundary in buffer of len {}",
                                self.cursor,
                                self.buffer.len()
                            );
                            self.buffer.insert(self.cursor, ch);
                            self.cursor += ch.len_utf8();
                            self.update_menu_state();
                        }
                    }
                }
            }
            return InputAction::Continue;
        }

        InputAction::Ignore
    }

    /// Accept a menu selection: replace buffer with the selected command
    /// (e.g., `/help`), position cursor at end, close menu.
    /// 同时进入 menu_locked 状态：Backspace 编辑已 accept 的命令不会自动重开菜单，
    /// 只有用户主动输入新的 `/` 才解锁。修复"选中后无法删除"问题。
    pub(crate) fn accept_menu_completion(&mut self, completion: &str) {
        self.buffer.clear();
        self.buffer.push_str(completion);
        self.cursor = self.buffer.len();
        self.menu_open = false;
        self.menu_locked = true;
    }

    /// 进入二级菜单时设置 buffer（如 `/mcp `），保持 menu_open=true
    /// 且不进入 menu_locked，让用户能继续编辑或用上下键选子选项。
    pub(crate) fn set_buffer_for_sub_menu(&mut self, content: &str) {
        self.buffer = content.to_string();
        self.cursor = self.buffer.len();
        self.menu_open = true;
        self.menu_locked = false;
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
    ///
    /// **安全加固**：粘贴内容在插入前经过 ANSI 转义序列和 C0 控制字符过滤，
    /// 防止从终端复制含 ratatui 渲染码的文本后粘贴导致 input buffer 被污染。
    pub(crate) fn insert_paste(&mut self, text: &str) {
        let text = strip_ansi_and_control(text);
        if text.is_empty() {
            return;
        }
        // 粘贴内容可能含任意合法字符（包括 `\n`、`\t`、CJK、emoji），
        // 全部按原样插入。debug_assert 检查光标在字符边界。
        debug_assert!(
            self.buffer.is_char_boundary(self.cursor),
            "cursor {} is not a char boundary in buffer of len {}",
            self.cursor,
            self.buffer.len()
        );
        self.buffer.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.update_menu_state();
    }

    /// 返回光标所在的"当前行"内容及在该行中的字节偏移。
    ///
    /// 用于多行 buffer 的光标定位：渲染时只显示当前行，光标 Y 坐标
    /// 固定在输入框第一行，X 坐标按当前行左侧文本的显示宽度计算。
    /// 这样多行粘贴或 `Ctrl+J` 多行编辑时光标位置始终正确。
    pub(crate) fn cursor_line_and_column(
        &self,
    ) -> (
        usize, /*line_idx*/
        usize, /*byte_offset_in_line*/
        &str,  /*line_content_before_cursor*/
    ) {
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
        // 修复"选中后无法删除"问题：
        // - accept_menu_completion 后 menu_locked=true，buffer 形如 `/help`
        // - 此时 Backspace 编辑不应重开菜单（否则用户改不动已选命令）
        // - 只有当 buffer 不再以 `/` 开头（删到 / 之前），或重新清空后输入新 `/` 时才解锁
        // 检测"用户主动输入新 /"：buffer 变成单个 `/` 或 `/` 后跟新字符
        if self.menu_locked {
            // 解锁条件：buffer 不以 `/` 开头（用户已删到 / 之前）
            if !self.buffer.starts_with('/') {
                self.menu_locked = false;
                self.menu_open = false;
                return;
            }
            // 解锁条件：buffer 仅剩 `/`（用户删完了 `/xxx`，重新进入输入态）
            // 此时如果用户继续输入字符，应重新弹菜单
            if self.buffer == "/" {
                self.menu_locked = false;
                self.menu_open = true;
                return;
            }
            // 锁定态下保持菜单关闭
            self.menu_open = false;
            return;
        }
        // 非锁定态：buffer 以 `/` 开头则开菜单
        self.menu_open = self.buffer.starts_with('/');
    }
}

impl Default for InputLine {
    fn default() -> Self {
        Self::new()
    }
}

/// 从粘贴文本中剥离 ANSI 转义序列和 C0 控制字符。
///
/// 防止从终端复制含 ratatui 渲染码（SGR 颜色、光标定位等）的文本后
/// 粘贴导致 input buffer 被 ANSI 序列污染，出现不可删除/不可发送的垃圾数据。
///
/// 处理规则：
/// - `\x1b[` 开头的 CSI 序列 → 跳过直到终止字母
/// - `\x1b` 及紧随的非 `[` 字符（如 `\x1b]` OSC 序列）→ 移除
/// - C0 控制字符（0x00-0x1F）→ 移除（保留 \n、\t、\r）
fn strip_ansi_and_control(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => {
                // ESC 序列：如果下一个是 `[`，跳过整个 CSI 序列
                if chars.peek() == Some(&'[') {
                    chars.next(); // consume `[`
                                  // 跳过参数部分（数字、分号、问号等），直到终止字母
                    while let Some(&c) = chars.peek() {
                        if c.is_ascii_alphabetic() || c == '~' {
                            chars.next(); // consume terminator
                            break;
                        }
                        chars.next(); // consume parameter char
                    }
                }
                // 否则只是孤立的 ESC，直接跳过
            }
            // 保留合法空白字符
            '\n' | '\t' | '\r' => result.push(ch),
            // 拒绝其他 C0 控制字符
            c if (c as u32) < 0x20 => {}
            // 正常字符
            _ => result.push(ch),
        }
    }

    result
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
        assert_eq!(
            line.handle_key(None, "Enter"),
            InputAction::Submit("hello".to_string())
        );
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

    // ── strip_ansi_and_control 测试 ──

    #[test]
    fn strip_sgr_leaves_plain_text() {
        // 模拟从终端复制含 SGR 颜色码的文本
        let input =
            "\x1b[38;5;240m[10:26:08]\x1b[0m \x1b[1m\x1b[38;5;3m模型 deepseek-v4-pro\x1b[0m";
        let result = super::strip_ansi_and_control(input);
        assert_eq!(result, "[10:26:08] 模型 deepseek-v4-pro");
    }

    #[test]
    fn strip_cup_and_cursor_sequences() {
        // 模拟光标定位序列（用户报告中出现的模式）
        let input = "\x1b[39m\x1b[49m\x1b[0m\x1b[?25h\x1b[46;3H\x1b[11;139H\x1b[1mhello";
        let result = super::strip_ansi_and_control(input);
        assert_eq!(result, "hello");
    }

    #[test]
    fn strip_lone_escape() {
        assert_eq!(super::strip_ansi_and_control("\x1bhello"), "hello");
    }

    #[test]
    fn strip_c0_control_chars_except_newline_tab_cr() {
        assert_eq!(
            super::strip_ansi_and_control("hello\x00world\x07!\n"),
            "helloworld!\n"
        );
    }

    #[test]
    fn preserve_newline_tab_cr() {
        assert_eq!(
            super::strip_ansi_and_control("line1\nline2\tindented\r"),
            "line1\nline2\tindented\r"
        );
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(super::strip_ansi_and_control("hello world"), "hello world");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(super::strip_ansi_and_control(""), "");
    }

    #[test]
    fn mixed_ansi_and_control_chars_full_cleanup() {
        // 真实场景：终端复制含 SGR + CUP + 控制字符
        let input = concat!(
            "\x1b[39m\x1b[49m\x1b[0m\x1b[?25h",
            "\x1b[46;3H\x1b[11;139H",
            "\x1b[1m\x1b[38;5;3;49m20\x1b[12;135H",
            "\x1b[22m\x1b[39;49m6\x1b[12;138Hedit",
            "\x1b[13;135H7\x1b[13;138Hread",
        );
        let result = super::strip_ansi_and_control(input);
        assert!(!result.contains('\x1b'));
        assert!(!result.contains('['));
        // 应该只保留可见文本
        assert_eq!(result, "206edit7read");
    }

    #[test]
    fn insert_paste_strips_ansi() {
        let mut line = InputLine::new();
        let dirty = "\x1b[38;5;240m[10:26:08]\x1b[0m hello \x1b[1mworld\x1b[0m";
        line.insert_paste(dirty);
        assert_eq!(line.buffer(), "[10:26:08] hello world");
    }

    #[test]
    fn insert_paste_preserves_multiline() {
        let mut line = InputLine::new();
        line.insert_paste("line1\nline2\nline3");
        assert_eq!(line.buffer(), "line1\nline2\nline3");
        // 确认 Enter 后整段作为一次 Submit 发送，而非逐行
        let action = line.handle_key(None, "Enter");
        assert_eq!(
            action,
            InputAction::Submit("line1\nline2\nline3".to_string())
        );
    }

    #[test]
    fn insert_paste_preserves_crlf() {
        let mut line = InputLine::new();
        line.insert_paste("line1\r\nline2\r\nline3");
        assert_eq!(line.buffer(), "line1\r\nline2\r\nline3");
    }

    #[test]
    fn insert_paste_multiline_with_ansi_stripped() {
        let mut line = InputLine::new();
        line.insert_paste("\x1b[32mline1\x1b[0m\n\x1b[31mline2\x1b[0m");
        assert_eq!(line.buffer(), "line1\nline2");
    }

    // ── C0 控制字符过滤测试（handle_key 逐字符路径） ──

    #[test]
    fn handle_key_filters_esc_character_from_buffer() {
        // ESC 字符 (U+001B) 不应进入 buffer。
        // 模拟逐字符粘贴含 ANSI 转义序列的文本（无 bracketed paste 终端）。
        let mut line = InputLine::new();
        // 粘贴 "\x1b[2;3Hhello" 逐字符
        for ch in "\x1b[2;3Hhello".chars() {
            line.handle_key(Some(ch), "");
        }
        // ESC + CSI 序列应被过滤，只剩 "hello"
        assert_eq!(line.buffer(), "hello");
    }

    #[test]
    fn handle_key_filters_c0_control_chars() {
        // NUL (0x00), BEL (0x07), BS (0x08), FF (0x0C) 等 C0 字符应被过滤
        let mut line = InputLine::new();
        for ch in "a\x00b\x07c\x08d\x0ce".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.buffer(), "abcde");
    }

    #[test]
    fn handle_key_filters_del_character() {
        // DEL (0x7F) 不应进入 buffer
        let mut line = InputLine::new();
        for ch in "x\x7fy".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.buffer(), "xy");
    }

    #[test]
    fn handle_key_normal_characters_still_work() {
        // 正常字符（含中文、emoji）不受 C0 过滤影响
        let mut line = InputLine::new();
        for ch in "你好🦀世界".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.buffer(), "你好🦀世界");
        assert_eq!(line.cursor_char_offset(), 5);
    }

    #[test]
    fn handle_key_backspace_works_after_c0_filtering() {
        // Backspace 删除应在 C0 过滤后仍正常工作
        let mut line = InputLine::new();
        for ch in "ab\x1b[31mc".chars() {
            line.handle_key(Some(ch), "");
        }
        // 期望: "abc" (ANSI 序列被过滤)
        assert_eq!(line.buffer(), "abc");
        // Backspace 删掉 'c'
        line.handle_key(None, "Backspace");
        assert_eq!(line.buffer(), "ab");
    }

    #[test]
    fn handle_key_simulated_ansi_paste_from_terminal() {
        // 真实场景模拟：从终端窗口复制含 CUP 光标定位序列 + SGR 颜色码的文本
        // "\x1b[2;3HWorkspace 版\x1b[2;15H本为 `2026.8.0`"
        let mut line = InputLine::new();
        let dirty = "\x1b[2;3HWorkspace 版\x1b[2;15H本为 `2026.8.0`";
        for ch in dirty.chars() {
            line.handle_key(Some(ch), "");
        }
        // CSI 序列的全部字符（\x1b, [, 数字, ;, H）被过滤，只剩可见文本
        assert_eq!(line.buffer(), "Workspace 版本为 `2026.8.0`");
    }

    #[test]
    fn handle_key_cursor_stays_correct_after_c0_filter() {
        // 过滤控制字符后光标位置应正确（跳过被滤掉的字节）。
        // 注意：\x1b 后的第一个非 [ / ] 字符也会被状态机消费（防止 CSI
        // 参数字符泄漏），因此 "a\x1bc" 实际留在 buffer 的只有 "a"。
        let mut line = InputLine::new();
        for ch in "a\x1bc".chars() {
            line.handle_key(Some(ch), "");
        }
        // \x1b 进入 ExpectingCsi，c 被消费（非 [ 非 ] → 回到 Normal 但不插入）
        assert_eq!(line.buffer(), "a");
        assert_eq!(line.cursor(), 1);
        // 再插入正常字符，应正确追加
        line.handle_key(Some('d'), "");
        assert_eq!(line.buffer(), "ad");
        assert_eq!(line.cursor(), 2);
    }

    // ── ESC peek-ahead 场景测试 ──
    // 模拟 conhost 粘贴含 ANSI 转义序列的内容。
    // route_key 中 KeyCode::Esc 被转换为 handle_key(Some('\x1b'), "")，
    // 而非 handle_key(None, "Esc")，确保 CSI 状态机正确消费整个序列。

    #[test]
    fn esc_char_feeds_into_csi_state_machine_not_key_handler() {
        // 核心场景：ESC 字符通过 Some('\x1b') 路径进入状态机，
        // 而非通过 None, "Esc" 路径触发 buffer reset。
        // 模拟 route_key 的 KeyCode::Esc → handle_key(Some('\x1b'), "") 调用。
        let mut line = InputLine::new();
        // 先插入一些内容
        for ch in "hello".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.buffer(), "hello");
        // ESC 字符进入状态机（而非 reset buffer）
        line.handle_key(Some('\x1b'), "");
        // buffer 不应被清空（ESC 进入 ExpectingCsi 状态）
        assert_eq!(line.buffer(), "hello");
        assert!(line.is_consuming_ansi());
    }

    #[test]
    fn ansi_csi_sequence_via_esc_char_fully_consumed() {
        // 模拟 conhost 粘贴 "\x1b[2;1Hhello" 的完整流程：
        // route_key 把 KeyCode::Esc 映射为 handle_key(Some('\x1b'), "")
        // 后续 [, 2, ;, 1, H 作为 KeyCode::Char 走 handle_key(Some(c), "")
        let mut line = InputLine::new();
        // ESC → ExpectingCsi
        line.handle_key(Some('\x1b'), "");
        // [ → ConsumingCsi
        line.handle_key(Some('['), "");
        // 参数字符被消费
        line.handle_key(Some('2'), "");
        line.handle_key(Some(';'), "");
        line.handle_key(Some('1'), "");
        // H 是终止符 → Normal
        line.handle_key(Some('H'), "");
        // 后续正常字符应被插入
        for ch in "hello".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.buffer(), "hello");
        assert!(!line.is_consuming_ansi());
    }

    #[test]
    fn multiple_ansi_sequences_via_esc_char_all_consumed() {
        // 模拟用户报告的场景：多个 ANSI 序列混合可见文本
        // "\x1b[2;1H\x1b[38;5;1;49m│ - pub fn\x1b[3;1H\x1b[39;49m├─ ✅"
        let mut line = InputLine::new();
        let dirty = "\x1b[2;1H\x1b[38;5;1;49m│ - pub fn\x1b[3;1H\x1b[39;49m├─ ✅";
        // 模拟 route_key：ESC → handle_key(Some('\x1b'), "")，其他字符 → handle_key(Some(c), "")
        for ch in dirty.chars() {
            if ch == '\x1b' {
                line.handle_key(Some('\x1b'), "");
            } else {
                line.handle_key(Some(ch), "");
            }
        }
        // ANSI 序列全部被过滤，只剩可见文本
        assert_eq!(line.buffer(), "│ - pub fn├─ ✅");
    }

    #[test]
    fn ansi_sgr_color_sequence_via_esc_char_consumed() {
        // SGR 颜色序列：\x1b[38;5;240m文本\x1b[0m
        let mut line = InputLine::new();
        let dirty = "\x1b[38;5;240m[10:26:08]\x1b[0m hello";
        for ch in dirty.chars() {
            if ch == '\x1b' {
                line.handle_key(Some('\x1b'), "");
            } else {
                line.handle_key(Some(ch), "");
            }
        }
        assert_eq!(line.buffer(), "[10:26:08] hello");
    }

    #[test]
    fn ansi_cup_sequence_via_esc_char_consumed() {
        // 光标定位序列 (CUP)：\x1b[2;1H + \x1b[3;7H 混合文本
        let mut line = InputLine::new();
        let dirty = "\x1b[2;1HWorkspace \x1b[2;15H版本为 `2026.8.0`";
        for ch in dirty.chars() {
            if ch == '\x1b' {
                line.handle_key(Some('\x1b'), "");
            } else {
                line.handle_key(Some(ch), "");
            }
        }
        assert_eq!(line.buffer(), "Workspace 版本为 `2026.8.0`");
    }

    #[test]
    fn genuine_esc_key_still_resets_buffer() {
        // 真正的 Esc 键通过 handle_key(None, "Esc") 调用，
        // 应该 reset buffer（原有行为不变）。
        let mut line = InputLine::new();
        for ch in "hello".chars() {
            line.handle_key(Some(ch), "");
        }
        assert_eq!(line.buffer(), "hello");
        // Esc 键 → reset
        let action = line.handle_key(None, "Esc");
        assert_eq!(action, InputAction::Continue);
        assert_eq!(line.buffer(), "");
    }

    #[test]
    fn genuine_esc_key_exits_when_buffer_empty() {
        let mut line = InputLine::new();
        assert_eq!(line.handle_key(None, "Esc"), InputAction::Exit);
    }

    #[test]
    fn is_consuming_ansi_reflects_csi_state() {
        let mut line = InputLine::new();
        assert!(!line.is_consuming_ansi()); // Normal
        line.handle_key(Some('\x1b'), "");
        assert!(line.is_consuming_ansi()); // ExpectingCsi
        line.handle_key(Some('['), "");
        assert!(line.is_consuming_ansi()); // ConsumingCsi
        line.handle_key(Some('H'), ""); // 终止符
        assert!(!line.is_consuming_ansi()); // Normal
    }

    #[test]
    fn osc_sequence_via_esc_char_consumed() {
        // OSC 序列：\x1b]0;title\x07
        let mut line = InputLine::new();
        let dirty = "\x1b]0;title\x07hello";
        for ch in dirty.chars() {
            if ch == '\x1b' {
                line.handle_key(Some('\x1b'), "");
            } else {
                line.handle_key(Some(ch), "");
            }
        }
        assert_eq!(line.buffer(), "hello");
    }

    #[test]
    fn osc_sequence_with_st_terminator_via_esc_char() {
        // OSC 序列用 ST (\x1b\\) 终止：\x1b]0;title\x1b\\hello
        // 第二个 \x1b 在 ConsumingOsc 状态下应正确进入 ExpectingCsi，
        // 然后 \\ 被消费（非 [ / ]），回到 Normal。
        let mut line = InputLine::new();
        let dirty = "\x1b]0;title\x1b\\hello";
        for ch in dirty.chars() {
            if ch == '\x1b' {
                line.handle_key(Some('\x1b'), "");
            } else {
                line.handle_key(Some(ch), "");
            }
        }
        assert_eq!(line.buffer(), "hello");
    }

    #[test]
    fn ansi_sequence_during_busy_does_not_leak() {
        // 模拟 cargo test 运行中（busy=true）时 reflected escape sequence 的处理。
        //
        // 根因：route_key 在 busy=true 且 peek-ahead poll(5ms) 超时时，
        // 旧代码丢弃 ESC（return Continue），后续 [, 2, ;, 1, H 作为普通
        // KeyCode::Char 插入 input buffer，造成输入栏被 ANSI 序列污染。
        //
        // 修复后：busy=true 时 route_key 总是把 \x1b 送入 CSI 状态机，
        // 后续字符由状态机消费。此测试验证状态机层面的行为。
        //
        // 场景来自用户截图：
        // \x1b[2;1H\x1b[38;2;163;190;140;48;5;236mt::tests::...ok
        let mut line = InputLine::new();
        // 模拟 route_key(busy=true)：ESC → handle_key(Some('\x1b'), "")
        line.handle_key(Some('\x1b'), "");
        // 后续字符作为 KeyCode::Char 到达，状态机应消费
        for ch in "[2;1H".chars() {
            line.handle_key(Some(ch), "");
        }
        // 第二个 ANSI 序列（SGR 颜色码）
        line.handle_key(Some('\x1b'), "");
        for ch in "[38;2;163;190;140;48;5;236m".chars() {
            line.handle_key(Some(ch), "");
        }
        // 可见文本应被正常插入
        for ch in "t::tests::upgrade_model_for_subagent_pro_returns_none ... ok".chars() {
            line.handle_key(Some(ch), "");
        }
        // buffer 只包含可见文本，无 ANSI 序列
        assert_eq!(
            line.buffer(),
            "t::tests::upgrade_model_for_subagent_pro_returns_none ... ok"
        );
        assert!(!line.is_consuming_ansi());
    }
}
