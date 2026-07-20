# TUI Phase 2 — 实时流式事件接入（emit → TuiApp 可见）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 Phase 1 已实现但未接入的 `StatusEmitter` hook 真正接到 `TuiApp`，让流式过程中的 `TextDelta` / `Usage` / `MessageStop` 事件实时驱动 `OutputView` 和 `StatusBarState`，消除 Phase 1 MVP 的"回合后同步"近似。

**Architecture:** 不改 `build_runtime` / `ApiClient` trait 签名（避免影响 12 个调用点），改用 **setter 注入** 模式：(1) `AnthropicRuntimeClient` 增加 `set_status_emitter(&mut self, emitter)` setter；(2) `LiveCli` 持有 feature-gated `status_emitter` 字段并在 `prepare_turn_runtime` 内通过 `runtime.api_client_mut().set_status_emitter(...)` 透传；(3) `TuiApp::handle_submit` 在调用 `cli.run_turn(line)` 之前构造 emitter，emitter 回调闭包同时写入 `OutputView` 和 `StatusBarState`。流式过程中 consume_stream 内原有的 6 个 `emit_status` 调用点（Task 7 已就位）会自动驱动 TUI 更新。

**Tech Stack:** `Arc<dyn Fn(StatusEvent) + Send + Sync>`（已有）、`Arc<Mutex<OutputBuffer>>` / `Arc<Mutex<StatusBarState>>`（已有）、`runtime::ConversationRuntime::api_client_mut()`（已有）。

---

## Workspace

- 工作分支：`feature/tui-refactor`（Phase 1 已用，继续在此分支）
- 工作目录：`d:\claw-code-src`
- 目标 crate：`rust/crates/rusty-claude-cli`
- 基线（默认构建）：`cargo test -p rusty-claude-cli --bin claw -- --test-threads=1` = 218 passed + 3 failed（pre-existing）
- 基线（feature 构建）：`cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1` = 264 passed + 3 failed

---

## 关键约束

1. **Feature flag 双重门控**：所有新增 LiveCli 字段/方法必须在 `#[cfg(feature = "full-tui")]` 内。`StatusEmitter` / `StatusEvent` 类型本身在 streaming.rs 中无门控（Phase 1 已就位），但 LiveCli 持有它必须 feature-gated。
2. **基线零回归**：每个 Task 完成后 `cargo check -p rusty-claude-cli`（不带 feature）必须通过；测试基线保持 218+3。Feature 构建保持 264+3。
3. **不改 `build_runtime` 签名**：12 个调用点不动。改用 setter 模式在 `prepare_turn_runtime` 内透传。
4. **不改 `consume_stream` 内部**：6 个 `emit_status` 调用点（Task 7 已就位）无需修改。
5. **不改 `ApiClient` trait**：`fn stream(&mut self, request: ApiRequest)` 签名不动。
6. **PowerShell 兼容**：测试命令用 `;` 分隔，文件写入用 `[System.IO.File]::WriteAllText`。

---

## 已确认的现有 API

### `AnthropicRuntimeClient`（streaming.rs）
- 字段 `status_emitter: Option<StatusEmitter>`（第 142 行，Phase 1 已加）
- `pub(crate) fn with_status_emitter(mut self, emitter: StatusEmitter) -> Self`（第 219 行，builder 模式）
- `fn emit_status(&self, event: StatusEvent)`（第 225 行，私有 helper）
- `pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>)`（第 212 行，参考模板）
- 6 个 `self.emit_status(...)` 调用点（第 382, 420, 452, 461, 471, 485 行）

### `LiveCli`（app.rs 第 416-432 行）
```rust
pub(crate) struct LiveCli {
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    system_prompt: Vec<String>,
    runtime: BuiltRuntime,
    session: SessionHandle,
    prompt_history: Vec<PromptHistoryEntry>,
    output_verbosity: OutputVerbosity,
    cumulative_usage: runtime::TokenUsage,
    goal_manager: runtime::GoalManager,
}
```

### `LiveCli::prepare_turn_runtime`（app.rs 第 707-728 行）
```rust
fn prepare_turn_runtime(
    &self,
    emit_output: bool,
) -> Result<(BuiltRuntime, HookAbortMonitor), Box<dyn std::error::Error>> {
    let hook_abort_signal = runtime::HookAbortSignal::new();
    let mut runtime = build_runtime(
        self.runtime.session().clone(),
        &self.session.id,
        self.model.clone(),
        self.system_prompt.clone(),
        true,
        emit_output,
        self.allowed_tools.clone(),
        self.permission_mode,
        None,
    )?
    .with_hook_abort_signal(hook_abort_signal.clone());
    runtime.set_tool_verbosity(self.output_verbosity);
    let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal);
    Ok((runtime, hook_abort_monitor))
}
```

