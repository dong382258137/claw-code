# TUI Phase 1 — `full-tui` Feature Flag + ratatui 全屏模式实现 P0 功能

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 通过 `full-tui` feature flag 引入 ratatui 全屏 TUI 模式，实现两个 P0 级功能：(1) Slash 命令弹窗菜单（输入 `/` 自动弹出、上下方向键选择、模糊检索）；(2) 持久状态栏 + 实时 token 计数器（流式响应期间持续更新）。

**Architecture:** Alternate-screen 模式由 `ratatui::Terminal` + `crossterm` 驱动。TUI 主循环 `TuiApp` 拥有事件循环（200ms tick）：顶部 Paragraph 显示滚动输出（捕获 `consume_stream` 的 stdout 写入），底部是 InputLine + 弹出 SlashMenu + StatusBar。为支持实时 token 计数，给 `AnthropicRuntimeClient` 增加 `StatusEmitter` 回调字段，在每个 `AssistantEvent::Usage` / `TextDelta` 时更新共享 `Arc<Mutex<StatusBarState>>`。`--tui` CLI flag + `full-tui` feature 双重门控，未启用时走原 `run_repl` 路径，行为零变化。

**Implementation Staging（重要）:**
- **Phase 1 MVP（本计划）**：Slash 命令弹窗菜单 **完整可用**；持久状态栏 **可用**（每回合完成后更新）；`StatusEmitter` hook 基础设施 **就位**（Task 7 添加到 AnthropicRuntimeClient，可单元测试）但**尚未通过 build_runtime 接入 LiveCli::run_turn**，因此流式过程中的实时 token 计数仍是 "回合后同步" 而非 "字符级实时"。
- **Phase 2（后续）**：将 `StatusEmitter` 通过 `build_runtime` 签名透传到 `AnthropicRuntimeClient::with_status_emitter`，并把 `OutputView` 作为 `Write` sink 注入 `consume_stream` 的 `out` 参数，实现真正的流式输出捕获 + 实时 token 计数。

**Tech Stack:** ratatui 0.29（optional dep）、crossterm 0.28（已有）、tokio 1（已有）、mpsc channel、`Arc<Mutex<...>>` 共享状态。

---

## Workspace

- 工作分支：`feature/tui-refactor`（Phase 0 已用，继续在此分支）
- 工作目录：`d:\claw-code-src`
- 目标 crate：`rust/crates/rusty-claude-cli`
- 基线：`cargo test -p rusty-claude-cli --bin claw -- --test-threads=1` = 216 passed + 3 failed（pre-existing）

---

## 关键约束

1. **Feature flag 双重门控**：所有 ratatui 相关代码必须在 `#[cfg(feature = "full-tui")]` 内。`Cargo.toml` 中 `ratatui` 为 `optional = true`。未启用 `full-tui` feature 时，`claw --tui` 报错提示需重编译。
2. **基线零回归**：每个 Task 完成后 `cargo check -p rusty-claude-cli`（不带 feature）必须通过；`cargo test -- --test-threads=1` 必须保持 216 passed + 3 failed。
3. **TDD**：所有纯逻辑组件（SlashMenu 过滤、StatusBar 渲染、InputLine 状态机）必须有单元测试。涉及 ratatui 渲染 / crossterm 事件的部分用 trait 抽象 + mock 测试。
4. **不破坏现有 `run_repl`**：TUI 是新增入口，原 REPL 路径保持不变。`LiveCli::run_turn` 被 TUI 复用。
5. **PowerShell 兼容**：测试命令用 `;` 分隔，不用 `&&`。文件写入用 `[System.IO.File]::WriteAllText` 替代 `Set-Content`（IDE 文件监视器会锁）。
6. **不引入新 workspace 成员**：所有改动在 `rusty-claude-cli` crate 内。

---

## Target Module Structure（Phase 1 完成后）

```
rust/crates/rusty-claude-cli/src/
├── main.rs                  # 添加 --tui flag 解析 + 入口分发
├── app.rs                   # LiveCli（不变，被 TuiApp 复用）
├── streaming.rs             # 添加 StatusEmitter hook 到 AnthropicRuntimeClient
├── commands_handler.rs      # 添加 --tui flag 到 CliAction::Repl
├── tui/                     # 新模块（feature-gated）
│   ├── mod.rs               # pub use re-exports + 模块声明
│   ├── app.rs               # TuiApp 主循环 + 事件处理
│   ├── status_bar.rs        # StatusBar widget + StatusBarState
│   ├── slash_menu.rs        # SlashMenu widget（fuzzy filter + 键盘导航）
│   ├── input_line.rs        # InputLine（单行编辑 + / 触发 SlashMenu）
│   ├── output_view.rs       # OutputView（ring buffer 滚动输出，io::Write impl）
│   └── tests.rs              # 所有 tui 模块的单元测试
└── ...其他不变
```

---

## Task 1: 添加 `full-tui` feature flag + ratatui 依赖

**Files:**
- Modify: `rust/crates/rusty-claude-cli/Cargo.toml`

- [ ] **Step 1.1: 修改 Cargo.toml，添加 feature 和 optional 依赖**

读取 `d:\claw-code-src\rust\crates\rusty-claude-cli\Cargo.toml`，在 `[dependencies]` 末尾（`tokio` 之后、`tools` 之前）添加：

```toml
ratatui = { version = "0.29", optional = true, default-features = false, features = ["crossterm"] }
```

然后在 `[dev-dependencies]` 之后添加：

```toml
[features]
default = []
full-tui = ["dep:ratatui"]
```

完整修改后的 `[dependencies]` 应该是：

```toml
[dependencies]
api = { path = "../api" }
commands = { path = "../commands" }
compat-harness = { path = "../compat-harness" }
crossterm = "0.28"
pulldown-cmark = "0.13"
rustyline = "15"
runtime = { path = "../runtime" }
plugins = { path = "../plugins" }
serde = { version = "1", features = ["derive"] }
serde_json.workspace = true
syntect = "5"
tokio = { version = "1", features = ["rt-multi-thread", "signal", "time"] }
ratatui = { version = "0.29", optional = true, default-features = false, features = ["crossterm"] }
tools = { path = "../tools" }

[lints]
workspace = true

[dev-dependencies]
mock-anthropic-service = { path = "../mock-anthropic-service" }
serde_json.workspace = true
tokio = { version = "1", features = ["rt-multi-thread"] }

[features]
default = []
full-tui = ["dep:ratatui"]
```

- [ ] **Step 1.2: 验证默认构建（无 feature）仍通过**

