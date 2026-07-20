# TUI Phase 0 — `main.rs` 模块拆分实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 17,222 行的 `rust/crates/rusty-claude-cli/src/main.rs` 巨石文件按职责拆分为 12 个聚焦模块，为后续引入 ratatui 全屏 TUI 模式与 P0 级 slash 命令菜单/任务状态显示扫清障碍。

**Architecture:** 纯结构重组，不改行为。所有函数/类型/常量从 `main.rs` 物理迁移到新模块；跨模块访问通过 `pub(crate)` 暴露；测试模块抽取到独立 `tests.rs` 文件。每个 Task 独立可验证：`cargo check -p rusty-claude-cli` + `cargo test -p rusty-claude-cli --lib` 必须通过。

**Tech Stack:** Rust 2024 edition、Cargo workspace、crossterm 0.28、rustyline 15、pulldown-cmark 0.13、syntect 5。无新增依赖（ratatui 留待 Phase 1 引入）。

**Workspace:**
- 工作分支：`feature/tui-refactor`（已创建）
- 工作目录：`d:\claw-code-src`
- 目标 crate：`rust/crates/rusty-claude-cli`
- 源文件：`rust/crates/rusty-claude-cli/src/main.rs`（17,222 行）

**Spec Reference:** `rust/TUI-ENHANCEMENT-PLAN.md` Phase 0（任务 0.1 / 0.2 / 0.3 / 0.4）+ 用户确认的"更激进拆 8-10 模块"决策。

---

## Pre-flight Checks

- [ ] **Step 0.1: 确认基线测试通过**

```bash
cd d:\claw-code-src\rust
cargo check -p rusty-claude-cli 2>&1 | tail -5
cargo test -p rusty-claude-cli --lib 2>&1 | tail -10
```

预期：`cargo check` 无错误；`cargo test --lib` 全部通过（记录通过数量作为基线）。

- [ ] **Step 0.2: 记录基线测试数量**

```bash
cargo test -p rusty-claude-cli --lib 2>&1 | grep -E "test result|running"
```

记录 `test result: ok. N passed` 中的 N 作为基线，每个 Task 完成后对比此值。

---

## Target Module Structure（最终状态）

```
rust/crates/rusty-claude-cli/src/
├── main.rs                  # 入口 + 共享常量/类型 + main/run (~600 行)
├── init.rs                  # 仓库初始化（不变）
├── input.rs                 # 行编辑（不变）
├── render.rs                # TerminalRenderer + Spinner（不变）
├── app.rs                   # LiveCli + BuiltRuntime + REPL 主循环 (~1700 行)
├── format.rs                # format_*/render_*/print_* 输出格式化 (~1100 行)
├── session_mgr.rs           # 会话 CRUD + 历史 + 导出 (~2400 行)
├── commands_handler.rs      # CLI 解析 + slash 命令分发辅助 (~1700 行)
├── streaming.rs             # ApiClient + 流式响应处理 (~1100 行)
├── tool_display.rs          # 工具调用显示 + CliToolExecutor (~800 行)
├── paste.rs                 # 粘贴折叠 + 剪贴板 (~230 行)
├── suggestion.rs            # Levenshtein 命令建议 (~150 行)
├── ultraplan.rs             # InternalPromptProgressReporter (~280 行)
├── doctor.rs                # 诊断/健康检查/preflight (~1300 行)
├── plugin_state.rs          # RuntimeMcpState + RuntimePluginState (~470 行)
└── tests.rs                 # 全部 #[cfg(test)] 模块 (~5330 行)
```

---

## 关键约束（所有 Task 必须遵守）

1. **纯结构重组，不改行为**：禁止重写函数体、改算法、改字符串字面量。只允许：迁移代码、添加 `pub(crate)`、修改 `use` 语句、调整可见性。
2. **`LiveCli` 的 impl 块不可拆分**：1236 行 `impl LiveCli {...}`（5105-6340）必须整体迁到 `app.rs`。Rust 不允许跨模块为同一类型添加内在方法。
3. **`AnthropicRuntimeClient` 的 3 个 impl 块必须同模块**（`streaming.rs`）。
4. **`CliToolExecutor` 类型定义与 `impl ToolExecutor` trait 必须同模块**（`tool_display.rs`，孤儿规则）。
5. **测试可见性**：所有被 `mod tests` `use super::{...}` 导入的符号必须 `pub(crate)`。完整清单见各 Task。
6. **不修改 `Cargo.toml`**：本计划不引入新依赖。
7. **每个 Task 完成必须 `cargo check` 通过**；涉及被测试访问的符号时还必须 `cargo test --lib` 通过。

---

## Task 1: 抽出 `tests.rs`（最低风险）

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/tests.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围（原 main.rs 行号）：**
- 11885-16704: `mod tests { ... }`（主测试模块，4819 行）
- 16706-16803: `fn write_mcp_server_fixture`（顶层 `#[cfg(test)]` 辅助函数）
- 16805-16859: `mod sandbox_report_tests`
- 16861-16939: `mod dump_manifests_tests`
- 16941-17038: `mod system_block_tests`
- 17040-17104: `mod tool_cache_tests`
- 17107-17222: `mod system_extraction_tests`

- [ ] **Step 1.1: 在 `main.rs` 顶部添加模块声明**

在 `mod init; mod input; mod render;` 之后添加：

```rust
#[cfg(test)]
mod tests;
```

- [ ] **Step 1.2: 创建 `tests.rs` 文件，迁移所有测试代码**

将 main.rs 行 11885-17222 的所有 `#[cfg(test)]` 内容剪切到 `tests.rs`。保留所有 `#[test]`、`#[test]` 函数体、`mod tests { use super::{...} ... }` 内部结构不变。

文件开头改为：

```rust
#![allow(dead_code, unused_imports, unused_variables)]

use crate::*;
```

将原 `mod tests { use super::{...} ... }` 中的 `use super::{...}` 改为 `use crate::{...}`（因为 `tests.rs` 现在是 crate 顶级模块，不再嵌套在 main.rs 内）。

- [ ] **Step 1.3: 为被测试访问的符号添加 `pub(crate)`**

以下符号在 main.rs 中目前是私有的，但 `tests.rs` 通过 `use crate::{...}` 引用，必须改为 `pub(crate)`：

**函数（必须 `pub(crate) fn`）：**
```
acp_status_json, build_runtime_plugin_state_with_loader, build_runtime_with_plugin_state,
classify_error_kind, classify_session_lifecycle_from_panes, collect_session_prompt_history,
create_managed_session_handle, describe_tool_progress, filter_tool_specs, format_bughunter_report,
format_commit_preflight_report, format_commit_skipped_report, format_compact_report,
format_connected_line, format_cost_report, format_history_timestamp,
format_internal_prompt_progress_line, format_issue_report, format_model_report,
format_model_switch_report, format_permissions_report, format_permissions_switch_report,
format_pr_report, format_resume_report, format_status_report, format_tool_call_start,
format_tool_result, format_ultraplan_report, format_unknown_slash_command,
format_unknown_slash_command_message, format_user_visible_api_error, merge_prompt_with_stdin,
normalize_permission_mode, parse_args, parse_export_args, parse_git_status_branch,
parse_git_status_metadata_for, parse_git_workspace_summary, parse_history_count,
permission_policy, print_help_to, push_output_block, render_config_report,
render_diff_report, render_diff_report_for, render_help_topic, render_help_topic_json,
render_memory_report, render_prompt_history_report, render_repl_help, render_resume_usage,
render_session_list, render_session_markdown, resolve_model_alias, resolve_model_alias_with_config,
resolve_repl_model, resolve_session_reference, response_to_events, resume_supported_slash_commands,
run_resume_command, short_tool_id, slash_command_completion_candidates_with_sessions,
split_error_hint, status_context, status_json_value, summarize_tool_payload_for_markdown,
try_resolve_bare_skill_prompt, validate_no_args, write_mcp_server_fixture
```

**类型（必须 `pub(crate)`）：**
```
CliAction, CliOutputFormat, CliToolExecutor, GitWorkspaceSummary, InternalPromptProgressEvent,
InternalPromptProgressState, LiveCli, LocalHelpTopic, OutputVerbosity, PromptHistoryEntry,
SessionLifecycleKind, SessionLifecycleSummary, SlashCommand, StatusUsage, TmuxPaneSnapshot
```

> 注：`SlashCommand`、`OutputVerbosity` 来自 `commands`/`render` crate，本就 `pub`，无需改。