### `ConversationRuntime::api_client_mut`（runtime/src/conversation.rs 第 788 行）
```rust
pub fn api_client_mut(&mut self) -> &mut C
```
返回 `&mut C`，其中 `C = AnthropicRuntimeClient`。因此可以调用 `runtime.api_client_mut().set_status_emitter(emitter)`。

### `BuiltRuntime` Deref（app.rs 第 494-510 行）
`BuiltRuntime` impl `Deref<Target = ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>>`，所以 `runtime.api_client_mut()` 直接可用。

### `TuiApp::handle_submit`（tui/app.rs，Phase 1 已实现）
```rust
fn handle_submit(
    cli: &mut LiveCli,
    line: &str,
    output_view: &mut OutputView,
    status_state: &Arc<Mutex<StatusBarState>>,
) -> Result<(), Box<dyn std::error::Error>> {
    {
        let mut guard = status_state.lock().expect("StatusBarState poisoned");
        guard.reset_turn();
    }
    let _ = output_view; // suppress unused warning for now
    let result = cli.run_turn(line);
    {
        let mut guard = status_state.lock().expect("StatusBarState poisoned");
        if guard.streaming {
            guard.finish_turn();
        }
    }
    sync_status_from_cli(status_state, cli);
    result?;
    Ok(())
}
```

### `OutputView`（tui/output_view.rs）
- `pub(crate) fn shared_handle(&self) -> Arc<Mutex<OutputBuffer>>` — 返回内部 buffer handle
- `OutputBuffer` 字段：`buffer: String`, `total_written: u64`, `truncated: bool`（私有）
- impl `io::Write for OutputView`（但 `OutputView` 本身不能跨线程共享，需要用 `shared_handle()` 拿到 `Arc<Mutex<OutputBuffer>>`）

### `StatusBarState`（tui/status_bar.rs）
- `pub(crate) fn shared() -> Arc<Mutex<Self>>`
- `pub(crate) fn reset_turn(&mut self)` — 标记 streaming = true，重置 turn_usage
- `pub(crate) fn finish_turn(&mut self)` — 标记 streaming = false，fold turn_usage 到 cumulative
- `pub(crate) fn total_tokens(&self) -> u128`
- 字段：`cumulative_usage`, `turn_usage`, `streaming`, `turn_elapsed_ms` 等都是 `pub`

### `StatusEvent`（streaming.rs）

> BUG 6 note: this snippet was originally written when Phase 2 was planned.
> The enum has since been extended (ToolUse gained `input`, ToolResult and
> Thinking variants were added during Phase 3). The block below is kept in
> sync with the actual `streaming.rs` definition — update both together.

```rust
pub(crate) enum StatusEvent {
    /// A usage delta arrived (input/output tokens updated).
    Usage(TokenUsage),
    /// A text delta arrived (incremental assistant output).
    TextDelta(String),
    /// A tool use started (tool name + input JSON provided).
    ToolUse { id: String, name: String, input: String },
    /// A tool finished executing (id, name, output, is_error).
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// A thinking block was observed during streaming.
    Thinking {
        char_count: Option<usize>,
        redacted: bool,
    },
    /// The model finished responding (MessageStop received).
    MessageStop,
    /// Streaming turn started (first event received).
    StreamStart,
}
```
- `TokenUsage` 是 `Copy + Clone`，字段：`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`（都是 `u32`）

---

## Task 1: 给 `AnthropicRuntimeClient` 添加 `set_status_emitter` setter

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/streaming.rs`

Phase 1 已有 `with_status_emitter(mut self, emitter) -> Self`（builder 模式，消耗 self），但 `prepare_turn_runtime` 内的 runtime 已经构造完毕，需要 `&mut self` 版本的 setter。

- [ ] **Step 1.1: 在 `with_status_emitter` 之后追加 `set_status_emitter`**

在 `streaming.rs` 第 219-222 行的 `with_status_emitter` 方法之后（第 222 行 `}` 之后、`emit_status` 之前）追加：

```rust
    /// Attach a status emitter callback to an already-constructed client.
    /// This is the `&mut self` counterpart to `with_status_emitter` — used
    /// when the client is already wrapped inside a `ConversationRuntime`
    /// and we only have `api_client_mut()` access.
    pub(crate) fn set_status_emitter(&mut self, emitter: StatusEmitter) {
        self.status_emitter = Some(emitter);
    }