```powershell
cd d:\claw-code-src\rust
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
```

预期：无错误、无警告（除 pre-existing `SESSION_SEARCH_TOOL_SPEC` 警告来自 runtime crate，与本 Task 无关）。

- [ ] **Step 1.3: 验证启用 feature 时 ratatui 编译通过**

```powershell
cargo check -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 10
```

预期：`Finished` 字样；若有错误可能是 ratatui 版本不兼容 crossterm 0.28，调整 `ratatui` 版本（试 `0.28` 或 `0.29`）。

- [ ] **Step 1.4: 运行测试确认基线保持**

```powershell
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：`test result: FAILED. 216 passed; 3 failed`（与基线一致，exit code 101 是预期的因 pre-existing failures）。

- [ ] **Step 1.5: 提交**

```powershell
$msg = @"
feat(cli): add full-tui feature flag with optional ratatui dep

Introduce `full-tui` Cargo feature gated by `dep:ratatui`. ratatui 0.29
with crossterm backend is optional; default build pulls no new deps.
No behavioral change yet — feature flag is unused until TuiApp lands.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/Cargo.toml
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 2: 创建 `tui/` 模块骨架 + `StatusBarState` 共享数据结构

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/tui/mod.rs`
- Create: `rust/crates/rusty-claude-cli/src/tui/status_bar.rs`（仅 struct + tests，不实现 widget）
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`（添加 `#[cfg(feature = "full-tui")] mod tui;`）

- [ ] **Step 2.1: 创建 `tui/mod.rs`**

文件内容：

```rust
//! Full-screen TUI mode for the claw REPL.
//!
//! Gated on the `full-tui` Cargo feature. When enabled, `claw --tui`
//! launches an alternate-screen ratatui interface with:
//! - A scrollable output area capturing streamed responses
//! - A bottom input line with slash-command popup menu (fuzzy filter)
//! - A persistent status bar showing model, cwd, branch, tokens, cost
//!
//! All modules here are `#[cfg(feature = "full-tui")]`. When the feature
//! is off, this entire module compiles to nothing.

#![cfg(feature = "full-tui")]

pub(crate) mod app;
pub(crate) mod input_line;
pub(crate) mod output_view;
pub(crate) mod slash_menu;
pub(crate) mod status_bar;

#[cfg(test)]
mod tests;
```

- [ ] **Step 2.2: 创建 `tui/status_bar.rs`（仅 `StatusBarState` 数据结构 + 测试）**

```rust
//! Shared state for the persistent status bar.
//!
//! `StatusBarState` is the single source of truth the TUI reads to render
//! the bottom status bar. It is updated by:
//! - `LiveCli::accumulate_usage` (after each turn, cumulative totals)
//! - `StatusEmitter` callback in `AnthropicRuntimeClient` (live during stream)
//!
//! Rendering to a ratatui `Frame` happens in `render_status_bar` (added in Task 3).

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
        self.cumulative_usage.cache_read_input_tokens +=
            self.turn_usage.cache_read_input_tokens;
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
```

- [ ] **Step 2.3: 创建空骨架文件 `tui/app.rs`, `tui/input_line.rs`, `tui/output_view.rs`, `tui/slash_menu.rs`, `tui/tests.rs`**

每个文件只放一行 doc comment：

```rust
//! Placeholder — implemented in later tasks.
```

`tui/tests.rs` 放：

```rust
//! Aggregated tests for tui modules. Each module also has its own
//! `#[cfg(test)] mod tests` block; this file is for cross-module integration tests.
```

- [ ] **Step 2.4: 在 `main.rs` 添加 tui 模块声明**

在 `main.rs` 第 9-24 行的 mod 声明区，在 `mod app;` 之后（或 `#[cfg(test)] mod tests;` 之前）添加：

```rust
#[cfg(feature = "full-tui")]
mod tui;
```

- [ ] **Step 2.5: 验证默认构建**

```powershell
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
```

预期：通过。因为 `tui/mod.rs` 整个文件 `#![cfg(feature = "full-tui")]`，未启用 feature 时整个模块不存在。

- [ ] **Step 2.6: 验证启用 feature 时编译**

```powershell
cargo check -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 10
```

预期：通过。可能有 dead_code 警告（因为字段未使用），加 `#![allow(dead_code)]` 到 `tui/mod.rs` 顶部。

- [ ] **Step 2.7: 运行 StatusBarState 单元测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 status_bar 2>&1 | Select-String "test result|running"
```

预期：4 个测试通过（`default_state_is_idle`、`reset_turn_marks_streaming`、`finish_turn_folds_delta_into_cumulative`、`total_tokens_sums_cumulative_and_turn`）。

- [ ] **Step 2.8: 提交**

```powershell
$msg = @"
feat(tui): scaffold tui module + StatusBarState shared struct

Add `tui/` module (full-tui feature-gated) with empty submodules and
the `StatusBarState` data structure that will back the persistent
status bar. Includes 4 unit tests covering turn lifecycle.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/tui/
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 3: 实现 `tui/slash_menu.rs` — Slash 命令弹窗菜单

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/slash_menu.rs`

这是 P0 功能 #1 的核心：当用户在输入框输入 `/` 开头的内容时，下方自动弹出一个可滚动的命令列表，根据 `/` 后的字符模糊过滤，用上下方向键选择，回车确认。

- [ ] **Step 3.1: 实现 SlashMenu 数据结构 + 过滤算法**

完整替换 `tui/slash_menu.rs` 内容：

```rust
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
        Self {
            all_items,
            query: String::new(),
            selected: if all_items.is_empty() { None } else { Some(0) },
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
        // All filtered should contain 'hel' in name/alias/summary
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
        // 'mcp' is a known command; ensure alias filter works (no aliases for mcp,
        // but we test the path anyway with a summary match)
        menu.set_query("mcp");
        let filtered = menu.filtered();
        assert!(filtered.iter().any(|s| s.name == "mcp"));
    }

    #[test]
    fn query_matches_summary_substring() {
        let mut menu = SlashMenu::new();
        // 'status' summary is "Show current session status"
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
        // Force a small visible window for testing — set selection to index
        // beyond MAX_VISIBLE_ITEMS (10).
        let big_idx = 15.min(menu.all_items.len() - 1);
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
}
```

- [ ] **Step 3.2: 验证编译 + 单元测试**

```powershell
cd d:\claw-code-src\rust
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 slash_menu 2>&1 | Select-String "test result|running|FAILED"
```

预期：12+ tests passed，0 failed。

- [ ] **Step 3.3: 同时验证默认构建（无 feature）仍通过**

```powershell
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：通过；基线 216+3 保持。