**常量（必须 `pub(crate)`）：**
```
DEFAULT_MODEL, LATEST_SESSION_REFERENCE, STUB_COMMANDS
```

- [ ] **Step 1.4: 验证编译**

```bash
cd d:\claw-code-src\rust
cargo check -p rusty-claude-cli 2>&1 | tail -20
```

预期：无错误。如有 E0425/E0603（未找到/私有）错误，根据错误信息给缺失符号加 `pub(crate)`。

- [ ] **Step 1.5: 验证测试通过**

```bash
cargo test -p rusty-claude-cli --lib 2>&1 | grep -E "test result|running"
```

预期：通过数量 ≥ Pre-flight 记录的基线。

- [ ] **Step 1.6: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/tests.rs
git commit -m "refactor(cli): extract tests module from main.rs into tests.rs

Move all #[cfg(test)] modules (11885-17222, ~5330 lines) from main.rs
into a dedicated tests.rs file. Promote 70+ private symbols to pub(crate)
to satisfy the cross-module use crate::{...} imports in tests."
```

---

## Task 2: 抽出 `paste.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/paste.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围（原 main.rs 行号）：**
- 10586-10587: `const PASTE_FOLD_CHAR_THRESHOLD`, `PASTE_FOLD_LINE_THRESHOLD`
- 10591-10594: `fn paste_cache_root`
- 10598-10601: `fn paste_cache_path`
- 10605-10607: `fn pasted_text_ref_num_lines`
- 10611-10617: `fn format_pasted_text_ref`
- 10621-10625: `fn should_fold_paste`
- 10629-10649: `fn store_paste_and_make_placeholder`
- 10659-10676: `fn fold_pasted_input`
- 10683-10726: `fn expand_paste_placeholders`
- 10728-10770: `fn read_clipboard_text`
- 10772-10813: `fn try_auto_expand_clipboard`

- [ ] **Step 2.1: 创建 `paste.rs`，迁移代码**

文件开头：

```rust
//! Paste handling: clipboard reading, paste folding, placeholder expansion.

use std::fs;
use std::path::PathBuf;

use crate::PRIVATE_PASTE_DIR_PLACEHOLDER;  // 如有跨模块常量

pub(crate) const PASTE_FOLD_CHAR_THRESHOLD: usize = 500;
pub(crate) const PASTE_FOLD_LINE_THRESHOLD: usize = 3;

pub(crate) fn paste_cache_root() -> Option<PathBuf> { ... }
pub(crate) fn paste_cache_path(id: &str) -> Option<PathBuf> { ... }
// ... 其余函数全部 pub(crate)
```

- [ ] **Step 2.2: 在 `main.rs` 添加 `mod paste;`**

在 `mod init; mod input; mod render;` 后添加：

```rust
mod paste;
```

- [ ] **Step 2.3: 从 `main.rs` 删除已迁移的代码**

删除原 10586-10813 行的 paste 相关代码（共约 230 行）。

- [ ] **Step 2.4: 修复跨模块引用**

`main.rs` 中 `run_repl`（4432-4578）和 `LiveCli::run_turn`（5301-5375）调用了：
- `fold_pasted_input` → `paste::fold_pasted_input`
- `expand_paste_placeholders` → `paste::expand_paste_placeholders`
- `try_auto_expand_clipboard` → `paste::try_auto_expand_clipboard`
- `read_clipboard_text` → `paste::read_clipboard_text`

在 `main.rs` 顶部添加 `use paste::{fold_pasted_input, expand_paste_placeholders, try_auto_expand_clipboard, read_clipboard_text};` 或直接在调用处加 `paste::` 前缀。

- [ ] **Step 2.5: 验证编译**

```bash
cargo check -p rusty-claude-cli 2>&1 | tail -10
```

- [ ] **Step 2.6: 验证测试**

```bash
cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 2.7: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/paste.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract paste handling into paste.rs

Move 230 lines of paste/clipboard code from main.rs into paste.rs.
All public-facing functions promoted to pub(crate) for cross-module use."
```

---

## Task 3: 抽出 `suggestion.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/suggestion.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 176-194: `const CLI_OPTION_SUGGESTIONS: &[&str]`（19 项）
- 1474-1476: `fn render_suggestion_line`
- 1478-1495: `fn suggest_slash_commands`
- 1497-1499: `fn suggest_closest_term`
- 1501-1541: `fn suggest_similar_subcommand`
- 1543-1548: `fn common_prefix_len`
- 1550-1555: `fn looks_like_subcommand_typo`
- 1557-1578: `fn ranked_suggestions`
- 1580-1604: `fn levenshtein_distance`

- [ ] **Step 3.1: 创建 `suggestion.rs`**

```rust
//! Levenshtein-based slash command and CLI option suggestions.

pub(crate) const CLI_OPTION_SUGGESTIONS: &[&str] = &[ ... ];

pub(crate) fn render_suggestion_line(...) -> String { ... }
pub(crate) fn suggest_slash_commands(...) -> Vec<String> { ... }
pub(crate) fn suggest_closest_term(...) -> Option<String> { ... }
pub(crate) fn suggest_similar_subcommand(...) -> Option<String> { ... }
pub(crate) fn common_prefix_len(...) -> usize { ... }
pub(crate) fn looks_like_subcommand_typo(...) -> bool { ... }
pub(crate) fn ranked_suggestions(...) -> Vec<(String, usize)> { ... }
pub(crate) fn levenshtein_distance(...) -> usize { ... }
```

- [ ] **Step 3.2: 在 `main.rs` 添加 `mod suggestion;`**

- [ ] **Step 3.3: 从 `main.rs` 删除已迁移代码**

删除原 176-194 + 1474-1604 行（共约 150 行）。

- [ ] **Step 3.4: 修复跨模块引用**

`commands_handler.rs`（仍在 main.rs 内，待 Task 10 抽出）中 `parse_args` 调用 `suggest_slash_commands`/`render_suggestion_line`。在 main.rs 顶部添加 `use suggestion::{suggest_slash_commands, render_suggestion_line, CLI_OPTION_SUGGESTIONS};`。

- [ ] **Step 3.5: 验证编译与测试**

```bash
cargo check -p rusty-claude-cli && cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 3.6: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/suggestion.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract suggestion helpers into suggestion.rs"
```

---

## Task 4: 抽出 `ultraplan.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/ultraplan.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 167: `const INTERNAL_PROGRESS_HEARTBEAT_INTERVAL: Duration`
- 8642-8649: `struct InternalPromptProgressState`
- 8652-8658: `enum InternalPromptProgressEvent`
- 8661-8665: `struct InternalPromptProgressShared`
- 8668-8670: `struct InternalPromptProgressReporter`
- 8673-8677: `struct InternalPromptProgressRun`
- 8679-8810: `impl InternalPromptProgressReporter`（132 行）
- 8811-8857: `impl InternalPromptProgressRun`
- 8858-8862: `impl Drop for InternalPromptProgressRun`
- 8864-8909: `fn format_internal_prompt_progress_line`
- 8911-8969: `fn describe_tool_progress`

- [ ] **Step 4.1: 创建 `ultraplan.rs`**

```rust
//! Ultraplan internal prompt progress reporter.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::OutputVerbosity;  // 若需引用

pub(crate) const INTERNAL_PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);

pub(crate) struct InternalPromptProgressState { ... }
pub(crate) enum InternalPromptProgressEvent { ... }
pub(crate) struct InternalPromptProgressShared { ... }
pub(crate) struct InternalPromptProgressReporter { ... }
pub(crate) struct InternalPromptProgressRun { ... }

impl InternalPromptProgressReporter { ... }
impl InternalPromptProgressRun { ... }
impl Drop for InternalPromptProgressRun { ... }

pub(crate) fn format_internal_prompt_progress_line(...) -> String { ... }
pub(crate) fn describe_tool_progress(...) -> String { ... }
```

- [ ] **Step 4.2: 在 `main.rs` 添加 `mod ultraplan;`**

- [ ] **Step 4.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 4.4: 修复跨模块引用**

`streaming.rs`（仍在 main.rs 内）的 `AnthropicRuntimeClient` 字段引用 `InternalPromptProgressReporter` → 在 main.rs 顶部加 `use ultraplan::{InternalPromptProgressReporter, InternalPromptProgressEvent, InternalPromptProgressState, format_internal_prompt_progress_line, describe_tool_progress, INTERNAL_PROGRESS_HEARTBEAT_INTERVAL};`。

