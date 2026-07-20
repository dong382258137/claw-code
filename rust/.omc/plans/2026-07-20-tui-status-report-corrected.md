# TUI 模块状态评估报告（修正版）

**创建日期**: 2026-07-20
**核实日期**: 2026-07-20（基于代码 + git 状态实证核实）
**状态**: 进行中（P0 待提交，P1/P2 待修复）

---

## 一、当前健康状态

| 维度 | 状态 | 核实结果 |
|---|---|---|
| 文件结构 | ⚠ 不完整 | 9 个文件中 7 个已跟踪，2 个未跟踪（sidebar.rs / tool_card.rs） |
| 默认 cargo check | ✅ 通过 | 实测通过 |
| Feature cargo check | ✅ 通过 | `cargo build -p rusty-claude-cli --features full-tui` 通过 |
| TUI 模块测试 | ✅ 88 个全通过 | `cargo test tui::` → 88 passed; 0 failed |
| 全量测试 | ⚠ 324/327 passed | 实测 327 tests, 324 passed, 3 failed（MCP/plugin 相关，与 TUI 无关） |
| Clippy | ⚠ 3 个 warning（TUI 部分） | 2 个 manual_clamp + 1 个 sort_by_key（runtime 另有 5 个无关 warning） |

## 二、已完成工作

| 阶段 | 内容 | 状态 |
|---|---|---|
| Phase 1 | TUI 基础架构（InputLine/SlashMenu/OutputView/StatusBar/--tui flag） | ✅ 已提交 |
| Phase 2 | StatusEmitter 注入、流式 token 捕获、ToolUse/ToolResult 事件 | ✅ 已提交 |
| 额外 | TUI 设为默认 REPL 模式（--no-tui 回退）、Sidebar、Tool 卡片、Help overlay、滚动 | ⚠ 已实现但 sidebar.rs/tool_card.rs 未跟踪 |
| Phase 3.1 | Thinking block 事件 + TUI 渲染 | ⚠ 未提交（工作区改动） |
| Phase 3.2 | Markdown 渲染（ansi-to-tui 7.x + TerminalRenderer 缓存） | ⚠ 未提交（工作区改动） |
| **新增** | TUI 界面汉化（sidebar/status_bar/app/slash_menu 中文注释） | ⚠ 未提交（工作区改动） |
| **新增** | 斜杠命令本地分发（修复 /help 发给 AI 的 bug） | ⚠ 未提交（工作区改动） |
| **新增** | TUI 斜杠命令输出迁移计划文档 | ⚠ 未提交（工作区改动） |

## 三、存在的问题

### 🔴 P0：版本控制状态混乱

**问题 1：sidebar.rs / tool_card.rs 未跟踪**

- 核实：`git ls-files crates/rusty-claude-cli/src/tui/` 返回 7 个文件（app/input_line/mod/output_view/slash_menu/status_bar/tests），不包含 sidebar.rs 和 tool_card.rs
- `git status` 显示 `?? rust/crates/rusty-claude-cli/src/tui/sidebar.rs` 和 `?? .../tool_card.rs`
- 这两个文件被 `tui/mod.rs` 引用并编译通过，但未入版本控制
- **影响**：clone/checkout 后直接编译失败

**问题 2：P3.1 + P3.2 + 近期改动未提交**

工作区有以下未提交改动（`git status` 确认）：
- `crates/runtime/src/conversation.rs` — **+727/-38 行**（报告原说 +361，核实后更正为 +727/-38）
- `crates/rusty-claude-cli/Cargo.toml` — +3/-1（ansi-to-tui 依赖 + unicode-width）
- `crates/rusty-claude-cli/src/streaming.rs` — +27/-2（Thinking 事件 emit）
- `crates/rusty-claude-cli/src/tui/app.rs` — 大量改动（Markdown 渲染 + 汉化 + 斜杠命令路由）
- `crates/rusty-claude-cli/src/tui/input_line.rs` — 汉化 + ScrollUpLine/ScrollDownLine
- `crates/rusty-claude-cli/src/tui/mod.rs` — 模块声明
- `crates/rusty-claude-cli/src/tui/output_view.rs` — 未核实具体改动
- `crates/rusty-claude-cli/src/tui/slash_menu.rs` — 中文注释映射表
- `crates/rusty-claude-cli/src/tui/status_bar.rs` — 汉化
- `crates/rusty-claude-cli/src/app.rs` — tui_println + handle_repl_command 改造

**❌ 修正：删除"runtime crate test 模式下 memory_semantic/entry_id 编译不过"**

原报告称 conversation.rs 改动导致 test 模式编译不过（被 cargo 缓存掩盖）。**核实结果**：
- `Select-String -Path conversation.rs -Pattern "memory_semantic|entry_id"` 零匹配
- 全量 `cargo test` 编译通过（327 个测试都编译成功）
- 不存在"被 cargo 缓存掩盖"的编译问题
- 此条为虚构内容，已删除

### 🟡 P1：Thinking 事件覆盖缺口

