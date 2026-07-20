# TUI 斜杠命令输出捕获迁移计划

**创建日期**: 2026-07-20
**状态**: 进行中（Phase 1 已完成，Phase 2-4 待办）
**相关文件**:
- [crates/rusty-claude-cli/src/app.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/app.rs) — `LiveCli::handle_repl_command`
- [crates/rusty-claude-cli/src/tui/app.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/app.rs) — `execute_slash_command` + Submit 分支路由
- [crates/rusty-claude-cli/src/format.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/format.rs) — `render_repl_help` 等格式化函数

---

## 背景与问题

CLI REPL 的 `run_repl` 流程：
1. `SlashCommand::parse(&trimmed)` 解析斜杠命令
2. 是斜杠命令 → `cli.handle_repl_command(command)` 本地处理
3. 不是 → `cli.run_turn(&input)` 发给 AI

TUI 模式下，斜杠命令已在 Submit 分支正确路由到 `execute_slash_command → handle_repl_command`（修复了"输入 `/help` 发送给 AI"的 bug）。但 `handle_repl_command` 内部大部分分支仍直接调用 `println!`，在 alternate screen 模式下输出会被 TUI 的 `draw` 调用立即覆盖，导致用户看不到命令输出。

## 解决方案架构

引入 **TUI 输出捕获** 机制：

```rust
// LiveCli 新增字段（feature-gated）
#[cfg(feature = "full-tui")]
tui_output: Option<Arc<Mutex<OutputBuffer>>>,

// TUI 感知的 println helper
fn tui_println(&self, msg: &str) -> bool {
    if let Some(handle) = &self.tui_output {
        if let Ok(mut buf) = handle.lock() {
            buf.append(msg);
            buf.append("\n");
        }
        true
    } else {
        false
    }
}
```

`handle_repl_command` 中每个 `println!` 分支改造为：

```rust
if !self.tui_println(&output) {
    println!("{output}");
}
```

## 命令分类清单

### ✅ Phase 1：已支持 TUI 输出（10 项）

这些命令已在 `handle_repl_command` 中用 `tui_println` 捕获输出到 OutputBuffer，TUI 下可见。

| 命令 | 说明 | 实现方式 |
|---|---|---|
| `/help` | 显示所有命令列表 | `render_repl_help()` → tui_println |
| `/search <query>` | 搜索对话历史 | 本地构造结果字符串 → tui_println |
| `/undo` | 撤销最近文件编辑 | `undo_last_file_edit` 返回值 → tui_println |
| `/stats` | 用量统计报告 | `format_cost_report` → tui_println |
| `/poor [action]` | 穷人模式切换 | `handle_poor_mode_action` → tui_println |
| `/goal [args]` | Goal 管理 | `handle_goal_command` → tui_println |
| `/bg [args]` | 后台任务查询 | `handle_bg_command` → tui_println |
| `/output-style [style]` | 输出样式设置 | 本地构造消息 → tui_println |
| 未知命令 | 错误提示 | `format_unknown_slash_command` → tui_println |
| 未实现命令（约 30 项） | "not yet implemented" 提示 | 本地构造消息 → tui_println |

**未实现命令列表**（共享同一分支）：`/login` `/logout` `/vim` `/upgrade` `/share` `/feedback` `/files` `/fast` `/exit` `/summary` `/desktop` `/brief` `/advisor` `/stickers` `/insights` `/thinkback` `/release-notes` `/security-review` `/keybindings` `/privacy-settings` `/plan` `/review` `/tasks` `/theme` `/voice` `/usage` `/rename` `/copy` `/hooks` `/context` `/color` `/effort` `/branch` `/rewind` `/ide` `/tag` `/add-dir`

---

### ⏳ Phase 2：未支持 TUI 输出 — 简单 println 改造（优先级 P1）

这些命令在 `handle_repl_command` 中直接 `println!`，改造方式：**直接替换为 `tui_println`**。

| 命令 | 当前实现 | 改造难度 |
|---|---|---|
| `/doctor` | `println!("{}", render_doctor_report()?.render())` | 简单（1 行替换） |