`app.rs`（LiveCli，仍在 main.rs 内）的 `LiveCli::run_ultraplan` 调用相关 → 已在上述 `use` 中。

- [ ] **Step 4.5: 验证编译与测试**

```bash
cargo check -p rusty-claude-cli && cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 4.6: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/ultraplan.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract ultraplan progress reporter into ultraplan.rs"
```

---

## Task 5: 抽出 `tool_display.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/tool_display.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 4722-4725: `struct ToolSearchRequest`
- 4728-4733: `struct McpToolRequest`
- 4736-4738: `struct ListMcpResourcesRequest`
- 4741-4744: `struct ReadMcpResourceRequest`
- 8345-8352: `fn short_tool_id`
- 9879: `const TOOL_CARD_PREFIX`
- 9974: `const USER_CARD_PREFIX`
- 9883-9892: `fn indent_with_card_prefix`
- 9894-9961: `fn format_tool_call_start`
- 9963-9977: `fn format_tool_result_card_close`
- 9979-9995: `fn format_user_message_card`
- 9997-10008: `fn print_user_card`
- 10010-10028: `fn clear_rustyline_echo`
- 10030-10036: `fn estimate_display_width`
- 10038-10065: `fn is_wide_char`
- 10815-10852: `fn format_tool_result`
- 10854-10871: `fn format_tool_result_compact`
- 10866-10871: `const DISPLAY_TRUNCATION_NOTICE`, `READ_DISPLAY_MAX_LINES`, `READ_DISPLAY_MAX_CHARS`, `TOOL_OUTPUT_DISPLAY_MAX_LINES`, `TOOL_OUTPUT_DISPLAY_MAX_CHARS`
- 10873-10881: `fn extract_tool_path`
- 10883-10893: `fn format_search_start`
- 10895-10904: `fn format_patch_preview`
- 10906-10919: `fn format_bash_call`
- 10921-10925: `fn first_visible_line`
- 10927-10967: `fn format_bash_result`
- 10969-10997: `fn format_read_result`
- 10999-11013: `fn format_write_result`
- 11015-11033: `fn format_structured_patch_preview`
- 11035-11062: `fn format_edit_result`
- 11064-11086: `fn format_glob_result`
- 11088-11130: `fn format_grep_result`
- 11132-11154: `fn format_generic_tool_result`
- 11156-11162: `fn summarize_tool_payload`
- 11164-11172: `fn truncate_for_summary`
- 11174-11216: `fn truncate_output_for_display`
- 11332-11339: `struct CliToolExecutor`
- 11341-11396: `impl CliToolExecutor`
- 11424-11496: `impl ToolExecutor for CliToolExecutor`

- [ ] **Step 5.1: 创建 `tool_display.rs`**

```rust
//! Tool call visualization: card rendering, diff previews, result formatting, CliToolExecutor.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::plugin_state::RuntimeMcpState;  // 待 Task 6 抽出，暂用 crate::RuntimeMcpState
use crate::OutputVerbosity;
use runtime::ToolExecutor;

pub(crate) const TOOL_CARD_PREFIX: &str = "\x1b[38;5;245m│\x1b[0m ";
pub(crate) const USER_CARD_PREFIX: &str = "\x1b[38;5;111m│\x1b[0m ";
pub(crate) const DISPLAY_TRUNCATION_NOTICE: &str = "...";
pub(crate) const READ_DISPLAY_MAX_LINES: usize = 80;
pub(crate) const READ_DISPLAY_MAX_CHARS: usize = 6_000;
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_LINES: usize = 60;
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_CHARS: usize = 4_000;

pub(crate) struct ToolSearchRequest { ... }
pub(crate) struct McpToolRequest { ... }
pub(crate) struct ListMcpResourcesRequest { ... }
pub(crate) struct ReadMcpResourceRequest { ... }
pub(crate) struct CliToolExecutor { ... }

impl CliToolExecutor { ... }
impl ToolExecutor for CliToolExecutor { ... }

// 所有 format_* / print_* / extract_* / truncate_* / summarize_* 函数
pub(crate) fn format_tool_call_start(...) -> String { ... }
pub(crate) fn format_tool_result(...) -> String { ... }
// ... 其余全部 pub(crate)
```

- [ ] **Step 5.2: 在 `main.rs` 添加 `mod tool_display;`**

- [ ] **Step 5.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 5.4: 修复跨模块引用**

`streaming.rs`（仍在 main.rs）的 `consume_stream` 调用 `format_tool_call_start`、`format_tool_result_card_close` → 在 main.rs 顶部加 `use tool_display::{format_tool_call_start, format_tool_result_card_close, print_user_card, format_user_message_card, indent_with_card_prefix, clear_rustyline_echo, truncate_output_for_display, short_tool_id};`。

`app.rs`（LiveCli）调用 `print_user_card`、`format_user_message_card`、`CliToolExecutor`、`clear_rustyline_echo` → 同上 `use`。

`BuiltRuntime` 字段引用 `CliToolExecutor` → 已在 `use` 中。

注意：`streaming.rs` 内的 `push_output_block`、`response_to_events`、`render_thinking_block_summary`、`push_prompt_cache_record`、`prompt_cache_record_to_runtime_event` **保留在 main.rs**（属于流处理，非工具显示），待 Task 7 一起迁到 streaming.rs。

- [ ] **Step 5.5: 验证编译**

```bash
cargo check -p rusty-claude-cli 2>&1 | tail -10
```

- [ ] **Step 5.6: 验证测试**

```bash
cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 5.7: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/tool_display.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract tool display into tool_display.rs"
```

---

## Task 6: 抽出 `plugin_state.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/plugin_state.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 197-200: `type RuntimePluginStateBuildOutput`
- 4622-4627: `struct RuntimePluginState`
- 4629-4634: `struct RuntimeMcpState`
- 4746-4939: `impl RuntimeMcpState`（194 行，9 方法）
- 4941-4958: `fn build_runtime_mcp_state`
- 4960-4976: `fn mcp_runtime_tool_definition`
- 4978-5026: `fn mcp_wrapper_tool_definitions`
- 5028-5040: `fn permission_mode_for_mcp_tool`
- 5042-5053: `fn mcp_annotation_flag`
- 8489-8507: `fn plugins_command_payload_for`
- 8509-8535: `fn plugins_command_payload_from_result`
- 8537-8542: `fn build_runtime_plugin_state`
- 8544-8595: `fn build_runtime_plugin_state_with_loader`
- 8597-8620: `fn build_plugin_manager`
- 8622-8631: `fn resolve_plugin_path`
- 8633-8676: `fn runtime_hook_config_from_plugin_hooks`

- [ ] **Step 6.1: 创建 `plugin_state.rs`**

```rust
//! Runtime plugin and MCP state construction.

use std::sync::{Arc, Mutex};

use plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use runtime::mcp::{McpConnection, McpServer};

pub(crate) type RuntimePluginStateBuildOutput = (...);

pub(crate) struct RuntimePluginState { ... }
pub(crate) struct RuntimeMcpState { ... }

impl RuntimeMcpState { ... }

pub(crate) fn build_runtime_mcp_state(...) -> RuntimeMcpState { ... }
pub(crate) fn mcp_runtime_tool_definition(...) -> ... { ... }
pub(crate) fn mcp_wrapper_tool_definitions(...) -> Vec<...> { ... }
pub(crate) fn permission_mode_for_mcp_tool(...) -> ... { ... }
pub(crate) fn mcp_annotation_flag(...) -> ... { ... }

pub(crate) fn plugins_command_payload_for(...) -> PluginsCommandPayload { ... }
pub(crate) fn plugins_command_payload_from_result(...) -> PluginsCommandPayload { ... }
pub(crate) fn build_runtime_plugin_state(...) -> RuntimePluginStateBuildOutput { ... }
pub(crate) fn build_runtime_plugin_state_with_loader(...) -> RuntimePluginStateBuildOutput { ... }
pub(crate) fn build_plugin_manager(...) -> PluginManager { ... }
pub(crate) fn resolve_plugin_path(...) -> Option<PathBuf> { ... }
pub(crate) fn runtime_hook_config_from_plugin_hooks(...) -> ... { ... }
```

- [ ] **Step 6.2: 在 `main.rs` 添加 `mod plugin_state;`**

- [ ] **Step 6.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 6.4: 修复跨模块引用**