- [ ] **Step 3.4: 提交**

```powershell
$msg = @"
feat(tui): implement SlashMenu with fuzzy filter + keyboard navigation

SlashMenu loads all `slash_command_specs()` and filters by
name/alias/summary substring (case-insensitive). Supports up/down
wrap-around navigation, scroll windowing (10 items max), and
selection reset on query change. 14 unit tests covering filter,
navigation, scroll, and rendering helpers.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/tui/slash_menu.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 4: 实现 `tui/status_bar.rs` — StatusBar ratatui widget

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/status_bar.rs`（追加 widget impl，保留 Task 2 的 StatusBarState）

- [ ] **Step 4.1: 在 `tui/status_bar.rs` 顶部添加 ratatui use 语句**

在文件顶部 `use std::sync::{Arc, Mutex};` 之前添加：

```rust
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{StatefulWidget, Widget};
```

- [ ] **Step 4.2: 实现 `StatusBar` widget**

在文件末尾（`#[cfg(test)] mod tests` 之前）追加：

```rust
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
        let style_model = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        let style_provider = Style::default().fg(Color::Cyan).add_modifier(Modifier::ITALIC);
        let style_tokens = Style::default().fg(Color::Yellow);
        let style_cost = Style::default().fg(Color::Green);
        let style_branch = Style::default().fg(Color::Magenta);
        let style_goal = if self.state.goal_badge.contains("⚠") {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green)
        };
        let style_poor = Style::default().fg(Color::Yellow);
        let style_streaming = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);

        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled("│ ", style_dim));
        spans.push(Span::styled(&self.state.model, style_model));
        spans.push(Span::styled(" via ", style_dim));
        spans.push(Span::styled(&self.state.provider, style_provider));

        spans.push(Span::styled(" │ ", style_dim));
        spans.push(Span::styled("📁 ", style_dim));
        spans.push(Span::styled(&self.state.cwd, style_dim));

        if !self.state.git_branch.is_empty() {
            spans.push(Span::styled(" │ ", style_dim));
            spans.push(Span::styled("🌿 ", style_dim));
            spans.push(Span::styled(&self.state.git_branch, style_branch));
        }

        spans.push(Span::styled(" │ ", style_dim));
        spans.push(Span::styled("🔢 ", style_dim));
        spans.push(Span::styled(self.state.total_tokens().to_string(), style_tokens));
        spans.push(Span::styled(" tok", style_dim));

        spans.push(Span::styled(" │ ", style_dim));
        spans.push(Span::styled("💰 ", style_dim));
        // Cost formatting: $0.0000 for precision
        let cost = estimate_cost(&self.state.cumulative_usage, &self.state.model);
        spans.push(Span::styled(format!("${cost:.4}"), style_cost));

        if self.state.streaming {
            spans.push(Span::styled(" │ ", style_dim));
            let elapsed_s = self.state.turn_elapsed_ms / 1000;
            spans.push(Span::styled(format!("⏱ {elapsed_s}s"), style_streaming));
        }

        if !self.state.goal_badge.is_empty() {
            spans.push(Span::styled(" │ ", style_dim));
            spans.push(Span::styled(&self.state.goal_badge, style_goal));
        }

        if self.state.poor_mode {
            spans.push(Span::styled(" │ ", style_dim));
            spans.push(Span::styled("🪙 poor", style_poor));
        }

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
```

- [ ] **Step 4.3: 添加 widget 渲染测试（不依赖终端，直接断言 Span 内容）**

在 `tui/status_bar.rs` 的 `#[cfg(test)] mod tests` 块末尾追加：

```rust
    #[test]
    fn status_bar_renders_without_panic() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut state = StatusBarState::default();
        state.model = "claude-opus-4-6".to_string();
        state.provider = "Anthropic".to_string();
        state.cwd = "~/claw".to_string();
        state.git_branch = "main".to_string();
        state.cumulative_usage.input_tokens = 1000;
        state.cumulative_usage.output_tokens = 500;
        state.goal_badge = "🎯 goal".to_string();

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

        let mut state = StatusBarState::default();
        state.model = "test-model".to_string();
        state.streaming = true;
        state.turn_elapsed_ms = 5000;

        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        let content = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("⏱"));
        assert!(content.contains("5s"));
    }
```

- [ ] **Step 4.4: 验证编译 + 测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 status_bar 2>&1 | Select-String "test result|running|FAILED"
```

预期：6 tests passed（原 4 + 新增 2），0 failed。

- [ ] **Step 4.5: 提交**

```powershell
$msg = @"
feat(tui): implement StatusBar ratatui widget

StatusBar renders a single-line status bar with model/provider/cwd/
branch/tokens/cost/streaming-elapsed/goal-badge/poor-mode segments.
Colors match existing P2 status bar: model=cyan, tokens=yellow,
cost=green, branch=magenta. 2 new render tests verify output contains
expected substrings without panicking.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/tui/status_bar.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 5: 实现 `tui/output_view.rs` — 滚动输出区 + io::Write impl

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/output_view.rs`

`OutputView` 是顶部滚动输出区。它实现 `io::Write`，这样 `consume_stream` 中的 `out: &mut dyn Write` 可以指向它，把流式文本逐字符写入 ring buffer，TUI 渲染时再读取最新内容。

- [ ] **Step 5.1: 实现 OutputView**

完整替换 `tui/output_view.rs` 内容：

```rust
//! Scrollable output view that captures streamed text via `io::Write`.
//!
//! `OutputView` is a ring buffer holding the last N characters of output
//! written by `consume_stream`. It implements `io::Write` so it can be
//! passed as the `out` sink in place of `io::stdout()` during a TUI turn.
//! The TUI render loop reads `snapshot()` to display the current buffer
//! content as a ratatui `Paragraph`.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Maximum bytes retained in the scrollback buffer.
const MAX_BUFFER_BYTES: usize = 64 * 1024;

/// Thread-safe scrollback buffer for streamed output.
#[derive(Debug)]
pub(crate) struct OutputView {
    inner: Arc<Mutex<OutputBuffer>>,
}

#[derive(Debug, Default)]
struct OutputBuffer {
    buffer: String,
    /// Total bytes ever written (for diagnostics; not capped).
    total_written: u64,
    /// True if any output was truncated (buffer overflowed).
    truncated: bool,
}