```

- [ ] **Step 1.2: 验证默认 + feature 编译**

```powershell
cd d:\claw-code-src\rust
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
cargo check -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 5
```

预期：两者均通过（新方法是 `pub(crate)`，未被调用时只是 dead_code 警告，但 streaming.rs 顶部已有 `#![allow(dead_code, ...)]`）。

- [ ] **Step 1.3: 验证测试基线**

```powershell
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：218 passed; 3 failed（默认） + 264 passed; 3 failed（feature）。

- [ ] **Step 1.4: 提交**

```powershell
$msg = @"
feat(streaming): add set_status_emitter setter for &mut self injection

Counterpart to with_status_emitter (builder). Allows attaching a
StatusEmitter to an AnthropicRuntimeClient that is already wrapped
inside ConversationRuntime, via `runtime.api_client_mut().set_status_emitter(...)`.
No behavioral change — emitter still defaults to None.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/streaming.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 2: `LiveCli` 持有 feature-gated `status_emitter` 字段并在 `prepare_turn_runtime` 透传

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/app.rs`

- [ ] **Step 2.1: 给 `LiveCli` struct 添加 feature-gated 字段**

在 `app.rs` 第 416-432 行的 `LiveCli` struct 定义中，在 `goal_manager` 字段之后（第 431 行之后、`}` 之前）添加：

```rust
    // Phase 2: feature-gated status_emitter holder. When set (by TuiApp
    // via set_status_emitter), prepare_turn_runtime injects it into the
    // freshly-constructed AnthropicRuntimeClient so streaming events drive
    // the TUI's StatusBarState + OutputView in real time.
    #[cfg(feature = "full-tui")]
    status_emitter: Option<crate::streaming::StatusEmitter>,
```

- [ ] **Step 2.2: 在 `LiveCli::new` 初始化字段为 None**

在 `app.rs` 的 `LiveCli::new` 函数末尾的 `Ok(Self { ... })` 块中（约第 575 行附近，goal_manager 字段初始化之后）添加：

```rust
            #[cfg(feature = "full-tui")]
            status_emitter: None,
```

**注意**：在 struct literal 中，`#[cfg(...)]` 属性可以放在字段之前。需要找到现有 `Ok(Self { ... })` 块并添加这一行。

- [ ] **Step 2.3: 在 `prepare_turn_runtime` 内透传 emitter**

修改 `app.rs` 第 707-728 行的 `prepare_turn_runtime`，在 `runtime.set_tool_verbosity(...)` 之后、`let hook_abort_monitor = ...` 之前添加：

```rust
        // Phase 2: if a status_emitter is attached (TUI mode), inject it
        // into the freshly-built AnthropicRuntimeClient so streaming events
        // drive the TUI's StatusBarState + OutputView in real time.
        #[cfg(feature = "full-tui")]
        if let Some(emitter) = &self.status_emitter {
            runtime.api_client_mut().set_status_emitter(Arc::clone(emitter));
        }
```

**注意**：`Arc::clone(emitter)` 因为 `StatusEmitter = Arc<dyn Fn(StatusEvent) + Send + Sync>`，需要 clone 出一个新的 Arc 传入 `set_status_emitter`（它消耗 emitter）。

同时确保 `use std::sync::Arc;` 已在 app.rs 顶部引入（检查现有 use 块）。

- [ ] **Step 2.4: 添加 `set_status_emitter` + `set_tui_mode` setter 到 LiveCli**

在 `impl LiveCli` 块中，紧邻 Phase 1 已添加的 `session_id_snapshot` 方法之后（约第 1760 行附近，最后访问器之后）添加：

```rust
    /// Phase 2: Attach a StatusEmitter that will be injected into every
    /// subsequently-built AnthropicRuntimeClient via prepare_turn_runtime.
    /// The emitter receives streaming events (TextDelta, Usage, MessageStop,
    /// StreamStart, ToolUse) and should update the caller's shared state
    /// (e.g., TuiApp's OutputView + StatusBarState).
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_status_emitter(&mut self, emitter: crate::streaming::StatusEmitter) {
        self.status_emitter = Some(emitter);
    }

    /// Phase 2: Detach any previously-attached status emitter. Useful for
    /// cleanup or switching emitters between sessions.
    #[cfg(feature = "full-tui")]
    pub(crate) fn clear_status_emitter(&mut self) {
        self.status_emitter = None;
    }

    /// Phase 2: Toggle TUI mode. When on, run_turn calls prepare_turn_runtime
    /// with emit_output=false so consume_stream's `out` goes to io::sink()
    /// instead of stdout — preventing duplicate output in alternate screen.
    /// Streaming content is captured via the status_emitter's TextDelta callback.
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_tui_mode(&mut self, on: bool) {
        self.tui_mode = on;
    }
```