`tool_display.rs` 的 `CliToolExecutor` 字段引用 `RuntimeMcpState` → 修改 `tool_display.rs` 顶部 `use crate::plugin_state::RuntimeMcpState;`。

`app.rs`（LiveCli、BuiltRuntime）调用 `build_runtime_plugin_state`、`build_runtime_plugin_state_with_loader`、`build_plugin_manager`、`build_runtime_mcp_state`、`RuntimePluginState`、`RuntimeMcpState` → 在 main.rs 顶部加 `use plugin_state::*;`。

`commands_handler.rs`（仍在 main.rs）调用 `plugins_command_payload_for`、`plugins_command_payload_from_result` → 已在 `use plugin_state::*;` 中。

- [ ] **Step 6.5: 验证编译**

```bash
cargo check -p rusty-claude-cli 2>&1 | tail -10
```

- [ ] **Step 6.6: 验证测试**

```bash
cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 6.7: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/plugin_state.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract plugin/mcp state into plugin_state.rs"
```

---

## Task 7: 抽出 `streaming.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/streaming.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 168: `const POST_TOOL_STALL_TIMEOUT: Duration`
- 5050-5053: `struct HookAbortMonitor`
- 5055-5103: `impl HookAbortMonitor`
- 9154-9165: `struct AnthropicRuntimeClient`
- 9167-9236: `impl AnthropicRuntimeClient`（new + set_reasoning_effort）
- 9238-9240: `fn resolve_cli_auth_source`
- 9243-9245: `fn resolve_cli_auth_source_for_cwd`
- 9257-9285: `fn build_system_blocks`
- 9287-9291: `fn mark_last_tool_with_cache_control`
- 9293-9333: `impl ApiClient for AnthropicRuntimeClient`（stream）
- 9335-9504: `impl AnthropicRuntimeClient`（consume_stream，170 行）
- 9508-9513: `fn request_ends_with_tool_result`
- 9515-9532: `fn format_user_visible_api_error`
- 9534-9604: `fn format_context_window_blocked_error`
- 9606-9622: `fn final_assistant_text`
- 9624-9638: `fn collect_tool_uses`
- 9640-9660: `fn collect_tool_results`
- 9662-9675: `fn collect_prompt_cache_events`
- 10070-10085: `const NETWORK_ERROR_KEYWORDS: &[&str]`
- 11218-11233: `fn render_thinking_block_summary`
- 11235-11277: `fn push_output_block`
- 11279-11304: `fn response_to_events`
- 11306-11317: `fn push_prompt_cache_record`
- 11319-11339: `fn prompt_cache_record_to_runtime_event`
- 11498-11509: `fn permission_policy`
- 11521-11539: `fn extract_system_messages`
- 11549-11624: `fn compact_tool_output_for_model`
- 11626-11710: `fn convert_messages`

- [ ] **Step 7.1: 创建 `streaming.rs`**

```rust
//! Streaming API client: SSE consumption, message conversion, error formatting.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::{AnthropicClient, ApiStreamEvent, ApiClient};
use runtime::AssistantEvent;

use crate::tool_display::{format_tool_call_start, format_tool_result_card_close};
use crate::ultraplan::InternalPromptProgressReporter;
use crate::OutputVerbosity;

pub(crate) const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const NETWORK_ERROR_KEYWORDS: &[&str] = &[ ... ];

pub(crate) struct HookAbortMonitor { ... }
pub(crate) struct AnthropicRuntimeClient { ... }

impl HookAbortMonitor { ... }
impl AnthropicRuntimeClient { ... }
impl ApiClient for AnthropicRuntimeClient { ... }

pub(crate) fn resolve_cli_auth_source(...) -> ... { ... }
pub(crate) fn resolve_cli_auth_source_for_cwd(...) -> ... { ... }
pub(crate) fn build_system_blocks(...) -> ... { ... }
pub(crate) fn mark_last_tool_with_cache_control(...) -> ... { ... }
pub(crate) fn request_ends_with_tool_result(...) -> bool { ... }
pub(crate) fn format_user_visible_api_error(...) -> String { ... }
pub(crate) fn format_context_window_blocked_error(...) -> String { ... }
pub(crate) fn final_assistant_text(...) -> Option<String> { ... }
pub(crate) fn collect_tool_uses(...) -> Vec<...> { ... }
pub(crate) fn collect_tool_results(...) -> Vec<...> { ... }
pub(crate) fn collect_prompt_cache_events(...) -> Vec<...> { ... }
pub(crate) fn render_thinking_block_summary(...) { ... }
pub(crate) fn push_output_block(...) { ... }
pub(crate) fn response_to_events(...) -> Vec<AssistantEvent> { ... }
pub(crate) fn push_prompt_cache_record(...) { ... }
pub(crate) fn prompt_cache_record_to_runtime_event(...) -> AssistantEvent { ... }
pub(crate) fn permission_policy(...) -> ... { ... }
pub(crate) fn extract_system_messages(...) -> ... { ... }
pub(crate) fn compact_tool_output_for_model(...) -> ... { ... }
pub(crate) fn convert_messages(...) -> ... { ... }
```

- [ ] **Step 7.2: 在 `main.rs` 添加 `mod streaming;`**

- [ ] **Step 7.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 7.4: 修复跨模块引用**

`app.rs`（LiveCli）调用 `format_user_visible_api_error`、`request_ends_with_tool_result`、`final_assistant_text`、`collect_tool_uses`、`collect_tool_results`、`collect_prompt_cache_events`、`AnthropicRuntimeClient`、`HookAbortMonitor`、`extract_system_messages`、`convert_messages`、`permission_policy`、`build_system_blocks`、`mark_last_tool_with_cache_control`、`resolve_cli_auth_source_for_cwd`、`NETWORK_ERROR_KEYWORDS` → 在 main.rs 顶部加 `use streaming::*;`。

`commands_handler.rs`（仍在 main.rs）的 `parse_args` 中可能涉及 `--reasoning` effort 参数 → 已在 `use streaming::*;` 中。

- [ ] **Step 7.5: 验证编译**

```bash
cargo check -p rusty-claude-cli 2>&1 | tail -10
```

- [ ] **Step 7.6: 验证测试 + 集成测试**

```bash
cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
cargo test -p rusty-claude-cli --test compact_output 2>&1 | tail -5
```

- [ ] **Step 7.7: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/streaming.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract streaming API client into streaming.rs"
```

---

## Task 8: 抽出 `session_mgr.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/session_mgr.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 169-175: `const PRIMARY_SESSION_EXTENSION`, `LEGACY_SESSION_EXTENSION`, `LATEST_SESSION_REFERENCE`, `SESSION_REFERENCE_ALIASES`
- 2743-2748: `fn resume_command_can_absorb_token`
- 2750-2765: `fn looks_like_slash_command_token`
- 2930-3080: `fn resume_session`（151 行）
- 3083-3086: `struct ResumeCommandOutcome`
- 3289-3310: `enum SessionLifecycleKind`
- 3314-3348: `struct SessionLifecycleSummary`
- 3295-3309: `impl SessionLifecycleKind`
- 3323-3348: `impl SessionLifecycleSummary`
- 3889-4318: `fn run_resume_command`（**429 行巨型函数**）
- 4581-4584: `struct SessionHandle`
- 4587-4596: `struct ManagedSessionSummary`
- 4617-4620: `struct PromptHistoryEntry`
- 6342-6344: `fn sessions_dir`
- 6346-6349: `fn current_session_store`
- 6351-6364: `fn new_cli_session`
- 6366-6382: `fn new_cli_session_with_roots`
- 6384-6392: `fn create_managed_session_handle`
- 6394-6402: `fn resolve_session_reference`
- 6404-6406: `fn session_reference_exists`
- 6408-6412: `fn resolve_managed_session_path`
- 6414-6432: `fn list_managed_sessions`
- 6434-6450: `fn latest_managed_session`
- 6452-6465: `fn load_session_reference`
- 6467-6473: `fn delete_managed_session`
- 6475-6483: `fn confirm_session_deletion`
- 6485-6501: `fn session_details_json`
- 6503-6525: `fn session_exists_json`
- 6527-6617: `fn run_resumed_session_command`
- 6619-6657: `fn render_session_list`
- 6659-6675: `fn format_session_modified_age`
- 6677-6684: `fn write_session_clear_backup`
- 6686-6696: `fn session_clear_backup_path`
- 7930: `const DEFAULT_HISTORY_LIMIT`
- 7932-7943: `fn parse_history_count`
- 7945-7963: `fn format_history_timestamp`
- 7965-7981: `fn civil_from_days`
- 7983-8011: `fn render_prompt_history_report`
- 8013-8039: `fn collect_session_prompt_history`
- 8041-8067: `fn recent_user_context`
- 8134-8168: `fn default_export_filename`
- 8170-8188: `fn resolve_export_path`
- 8188: `const SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT`
- 8190-8199: `fn summarize_tool_payload_for_markdown`
- 8201-8252: `fn run_export`
- 8254-8343: `fn render_session_markdown`