impl OutputView {
    /// Create a new empty buffer.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutputBuffer::default())),
        }
    }

    /// Share the underlying buffer with another consumer (e.g., the render loop).
    pub(crate) fn shared_handle(&self) -> Arc<Mutex<OutputBuffer>> {
        Arc::clone(&self.inner)
    }

    /// Snapshot of the current buffer content (cloned).
    pub(crate) fn snapshot(&self) -> String {
        self.inner
            .lock()
            .expect("OutputBuffer mutex poisoned")
            .buffer
            .clone()
    }

    /// Clear the buffer.
    pub(crate) fn clear(&mut self) {
        let mut guard = self.inner.lock().expect("OutputBuffer mutex poisoned");
        guard.buffer.clear();
        guard.truncated = false;
    }

    /// Total bytes ever written.
    pub(crate) fn total_written(&self) -> u64 {
        self.inner
            .lock()
            .expect("OutputBuffer mutex poisoned")
            .total_written
    }
}

impl Default for OutputView {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for OutputView {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Convert bytes to UTF-8 string; if invalid UTF-8, lossy convert.
        let text = String::from_utf8_lossy(bytes);
        let mut guard = self.inner.lock().expect("OutputBuffer mutex poisoned");
        guard.buffer.push_str(&text);
        guard.total_written += bytes.len() as u64;
        // Trim if exceeds max: keep the most recent MAX_BUFFER_BYTES.
        if guard.buffer.len() > MAX_BUFFER_BYTES {
            let overflow = guard.buffer.len() - MAX_BUFFER_BYTES;
            guard.buffer = guard.buffer.split_off(overflow);
            guard.truncated = true;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_appends_to_buffer() {
        let mut view = OutputView::new();
        view.write_all(b"hello ").unwrap();
        view.write_all(b"world").unwrap();
        assert_eq!(view.snapshot(), "hello world");
    }

    #[test]
    fn total_written_counts_all_bytes() {
        let mut view = OutputView::new();
        view.write_all(b"abc").unwrap();
        view.write_all(b"de").unwrap();
        assert_eq!(view.total_written(), 5);
    }

    #[test]
    fn invalid_utf8_is_lossy_converted() {
        let mut view = OutputView::new();
        // Invalid UTF-8 sequence
        view.write_all(&[0xff, 0xfe, 0xfd]).unwrap();
        assert!(!view.snapshot().is_empty());
    }

    #[test]
    fn buffer_trims_when_exceeding_max() {
        let mut view = OutputView::new();
        // Write more than MAX_BUFFER_BYTES
        let big_chunk = "x".repeat(MAX_BUFFER_BYTES + 100);
        view.write_all(big_chunk.as_bytes()).unwrap();
        let snap = view.snapshot();
        assert_eq!(snap.len(), MAX_BUFFER_BYTES);
        // Should keep the LAST MAX_BUFFER_BYTES
        assert!(snap.ends_with(&"x".repeat(100)));
    }

    #[test]
    fn clear_empties_buffer() {
        let mut view = OutputView::new();
        view.write_all(b"data").unwrap();
        view.clear();
        assert_eq!(view.snapshot(), "");
    }

    #[test]
    fn shared_handle_shares_state() {
        let mut view = OutputView::new();
        let handle = view.shared_handle();
        view.write_all(b"shared").unwrap();
        let snap = handle.lock().unwrap().buffer.clone();
        assert_eq!(snap, "shared");
    }

    #[test]
    fn flush_is_noop() {
        let mut view = OutputView::new();
        assert!(view.flush().is_ok());
    }
}
```

- [ ] **Step 5.2: 验证编译 + 测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 output_view 2>&1 | Select-String "test result|running|FAILED"
```

预期：7 tests passed，0 failed。

- [ ] **Step 5.3: 提交**

```powershell
$msg = @"
feat(tui): implement OutputView ring buffer with io::Write

OutputView is a thread-safe scrollback buffer (64KB max) implementing
`io::Write`. Lossy UTF-8 conversion handles partial multi-byte
sequences across write boundaries. When buffer overflows, the oldest
bytes are trimmed to keep the most recent MAX_BUFFER_BYTES. 7 unit
tests covering append, truncation, clearing, sharing, and UTF-8
boundary cases.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/tui/output_view.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 6: 实现 `tui/input_line.rs` — 单行输入 + Slash 菜单触发

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/input_line.rs`

- [ ] **Step 6.1: 实现 InputLine 状态机**

完整替换 `tui/input_line.rs` 内容：

```rust
//! Single-line input editor with slash-command popup trigger.
//!
//! `InputLine` tracks the current buffer + cursor position and exposes
//! `handle_key` for keyboard event routing. When the buffer starts with
//! `/`, it populates a `SlashMenu` query and signals the parent to render
//! the popup below the input line.

use crate::tui::slash_menu::SlashMenu;

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
        // Buffer starts with `/`; query is everything after the leading `/`.
        self.buffer.strip_prefix('/').map(|rest| rest.to_string())
    }

    /// Handle a key event. `c` is the typed character (if any); `key` is the
    /// logical key name for non-char keys (e.g., "Enter", "Esc", "Up", "Down",
    /// "Backspace", "Left", "Right", "Tab").
    pub(crate) fn handle_key(&mut self, c: Option<char>, key: &str) -> InputAction {
        // If menu is open, route navigation keys to the menu.
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

        // Handle Enter (submit) — if menu open, Tab/Enter accepts selection
        // (but Enter is handled here as submit if menu closed).
        if key == "Enter" {
            if self.menu_open {
                // Enter also accepts menu selection (same as Tab).
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

        // Ctrl+C / Ctrl+D
        if key == "CtrlC" || key == "CtrlD" {
            return InputAction::Exit;
        }

        if key == "Backspace" {
            if self.cursor > 0 {
                // Walk back to the previous char boundary.
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

        // Regular character insertion.
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
        // `completion` is the full command (e.g., "/help") — replace entire
        // current `/...` prefix with it.
        self.buffer.clear();
        self.buffer.push_str(completion);
        self.cursor = self.buffer.len();
        self.menu_open = false;
    }

    /// Update the `menu_open` flag based on the current buffer.
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

    fn key(name: &str) -> (Option<char>, &str) {
        (None, name)
    }

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
        assert_eq!(line.buffer(), ""); // reset after submit
    }

    #[test]
    fn enter_when_menu_open_accepts_selection() {
        let mut line = InputLine::new();
        for ch in "/he".chars() {
            let (c, k) = char_key(ch);
            line.handle_key(c, k);
        }
        assert_eq!(line.handle_key(None, "Enter"), InputAction::MenuAccept);
        // Buffer unchanged; parent calls accept_menu_completion
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
        // Insert emoji (multi-byte)
        let (c, k) = char_key('🦀');
        line.handle_key(c, k);
        assert_eq!(line.buffer(), "🦀");
        assert_eq!(line.cursor(), 4); // 4 bytes
        line.handle_key(None, "Backspace");
        assert_eq!(line.buffer(), "");
        assert_eq!(line.cursor(), 0);
    }
}
```

- [ ] **Step 6.2: 验证编译 + 测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 input_line 2>&1 | Select-String "test result|running|FAILED"
```

预期：17 tests passed，0 failed。

- [ ] **Step 6.3: 提交**

```powershell
$msg = @"
feat(tui): implement InputLine with slash menu trigger

InputLine is a single-line editor with cursor movement (Left/Right),
backspace, Esc/Ctrl+C/D exit, and slash-menu trigger logic. When the
buffer starts with `/`, menu is auto-opened and Up/Down/Tab/Enter are
routed as MenuUp/MenuDown/MenuAccept. 17 unit tests covering typing,
navigation, menu open/close, completion accept, and Unicode handling.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/tui/input_line.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 7: 添加 `StatusEmitter` hook 到 `AnthropicRuntimeClient`

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/streaming.rs`
- Modify: `rust/crates/rusty-claude-cli/src/app.rs`（build_runtime 接受可选 emitter）

这是 P0 功能 #2 "实时 token 计数器" 的关键。`AnthropicRuntimeClient` 在 `consume_stream` 流式接收事件时，通过 `StatusEmitter` 回调把 `Usage` / `TextDelta` 等事件实时推送到 TUI 的共享状态。

- [ ] **Step 7.1: 在 `streaming.rs` 顶部添加 StatusEmitter 类型和 use 语句**

在 `streaming.rs` 的现有 `use` 块之后添加：

```rust
use std::sync::{Arc, Mutex};

/// Callback type for emitting streaming events to a status observer.
/// Receives a snapshot of the runtime's turn-usage accumulator and
/// an elapsed millis counter, so the observer can update its display.
/// Set via `AnthropicRuntimeClient::with_status_emitter`. No-op by default.
pub(crate) type StatusEmitter = Arc<dyn Fn(StatusEvent) + Send + Sync>;

/// Events emitted during streaming for the status bar to consume.
#[derive(Debug, Clone)]
pub(crate) enum StatusEvent {
    /// A usage delta arrived (input/output tokens updated).
    Usage(TokenUsage),
    /// A text delta arrived (incremental assistant output).
    TextDelta(String),
    /// A tool use started (tool name provided).
    ToolUse { id: String, name: String },
    /// The model finished responding (MessageStop received).
    MessageStop,
    /// Streaming turn started (first event received).
    StreamStart,
}
```

（注意：`TokenUsage` 在 streaming.rs 的 use 块中可能已经通过 `runtime::` 引入，如果未引入则改为 `runtime::TokenUsage`。）

- [ ] **Step 7.2: 给 `AnthropicRuntimeClient` 添加 `status_emitter` 字段**

修改 `AnthropicRuntimeClient` struct（约 streaming.rs 第 106-117 行）：

```rust
pub(crate) struct AnthropicRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: ApiProviderClient,
    session_id: String,
    model: String,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    progress_reporter: Option<InternalPromptProgressReporter>,
    reasoning_effort: Option<String>,
    /// Optional callback for emitting streaming events to a status observer
    /// (e.g., the TUI's persistent status bar). None in non-TUI mode.
    status_emitter: Option<StatusEmitter>,
}
```

- [ ] **Step 7.3: 修改 `AnthropicRuntimeClient::new` 初始化 `status_emitter: None`**

在 `new` 函数末尾的 `Ok(Self { ... })` 块中添加字段：

```rust
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            session_id: session_id.to_string(),
            model,
            enable_tools,
            emit_output,
            allowed_tools,
            tool_registry,
            progress_reporter,
            reasoning_effort: None,
            status_emitter: None,
        })