- [ ] **Step 2.4.1: 给 `LiveCli` struct 添加 `tui_mode` 字段**

在 Step 2.1 添加的 `status_emitter` 字段之后追加：

```rust
    /// Phase 2: When true, run_turn suppresses emit_output (consume_stream
    /// writes to io::sink instead of stdout). TUI captures content via the
    /// status_emitter's TextDelta callback. Set by TuiApp via set_tui_mode.
    #[cfg(feature = "full-tui")]
    tui_mode: bool,
```

- [ ] **Step 2.4.2: 在 `LiveCli::new` 初始化 `tui_mode` 为 false**

在 Step 2.2 的 `Ok(Self { ... })` 块中，在 `status_emitter: None,` 之后追加：

```rust
            #[cfg(feature = "full-tui")]
            tui_mode: false,
```

- [ ] **Step 2.4.3: 修改 `run_turn` 让 `emit_output` 跟随 `tui_mode`**

在 `app.rs` 第 736 行 `pub(crate) fn run_turn(&mut self, input: &str) -> ...` 内，找到第 737 行 `let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(true)?;` 并修改为：

```rust
        let emit_output = {
            #[cfg(feature = "full-tui")]
            { !self.tui_mode }
            #[cfg(not(feature = "full-tui"))]
            { true }
        };
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(emit_output)?;
```

**说明**：用 `cfg` block 表达式确保默认构建（无 feature）行为不变，feature 构建时根据 tui_mode 切换。

- [ ] **Step 2.5: 验证默认 + feature 编译**

```powershell
cd d:\claw-code-src\rust
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
cargo check -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 10
```

预期：默认通过；feature 通过（可能有 dead_code 警告 — `clear_status_emitter` 暂无调用方，可加 `#[allow(dead_code)]` 或忽略）。

如有编译错误：
- E0601 main.rs: `use std::sync::Arc;` 已在 app.rs 第 22 行存在（`use std::sync::{Arc, Mutex};`），所以 `Arc::clone` 可用
- E0433: `crate::streaming::StatusEmitter` 路径 — 检查 streaming.rs 中 `pub(crate) type StatusEmitter = ...` 是否可见

- [ ] **Step 2.6: 验证测试基线**

```powershell
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：218 passed; 3 failed（默认） + 264 passed; 3 failed（feature）。

- [ ] **Step 2.7: 提交**

```powershell
$msg = @"
feat(app): LiveCli holds feature-gated status_emitter, injects via prepare_turn_runtime

LiveCli gains `status_emitter: Option<StatusEmitter>` field (feature-gated).
prepare_turn_runtime calls `runtime.api_client_mut().set_status_emitter(...)`
when the field is set, so streaming events flow to the attached observer.
Two new LiveCli setters: set_status_emitter, clear_status_emitter.