- [ ] **Step 8.1: 创建 `session_mgr.rs`**

```rust
//! Session CRUD, history, export, resume.

use std::path::{Path, PathBuf};

use runtime::{Session, SessionStore};

pub(crate) const PRIMARY_SESSION_EXTENSION: &str = "jsonl";
pub(crate) const LEGACY_SESSION_EXTENSION: &str = "json";
pub(crate) const LATEST_SESSION_REFERENCE: &str = "latest";
pub(crate) const SESSION_REFERENCE_ALIASES: &[&str] = &[ ... ];
pub(crate) const DEFAULT_HISTORY_LIMIT: usize = 20;
pub(crate) const SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT: usize = 280;

pub(crate) struct ResumeCommandOutcome { ... }
pub(crate) enum SessionLifecycleKind { ... }
pub(crate) struct SessionLifecycleSummary { ... }
pub(crate) struct SessionHandle { ... }
pub(crate) struct ManagedSessionSummary { ... }
pub(crate) struct PromptHistoryEntry { ... }

impl SessionLifecycleKind { ... }
impl SessionLifecycleSummary { ... }

pub(crate) fn resume_command_can_absorb_token(...) -> bool { ... }
pub(crate) fn looks_like_slash_command_token(...) -> bool { ... }
pub(crate) fn resume_session(...) -> Result<...> { ... }
pub(crate) fn run_resume_command(...) -> Result<...> { ... }
pub(crate) fn sessions_dir(...) -> PathBuf { ... }
pub(crate) fn current_session_store(...) -> ... { ... }
pub(crate) fn new_cli_session(...) -> Session { ... }
pub(crate) fn new_cli_session_with_roots(...) -> Session { ... }
pub(crate) fn create_managed_session_handle(...) -> SessionHandle { ... }
pub(crate) fn resolve_session_reference(...) -> Option<String> { ... }
pub(crate) fn session_reference_exists(...) -> bool { ... }
pub(crate) fn resolve_managed_session_path(...) -> Option<PathBuf> { ... }
pub(crate) fn list_managed_sessions(...) -> Vec<ManagedSessionSummary> { ... }
pub(crate) fn latest_managed_session(...) -> Option<ManagedSessionSummary> { ... }
pub(crate) fn load_session_reference(...) -> Result<Session, ...> { ... }
pub(crate) fn delete_managed_session(...) -> Result<...> { ... }
pub(crate) fn confirm_session_deletion(...) -> bool { ... }
pub(crate) fn session_details_json(...) -> ... { ... }
pub(crate) fn session_exists_json(...) -> ... { ... }
pub(crate) fn run_resumed_session_command(...) -> Result<...> { ... }
pub(crate) fn render_session_list(...) -> String { ... }
pub(crate) fn format_session_modified_age(...) -> String { ... }
pub(crate) fn write_session_clear_backup(...) -> Result<...> { ... }
pub(crate) fn session_clear_backup_path(...) -> Option<PathBuf> { ... }
pub(crate) fn parse_history_count(...) -> usize { ... }
pub(crate) fn format_history_timestamp(...) -> String { ... }
pub(crate) fn civil_from_days(...) -> ... { ... }
pub(crate) fn render_prompt_history_report(...) -> String { ... }
pub(crate) fn collect_session_prompt_history(...) -> Vec<PromptHistoryEntry> { ... }
pub(crate) fn recent_user_context(...) -> Option<String> { ... }
pub(crate) fn default_export_filename(...) -> String { ... }
pub(crate) fn resolve_export_path(...) -> PathBuf { ... }
pub(crate) fn summarize_tool_payload_for_markdown(...) -> String { ... }
pub(crate) fn run_export(...) -> Result<...> { ... }
pub(crate) fn render_session_markdown(...) -> String { ... }
```

- [ ] **Step 8.2: 在 `main.rs` 添加 `mod session_mgr;`**

- [ ] **Step 8.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 8.4: 修复跨模块引用**

`app.rs`（LiveCli）调用 `SessionHandle`、`ManagedSessionSummary`、`PromptHistoryEntry`、`new_cli_session`、`create_managed_session_handle`、`list_managed_sessions`、`latest_managed_session`、`load_session_reference`、`resolve_session_reference`、`session_reference_exists`、`resolve_managed_session_path`、`delete_managed_session`、`confirm_session_deletion`、`session_details_json`、`session_exists_json`、`run_resumed_session_command`、`render_session_list`、`run_resume_command`、`render_session_markdown`、`run_export`、`parse_history_count`、`collect_session_prompt_history`、`render_prompt_history_report`、`recent_user_context`、`write_session_clear_backup`、`session_clear_backup_path` → 在 main.rs 顶部加 `use session_mgr::*;`。

`commands_handler.rs`（仍在 main.rs）的 `parse_args`、`parse_resume_args` 调用 `resume_command_can_absorb_token`、`looks_like_slash_command_token`、`resolve_session_reference` → 已在 `use session_mgr::*;` 中。

- [ ] **Step 8.5: 验证编译与测试**

```bash
cargo check -p rusty-claude-cli && cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 8.6: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/session_mgr.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract session management into session_mgr.rs"
```

---

## Task 9: 抽出 `doctor.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/doctor.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 1967-1986: `enum DiagnosticLevel`
- 1988-2042: `struct DiagnosticCheck`
- 2045-2116: `struct DoctorReport`（impl 行 2049-2116）
- 1973-1992: `impl DiagnosticLevel`
- 1996-2043: `impl DiagnosticCheck`
- 2118-2130: `fn render_diagnostic_check`
- 2132-2184: `fn render_doctor_report`
- 2186-2199: `fn run_doctor`
- 2210-2243: `fn run_worker_state`
- 2245-2272: `fn run_mcp_serve`
- 2275-2378: `fn check_auth_health`
- 2380-2471: `fn check_config_health`
- 2473-2501: `fn check_install_source_health`
- 2503-2582: `fn check_workspace_health`
- 2584-2649: `fn check_boot_preflight_health`
- 2651-2712: `fn check_sandbox_health`
- 2714-2741: `fn check_system_health`
- 3090-3109: `struct StatusContext`
- 3114-3160: `struct BranchFreshness`（impl 行 3121-3160）
- 3163-3176: `struct BinaryPreflight`（impl 行 3168-3174）
- 3178-3183: `struct ControlSocketPreflight`（impl 行 3185-3182）
- 3197-3267: `struct BootPreflightSnapshot`（impl 行 3213-3267）
- 3270-3276: `struct StatusUsage`
- 3280-3286: `struct GitWorkspaceSummary`（impl 行 3357-3364）
- 3351-3354: `struct TmuxPaneSnapshot`
- 3388-3390: `fn classify_session_lifecycle_for`
- 3392-3435: `fn classify_session_lifecycle_from_panes`
- 3437-3454: `fn discover_tmux_panes`
- 3456-3474: `fn parse_tmux_pane_snapshots`
- 3476-3483: `fn pane_path_matches_workspace`
- 3485-3491: `fn is_idle_shell_command`
- 3493-3503: `fn git_worktree_is_dirty`
- 3657-3662: `fn parse_git_status_metadata`
- 3664-3677: `fn parse_git_status_branch`
- 3679-3715: `fn parse_git_workspace_summary`
- 3717-3779: `fn build_boot_preflight_snapshot`
- 3781-3787: `fn run_git_bool`
- 3789-3794: `fn command_available`
- 3796-3808: `fn tmux_control_socket_preflight`
- 3810-3820: `fn last_failed_boot_reason`
- 3822-3832: `fn path_matches_trusted_root_local`
- 3834-3850: `fn resolve_git_branch_for`
- 3852-3862: `fn run_git_capture_in`
- 3864-3877: `fn find_git_root_in`
- 3879-3887: `fn parse_git_status_metadata_for`

- [ ] **Step 9.1: 创建 `doctor.rs`**