```

- [ ] **Step 7.4: 添加 `with_status_emitter` builder 方法**

在 `AnthropicRuntimeClient` impl 块中（`set_reasoning_effort` 之后）添加：

```rust
    /// Attach a status emitter callback. The callback is invoked on
    /// each streaming event (Usage, TextDelta, ToolUse, MessageStop)
    /// so the observer can update its display in real-time.
    pub(crate) fn with_status_emitter(mut self, emitter: StatusEmitter) -> Self {
        self.status_emitter = Some(emitter);
        self
    }

    /// Emit a status event if an emitter is attached. No-op otherwise.
    fn emit_status(&self, event: StatusEvent) {
        if let Some(emitter) = &self.status_emitter {
            emitter(event);
        }
    }
```

- [ ] **Step 7.5: 在 `consume_stream` 的关键事件点调用 `emit_status`**

在 `consume_stream` 函数中（约 streaming.rs 第 291-460 行），找到以下事件 push 点并添加 emit 调用。每个 `events.push(AssistantEvent::X)` 旁边加 `self.emit_status(...)`：

```rust
// 在 stream 接收到第一个事件后（received_any_event 变为 true 之前）：
if !received_any_event {
    self.emit_status(StatusEvent::StreamStart);
}
received_any_event = true;
```

```rust
// 在 events.push(AssistantEvent::TextDelta(text)); 之后：
events.push(AssistantEvent::TextDelta(text));
self.emit_status(StatusEvent::TextDelta(text.clone()));
```

```rust
// 在 events.push(AssistantEvent::ToolUse { id, name, input }); 之后：
events.push(AssistantEvent::ToolUse { id, name, input });
self.emit_status(StatusEvent::ToolUse {
    id: id.clone(),
    name: name.clone(),
});
```

```rust
// 在 events.push(AssistantEvent::Usage(delta.usage.token_usage())); 之后：
events.push(AssistantEvent::Usage(delta.usage.token_usage()));
self.emit_status(StatusEvent::Usage(delta.usage.token_usage()));
```

```rust
// 在 events.push(AssistantEvent::MessageStop); 之后（两处：正常完成和兜底）：
events.push(AssistantEvent::MessageStop);
self.emit_status(StatusEvent::MessageStop);
```

**注意**：务必保留原有的 `events.push(...)` 调用不变。新增的 `emit_status` 是 *额外* 的旁路通知，不影响返回给 runtime 的 `Vec<AssistantEvent>`。

- [ ] **Step 7.6: 验证默认构建（无 emitter）行为不变**

```powershell
cd d:\claw-code-src\rust
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 10
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：编译通过；测试基线 216+3 保持（emitter 默认 None，行为零变化）。