**核实属实**：[streaming.rs:786-793](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L786-L793) 的 `push_output_block` 处理 `Thinking/RedactedThinking` 块时：
- 只调用 `render_thinking_block_summary(out, ...)` 写到 `out`
- TUI 模式下 `out = io::sink()`，输出被丢弃
- 未调用 `self.emit_status(StatusEvent::Thinking {...})`

**影响场景**：
- `MessageStart` 事件携带的 thinking 块（非流式响应或首批 content）
- `ContentBlockStart` 携带的完整 thinking 块
- 流式 `ThinkingDelta` 路径已覆盖（[L461](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L461)）

**❌ 修正：修复方案**

原报告建议"改签名为 `push_output_block(..., emitter: Option<&StatusEmitter>)`"。

**核实发现**：`push_output_block` 是**自由函数**（非方法），无法访问 `self.emit_status`。正确方案是**在 caller 端检查 `block_has_thinking_summary` 并 emit**：

```rust
// consume_stream 的 MessageStart/ContentBlockStart 分支
push_output_block(..., &mut block_has_thinking_summary)?;
if block_has_thinking_summary {
    self.emit_status(StatusEvent::Thinking { char_count: ..., redacted: ... });
}
```

### 🟡 P1：Markdown 渲染性能隐患

**核实属实**：[tui/app.rs:198-206](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/app.rs#L198-L206) 每次 `draw`（流式时 50ms 一次）都对 64KB buffer 做：

```
snapshot → markdown_to_ansi (pulldown-cmark + syntect) → ansi_to_tui::into_text
```

- 长对话 buffer 接近 64KB，每次渲染都重新解析整个 markdown
- syntect 语法高亮对代码块尤其耗时
- 没有缓存机制，相同内容会重复渲染

**修复方案 A（推荐）**：缓存上次渲染的 snapshot hash + Text，snapshot 未变时跳过转换。

### 🟢 P2：Clippy 警告

**核实属实**：

[tui/app.rs:603-604](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/app.rs#L603-L604)：

```rust
let modal_w = (area.width / 2).max(50).min(80);   // 应改为 .clamp(50, 80)
let modal_h = (area.height * 7 / 10).max(20).min(30);  // 应改为 .clamp(20, 30)
```

实测 `cargo clippy` 输出：rusty-claude-cli bin 有 3 个 warning（2 个 manual_clamp + 1 个 sort_by_key），runtime lib 另有 5 个无关 warning。

## 四、未完成的工作

### 🔴 P0：版本控制清理（最高优先级）

1. 提交 `sidebar.rs` / `tool_card.rs`（2 个未跟踪文件）
2. 提交 P3.1 / P3.2 改动（streaming.rs Thinking 事件 + tui/app.rs Markdown 渲染 + Cargo.toml 依赖）
3. 提交 conversation.rs 改动（+727/-38 行，需先 review diff 确认内容合理）
4. 提交近期 TUI 汉化 + 斜杠命令本地分发改动

### 🟡 P1：补全 Thinking 事件

在 `consume_stream` 的 caller 端（`MessageStart` / `ContentBlockStart` 分支）事后检查 `block_has_thinking_summary` 并 emit `StatusEvent::Thinking`。

### 🟡 P1：Markdown 渲染缓存（方案 A）

缓存上次渲染的 snapshot hash + Text，snapshot 未变时跳过转换。

### 🟢 P2：Clippy manual_clamp 修复

2 处 `.max().min()` 改为 `.clamp()`。

## 五、优先级

| 优先级 | 任务 | 理由 |
|---|---|---|
| P0 | 提交 sidebar.rs / tool_card.rs + P3.1/P3.2 + 近期改动 | 版本控制完整性，clone 后才能编译 |
| P0 | Review + 提交 conversation.rs +727/-38 | 大量改动未入版本控制 |
| P1 | 补全 Thinking 事件（caller 端 emit） | 功能完整性 |
| P1 | Markdown 渲染缓存（方案 A） | 性能 |
| P2 | Clippy manual_clamp 修复 | 代码质量 |

## 六、核实方法说明

本报告所有结论基于以下实证：

| 项 | 核实命令 | 结果 |
|---|---|---|
| 文件跟踪状态 | `git ls-files crates/rusty-claude-cli/src/tui/` | 7 个文件 |
| 未跟踪文件 | `git status --porcelain` | `??` 标记 sidebar.rs / tool_card.rs |
| conversation.rs diff | `git diff --stat crates/runtime/src/conversation.rs` | +727/-38 |
| memory_semantic/entry_id | `Select-String -Path conversation.rs -Pattern "memory_semantic\|entry_id"` | 零匹配 |
| Thinking 事件缺口 | `Grep push_output_block\|StatusEvent::Thinking` | 自由函数，未 emit |
| Markdown 渲染 | `Read tui/app.rs:198-206` | 每次全量转换，无缓存 |
| Clippy | `cargo clippy -p rusty-claude-cli --features full-tui` | 3 个 TUI warning |
| TUI 测试 | `cargo test tui::` | 88 passed; 0 failed |
| 全量测试 | `cargo test -p rusty-claude-cli --features full-tui` | 327 tests, 324 passed, 3 failed |