**改造模板**：

```rust
SlashCommand::Doctor => {
    let report = render_doctor_report()?.render();
    if !self.tui_println(&report) {
        println!("{report}");
    }
    false
}
```

---

### ⏳ Phase 3：未支持 TUI 输出 — 调用 `Self::print_xxx()` 关联方法（优先级 P2）

这些命令调用 `Self::print_xxx()` 静态方法，方法内部多处 `println!`。需要重构方法签名，**返回 `String`** 而非直接 println，再由调用方决定输出到哪。

| 命令 | 当前调用 | 改造难度 | 备注 |
|---|---|---|---|
| `/sandbox` | `Self::print_sandbox_status()` | 中 | 改为 `Self::sandbox_status() -> String` |
| `/config [section]` | `Self::print_config(section)?` | 中 | 改为 `Self::config_text(section) -> Result<String>` |
| `/mcp [action]` | `Self::print_mcp(args, Text)?` | 中 | 已有 `handle_mcp_slash_command_json`，可复用 |
| `/memory` | `Self::print_memory()?` | 中 | 改为 `Self::memory_text() -> Result<String>` |
| `/diff` | `Self::print_diff()?` | 中 | 改为 `Self::diff_text() -> Result<String>` |
| `/version` | `Self::print_version(Text)` | 简单 | 已有 `version_json`，加 `version_text` |
| `/agents [args]` | `Self::print_agents(args, Text)?` | 中 | 已有 `print_agents_json`，可仿照 |
| `/skills [args]` | `Self::print_skills(args, Text)?` | 中 | 已有 `print_skills_json`，可仿照 |

**改造模板**（以 `/sandbox` 为例）：

```rust
// 1. 重命名关联方法，返回 String
fn sandbox_status_text() -> String {
    // 原来的 print_sandbox_status 逻辑，但 collect 到 String
}

// 2. 保留原 println 包装方法（CLI 兼容）
fn print_sandbox_status() {
    println!("{}", Self::sandbox_status_text());
}

// 3. handle_repl_command 中调用
SlashCommand::Sandbox => {
    let text = Self::sandbox_status_text();
    if !self.tui_println(&text) {
        println!("{text}");
    }
    false
}
```

---

### ⏳ Phase 4：未支持 TUI 输出 — 调用 `self.xxx()` 实例方法（优先级 P3）

这些命令调用 `self.xxx()` 方法，方法内部可能涉及复杂的副作用（写文件、调 API、修改运行时状态），改造需谨慎。

| 命令 | 当前调用 | 副作用 | 改造难度 |
|---|---|---|---|
| `/status` | `self.print_status()` | 无（纯查询） | 中 |
| `/cost` | `self.print_cost()` | 无（纯查询） | 中 |
| `/history [count]` | `self.print_prompt_history(count)` | 无（纯查询） | 中 |
| `/compact` | `self.compact()?` | 修改会话历史 | 中（输出+副作用分离） |
| `/clear [--confirm]` | `self.clear_session(confirm)?` | 重置会话 | 中（输出+副作用分离） |
| `/model [model]` | `self.set_model(model)?` | 修改运行时配置 | 中 |
| `/permissions [mode]` | `self.set_permissions(mode)?` | 修改运行时配置 | 中 |
| `/init` | `run_init(Text)?` | 写 CLAUDE.md 文件 | 中 |
| `/export [path]` | `self.export_session(path)?` | 写文件 | 中 |
| `/resume <path>` | `self.resume_session(path)?` | 加载会话 | 中 |
| `/session <action>` | `self.handle_session_command(...)?` | 多种（list/switch/delete） | 高（子命令分发） |
| `/plugins <action>` | `self.handle_plugins_command(...)?` | 多种 | 高（子命令分发） |
| `/debug-tool-call` | `self.run_debug_tool_call(None)?` | 重放工具调用 | 高 |
| `/teleport <target>` | `Self::run_teleport(target)?` | 跳转符号 | 高 |

**改造模板**（以 `/status` 为例）：