```rust
//! Diagnostics, health checks, boot preflight.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::session_mgr::{SessionLifecycleKind, SessionLifecycleSummary};

pub(crate) enum DiagnosticLevel { ... }
pub(crate) struct DiagnosticCheck { ... }
pub(crate) struct DoctorReport { ... }
pub(crate) struct StatusContext { ... }
pub(crate) struct BranchFreshness { ... }
pub(crate) struct BinaryPreflight { ... }
pub(crate) struct ControlSocketPreflight { ... }
pub(crate) struct BootPreflightSnapshot { ... }
pub(crate) struct StatusUsage { ... }
pub(crate) struct GitWorkspaceSummary { ... }
pub(crate) struct TmuxPaneSnapshot { ... }

impl DiagnosticLevel { ... }
impl DiagnosticCheck { ... }
impl DoctorReport { ... }
impl BranchFreshness { ... }
impl BinaryPreflight { ... }
impl ControlSocketPreflight { ... }
impl BootPreflightSnapshot { ... }
impl GitWorkspaceSummary { ... }

pub(crate) fn render_diagnostic_check(...) -> String { ... }
pub(crate) fn render_doctor_report(...) -> String { ... }
pub(crate) fn run_doctor(...) -> Result<...> { ... }
pub(crate) fn run_worker_state(...) -> Result<...> { ... }
pub(crate) fn run_mcp_serve(...) -> Result<...> { ... }
pub(crate) fn check_auth_health(...) -> Vec<DiagnosticCheck> { ... }
pub(crate) fn check_config_health(...) -> Vec<DiagnosticCheck> { ... }
pub(crate) fn check_install_source_health(...) -> Vec<DiagnosticCheck> { ... }
pub(crate) fn check_workspace_health(...) -> Vec<DiagnosticCheck> { ... }
pub(crate) fn check_boot_preflight_health(...) -> Vec<DiagnosticCheck> { ... }
pub(crate) fn check_sandbox_health(...) -> Vec<DiagnosticCheck> { ... }
pub(crate) fn check_system_health(...) -> Vec<DiagnosticCheck> { ... }
pub(crate) fn classify_session_lifecycle_for(...) -> ... { ... }
pub(crate) fn classify_session_lifecycle_from_panes(...) -> ... { ... }
pub(crate) fn discover_tmux_panes(...) -> Vec<TmuxPaneSnapshot> { ... }
pub(crate) fn parse_tmux_pane_snapshots(...) -> Vec<TmuxPaneSnapshot> { ... }
pub(crate) fn pane_path_matches_workspace(...) -> bool { ... }
pub(crate) fn is_idle_shell_command(...) -> bool { ... }
pub(crate) fn git_worktree_is_dirty(...) -> bool { ... }
pub(crate) fn parse_git_status_metadata(...) -> ... { ... }
pub(crate) fn parse_git_status_branch(...) -> Option<String> { ... }
pub(crate) fn parse_git_workspace_summary(...) -> Option<GitWorkspaceSummary> { ... }
pub(crate) fn build_boot_preflight_snapshot(...) -> BootPreflightSnapshot { ... }
pub(crate) fn run_git_bool(...) -> bool { ... }
pub(crate) fn command_available(...) -> bool { ... }
pub(crate) fn tmux_control_socket_preflight(...) -> ControlSocketPreflight { ... }
pub(crate) fn last_failed_boot_reason(...) -> Option<String> { ... }
pub(crate) fn path_matches_trusted_root_local(...) -> bool { ... }
pub(crate) fn resolve_git_branch_for(...) -> Option<String> { ... }
pub(crate) fn run_git_capture_in(...) -> Option<String> { ... }
pub(crate) fn find_git_root_in(...) -> Option<PathBuf> { ... }
pub(crate) fn parse_git_status_metadata_for(...) -> ... { ... }
```

> 注：`StatusContext` 同时被 `format.rs`（待 Task 11）和 `app.rs` 使用。留在 doctor.rs 并 `pub(crate)` 即可。

- [ ] **Step 9.2: 在 `main.rs` 添加 `mod doctor;`**

- [ ] **Step 9.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 9.4: 修复跨模块引用**

`commands_handler.rs`（仍在 main.rs）的 `run` 函数调用 `run_doctor`、`run_worker_state`、`run_mcp_serve`、`check_*_health`、`render_doctor_report` → 在 main.rs 顶部加 `use doctor::*;`。

`app.rs`（LiveCli）启动期调用 `BootPreflightSnapshot`、`build_boot_preflight_snapshot`、`BranchFreshness`、`BinaryPreflight`、`ControlSocketPreflight` → 已在 `use doctor::*;` 中。

`format.rs`（待抽）调用 `StatusContext`、`GitWorkspaceSummary`、`parse_git_status_branch` → 已在 `use doctor::*;` 中。

- [ ] **Step 9.5: 验证编译与测试**

```bash
cargo check -p rusty-claude-cli && cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 9.6: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/doctor.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract diagnostics/preflight into doctor.rs"
```

---

## Task 10: 抽出 `commands_handler.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/commands_handler.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围：**
- 547-661: `enum CliAction`
- 664-678: `enum LocalHelpTopic`
- 699-1186: `fn parse_args`（487 行巨型函数）
- 1188-1218: `fn parse_local_help_action`
- 1220-1222: `fn is_help_flag`
- 1224-1288: `fn parse_single_word_command_alias`
- 1290-1321: `fn bare_slash_command_guidance`
- 1323-1327: `fn removed_auth_surface_error`
- 1329-1337: `fn parse_acp_args`
- 1339-1353: `fn try_resolve_bare_skill_prompt`
- 1355-1360: `fn join_optional_args`
- 1362-1424: `fn parse_direct_slash_cli_action`
- 1426-1435: `fn format_unknown_option`
- 1437-1450: `fn format_unknown_direct_slash_command`
- 1452-1465: `fn format_unknown_slash_command`
- 2775: `const DUMP_MANIFESTS_OVERRIDE_HINT`
- 2767-2777: `fn dump_manifests`
- 2779-2849: `fn dump_manifests_at_path`
- 2851-2872: `fn print_bootstrap_plan`
- 2874-2904: `fn print_system_prompt`
- 2906-2914: `fn print_version`
- 2916-2928: `fn version_json_value`
- 8480: `struct PluginsCommandPayload`
- 9681-9792: `const STUB_COMMANDS: &[&str]`（约 100 项）
- 9794-9881: `fn slash_command_completion_candidates_with_sessions`
- 10101-10146: `fn handle_goal_command`
- 10149-10167: `fn render_goal_status`
- 10170-10188: `fn format_timestamp_ms`
- 10192-10201: `fn split_first_word`
- 10205-10217: `fn parse_goal_budget`
- 10230-10278: `fn handle_poor_mode_action`
- 10291-10487: `fn handle_bg_command`

- [ ] **Step 10.1: 创建 `commands_handler.rs`**

```rust
//! CLI argument parsing and slash command handler helpers.

use std::path::PathBuf;

use commands::{SlashCommand, SlashCommandSpec, slash_command_specs};
use runtime::Config;

use crate::session_mgr::{ManagedSessionSummary, list_managed_sessions};
use crate::suggestion::{suggest_slash_commands, render_suggestion_line, CLI_OPTION_SUGGESTIONS};

pub(crate) const DUMP_MANIFESTS_OVERRIDE_HINT: &str = "...";
pub(crate) const STUB_COMMANDS: &[&str] = &[ ... ];

pub(crate) enum CliAction { ... }
pub(crate) enum LocalHelpTopic { ... }
pub(crate) struct PluginsCommandPayload { ... }

pub(crate) fn parse_args() -> CliAction { ... }
pub(crate) fn parse_local_help_action(...) -> ... { ... }
pub(crate) fn is_help_flag(...) -> bool { ... }
pub(crate) fn parse_single_word_command_alias(...) -> ... { ... }
pub(crate) fn bare_slash_command_guidance(...) -> ... { ... }
pub(crate) fn removed_auth_surface_error() -> ... { ... }
pub(crate) fn parse_acp_args(...) -> ... { ... }
pub(crate) fn try_resolve_bare_skill_prompt(...) -> ... { ... }
pub(crate) fn join_optional_args(...) -> ... { ... }
pub(crate) fn parse_direct_slash_cli_action(...) -> ... { ... }
pub(crate) fn format_unknown_option(...) -> String { ... }
pub(crate) fn format_unknown_direct_slash_command(...) -> String { ... }
pub(crate) fn format_unknown_slash_command(...) -> String { ... }
pub(crate) fn dump_manifests(...) -> ... { ... }
pub(crate) fn dump_manifests_at_path(...) -> ... { ... }
pub(crate) fn print_bootstrap_plan(...) { ... }
pub(crate) fn print_system_prompt(...) { ... }
pub(crate) fn print_version() { ... }
pub(crate) fn version_json_value() -> ... { ... }
pub(crate) fn slash_command_completion_candidates_with_sessions(...) -> Vec<String> { ... }
pub(crate) fn handle_goal_command(...) -> ... { ... }
pub(crate) fn render_goal_status(...) -> String { ... }
pub(crate) fn format_timestamp_ms(...) -> String { ... }
pub(crate) fn split_first_word(...) -> ... { ... }
pub(crate) fn parse_goal_budget(...) -> ... { ... }
pub(crate) fn handle_poor_mode_action(...) -> ... { ... }
pub(crate) fn handle_bg_command(...) -> ... { ... }
```