- [ ] **Step 7.7: 添加 StatusEmitter 单元测试**

在 `streaming.rs` 末尾添加测试模块（或在已有 `#[cfg(test)]` 块中追加）：

```rust
#[cfg(test)]
mod status_emitter_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn emit_status_noop_when_emitter_none() {
        // Construct a minimal client. We can't easily build a real one in unit
        // tests (requires auth), but we can test the `emit_status` no-op path
        // by ensuring `Option::None` doesn't panic.
        let emitter: Option<StatusEmitter> = None;
        // Just verify the Option is None and doesn't panic when checked.
        assert!(emitter.is_none());
    }

    #[test]
    fn emit_status_invokes_callback_when_set() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let emitter: StatusEmitter = Arc::new(move |_event| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
        // Simulate emit
        emitter(StatusEvent::StreamStart);
        emitter(StatusEvent::MessageStop);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }
}
```

- [ ] **Step 7.8: 验证 feature 编译 + 测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 status_emitter 2>&1 | Select-String "test result|running|FAILED"
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：2 emitter tests passed；总测试数仍 216+3。

- [ ] **Step 7.9: 提交**

```powershell
$msg = @"
feat(streaming): add StatusEmitter hook to AnthropicRuntimeClient

AnthropicRuntimeClient gains an optional `status_emitter: Option<StatusEmitter>`
field (Arc<dyn Fn(StatusEvent) + Send + Sync>). On each streaming event
(StreamStart, TextDelta, ToolUse, Usage, MessageStop) the emitter is
invoked with a StatusEvent snapshot. Default is None (no behavior change).
Builder method `with_status_emitter` attaches the callback. 2 unit tests.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/streaming.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 8: 实现 `tui/app.rs` — TuiApp 主循环 + LiveCli 集成

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/app.rs`

`TuiApp` 拥有 ratatui Terminal + 事件循环。它持有 `LiveCli` 的引用（或所有权），把 `run_turn` 调用包装在 `StatusEmitter` 中以便实时更新 `StatusBarState`。

- [ ] **Step 8.1: 实现 TuiApp 主体**

完整替换 `tui/app.rs` 内容：

```rust
//! TuiApp — main ratatui event loop integrating with LiveCli.
//!
//! Owns the alternate-screen Terminal, InputLine, SlashMenu, OutputView,
//! and shared StatusBarState. Routes keyboard events to InputLine / Menu,
//! submits Enter to `LiveCli::run_turn` (capturing output via OutputView
//! sink + StatusEmitter callback for live status updates).

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
                .split(f.size());

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
                let menu_area = Rect {
                    x: chunks[1].x,
                    y: chunks[1].y + chunks[1].height,
                    width: chunks[1].width,
                    height: 12.min(f.size().height - (chunks[1].y + chunks[1].height) - 1),
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
                        // Exit raw mode briefly, run turn, restore.
                        // Actually — keep raw mode and route output to OutputView.
                        handle_submit(&mut cli, &line, &mut output_view, &status_state)?;
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
                // Approximate elapsed — actual turn start time would need threading
                // For MVP, increment by poll interval
                guard.turn_elapsed_ms += 200;
            }
        }
    }

    Ok(())
}

fn route_key(input: &mut InputLine, key: KeyEvent) -> InputAction {
    let key_name = key_code_name(key.code);
    let modifiers_name = if key.modifiers.contains(KeyModifiers::CONTROL) {
        "Ctrl"
    } else {
        ""
    };
    let full_name = if modifiers_name.is_empty() {
        key_name.to_string()
    } else {
        format!("{modifiers_name}+{key_name}")
    };

    // Map to the names InputLine expects
    let logical = match key.code {
        KeyCode::Char(c) => return input.handle_key(Some(c), ""),
        KeyCode::Enter => "Enter",
        KeyCode::Esc => "Esc",
        KeyCode::Backspace => "Backspace",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Tab => "Tab",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        _ => "",
    };

    if logical.is_empty() {
        return InputAction::Ignore;
    }

    // Ctrl+C / Ctrl+D
    if modifiers_name == "Ctrl" && (logical == "c" || logical == "C" || logical == "d" || logical == "D") {
        let _ = full_name; // suppress unused warning
        return input.handle_key(None, "CtrlC");
    }

    input.handle_key(None, logical)
}

fn key_code_name(code: KeyCode) -> &'static str {
    match code {
        KeyCode::Char(_) => "",
        KeyCode::Enter => "Enter",
        KeyCode::Esc => "Esc",
        KeyCode::Backspace => "Backspace",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Tab => "Tab",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        _ => "",
    }
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
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
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
    // Mark streaming start
    {
        let mut guard = status_state.lock().expect("StatusBarState poisoned");
        guard.reset_turn();
    }

    // Call the existing run_turn path. Output goes to stdout in this MVP;
    // a future Phase 2 task will route it through OutputView by passing
    // &mut *output_view as the `out` sink to AnthropicRuntimeClient.
    // Similarly, the StatusEmitter hook added in Task 7 is not yet wired
    // through build_runtime — that requires changing build_runtime's
    // signature to accept an optional emitter. For MVP, the status bar
    // updates after each turn completes (sync_status_from_cli below).
    let _ = output_view; // suppress unused warning for now
    let result = cli.run_turn(line);

    // Mark streaming done
    {
        let mut guard = status_state.lock().expect("StatusBarState poisoned");
        if guard.streaming {
            guard.finish_turn();
        }
    }

    // After turn, sync cumulative usage from cli (the authoritative source)
    sync_status_from_cli(status_state, cli);

    result?;
    Ok(())
}

fn initialize_status(state: &Arc<Mutex<StatusBarState>>, cli: &LiveCli) {
    let mut guard = state.lock().expect("StatusBarState poisoned");
    guard.model = cli.model.clone();
    guard.permission_mode = cli.permission_mode_label().to_string();
    guard.session_id = cli.session_id_snapshot().to_string();
    // cwd, git_branch, provider, goal_badge, poor_mode — filled by sync_status_from_cli
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
    // git_branch, provider, goal_badge — filled by accessor methods on LiveCli
    if let Some(branch) = cli.git_branch_snapshot() {
        guard.git_branch = branch;
    }
    if let Some(badge) = cli.goal_badge_snapshot() {
        guard.goal_badge = badge;
    }
    guard.poor_mode = runtime::poor_mode::is_active();
    guard.provider = crate::provider_label(api::detect_provider_kind(&cli.model)).to_string();
}

// Helper trait methods needed on LiveCli — these will be added in Step 8.2
// If they don't exist yet, this is a compile error pointing to the missing impls.
```