This wires Phase 1's StatusEmitter hook (Task 7) into the actual call
chain — TuiApp can now attach an emitter that updates StatusBarState
and OutputView in real time during streaming.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/app.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 3: `TuiApp::handle_submit` 构造 emitter 并注入 LiveCli，实时更新 OutputView + StatusBarState

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/app.rs`

这是 Phase 2 的核心 — 在 TuiApp 调用 `cli.run_turn(line)` 之前，构造一个 `StatusEmitter` 闭包，它同时更新 `OutputView`（写 TextDelta）和 `StatusBarState`（累加 Usage、设置 streaming flag）。emitter 通过 `cli.set_status_emitter(...)` 注入。

- [ ] **Step 3.1: 在 `handle_submit` 之前构造 emitter，注入 cli**

替换 `tui/app.rs` 中的 `handle_submit` 函数为以下版本：

```rust
fn handle_submit(
    cli: &mut LiveCli,
    line: &str,
    output_view: &mut OutputView,
    status_state: &Arc<Mutex<StatusBarState>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::streaming::{StatusEmitter, StatusEvent};
    use std::sync::Arc;

    // Phase 2: Construct a StatusEmitter that updates OutputView + StatusBarState
    // in real time as streaming events arrive. The emitter is injected into LiveCli
    // via set_status_emitter, and prepare_turn_runtime will forward it to the
    // freshly-built AnthropicRuntimeClient.
    let output_handle = output_view.shared_handle();
    let status_handle = Arc::clone(status_state);
    let emitter: StatusEmitter = Arc::new(move |event: StatusEvent| {
        match event {
            StatusEvent::TextDelta(text) => {
                if let Ok(mut buf) = output_handle.lock() {
                    buf.buffer.push_str(&text);
                    buf.total_written += text.len() as u64;
                    // Trim if exceeds max (mirror OutputView::write behavior)
                    const MAX_BUFFER_BYTES: usize = 64 * 1024;
                    if buf.buffer.len() > MAX_BUFFER_BYTES {
                        let overflow = buf.buffer.len() - MAX_BUFFER_BYTES;
                        buf.buffer = buf.buffer.split_off(overflow);
                        buf.truncated = true;
                    }
                }
            }
            StatusEvent::Usage(usage) => {
                if let Ok(mut guard) = status_handle.lock() {
                    guard.turn_usage.input_tokens += usage.input_tokens;
                    guard.turn_usage.output_tokens += usage.output_tokens;
                    guard.turn_usage.cache_creation_input_tokens +=
                        usage.cache_creation_input_tokens;
                    guard.turn_usage.cache_read_input_tokens +=
                        usage.cache_read_input_tokens;
                }
            }
            StatusEvent::StreamStart => {
                if let Ok(mut guard) = status_handle.lock() {
                    guard.reset_turn();
                }
            }
            StatusEvent::MessageStop => {
                if let Ok(mut guard) = status_handle.lock() {
                    if guard.streaming {
                        guard.finish_turn();
                    }
                }
            }
            StatusEvent::ToolUse { .. } => {
                // Tool use events don't directly update the status bar or output view;
                // the rendered tool call display is written to stdout by consume_stream,
                // which TUI mode suppresses (emit_output=false path doesn't apply here).
                // For MVP, TUI shows only TextDelta content; tool call cards are a
                // future enhancement (Phase 2.5).
            }
        }
    });
    cli.set_status_emitter(emitter);
    cli.set_tui_mode(true);

    // Call the existing run_turn path. StatusEmitter callback will fire
    // during streaming, updating output_view and status_state in real time.
    // set_tui_mode(true) makes prepare_turn_runtime use emit_output=false,
    // so consume_stream writes to io::sink instead of stdout — preventing
    // duplicate output under TUI's alternate screen.
    let result = cli.run_turn(line);

    // Detach emitter and reset TUI mode so next turn starts clean
    cli.clear_status_emitter();
    cli.set_tui_mode(false);

    // After turn, sync the authoritative cumulative_usage from cli (the
    // emitter only saw turn_usage deltas; cumulative is still tracked by LiveCli).
    sync_status_from_cli(status_state, cli);

    result?;
    Ok(())
}
```

**关键变化**：
1. 不再手动调用 `guard.reset_turn()` — `StatusEvent::StreamStart` 回调会做
2. 不再手动调用 `guard.finish_turn()` — `StatusEvent::MessageStop` 回调会做
3. `output_view` 不再被 `_ = output_view` 忽略 — `shared_handle()` 拿到内部 buffer
4. 仍调用 `sync_status_from_cli` 同步 cumulative_usage（emitter 只看到 turn_usage 增量，cumulative 是 LiveCli 的权威）

- [ ] **Step 3.2: 移除 Phase 1 的 `let _ = output_view;` 死代码**

如果新 `handle_submit` 完全覆盖旧版本，`let _ = output_view;` 应该已经被替换掉了。验证 tui/app.rs 中没有遗留死代码。

- [ ] **Step 3.3: 验证 feature 编译**

```powershell
cd d:\claw-code-src\rust
cargo check -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 15
```

预期：通过。可能有警告：
- `OutputBuffer` 的 `buffer` / `total_written` / `truncated` 字段是私有的 — 如果 `output_view.rs` 的 `OutputBuffer` struct 不是 `pub(crate)`，emitter 闭包内的 `buf.buffer.push_str(...)` 会编译失败。

如果 E0616 field is private：
- 方案 A：把 `OutputBuffer` 的字段改为 `pub(crate)`
- 方案 B：给 `OutputBuffer` 加 `pub(crate) fn append(&mut self, text: &str)` 方法

推荐方案 B（更封装）：
在 `tui/output_view.rs` 的 `impl OutputBuffer` 中加：

```rust
impl OutputBuffer {
    pub(crate) fn append(&mut self, text: &str) {
        self.buffer.push_str(text);
        self.total_written += text.len() as u64;
        const MAX_BUFFER_BYTES: usize = 64 * 1024;
        if self.buffer.len() > MAX_BUFFER_BYTES {
            let overflow = self.buffer.len() - MAX_BUFFER_BYTES;
            self.buffer = self.buffer.split_off(overflow);
            self.truncated = true;
        }
    }
}
```

然后 `handle_submit` 中改为 `buf.append(&text);`。同时把 `OutputView::write` 的实现也改为调用 `self.append(text)`（DRY 原则）。

如果选方案 B，需要：
1. 在 `tui/output_view.rs` 加 `impl OutputBuffer { pub(crate) fn append(...) }`
2. 重构 `OutputView::write` 调用 `self.inner.lock().unwrap().append(&text)`
3. `handle_submit` 调用 `buf.append(&text)`

- [ ] **Step 3.4: 验证默认构建（无 feature）基线**

```powershell
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：通过；218 passed; 3 failed（基线保持）。