- [ ] **Step 10.2: 在 `main.rs` 添加 `mod commands_handler;`**

- [ ] **Step 10.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 10.4: 修复跨模块引用**

`main.rs` 的 `run` 函数调用 `parse_args`、`run_doctor` 等 → 在 main.rs 顶部加 `use commands_handler::{parse_args, CliAction, ...};`。

`app.rs`（LiveCli）的 `handle_repl_command`（5468-5671）调用 `format_unknown_slash_command`、`handle_goal_command`、`handle_poor_mode_action`、`handle_bg_command`、`render_goal_status`、`slash_command_completion_candidates_with_sessions`、`try_resolve_bare_skill_prompt`、`format_timestamp_ms`、`STUB_COMMANDS`、`CliAction` → 已在 `use commands_handler::*;` 中。

- [ ] **Step 10.5: 验证编译与测试**

```bash
cargo check -p rusty-claude-cli && cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 10.6: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/commands_handler.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract CLI parsing + slash command helpers into commands_handler.rs"
```

---

## Task 11: 抽出 `format.rs`

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/format.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围（所有剩余 `format_*` / `render_*` / `print_*` 函数）：**

`format_*` 系列：
- 3522-3533: `fn format_model_report`
- 3535-3542: `fn format_model_switch_report`
- 3544-3585: `fn format_permissions_report`
- 3587-3596: `fn format_permissions_switch_report`
- 3598-3615: `fn format_cost_report`
- 3617-3624: `fn format_resume_report`
- 3635-3651: `fn format_compact_report`
- 3653-3655: `fn format_auto_compaction_notice`
- 10489-10508: `fn format_age_ms`
- 10510-10537: `fn format_status_bar`
- 10542-10568: `fn shorten_cwd_for_statusbar`
- 7848-7856: `fn format_bughunter_report`
- 7858-7866: `fn format_ultraplan_report`
- 7868-7877: `fn format_pr_report`
- 7879-7887: `fn format_issue_report`
- 3506-3519: `fn format_unknown_slash_command_message`（`#[cfg(test)]`，需保留可见性 + `#[cfg(test)] pub(crate)`）
- 7044-7085: `fn format_sandbox_report`
- 7087-7099: `fn format_commit_preflight_report`
- 7101-7108: `fn format_commit_skipped_report`

`render_*` 系列：
- 3626-3633: `fn render_resume_usage`
- 6619-6657: `fn render_session_list`（**已迁到 session_mgr.rs**，本 Task 跳过）
- 6698-6720: `fn render_repl_help`
- 6774-6871: `fn status_json_value`（保持原函数名）
- 6873-6927: `fn status_context`（**已迁到 doctor.rs**，本 Task 跳过）
- 6933-7042: `fn format_status_report`
- 7110-7127: `fn print_sandbox_status_snapshot`
- 7129-7146: `fn sandbox_json_value`
- 7148-7232: `fn render_help_topic`
- 7234-7248: `fn local_help_topic_command`
- 7250-7291: `fn render_export_help_json`
- 7293-7304: `fn render_help_topic_json`
- 7306-7318: `fn print_help_topic`
- 7320-7322: `fn acp_status_message`
- 7324-7360: `fn acp_status_json`
- 7362-7375: `fn print_acp_status`
- 7377-7454: `fn render_config_report`
- 7456-7536: `fn render_config_json`
- 7538-7575: `fn render_memory_report`
- 7577-7597: `fn render_memory_json`
- 7599-7602: `fn init_claude_md`
- 7604-7618: `fn run_init`
- 7620-7632: `fn init_json_value`
- 7634-7641: `fn normalize_permission_mode`
- 7643-7645: `fn render_diff_report`
- 7647-7681: `fn render_diff_report_for`
- 7683-7705: `fn render_diff_json_for`
- 7707-7720: `fn run_git_diff_command_in`
- 7722-7770: `fn render_teleport_report`
- 7772-7824: `fn render_last_tool_debug_report`
- 7826-7833: `fn indent_block`
- 7835-7846: `fn validate_no_args`
- 8092-8098: `fn render_version_report`
- 8100-8132: `fn render_export_text`
- 7983-8011: `fn render_prompt_history_report`（**已迁到 session_mgr.rs**，本 Task 跳过）
- 6722-6772: `fn print_status_snapshot`

`print_*` 系列：
- 11714-11866: `fn print_help_to`
- 11868-11883: `fn print_help`

- [ ] **Step 11.1: 创建 `format.rs`**

```rust
//! Output formatting: reports, JSON, status bar, help text, diff rendering.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::commands_handler::{CliAction, LocalHelpTopic};
use crate::doctor::{StatusContext, GitWorkspaceSummary, BootPreflightSnapshot};
use crate::session_mgr::{SessionHandle, ManagedSessionSummary, SessionLifecycleSummary};

pub(crate) fn format_model_report(...) -> String { ... }
pub(crate) fn format_model_switch_report(...) -> String { ... }
// ... 所有 format_* / render_* / print_* 函数全部 pub(crate)

#[cfg(test)]
pub(crate) fn format_unknown_slash_command_message(...) -> String { ... }
```

- [ ] **Step 11.2: 在 `main.rs` 添加 `mod format;`**

- [ ] **Step 11.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 11.4: 修复跨模块引用**

`app.rs`（LiveCli）调用几乎所有 format/render 函数 → 在 main.rs 顶部加 `use format::*;`。

`commands_handler.rs` 调用 `format_unknown_slash_command`、`format_unknown_option`、`format_unknown_direct_slash_command` → 已在 `use format::*;` 中（注：`format_unknown_*` 已迁到 commands_handler.rs 还是 format.rs？根据 Task 10 的迁移清单，`format_unknown_*` 在 commands_handler.rs，所以这里 commands_handler.rs 内部使用，无需跨模块）。

- [ ] **Step 11.5: 验证编译与测试**

```bash
cargo check -p rusty-claude-cli && cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 11.6: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/format.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract output formatting into format.rs"
```

---

## Task 12: 抽出 `app.rs`（最后）

**Files:**
- Create: `rust/crates/rusty-claude-cli/src/app.rs`
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`

**迁移范围（LiveCli 核心与启动逻辑）：**

- 4581-4584: `struct SessionHandle`（**已迁到 session_mgr.rs**，跳过）
- 4598-4614: `struct LiveCli`
- 4636-4642: `struct BuiltRuntime`
- 4644-4694: `impl BuiltRuntime`
- 4696-4704: `impl Deref for BuiltRuntime`
- 4706-4712: `impl DerefMut for BuiltRuntime`
- 4714-4719: `impl Drop for BuiltRuntime`
- 5105-6340: `impl LiveCli`（1236 行）
- 9066: `struct CliHookProgressReporter`
- 9068-9097: `impl HookProgressReporter for CliHookProgressReporter`
- 9099-9101: `struct CliPermissionPrompter`
- 9103-9107: `impl CliPermissionPrompter`
- 9109-9146: `impl PermissionPrompter for CliPermissionPrompter`
- 4322-4338: `fn detect_broad_cwd`
- 4340-4400: `fn enforce_broad_cwd_policy`
- 4402-4405: `fn stale_base_state_for`
- 4407-4419: `fn stale_base_json_value`
- 4421-4429: `fn run_stale_base_preflight`
- 4432-4578: `fn run_repl`
- 8971-8997: `fn build_runtime`
- 8999-9063: `fn build_runtime_with_plugin_state`

- [ ] **Step 12.1: 创建 `app.rs`**

```rust
//! LiveCli REPL core: REPL loop, turn execution, runtime construction.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crossterm::style::Color;
use runtime::{HookProgressReporter, PermissionPrompter, ConversationRuntime, Session, TokenUsage};