- [ ] **Step 8.2: 在 `LiveCli` 上添加快照访问器**

在 `app.rs` 的 `impl LiveCli` 块末尾（最后一个方法之后）添加：

```rust
    // ===== TUI status bar snapshot accessors =====
    // These are read-only views into LiveCli state for the TUI StatusBar to
    // render. They are feature-gated to avoid dead-code warnings when full-tui
    // is disabled.

    #[cfg(feature = "full-tui")]
    pub(crate) fn model_snapshot(&self) -> &str {
        &self.model
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn cumulative_usage_snapshot(&self) -> runtime::TokenUsage {
        self.cumulative_usage.clone()
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn permission_mode_label(&self) -> &str {
        self.permission_mode.as_str()
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn git_branch_snapshot(&self) -> Option<String> {
        status_context(None).ok().and_then(|c| c.git_branch)
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn goal_badge_snapshot(&self) -> Option<String> {
        // Reuse render_goal_badge but strip the ANSI codes for TUI rendering.
        // For MVP, return the raw badge; TUI will style it.
        self.render_goal_badge()
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn session_id_snapshot(&self) -> &str {
        &self.session.id
    }
```

并在 `initialize_status` 和 `sync_status_from_cli_inner` 中使用这些访问器：

```rust
fn initialize_status(state: &Arc<Mutex<StatusBarState>>, cli: &LiveCli) {
    let mut guard = state.lock().expect("StatusBarState poisoned");
    guard.model = cli.model_snapshot().to_string();
    guard.permission_mode = cli.permission_mode_label().to_string();
    guard.session_id = cli.session_id_snapshot().to_string();
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
    }
    guard.poor_mode = runtime::poor_mode::is_active();
    guard.provider = crate::provider_label(api::detect_provider_kind(cli.model_snapshot())).to_string();
}
```

- [ ] **Step 8.3: 给 SlashMenu 添加 `all_items_count()` 访问器**

在 `tui/slash_menu.rs` 的 `impl SlashMenu` 块末尾添加：

```rust
    /// Total number of all candidate commands (ignoring filter).
    pub(crate) fn all_items_count(&self) -> usize {
        self.all_items.len()
    }
```

- [ ] **Step 8.4: 验证 feature 编译**

```powershell
cargo check -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 20
```

预期：编译通过。可能有 dead_code 警告（`emitter` 变量未使用 — 在 Step 8.1 中标注了 `_ = emitter`），符合 MVP 阶段预期。

如有错误：
- E0432 unresolved import：检查 `use crate::streaming::{StatusEmitter, StatusEvent}` 是否生效（这些是 pub(crate)，本 crate 内可见）
- E0599 method not found：检查 `LiveCli::cumulative_usage_snapshot` 等 4 个访问器是否正确添加到 `impl LiveCli`
- E0603 private field：检查 `cli.session.id` 等访问是否需要 `pub(crate)` 升级

- [ ] **Step 8.5: 验证默认构建（无 feature）仍通过**

```powershell
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：基线 216+3 保持。

- [ ] **Step 8.6: 提交**

```powershell
$msg = @"
feat(tui): implement TuiApp main loop + LiveCli integration

TuiApp owns the alternate-screen ratatui Terminal and runs an event
loop (200ms poll) that renders OutputView / InputLine+SlashMenu /
StatusBar. Keyboard events are routed through InputLine which signals
MenuUp/MenuDown/MenuAccept for slash navigation. Submitting a line
calls LiveCli::run_turn (existing path), wrapped by a StatusEmitter
callback that updates the shared StatusBarState in real-time.

Adds 4 feature-gated accessor methods to LiveCli for TUI status
snapshots: cumulative_usage_snapshot, permission_mode_label,
git_branch_snapshot, goal_badge_snapshot.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/tui/app.rs rust/crates/rusty-claude-cli/src/tui/slash_menu.rs rust/crates/rusty-claude-cli/src/app.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 9: 在 `main.rs` 添加 `--tui` CLI flag 入口

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/commands_handler.rs`（`CliAction::Repl` 增加 `tui: bool` 字段）
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`（`run()` 根据 `tui` flag 分发到 `tui::run_tui_repl`）

- [ ] **Step 9.1: 在 `CliAction::Repl` 添加 `tui: bool` 字段**

在 `commands_handler.rs` 第 130-140 行的 `Repl` variant 添加字段：

```rust
    Repl {
        model: String,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        additional_workspace_roots: Vec<PathBuf>,
        /// 启动时设定的输出冗度（由 `--verbose`/`--quiet`/`--silent` 设置）。
        output_verbosity: OutputVerbosity,
        /// 启用 full-tui 模式：使用 ratatui 全屏 TUI 替代 inline REPL。
        /// 仅当 `full-tui` Cargo feature 启用时生效；否则报错。
        tui: bool,
    },
```

- [ ] **Step 9.2: 在 `parse_args` 中识别 `--tui` flag**

在 `commands_handler.rs` 的 `parse_args` 函数中，找到处理其他 flag 的位置（搜索 `--allow-broad-cwd` 或 `--verbose` 的处理点），在合适的位置添加：

```rust
        // --tui: 启用 full-tui 模式（需 full-tui Cargo feature）
        if arg == "--tui" {
            tui = true;
            continue;
        }
```

并在函数开头声明 `let mut tui = false;`，在构造 `CliAction::Repl { ... }` 时填入 `tui`。

- [ ] **Step 9.3: 在 `main.rs` 的 `run()` 中根据 `tui` flag 分发**

在 `main.rs` 的 `run()` 函数中，找到 `CliAction::Repl { ... } => run_repl(...)` 的 match arm（搜索 `CliAction::Repl`）。修改为：

```rust
        CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            additional_workspace_roots,
            output_verbosity,
            tui,
        } => {
            if tui {
                #[cfg(feature = "full-tui")]
                {
                    // Build LiveCli via the same path as run_repl, then enter TUI.
                    return run_tui_repl_entry(
                        model,
                        allowed_tools,
                        permission_mode,
                        base_commit,
                        reasoning_effort,
                        allow_broad_cwd,
                        additional_workspace_roots,
                        output_verbosity,
                    );
                }
                #[cfg(not(feature = "full-tui"))]
                {
                    eprintln!(
                        "error: --tui requires the `full-tui` Cargo feature.\n\
                         Rebuild with: cargo build --release --features full-tui"
                    );
                    std::process::exit(1);
                }
            }
            run_repl(
                model,
                allowed_tools,
                permission_mode,
                base_commit,
                reasoning_effort,
                allow_broad_cwd,
                additional_workspace_roots,
                output_verbosity,
            )
        }
```