- [ ] **Step 3.5: 验证 feature 构建测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：264 passed; 3 failed（基线保持）。

- [ ] **Step 3.6: 提交**

```powershell
$msg = @"
feat(tui): wire StatusEmitter to drive OutputView + StatusBarState in real time

TuiApp::handle_submit constructs a StatusEmitter closure that:
- Writes TextDelta events into OutputView's ring buffer (via shared_handle)
- Accumulates Usage deltas into StatusBarState.turn_usage
- Calls reset_turn on StreamStart, finish_turn on MessageStop

The emitter is injected into LiveCli via set_status_emitter, which
prepare_turn_runtime forwards to the freshly-built AnthropicRuntimeClient.
This eliminates Phase 1 MVP's "after-turn sync" approximation — status
bar now updates live during streaming.

Also adds OutputBuffer::append() helper to keep ring-buffer trimming
logic DRY between OutputView::write and the emitter callback.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/tui/app.rs rust/crates/rusty-claude-cli/src/tui/output_view.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 4: 单元测试 — 验证 emitter 闭包正确更新 OutputView + StatusBarState

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/app.rs`（在文件末尾追加 `#[cfg(test)] mod tests`）

- [ ] **Step 4.1: 在 `tui/app.rs` 末尾追加测试模块**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::{StatusEmitter, StatusEvent};
    use crate::tui::output_view::OutputView;
    use crate::tui::status_bar::StatusBarState;
    use runtime::TokenUsage;
    use std::sync::{Arc, Mutex};

    /// Build an emitter identical to the one handle_submit constructs, for
    /// direct testing without spinning up a full LiveCli.
    fn build_test_emitter(
        output_handle: Arc<Mutex<crate::tui::output_view::OutputBuffer>>,
        status_handle: Arc<Mutex<StatusBarState>>,
    ) -> StatusEmitter {
        Arc::new(move |event: StatusEvent| {
            match event {
                StatusEvent::TextDelta(text) => {
                    if let Ok(mut buf) = output_handle.lock() {
                        buf.append(&text);
                    }
                }
                StatusEvent::Usage(usage) => {
                    if let Ok(mut guard) = status_handle.lock() {
                        guard.turn_usage.input_tokens += usage.input_tokens;
                        guard.turn_usage.output_tokens += usage.output_tokens;
                        guard.turn_usage.cache_creation_input_tokens +=
                            usage.cache_creation_input_tokens;
                        guard.turn_usage.cache_read_input_tokens +=
                            usage.cache_read_input_tokens;
                    }
                }
                StatusEvent::StreamStart => {
                    if let Ok(mut guard) = status_handle.lock() {
                        guard.reset_turn();
                    }
                }
                StatusEvent::MessageStop => {
                    if let Ok(mut guard) = status_handle.lock() {
                        if guard.streaming {
                            guard.finish_turn();
                        }
                    }
                }
                StatusEvent::ToolUse { .. } => {}
            }
        })
    }

    #[test]
    fn emitter_textdelta_appends_to_output_view() {
        let mut output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::TextDelta("Hello ".to_string()));
        emitter(StatusEvent::TextDelta("world!".to_string()));

        assert_eq!(output_view.snapshot(), "Hello world!");
    }

    #[test]
    fn emitter_usage_accumulates_into_turn_usage() {
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        let usage1 = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        let usage2 = TokenUsage {
            input_tokens: 200,
            output_tokens: 75,
            ..Default::default()
        };
        emitter(StatusEvent::Usage(usage1));
        emitter(StatusEvent::Usage(usage2));

        let guard = status.lock().unwrap();
        assert_eq!(guard.turn_usage.input_tokens, 300);
        assert_eq!(guard.turn_usage.output_tokens, 125);
    }

    #[test]
    fn emitter_streamstart_then_messagestop_folds_turn_into_cumulative() {
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        emitter(StatusEvent::StreamStart);
        {
            let guard = status.lock().unwrap();
            assert!(guard.streaming);
        }

        let usage = TokenUsage {
            input_tokens: 500,
            output_tokens: 250,
            ..Default::default()
        };
        emitter(StatusEvent::Usage(usage));

        emitter(StatusEvent::MessageStop);
        {
            let guard = status.lock().unwrap();
            assert!(!guard.streaming);
            assert_eq!(guard.cumulative_usage.input_tokens, 500);
            assert_eq!(guard.cumulative_usage.output_tokens, 250);
            assert_eq!(guard.turn_usage.total_tokens(), 0);
        }
    }

    #[test]
    fn emitter_does_not_panic_when_lock_contended() {
        // Verify the emitter doesn't panic if a lock is held during callback
        let output_view = OutputView::new();
        let handle = output_view.shared_handle();
        let status = StatusBarState::shared();
        let emitter = build_test_emitter(handle, status);

        // Holding the status lock should not cause emitter to deadlock —
        // it uses .lock().expect(...), so it would block, not skip. But
        // since we're in the same thread, holding the lock then calling
        // emitter would deadlock. Instead, just verify no panic when
        // emitter is called without contention.
        emitter(StatusEvent::StreamStart);
        emitter(StatusEvent::TextDelta("safe".to_string()));
        emitter(StatusEvent::MessageStop);
    }
}
```

**注意**：测试中 `crate::tui::output_view::OutputBuffer` 需要是 `pub(crate)` — 如果不是，需要升级。检查 `tui/output_view.rs` 中 `struct OutputBuffer` 的可见性。当前是私有（`struct OutputBuffer {`）— 需要改为 `pub(crate) struct OutputBuffer`。

- [ ] **Step 4.2: 升级 `OutputBuffer` 为 `pub(crate)`**

修改 `tui/output_view.rs` 中 `struct OutputBuffer` 的定义：

```rust
#[derive(Debug, Default)]
pub(crate) struct OutputBuffer {
    buffer: String,
    total_written: u64,
    truncated: bool,
}
```

同时确保 `append` 方法是 `pub(crate)`：

```rust
impl OutputBuffer {
    pub(crate) fn append(&mut self, text: &str) {
        // ... existing impl
    }

    // For test access (read-only):
    pub(crate) fn buffer(&self) -> &str {
        &self.buffer
    }

    pub(crate) fn total_written(&self) -> u64 {
        self.total_written
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }
}
```

**重要**：如果 Task 3 中选择方案 B（加 `append` 方法），OutputBuffer 已经需要 pub(crate) 字段或方法 — 这一步只是确保它被声明出来。

- [ ] **Step 4.3: 验证 feature 测试**

```powershell
cd d:\claw-code-src\rust
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 tui::app::tests 2>&1 | Select-String "test result|running|FAILED"
```

预期：4 tests passed, 0 failed。

如果有编译错误：
- E0603: `OutputBuffer` 私有 — 升级为 `pub(crate)`
- E0599: `append` 方法不存在 — 在 Task 3 已添加
- E0433: `crate::tui::output_view::OutputBuffer` 路径 — 检查模块声明

- [ ] **Step 4.4: 验证默认构建基线**

```powershell
cargo check -p rusty-claude-cli 2>&1 | Select-Object -Last 5
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：通过；218 passed; 3 failed（基线保持）。

- [ ] **Step 4.5: 验证 feature 全量测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：264 + 4 = 268 passed; 3 failed。

- [ ] **Step 4.6: 提交**

```powershell
$msg = @"
test(tui): add unit tests for StatusEmitter closure in handle_submit

4 tests verify the emitter closure used by handle_submit correctly:
- Appends TextDelta events to OutputView's buffer
- Accumulates Usage deltas into StatusBarState.turn_usage
- Folds turn_usage into cumulative_usage on StreamStart→MessageStop
- Does not panic under normal (uncontended) usage

Upgrades OutputBuffer to pub(crate) and adds buffer()/total_written()/
truncated() read accessors so tests can verify internal state.
"@
$msgFile = New-TemporaryFile
[System.IO.File]::WriteAllText($msgFile.FullName, $msg)
git -C d:\claw-code-src add rust/crates/rusty-claude-cli/src/tui/app.rs rust/crates/rusty-claude-cli/src/tui/output_view.rs
git -C d:\claw-code-src commit -F $msgFile.FullName
Remove-Item $msgFile
```

---

## Task 5: 最终验证 + Phase 2 完成

- [ ] **Step 5.1: 默认 workspace 全量检查**

```powershell
cd d:\claw-code-src\rust
cargo check --workspace 2>&1 | Select-Object -Last 5
```

预期：通过（仅 pre-existing `SESSION_SEARCH_TOOL_SPEC` 警告）。

- [ ] **Step 5.2: feature workspace 全量检查**

```powershell
cargo check --workspace --features rusty-claude-cli/full-tui 2>&1 | Select-Object -Last 5
```

预期：通过。

- [ ] **Step 5.3: 默认构建测试基线**

```powershell
cargo test -p rusty-claude-cli --bin claw -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：218 passed; 3 failed（基线保持）。

- [ ] **Step 5.4: feature 构建测试**

```powershell
cargo test -p rusty-claude-cli --bin claw --features full-tui -- --test-threads=1 2>&1 | Select-String "test result"
```

预期：268 passed; 3 failed（原 264 + 4 新 emitter 测试）。

- [ ] **Step 5.5: 体积检查**

```powershell
$files = @("main.rs","app.rs","streaming.rs","commands_handler.rs","tui/mod.rs","tui/app.rs","tui/status_bar.rs","tui/slash_menu.rs","tui/input_line.rs","tui/output_view.rs")
foreach ($f in $files) { $p = "d:\claw-code-src\rust\crates\rusty-claude-cli\src\$f"; if (Test-Path $p) { $lines = (Get-Content $p | Measure-Object -Line).Lines; Write-Host "$f : $lines lines" } }
```

预期：
- `streaming.rs`: ~1010 行（+17 行 set_status_emitter）
- `app.rs`: ~2035 行（+17 行字段+setter+透传）
- `tui/app.rs`: ~360 行（+96 行新 handle_submit + 4 测试）
- `tui/output_view.rs`: ~155 行（+22 行 append + 访问器）

- [ ] **Step 5.6: 手动 smoke test（可选）**

```powershell
cargo build -p rusty-claude-cli --features full-tui 2>&1 | Select-Object -Last 3
.\target\debug\claw --tui
```

预期行为（与 Phase 1 相比，改进点）：
- 输入 prompt → 提交 → 状态栏的 token 计数应该**实时增长**（不再等到回合结束才跳变）
- streaming 进行时 `⏱ Xs` 计时器可见
- Output 区域应显示模型文本（来自 TextDelta，未经 markdown 渲染）
- 回合完成后状态栏的 cost 应正确反映 cumulative_usage

（手动验证，无需自动化）

- [ ] **Step 5.7: Phase 2 完成 — 查看 git log**

```powershell
git log --oneline -8
```

应该看到 Phase 2 的 4 个新 commits（Task 1-4）。

---

## 实施顺序总结

1. **Task 1**: streaming.rs 加 `set_status_emitter(&mut self, ...)` setter — 独立
2. **Task 2**: app.rs LiveCli 加字段+setter+prepare_turn_runtime 透传 — 依赖 Task 1
3. **Task 3**: tui/app.rs handle_submit 重写 + output_view.rs 加 append — 依赖 Task 2
4. **Task 4**: tui/app.rs 加测试 + output_view.rs 升级 pub(crate) — 依赖 Task 3
5. **Task 5**: 最终验证

Task 1 和 Task 2 可以并行（修改不同文件，但 Task 2 引用 Task 1 的 set_status_emitter 方法 — 实际上需要 Task 1 先完成，或并行但 Task 2 的测试要等 Task 1 落地）。

**推荐执行顺序**：Task 1 → Task 2 → Task 3 → Task 4 → Task 5（串行，因为依赖链清晰）。

---

## 已知限制（Phase 3 候选）

1. **ToolUse 事件未渲染到 TUI**：emitter 回调收到 `StatusEvent::ToolUse { id, name }` 但当前忽略。未来可渲染工具调用卡片。
2. **thinking blocks 未显示**：consume_stream 内 `render_thinking_block_summary` 写到 stdout，TUI 模式下不可见。需要额外 StatusEvent variant 或类似。
3. **markdown 未渲染**：TUI 显示的是原始 TextDelta（纯文本），consume_stream 内的 markdown_stream 渲染仍写到 stdout（不可见）。未来需要把渲染逻辑搬到 TUI 侧。
4. **emit_output 已通过 tui_mode flag 控制**：TUI 模式下 `set_tui_mode(true)` 让 `run_turn` 内 `prepare_turn_runtime(false)` 走 `emit_output=false` 路径，consume_stream 的 `out` 指向 `io::sink()`，不再写 stdout。所有内容通过 emitter 的 TextDelta 回调捕获。

---