use crate::commands_handler::*;
use crate::doctor::*;
use crate::format::*;
use crate::paste::*;
use crate::plugin_state::*;
use crate::session_mgr::*;
use crate::streaming::*;
use crate::tool_display::*;
use crate::ultraplan::*;

pub(crate) struct LiveCli { ... }
pub(crate) struct BuiltRuntime { ... }
pub(crate) struct CliHookProgressReporter { ... }
pub(crate) struct CliPermissionPrompter { ... }

impl BuiltRuntime { ... }
impl Deref for BuiltRuntime { ... }
impl DerefMut for BuiltRuntime { ... }
impl Drop for BuiltRuntime { ... }
impl LiveCli { ... }
impl HookProgressReporter for CliHookProgressReporter { ... }
impl CliPermissionPrompter { ... }
impl PermissionPrompter for CliPermissionPrompter { ... }

pub(crate) fn detect_broad_cwd(...) -> ... { ... }
pub(crate) fn enforce_broad_cwd_policy(...) -> ... { ... }
pub(crate) fn stale_base_state_for(...) -> ... { ... }
pub(crate) fn stale_base_json_value(...) -> ... { ... }
pub(crate) fn run_stale_base_preflight(...) -> ... { ... }
pub(crate) fn run_repl(...) -> Result<...> { ... }
pub(crate) fn build_runtime(...) -> BuiltRuntime { ... }
pub(crate) fn build_runtime_with_plugin_state(...) -> BuiltRuntime { ... }
```

- [ ] **Step 12.2: 在 `main.rs` 添加 `mod app;`**

- [ ] **Step 12.3: 从 `main.rs` 删除已迁移代码**

- [ ] **Step 12.4: 修复跨模块引用**

`main.rs` 的 `run` 调用 `run_repl`、`build_runtime`、`enforce_broad_cwd_policy`、`detect_broad_cwd`、`stale_base_state_for`、`run_stale_base_preflight`、`LiveCli` → 在 main.rs 顶部加 `use app::*;`。

- [ ] **Step 12.5: 验证编译**

```bash
cargo check -p rusty-claude-cli 2>&1 | tail -10
```

- [ ] **Step 12.6: 验证测试**

```bash
cargo test -p rusty-claude-cli --lib 2>&1 | grep "test result"
```

- [ ] **Step 12.7: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/app.rs rust/crates/rusty-claude-cli/src/main.rs
git commit -m "refactor(cli): extract LiveCli REPL core into app.rs

main.rs is now ~600 lines: entry, shared constants/types, main(), run(),
and basic helpers (classify_error_kind, split_error_hint, read_piped_stdin,
merge_prompt_with_stdin, plugin_command_json, plugin_summary_json,
plugin_load_failure_json, max_tokens_for_model)."
```

---

## Task 13: 最终验证与 main.rs 体积检查

- [ ] **Step 13.1: 验证整体编译**

```bash
cd d:\claw-code-src\rust
cargo check --workspace 2>&1 | tail -5
```

预期：无错误。

- [ ] **Step 13.2: 验证全测试通过**

```bash
cargo test -p rusty-claude-cli 2>&1 | tail -20
```

预期：所有测试通过，数量 ≥ Pre-flight 基线。

- [ ] **Step 13.3: 检查 main.rs 行数**

```bash
wc -l rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/*.rs
```

预期：`main.rs` ≤ 700 行；其余模块按上述目标行数。

- [ ] **Step 13.4: 检查无 unused warning**

```bash
cargo check -p rusty-claude-cli 2>&1 | grep -E "warning|unused"
```

如有未使用警告，删除对应 `use` 或加 `#[allow(dead_code)]`。

- [ ] **Step 13.5: 最终提交（如需）**

```bash
git status
git add -A
git commit -m "chore(cli): final cleanup after main.rs extraction"
```

---

## Self-Review

**1. Spec coverage**（对照 TUI-ENHANCEMENT-PLAN.md Phase 0）：

- ✅ Task 0.1 "Extract LiveCli into app.rs" → Task 12
- ✅ Task 0.2 "Keep legacy CliApp removed" → 不涉及（无残留代码）
- ✅ Task 0.3 "Extract main.rs arg parsing" → Task 10（commands_handler.rs）
- ✅ Task 0.4 "Create tui/ module" → 暂未创建，留待 Phase 1（用户决策"Phase 0 拆分完成并验证编译后再引入 ratatui"）
- ✅ 用户决策"更激进拆 8-10 模块" → 实际拆 12 个（tests/paste/suggestion/ultraplan/tool_display/plugin_state/streaming/session_mgr/doctor/commands_handler/format/app）+ main.rs 留入口，达成目标

**2. Placeholder scan**：

- ✅ 所有 Task 都给出了具体行号、文件路径、迁移函数清单
- ✅ 所有 Step 都给出了具体命令（cargo check/test、git add/commit）
- ✅ 无 "TBD"、"TODO"、"implement later"

**3. Type consistency**：

- ✅ `LiveCli` 类型在 Task 12 定义，被 main.rs `run` 调用
- ✅ `BuiltRuntime` 在 Task 12 定义，被 `LiveCli` 字段引用
- ✅ `CliToolExecutor` 在 Task 5 定义，被 `BuiltRuntime` 字段引用（Task 12）
- ✅ `AnthropicRuntimeClient` 在 Task 7 定义，被 `BuiltRuntime` 字段引用（Task 12）
- ✅ `RuntimeMcpState`、`RuntimePluginState` 在 Task 6 定义，被 `CliToolExecutor`、`BuiltRuntime` 字段引用
- ✅ `SessionHandle`、`ManagedSessionSummary`、`PromptHistoryEntry` 在 Task 8 定义，被 `LiveCli` 字段引用（Task 12）
- ✅ `InternalPromptProgressReporter` 在 Task 4 定义，被 `AnthropicRuntimeClient` 字段引用（Task 7）
- ✅ `StatusContext`、`GitWorkspaceSummary` 在 Task 9 定义，被 `format.rs`（Task 11）调用

**4. 拆分顺序合理性**：

按依赖深度从浅到深：tests(1) → paste(2) → suggestion(3) → ultraplan(4) → tool_display(5) → plugin_state(6) → streaming(7) → session_mgr(8) → doctor(9) → commands_handler(10) → format(11) → app(12)。

每个 Task 之间无循环依赖（除 Task 5 与 Task 6 之间存在 `CliToolExecutor` ↔ `RuntimeMcpState` 字段引用，但通过 `use crate::plugin_state::RuntimeMcpState` 解决）。

**5. 风险点提醒**：

- ⚠️ Task 1（tests.rs）需为 70+ 私有符号加 `pub(crate)`，工作量最大但风险最低
- ⚠️ Task 8（session_mgr.rs）含 429 行的 `run_resume_command`，整体迁移不要试图重构
- ⚠️ Task 10（commands_handler.rs）含 487 行的 `parse_args`，整体迁移不要试图重构
- ⚠️ Task 12（app.rs）的 `impl LiveCli`（1236 行）不可拆分到多个模块

---

## Execution Handoff

**Plan complete and saved to `rust/.omc/plans/2026-07-19-tui-phase0-refactor.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — 每个 Task 派发一个 fresh subagent 执行，主流程审阅后继续下一个 Task。适合 Task 1-12 之间有依赖关系但每个 Task 内部独立的场景。

**2. Inline Execution** — 在当前会话中按顺序执行，每个 Task 完成后检查点审阅。适合需要紧密调试的场景。

**推荐：Subagent-Driven。**

- Task 1（tests.rs 抽取，需修改 70+ 符号可见性）独立派发
- Task 2-4（paste/suggestion/ultraplan）边界清晰，可串行派发
- Task 5-6（tool_display/plugin_state）有相互依赖，建议串行
- Task 7-9（streaming/session_mgr/doctor）独立模块大，串行派发
- Task 10-11（commands_handler/format）相互独立
- Task 12（app.rs）必须最后

每个 subagent 任务描述需包含：本 plan 中对应 Task 的全部内容（行号、迁移清单、`pub(crate)` 列表、验证命令），并指示 subagent 在完成后跑 `cargo check` + `cargo test --lib` 并报告结果。

执行前请用户确认本计划。