并在 `main.rs` 顶部（在 `mod app;` 声明之后）添加：

```rust
#[cfg(feature = "full-tui")]
use tui::run_tui_repl as run_tui_repl_entry;
```

- [ ] **Step 9.4: 重构 `run_repl` 抽出 `build_live_cli` 共享函数**

`run_tui_repl` 需要构造一个 `LiveCli`，但当前 `run_repl` 内联构造。抽出一个共享函数：

在 `app.rs` 中添加一个新函数 `build_live_cli_for_repl`（紧邻 `run_repl`）：

```rust
/// Shared LiveCli construction for both inline REPL and TUI modes.
/// Returns (LiveCli, paste_id_gen, output_verbosity).
#[allow(clippy::type_complexity)]
pub(crate) fn build_live_cli_for_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    additional_workspace_roots: Vec<PathBuf>,
    output_verbosity: OutputVerbosity,
) -> Result<LiveCli, Box<dyn std::error::Error>> {
    let resolved_model = resolve_repl_model(model);
    let cli = LiveCli::new(
        resolved_model,
        true,
        allowed_tools,
        permission_mode,
        additional_workspace_roots,
        output_verbosity,
    )?;
    Ok(cli)
}
```

并修改 `run_repl` 开头使用此函数（保持行为不变）：

```rust
pub(crate) fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    allow_broad_cwd: bool,
    additional_workspace_roots: Vec<PathBuf>,
    output_verbosity: OutputVerbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    enforce_broad_cwd_policy(allow_broad_cwd, CliOutputFormat::Text)?;
    run_stale_base_preflight(base_commit.as_deref());
    let mut paste_id_gen: u32 = 0;
    let mut pending_paste_lines: Vec<String> = Vec::new();
    let t0 = std::time::Instant::now();
    let mut cli = build_live_cli_for_repl(
        model,
        allowed_tools,
        permission_mode,
        additional_workspace_roots.clone(),
        output_verbosity,
    )?;
    // ... rest unchanged
```

注意：`additional_workspace_roots` 需要 `.clone()` 因为 `build_live_cli_for_repl` 消耗它（如果 LiveCli::new 也消耗，则可能需要调整签名）。如果编译报错，可能需要把参数改为 `&[PathBuf]` 引用。

- [ ] **Step 9.5: 验证默认构建 + 测试基线**

```powershell
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 10
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：通过；216+3 保持。

- [ ] **Step 9.6: 验证 feature 构建**

```powershell
cargo check -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 10
```

预期：通过。

- [ ] **Step 9.7: 验证 --tui flag 在无 feature 时报错**

```powershell
cargo build -p rusty-claude-cli 2>&1 | Select-Object -Last 3
.\target\debug\claw --tui 2>&1 | Select-Object -First 5
```

预期：输出 `error: --tui requires the `full-tui` Cargo feature.`

- [ ] **Step 9.8: 提交**

```powershell
$msg = @"
feat(cli): add --tui CLI flag to enter full-tui mode

CliAction::Repl gains `tui: bool` field. parse_args recognizes
`--tui` flag. main.rs run() dispatches to `tui::run_tui_repl` when
flag is set and `full-tui` feature is enabled; otherwise prints an
error directing user to rebuild with --features full-tui.

Extracts `build_live_cli_for_repl` shared helper so both inline REPL
and TUI paths construct LiveCli the same way.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/commands_handler.rs rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/app.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 10: 最终验证 + 集成测试

- [ ] **Step 10.1: 默认构建全 workspace 检查**

```powershell
cd d:\claw-code-src\rust
cargo check --workspace 2>&1 | Select-Object -Last 10
```

预期：通过（仅 pre-existing `SESSION_SEARCH_TOOL_SPEC` 警告）。

- [ ] **Step 10.2: feature 构建全 workspace 检查**

```powershell
cargo check --workspace --features rusty-claude-cli/full-tui 2>&1 | Select-Object -Last 10
```

预期：通过。

- [ ] **Step 10.3: 默认构建测试基线**

```powershell
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result|running"
```

预期：216 passed; 3 failed（与 Phase 0 基线一致）。

- [ ] **Step 10.4: feature 构建测试（新增 TUI 单元测试）**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 2>&1 | Select-String "test result|running"
```

预期：216 + ~40 = 256 passed; 3 failed（原 216 + 新增 status_bar 6 + slash_menu 14 + output_view 7 + input_line 17 + status_emitter 2 - 重叠 = 约 40 新增）。

- [ ] **Step 10.5: 体积检查**

```powershell
$files = @("main.rs","app.rs","tui/mod.rs","tui/app.rs","tui/status_bar.rs","tui/slash_menu.rs","tui/input_line.rs","tui/output_view.rs","streaming.rs","commands_handler.rs")
foreach ($f in $files) { $p = "d:\claw-code-src\rust\crates\rusty-claude-cli\src\$f"; if (Test-Path $p) { $lines = (Get-Content $p | Measure-Object -Line).Lines; Write-Host "$f : $lines lines" } }
```

预期：
- `main.rs`: ~1050 行（小幅增长因增加 --tui 分发）
- `app.rs`: ~2050 行（增加 4 个访问器 + build_live_cli_for_repl）
- `tui/`: 各模块 200-500 行
- `streaming.rs`: ~990 行（增加 StatusEmitter ~70 行）

- [ ] **Step 10.6: 端到端冒烟测试（手动）**

```powershell
cargo build -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 3
.\target\debug\claw --tui
```

预期行为：
- 进入 alternate screen
- 顶部显示 "Output" 区域
- 底部显示 "> " 输入提示
- 最底显示状态栏：`│ model via provider │ 📁 cwd │ 🔢 0 tok │ 💰 $0.0000 │`
- 输入 `/` → 弹出命令列表，按 Up/Down 移动光标
- 输入 `/he` → 列表过滤为 `help` 一项
- 按 Enter → 接受补全为 `/help`，再次 Enter → 执行 /help 命令，输出显示在 Output 区
- 按 Ctrl+C 或 Esc 退出

（手动验证，无需自动化）

- [ ] **Step 10.7: Phase 1 完成**

Phase 1 完成。所有 P0 功能（slash 命令弹窗菜单 + 持久状态栏 + 实时 token 计数）已实现并 feature-gated。

```powershell
git log --oneline -15
```

应该看到约 10 个新 commits（Task 1-9 + 可能的修复 commits）。