```rust
// 1. 拆分 print_status 为 status_text + print_status
fn status_text(&self) -> String {
    let cumulative = self.runtime.usage().cumulative_usage();
    let latest = self.runtime.usage().current_turn_usage();
    format_status_report(/* ... */)
}

fn print_status(&self) {
    println!("{}", self.status_text());
}

// 2. handle_repl_command 中调用
SlashCommand::Status => {
    let text = self.status_text();
    if !self.tui_println(&text) {
        println!("{text}");
    }
    false
}
```

---

### 🔀 特殊：流式命令（部分已捕获）

这些命令实际会触发 AI 对话流（`self.run_turn()`），输出通过 `StatusEmitter::TextDelta` 已被 TUI 捕获到 OutputBuffer。**这部分命令的输出在 TUI 下已可见**，但需要验证。

| 命令 | 当前调用 | TUI 输出状态 |
|---|---|---|
| `/bughunter [scope]` | `self.run_bughunter(scope)?` | 已捕获（通过 run_turn） |
| `/commit` | `self.run_commit(None)?` | 已捕获（通过 run_turn） |
| `/pr [context]` | `self.run_pr(context)?` | 已捕获（通过 run_turn） |
| `/issue [context]` | `self.run_issue(context)?` | 已捕获（通过 run_turn） |
| `/ultraplan [task]` | `self.run_ultraplan(task)?` | 已捕获（通过 run_turn） |
| `/skills invoke <skill>` | `self.run_turn(&prompt)?` | 已捕获 |

**待办**：实测验证这些命令在 TUI 下的输出完整性，特别是工具调用卡片（ToolUse/ToolResult）是否正确显示。

---

## 改造进度跟踪

| Phase | 命令数 | 状态 | 完成日期 |
|---|---|---|---|
| Phase 1（已支持） | 10 项 | ✅ 完成 | 2026-07-20 |
| Phase 2（简单 println） | 1 项 | ⏳ 待办 | - |
| Phase 3（关联方法重构） | 8 项 | ⏳ 待办 | - |
| Phase 4（实例方法重构） | 14 项 | ⏳ 待办 | - |
| 特殊（流式命令验证） | 6 项 | ⏳ 待验证 | - |

## 改造优先级建议

1. **P1（立即）**: Phase 2 — `/doctor` 一行替换即可
2. **P2（短期）**: Phase 3 中用户高频命令 — `/version` `/sandbox` `/diff` `/memory`
3. **P3（中期）**: Phase 3 剩余 + Phase 4 查询类 — `/config` `/mcp` `/agents` `/skills` `/status` `/cost` `/history`
4. **P4（长期）**: Phase 4 副作用类 — `/clear` `/compact` `/model` `/permissions` `/session` `/plugins`
5. **P5（验证）**: 特殊流式命令 — 实测 `/bughunter` `/commit` `/pr` `/issue` `/ultraplan` 在 TUI 下输出完整性

## 测试验证清单

每完成一项改造，需验证：

- [ ] CLI 模式（非 TUI）：输出仍正确显示在 stdout
- [ ] TUI 模式：输出显示在 OutputView 输出区
- [ ] TUI 模式：长输出能上下滚动（`Up`/`Down`/`PgUp`/`PgDn`）
- [ ] TUI 模式：Markdown 渲染正常（标题、代码块、列表）
- [ ] TUI 模式：命令执行后 alternate screen 无损坏
- [ ] 单元测试：`cargo test -p rusty-claude-cli --features full-tui` 通过

## 已知限制

1. **`tui_println` 是同步追加**：长输出（如 `/doctor` 完整报告）会一次性写入 OutputBuffer，不流式显示。当前可接受，未来可考虑分块追加。
2. **Markdown 渲染**：OutputView 通过 `TerminalRenderer::markdown_to_ansi` + `ansi_to_tui::IntoText` 渲染。部分命令输出非 Markdown 格式（如 `/sandbox` 纯文本），渲染为普通段落，不影响可读性。
3. **子命令分发**：`/session` `/plugins` 等多子命令的改造需逐个子命令处理，工作量较大。
