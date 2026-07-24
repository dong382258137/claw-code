# Claw Plus 设计文档实现验证清单

> **用途**：让 claw.exe 内的 LLM 模型逐项验证设计文档承诺的功能是否真正实现，是否存在 BUG。
>
> **使用方式**：每个验证项包含「来源文档」「验证方法」「预期行为」「通过判定」四要素。LLM 在 claw.exe 内逐项执行后，按 PASS / FAIL / BUG / SKIP 标注结果并附实际观测。
>
> **完成要求**：所有 P0/P1 项必须 PASS 或显式标注 BUG 并附根因；P2/P3 可标注 DEFER 但需记录原因。

---

## 验证结果汇总表

| 组 | 项目数 | PASS | FAIL | BUG | SKIP | 进度 |
|---|---|---|---|---|---|---|
| G1 CLI 命令与 flag | 22 | 0 | 0 | 0 | 0 | 0% |
| G2 TUI 模式行为 | 30 | 0 | 0 | 0 | 0 | 0% |
| G3 Slash 命令 | 18 | 0 | 0 | 0 | 0 | 0% |
| G4 Provider 路由与模型兼容 | 16 | 0 | 0 | 0 | 0 | 0% |
| G5 工具系统 (40 tools) | 14 | 0 | 0 | 0 | 0 | 0% |
| G6 会话/状态/分支恢复 | 14 | 0 | 0 | 0 | 0 | 0% |
| G7 安全/权限/沙箱 | 12 | 0 | 0 | 0 | 0 | 0% |
| G8 多 agent / DAG / Plan | 12 | 0 | 0 | 0 | 0 | 0% |
| G9 Hooks / Plugin / MCP | 14 | 0 | 0 | 0 | 0 | 0% |
| G10 已知 BUG 复核（17 项） | 17 | 0 | 0 | 0 | 0 | 0% |
| G11 测试套件基线 | 10 | 0 | 0 | 0 | 0 | 0% |
| G12 文档/构建/发布产物 | 10 | 0 | 0 | 0 | 0 | 0% |
| **总计** | **189** | **0** | **0** | **0** | **0** | **0%** |

---

## G1 — CLI 命令与 flag 验证（顶层行为契约）

**来源文档**：`README.md`、`USAGE.md`、`rust/README.md`、`rust/PARITY.md`、`docs/windows-install-release.md`、`docs/g011-acp-json-rpc-status-contract.md`

### G1.1 `claw doctor` 是顶层 CLI 入口
- **验证方法**：在 `d:\claw-code-src\rust` 运行 `cargo run -p rusty-claude-cli -- doctor --output-format json`
- **预期行为**：退出码 0；stdout 输出 JSON，含 `schema_version`、`kind: "doctor"`、`checks[]` 数组（每项含 `name`/`status`/`summary`/`details`）
- **通过判定**：退出码 0 且 JSON 结构匹配

### G1.2 `claw --help` 渲染干净（无 warning wall）
- **验证方法**：运行 `cargo run -q -p rusty-claude-cli -- --help`
- **预期行为**：stderr 无 warning；stdout 输出 canonical help text；列出 `prompt`/`help`/`version`/`status`/`sandbox`/`acp`/`dump-manifests`/`bootstrap-plan`/`agents`/`mcp`/`skills`/`system-prompt`/`init`/`doctor` 14 个子命令
- **通过判定**：stderr 为空（或仅含 cargo 编译信息）；14 个子命令齐全

### G1.3 ACP 状态查询契约
- **验证方法**：依次运行 `claw acp`、`claw acp serve`、`claw --acp`、`claw -acp`、`claw acp --output-format json`、`claw acp serve --output-format json`
- **预期行为**：6 种调用全部退出码 0；JSON 输出含 `schema_version: "1.0"`、`kind: "acp"`、`status: "unsupported"`、`phase: "discoverability_only"`、`supported: false`、`serve_alias_only: true`、`protocol.json_rpc: false`、`protocol.daemon: false`
- **通过判定**：6/6 调用退出码 0 且字段匹配

### G1.4 ACP malformed invocation 拒绝
- **验证方法**：运行 `claw acp start`
- **预期行为**：退出码 1；stderr 输出 `{"type":"error","kind":"unsupported_acp_invocation","exit_code":1}`
- **通过判定**：退出码 1 且 error kind 匹配

### G1.5 `--output-format json` 全诊断动词支持
- **验证方法**：依次运行 `claw doctor --output-format json`、`claw status --output-format json`、`claw sandbox --output-format json`、`claw version --output-format json`、`claw config --output-format json`
- **预期行为**：5 个动词全部退出码 0；输出结构化 JSON（非 prose 表）；每项含 `schema_version` 和 `kind` 字段
- **通过判定**：5/5 命令 JSON 输出有效

### G1.6 `--json` 后缀必须被拒绝
- **验证方法**：运行 `claw doctor --json`
- **预期行为**：parse 阶段错误；exit 非 0；不 fall through 到 prompt 派发
- **通过判定**：命令被拒绝且报错明确

### G1.7 `claw init` 幂等性
- **验证方法**：在临时空目录运行 `claw init`，记录输出；再次运行 `claw init`
- **预期行为**：第二次运行时已存在文件标记为 `skipped`；JSON 输出含 `created[]`/`updated[]`/`skipped[]`/`artifacts[]`/`message`
- **通过判定**：第二次运行无文件被覆盖（仅 skipped）

### G1.8 `claw state` 读取 worker-state
- **验证方法**：在无 `.claw/worker-state.json` 的工作目录运行 `claw state`；在运行过 `claw prompt "hi"` 后再运行 `claw state --output-format json`
- **预期行为**：无文件时返回结构化错误含 hint `error: no worker state file found at .claw/worker-state.json`；有文件时 JSON 输出含 `status`/`is_ready`/`seconds_since_update`/`trust_gate_cleared`/`last_event`/`updated_at`
- **通过判定**：两种场景行为均符合

### G1.9 模型别名解析
- **验证方法**：运行 `claw --model sonnet prompt "say hi"`（需配 Anthropic 凭据）；运行 `claw --model opus --help`、`claw --model haiku --help`
- **预期行为**：`opus` → `claude-opus-4-6`；`sonnet` → `claude-sonnet-4-6`；`haiku` → `claude-haiku-4-5-20251213`
- **通过判定**：别名被正确展开（可通过 `--output-format json status` 查看 resolved model）

### G1.10 三种权限模式
- **验证方法**：依次运行 `claw --permission-mode read-only --help`、`claw --permission-mode workspace-write --help`、`claw --permission-mode danger-full-access --help`
- **预期行为**：3 个值都被接受；其他值（如 `--permission-mode foo`）被拒绝
- **通过判定**：3 个有效值通过，无效值报错

### G1.11 `--add-dir` 多根 workspace
- **验证方法**：运行 `claw --add-dir /tmp --add-dir /var --help` 和 `claw --add-dir=/tmp --help`
- **预期行为**：repeatable flag；两种语法都接受
- **通过判定**：两种语法都通过

### G1.12 `--reasoning-effort` flag
- **验证方法**：运行 `claw --reasoning-effort low --help`、`claw --reasoning-effort medium --help`、`claw --reasoning-effort high --help`、`claw --reasoning-effort foo --help`
- **预期行为**：3 个有效值通过；`foo` 被拒绝
- **通过判定**：见预期

### G1.13 OutputVerbosity 控制
- **验证方法**：依次运行 `claw --verbose --help`、`claw --quiet --help`、`claw --silent --help`、`claw --output-verbosity=minimal --help`
- **预期行为**：4 种 flag/值均被接受；`OutputVerbosity` 枚举值 `Full`/`Compact`/`Silent`/`Minimal` 对应
- **通过判定**：4/4 接受

### G1.14 `--manifests-dir` flag
- **验证方法**：运行 `claw dump-manifests --manifests-dir /tmp/manifests`
- **预期行为**：接受 flag；pre-check `src/commands.ts`/`src/tools.ts`/`src/entrypoints/cli.tsx` 存在性
- **通过判定**：命令执行且生成 manifests

### G1.15 stdin pipe → Prompt 模式
- **验证方法**：运行 `echo "hello" | claw`（无其他 args）
- **预期行为**：dispatch 为 `CliAction::Prompt`，非 REPL
- **通过判定**：进程一次性消费 stdin 并退出（非进入 REPL）

### G1.16 `claw skills` 命令
- **验证方法**：运行 `claw skills --output-format json`
- **预期行为**：JSON 输出含 `installed[]` skills 列表，每项有 `name`/`description`
- **通过判定**：JSON 有效

### G1.17 `claw agents` 命令
- **验证方法**：运行 `claw agents --output-format json`
- **预期行为**：JSON 输出 agent 清单
- **通过判定**：JSON 有效

### G1.18 `claw mcp` 命令
- **验证方法**：运行 `claw mcp --output-format json`
- **预期行为**：JSON 输出 MCP server 状态
- **通过判定**：JSON 有效

### G1.19 已移除的 login/logout
- **验证方法**：运行 `claw login` 和 `claw logout`
- **预期行为**：均报错"已移除"或类似 helpful error；不进入 OAuth 流程
- **通过判定**：2/2 命令 helpful error

### G1.20 broad-CWD 警告
- **验证方法**：在 `$HOME` 或 fs root 运行 `claw --help`（不带 `--allow-broad-cwd`）
- **预期行为**：warning 提示 broad CWD；带 `--allow-broad-cwd` 时无 warning
- **通过判定**：警告出现/消失符合预期

### G1.21 `claw dump-manifests` 默认行为
- **验证方法**：运行 `claw dump-manifests`（不带 `--manifests-dir`）
- **预期行为**：使用默认 manifests 目录；不报错
- **通过判定**：命令成功执行

### G1.22 typed-error envelope contract
- **验证方法**：运行 `claw export --output /tmp/nonexistent/dir/out.md --output-format json`
- **预期行为**：JSON 错误输出含 `error.kind: "filesystem"`、`error.operation: "write"`、`error.target`、`error.errno: "ENOENT"`、`error.hint`、`error.retryable: true`、`type: "error"`
- **通过判定**：所有 6 个字段存在且值匹配

---

## G2 — TUI 模式行为验证（已知 BUG 重灾区）

**来源文档**：`rust/TUI-ENHANCEMENT-PLAN.md`、`rust/.omc/plans/2026-07-19-tui-phase1-ratatui.md`、`rust/.omc/plans/2026-07-20-tui-phase2-realtime-streaming.md`、`rust/.omc/plans/2026-07-20-tui-status-report-corrected.md`、`rust/.omc/plans/2026-07-20-code-review-fix-plan.md`

### G2.1 `--tui` flag 触发 alternate screen
- **验证方法**：运行 `claw --tui`（需配凭据）；观察是否进入全屏 TUI
- **预期行为**：进入 alternate screen；顶部 Output 区、底部 Input、最底 StatusBar 三段布局
- **通过判定**：三段布局可见

### G2.2 `--tui` 缺 `full-tui` feature 报错
- **验证方法**：用默认构建（不带 feature）运行 `claw --tui`
- **预期行为**：报错 `error: --tui requires the `full-tui` Cargo feature.\nRebuild with: cargo build --release --features full-tui`；exit 1
- **通过判定**：错误信息和 exit code 匹配

### G2.3 StatusBar 段顺序与内容
- **验证方法**：进入 TUI 模式后观察 StatusBar
- **预期行为**：段顺序 `│ model via provider │ 📁 cwd │ 🌿 branch │ 🔢 tokens tok │ 💰 $cost │ ⏱ Xs（仅 streaming 时） │ 🎯 goal_badge（仅非空时） │ 🪙 poor（仅 poor_mode 时） │`
- **通过判定**：所有"常驻"段可见，"条件"段按状态显隐

### G2.4 StatusBar 颜色
- **验证方法**：在 TUI 模式观察 StatusBar 各段颜色
- **预期行为**：model=Cyan+BOLD；provider=Cyan+ITALIC；cwd=DarkGray；branch=Magenta；tokens=Yellow；cost=Green；streaming=Cyan+BOLD；goal_badge 含 `⚠` 时 Yellow 否则 Green；poor=Yellow
- **通过判定**：颜色匹配（截图或 dump 终端输出）

### G2.5 输入 `/` 弹出 SlashMenu
- **验证方法**：TUI 模式下输入 `/`
- **预期行为**：弹出 menu popup 在 input 下方；标题 `Commands ({visible}/{total})`；选中项 `Style::default().fg(Black).bg(Cyan).add_modifier(BOLD)`
- **通过判定**：menu 弹出且选中项高亮

### G2.6 SlashMenu fuzzy 过滤
- **验证方法**：TUI 模式下依次输入 `/`、`/he`
- **预期行为**：`/he` 后列表过滤为含 `help` 的项；selected 重置为 0；scroll 重置为 0
- **通过判定**：过滤行为正确

### G2.7 SlashMenu Up/Down wrap-around
- **验证方法**：TUI 模式输入 `/` 后按 Down 直到底部再按 Down；按 Up 直到顶部再按 Up
- **预期行为**：到底部再按 Down wrap 到 0；到顶部再按 Up wrap 到 len-1
- **通过判定**：双向 wrap 正确

### G2.8 SlashMenu Tab/Enter 接受补全
- **验证方法**：TUI 模式输入 `/he` 后按 Tab；再按 Enter
- **预期行为**：Tab 接受补全为 `/help`，cursor 置末，关闭 menu；Enter 在 menu 关闭时提交 buffer
- **通过判定**：补全行为正确；需第二次 Enter 才执行命令（不自动提交）

### G2.9 SlashMenu Backspace 关闭
- **验证方法**：TUI 模式输入 `/he` 后按 Backspace 直到删除 `/`
- **预期行为**：删到 `/` 时 `menu_open=false`，menu 关闭
- **通过判定**：menu 关闭时机正确

### G2.10 Esc 行为分支
- **验证方法**：TUI 模式输入 `/he` 后按 Esc（menu 开）；清空 buffer 后按 Esc（menu 关闭且 buffer 空）
- **预期行为**：menu 开时 Esc 关闭 menu；menu 关闭且 buffer 空时 Esc 退出 TUI；menu 关闭且 buffer 非空时 Esc 清空 buffer
- **通过判定**：3 种场景行为符合

### G2.11 Ctrl+C/Ctrl+D 退出
- **验证方法**：TUI 模式按 Ctrl+C；重启后按 Ctrl+D
- **预期行为**：均退出 TUI；恢复终端状态（raw mode、alternate screen、mouse capture）
- **通过判定**：退出后终端正常（无残留 alternate screen 或 raw mode）

### G2.12 OutputBuffer 64KB ring buffer
- **验证方法**：grep `MAX_BUFFER_BYTES` in `tui/output_view.rs`；运行长输出（如 `read_file` 大文件）观察是否截断
- **预期行为**：常量 `MAX_BUFFER_BYTES = 64 * 1024`；超出时 split_off 保留最新部分；`truncated=true`
- **通过判定**：常量存在且截断行为正确

### G2.13 流式 TextDelta 实时显示
- **验证方法**：TUI 模式提交一个 prompt；观察 Output 区
- **预期行为**：模型文本实时增量显示（不再回合后跳变）；来自 `StatusEvent::TextDelta` 调用 `OutputBuffer::append`
- **通过判定**：实时渲染可见

### G2.14 StatusBar token 实时增长
- **验证方法**：TUI 模式提交 prompt；观察 StatusBar `🔢 tokens tok` 段
- **预期行为**：token 计数实时增长（来自 `StatusEvent::Usage` 累加到 `turn_usage`）
- **通过判定**：实时增长可见

### G2.15 StatusBar `⏱ Xs` 计时器
- **验证方法**：TUI 模式提交 prompt；观察 StatusBar streaming 段
- **预期行为**：streaming 时显示 `⏱ Xs`；每 200ms poll 后 +200；streaming 结束后消失
- **通过判定**：计时器在 streaming 期间可见，结束后消失

### G2.16 reset_turn 在 StreamStart
- **验证方法**：grep `StatusEvent::StreamStart` 处理逻辑 in `tui/app.rs`
- **预期行为**：`StreamStart` 触发 `guard.reset_turn()`：设 `streaming=true`、清零 `turn_usage`、清零 `turn_elapsed_ms`
- **通过判定**：逻辑存在且执行

### G2.17 finish_turn 在 MessageStop
- **验证方法**：grep `StatusEvent::MessageStop` 处理逻辑
- **预期行为**：`MessageStop` 触发 `guard.finish_turn()`：fold `turn_usage` 到 `cumulative_usage`；设 `streaming=false`
- **通过判定**：逻辑存在且执行

### G2.18 TUI 模式下 consume_stream 写 io::sink
- **验证方法**：grep `emit_output = !self.tui_mode` in `app.rs`
- **预期行为**：TUI 模式下 `emit_output=false`，stdout 不被污染；TextDelta 通过 emitter 走 OutputBuffer
- **通过判定**：TUI 模式下 alternate screen 不被 stdout 输出破坏

### G2.19 ToolCard 折叠
- **验证方法**：TUI 模式触发工具调用，结果 > 5 行
- **预期行为**：自动折叠；显示 summary + 行数 + `[+] Expand` 提示
- **通过判定**：折叠行为可见

### G2.20 ToolCard 展开/折叠切换
- **验证方法**：TUI 模式折叠的 ToolCard 上按 Ctrl+T 或鼠标左键点击
- **预期行为**：切换展开/折叠状态
- **通过判定**：切换有效

### G2.21 OutputView 滚动
- **验证方法**：TUI 模式下输出多行后按 Up/Down（行滚）、PgUp/PgDn（页滚）
- **预期行为**：垂直滚动有效；auto-follow 模式（滚到底部时跟随新输出）
- **通过判定**：滚动行为正确

### G2.22 `?` 键弹出 help overlay
- **验证方法**：TUI 模式按 `?`
- **预期行为**：弹出 keybindings help overlay；overlay 期间所有输入（除 `?`/`Esc`/`Ctrl+C`/`Ctrl+D`）被阻止
- **通过判定**：overlay 显示且输入被阻止（buffer 不被污染）

### G2.23 Help overlay 是真正模态
- **验证方法**：TUI 模式按 `?` 后输入字符和 Enter
- **预期行为**：字符不写入 input buffer；Enter 不触发提交；`route_key(&mut input, key, help_visible)` 在 help_visible 时短路返回 `InputAction::Ignore`
- **通过判定**：input buffer 在 overlay 期间保持空

### G2.24 Shift+Enter / Ctrl+J 多行输入
- **验证方法**：TUI 模式按 Shift+Enter 或 Ctrl+J
- **预期行为**：插入换行；支持多行输入
- **通过判定**：多行可见

### G2.25 CJK 字符宽度
- **验证方法**：TUI 模式输入中文（如 `你好`）；检查 cursor 位置
- **预期行为**：使用 `unicode-width` 计算光标位置；无错位
- **通过判定**：光标位置正确

### G2.26 bracketed paste（DECSET 2004）
- **验证方法**：TUI 模式粘贴多行内容
- **预期行为**：启用 bracketed paste；多行粘贴作为原子操作
- **通过判定**：粘贴行为正确

### G2.27 Ctrl+V 剪贴板粘贴
- **验证方法**：TUI 模式（conhost 终端）按 Ctrl+V
- **预期行为**：读取剪贴板并插入到 input
- **通过判定**：剪贴板内容被插入

### G2.28 Windows KeyEventKind 过滤
- **验证方法**：grep `KeyEventKind` in `tui/app.rs` 或 `tui/input_line.rs`
- **预期行为**：只处理 `Press`/`Repeat` 事件；忽略 `Release` 防止重复输入
- **通过判定**：过滤逻辑存在

### G2.29 StatusBarState mutex 防中毒
- **验证方法**：grep `unwrap_or_else(|e| e.into_inner())` in `tui/app.rs` 或 `tui/status_bar.rs`
- **预期行为**：StatusBarState mutex 不用 `expect`；用 `into_inner` 恢复 poisoned mutex
- **通过判定**：用 `into_inner` 而非 `expect`

### G2.30 终端状态 Drop 恢复
- **验证方法**：grep `Drop for` in `tui/app.rs`；触发 panic 后检查终端状态
- **预期行为**：Drop guard 恢复 raw mode、alternate screen、mouse capture；即使 panic 也恢复
- **通过判定**：Drop 实现存在

---

## G3 — Slash 命令验证

**来源文档**：`rust/USAGE.md`、`rust/README.md`、`rust/PARITY.md`、`rust/.omc/plans/2026-07-20-tui-slash-command-output-migration.md`、`ROADMAP.md`

### G3.1 `/help` 显示分组与快捷键
- **验证方法**：REPL 内运行 `/help`
- **预期行为**：渲染 grouped categories 与 keyboard shortcuts；不发给 AI
- **通过判定**：输出分组结构，非 AI 响应

### G3.2 `/doctor` REPL 内可用
- **验证方法**：REPL 内运行 `/doctor`
- **预期行为**：执行诊断；输出 doctor report
- **通过判定**：输出诊断信息

### G3.3 `/status`、`/cost`、`/config`、`/memory` REPL 可用
- **验证方法**：REPL 内依次运行 4 个命令
- **预期行为**：4 个命令均本地处理；输出对应 report
- **通过判定**：4/4 输出非 AI 响应

### G3.4 `/ultraplan <task>` 输出结构化计划
- **验证方法**：REPL 内运行 `/ultraplan 重构 file_ops.rs`
- **预期行为**：输出带编号步骤的结构化计划
- **通过判定**：步骤编号可见

### G3.5 `/teleport <symbol-or-path>` 跳转
- **验证方法**：REPL 内运行 `/teleport rust/crates/runtime/src/lib.rs`
- **预期行为**：跳转到指定文件/符号
- **通过判定**：跳转有效

### G3.6 `/bughunter [path]` 扫描
- **验证方法**：REPL 内运行 `/bughunter rust/crates/runtime/src/conversation.rs`
- **预期行为**：扫描常见 bug 模式；输出 `file:line + suggested fix`
- **通过判定**：输出格式匹配

### G3.7 `/skills list/install/<name>` 三个子命令
- **验证方法**：REPL 内运行 `/skills list`；`/skills install /tmp/my-skill`（需先创建 SKILL.md）；`/skills <name>`
- **预期行为**：list 列出已装；install 安装；`<name>` 调用
- **通过判定**：3 个子命令行为正确

### G3.8 `/tokens`、`/cache` 别名到 `/stats`
- **验证方法**：REPL 内运行 `/tokens` 和 `/cache`
- **预期行为**：均路由到 `SlashCommand::Stats`；输出 stats
- **通过判定**：2 个别名有效

### G3.9 `/session list` 在 `--output-format json --resume` 模式
- **验证方法**：运行 `claw --output-format json --resume latest /session list`
- **预期行为**：JSON 输出 `{kind: "session_list", sessions: [...ids], active: <id>}`
- **通过判定**：JSON 结构匹配

### G3.10 `--resume <session>` 无 slash 时输出 restored
- **验证方法**：运行 `claw --output-format json --resume latest`
- **预期行为**：JSON 输出 `{kind: "restored", session_id, path, message_count}`
- **通过判定**：JSON 字段匹配

### G3.11 session load 错误 JSON
- **验证方法**：运行 `claw --output-format json --resume nonexistent`
- **预期行为**：JSON 输出 `{type: "error", error: "failed to restore session: <detail>"}`
- **通过判定**：JSON 错误结构匹配

### G3.12 resumed slash 错误 JSON
- **验证方法**：运行 `claw --output-format json --resume latest /nonexistent`
- **预期行为**：JSON 输出 `{type: "error", error: "...", command: "..."}`
- **通过判定**：JSON 字段含 `command`

### G3.13 STUB_COMMANDS 隐藏 stub 命令
- **验证方法**：运行 `claw --help`；grep `STUB_COMMANDS` in `commands_handler.rs`
- **预期行为**：`/branch`、`/rewind`、`/ide`、`/tag`、`/output-style`、`/add-dir` 等约 23 个 stub 不出现在 `--help` Resume-safe 摘要
- **通过判定**：Resume-safe help 不含 stub

### G3.14 `/effort` 已实现
- **验证方法**：grep `STUB_COMMANDS` 确认不含 `"effort"`；REPL 内运行 `/effort high`
- **预期行为**：`STUB_COMMANDS` 不含 `"effort"`；`/effort` 实际执行（非 stub 提示）
- **通过判定**：`/effort` 不返回 "not yet implemented"

### G3.15 slash_menu 过滤 STUB_COMMANDS
- **验证方法**：grep `SlashMenu::new` in `tui/slash_menu.rs`；统计 `SLASH_COMMAND_SPECS` 总数
- **预期行为**：SlashMenu 加载时过滤 `STUB_COMMANDS`；总数 ≈ 45（145 - 100）
- **通过判定**：TUI menu 不显示 stub

### G3.16 TUI slash 命令输出可见
- **验证方法**：TUI 模式运行 `/help`、`/status`、`/cost` 等
- **预期行为**：输出显示在 OutputView 区；alternate screen 不被破坏
- **通过判定**：输出可见且界面正常

### G3.17 `/login`、`/logout` 已移除
- **验证方法**：REPL 内运行 `/login` 和 `/logout`
- **预期行为**：均报错 helpful error，不进入 OAuth 流程
- **通过判定**：2 个命令 helpful error

### G3.18 `/output-style [level]` 控制 verbosity
- **验证方法**：REPL 内运行 `/output-style compact`；触发工具调用
- **预期行为**：`OutputVerbosity::Compact`；工具结果显示 one-line marker
- **通过判定**：compact 输出格式可见

---

## G4 — Provider 路由与模型兼容性验证

**来源文档**：`docs/MODEL_COMPATIBILITY.md`、`USAGE.md`、`docs/local-openai-compatible-providers.md`、`prd.json` US-008/009/010/023/024

### G4.1 Anthropic 凭据双轨制
- **验证方法**：grep `x-api-key` 和 `Authorization: Bearer` in `api/src/`
- **预期行为**：`sk-ant-*` 走 `ANTHROPIC_API_KEY`（`x-api-key` header）；OAuth/bearer 走 `ANTHROPIC_AUTH_TOKEN`（`Authorization: Bearer`）
- **通过判定**：两种 header 路径均存在

### G4.2 401 + `sk-ant-*` 在 Bearer slot 的修正 hint
- **验证方法**：构造场景：`ANTHROPIC_AUTH_TOKEN=sk-ant-xxx`；触发 API 调用
- **预期行为**：检测到 401 + `sk-ant-*` 在 Bearer slot；附加修正 hint
- **通过判定**：hint 出现

### G4.3 `openai/` 前缀路由
- **验证方法**：设 `OPENAI_API_KEY`；运行 `claw --model "openai/gpt-4.1-mini" prompt "hi"`
- **预期行为**：路由到 OpenAI-compat；`openai/` 前缀被剥离
- **通过判定**：请求实际打到 OpenAI endpoint

### G4.4 自定义 `OPENAI_BASE_URL` 保留 slash slug
- **验证方法**：设 `OPENAI_BASE_URL=https://openrouter.ai/api/v1` + `OPENAI_API_KEY`；运行 `claw --model "openai/gpt-4o" prompt "hi"`
- **预期行为**：`openai/gpt-4o` 保留（不剥离）；打到 OpenRouter
- **通过判定**：模型名保留

### G4.5 DashScope 路由
- **验证方法**：设 `DASHSCOPE_API_KEY`；运行 `claw --model "qwen-max" prompt "hi"` 或 `claw --model "qwen/qwen-max" prompt "hi"`
- **预期行为**：路由到 `https://dashscope.aliyuncs.com/compatible-mode/v1`
- **通过判定**：endpoint 匹配

### G4.6 xAI 路由
- **验证方法**：设 `XAI_API_KEY`；运行 `claw --model "grok-3" prompt "hi"`
- **预期行为**：路由到 `https://api.x.ai/v1`
- **通过判定**：endpoint 匹配

### G4.7 `OPENAI_BASE_URL` + `OPENAI_API_KEY` 优先于 Anthropic env-check
- **验证方法**：同时设 `ANTHROPIC_API_KEY` 和 `OPENAI_BASE_URL`+`OPENAI_API_KEY`；运行 `claw prompt "hi"`（不指定 model）
- **预期行为**：走 OpenAI-compat 路径
- **通过判定**：实际 provider 是 OpenAI-compat

### G4.8 `OPENAI_BASE_URL` 单独（Ollama）是 Anthropic 默认前最后回退
- **验证方法**：仅设 `OPENAI_BASE_URL`（无 `OPENAI_API_KEY`、无 Anthropic 凭据）；运行 `claw prompt "hi"`
- **预期行为**：走 OpenAI-compat 路径（最后回退）
- **通过判定**：实际 provider 是 OpenAI-compat

### G4.9 kimi 别名解析
- **验证方法**：grep `kimi` in `MODEL_REGISTRY`；运行 `claw --model kimi prompt "hi"`（设 `DASHSCOPE_API_KEY`）
- **预期行为**：`kimi` → `kimi-k2.5`；max output 16384；context 256000
- **通过判定**：别名链解析正确

### G4.10 kimi 模型排除 `is_error` 字段
- **验证方法**：grep `model_rejects_is_error_field` in `api/src/providers/openai_compat.rs`；运行 `cargo test --package api model_rejects_is_error_field -- --nocapture`
- **预期行为**：`kimi-k2.5`、`kimi-k1.5`、`kimi-moonshot`、`dashscope/kimi-k2.5` 均被识别；tool result messages 不含 `is_error` 字段
- **通过判定**：测试通过

### G4.11 非 kimi 模型保留 `is_error`
- **验证方法**：运行 `cargo test --package api translate_message_includes_is_error_for_non_kimi_models -- --nocapture`
- **预期行为**：`gpt-4o`、`grok-3`、`claude-*` 等模型 tool result 含 `is_error`
- **通过判定**：测试通过

### G4.12 Reasoning 模型剥离 tuning 参数
- **验证方法**：grep `is_reasoning_model` in `openai_compat.rs`；运行 `cargo test --package api reasoning_model -- --nocapture`
- **预期行为**：`o1`/`o3`/`o4`/`grok-3-mini`/`qwen-qwq-*`/`qwq-*`/`qwen3-*-thinking` 模型剥离 `temperature`/`top_p`/`frequency_penalty`/`presence_penalty`；保留 `reasoning_effort`
- **通过判定**：测试通过

### G4.13 GPT-5 使用 `max_completion_tokens`
- **验证方法**：grep `gpt-5` in `openai_compat.rs`；运行 `cargo test --package api gpt5 -- --nocapture`
- **预期行为**：`gpt-5*` 模型 emit `max_completion_tokens` 而非 `max_tokens`
- **通过判定**：测试通过

### G4.14 `extra_body` 透传与核心字段保护
- **验证方法**：grep `extra_body` in `api/src/types.rs` 和 `openai_compat.rs`；运行 `cargo test --package api custom_openai_gateway_preserves_slash_model_ids_and_extra_body_params -- --nocapture`
- **预期行为**：`MessageRequest::extra_body` 透传 `web_search_options`、`parallel_tool_calls` 等；核心字段 `model`/`messages`/`stream`/`tools`/`tool_choice`/`max_tokens`/`max_completion_tokens` 不可被覆盖
- **通过判定**：测试通过

### G4.15 请求体大小预检
- **验证方法**：grep `estimate_request_body_size` 和 `check_request_body_size` in `api/src/`
- **预期行为**：DashScope 6MB（6_291_456）、OpenAI 100MB（104_857_600）、xAI 50MB（52_428_800）；超限返回 `RequestBodySizeExceeded` 错误
- **通过判定**：函数存在且常量匹配

### G4.16 `model_token_limit` for kimi
- **验证方法**：grep `model_token_limit` in `api/src/`；运行 `cargo test --package api model_token_limit -- --nocapture`（若有）
- **预期行为**：`model_token_limit('kimi-k2.5')` 返回 `Some(ModelTokenLimit { max_output_tokens: 16384, context_window_tokens: 256000 })`；`model_token_limit('kimi')` 走别名链
- **通过判定**：返回值匹配

---

## G5 — 工具系统验证（40 tools）

**来源文档**：`rust/PARITY.md`、`PARITY.md`、`rust/README.md`

### G5.1 `mvp_tool_specs()` 返回 40 个 specs
- **验证方法**：grep `mvp_tool_specs` in `rust/crates/tools/src/lib.rs`；统计返回的 spec 数量
- **预期行为**：40 个 tool specs
- **通过判定**：数量为 40

### G5.2 核心 6 工具实现
- **验证方法**：grep `"bash"`、`"read_file"`、`"write_file"`、`"edit_file"`、`"glob_search"`、`"grep_search"` in `tools/src/lib.rs`
- **预期行为**：6 个工具均有真实 handler（非 stub）
- **通过判定**：6/6 工具有 handler

### G5.3 `edit_file` 含 `replace_all`
- **验证方法**：grep `replace_all` in `tools/src/lib.rs` 和 `runtime/src/file_ops.rs`
- **预期行为**：edit_file 支持 `replace_all` 参数
- **通过判定**：参数存在且生效

### G5.4 `glob_search` 支持 brace expansion
- **验证方法**：grep `expand_braces` in `runtime/src/file_ops.rs` 或 `tools/src/lib.rs`
- **预期行为**：支持 `Assets/**/*.{cs,uxml,uss}` 模式
- **通过判定**：函数存在

### G5.5 `bash` 工具 9 个 validation 子模块
- **验证方法**：grep `sedValidation|pathValidation|readOnlyValidation|destructiveCommandWarning|commandSemantics|bashPermissions|bashSecurity|modeValidation|shouldUseSandbox` in `runtime/src/bash_validation.rs`
- **预期行为**：9 个子模块全部存在
- **通过判定**：9/9 命中

### G5.6 `bash` 在 read-only 模式拒绝 mutating 命令
- **验证方法**：REPL 内 `--permission-mode read-only` 下运行 bash 工具调用 `echo hi > /tmp/test`
- **预期行为**：被拒绝
- **通过判定**：拒绝消息明确

### G5.7 `check_file_write` workspace boundary
- **验证方法**：REPL 内 `--permission-mode workspace-write` 下尝试 `write_file` 到 workspace 外路径
- **预期行为**：被拒绝；symlink following、`../` escapes 被检测
- **通过判定**：拒绝且检测覆盖 symlink/`../`

### G5.8 文件大小限制
- **验证方法**：grep `MAX_READ_SIZE`、`MAX_WRITE_SIZE` in `runtime/src/file_ops.rs`
- **预期行为**：常量存在；超限拒绝
- **通过判定**：常量存在且行为正确

### G5.9 二进制文件检测
- **验证方法**：grep `NUL` 或 `binary` in `runtime/src/file_ops.rs`
- **预期行为**：NUL-byte binary detection 生效
- **通过判定**：检测逻辑存在

### G5.10 TaskCreate/Get/List/Stop/Update/Output registry-backed
- **验证方法**：grep `task_registry` in `runtime/src/task_registry.rs`；统计 LOC
- **预期行为**：6 个 task 工具 registry-backed；`task_registry.rs` ~335 LOC
- **通过判定**：函数齐全

### G5.11 TeamCreate/Delete + CronCreate/Delete/List wired
- **验证方法**：grep `TeamCreate|TeamDelete|CronCreate|CronDelete|CronList` in `tools/src/lib.rs`
- **预期行为**：5 个工具 wired；`team_cron_registry.rs` ~363 LOC
- **通过判定**：5/5 工具 wired

### G5.12 LSP 工具 6 方法
- **验证方法**：grep `symbols|references|diagnostics|definition|hover|formatting` in `runtime/src/lsp_client.rs`
- **预期行为**：6 个 LSP 方法 exposed；`lsp_client.rs` ~438 LOC
- **通过判定**：6/6 方法存在

### G5.13 MCP 工具桥接
- **验证方法**：grep `ListMcpResources|ReadMcpResource|McpAuth|MCP` in `runtime/src/mcp_tool_bridge.rs`
- **预期行为**：4 个工具 wired；`mcp_tool_bridge.rs` ~406 LOC
- **通过判定**：4/4 工具 wired

### G5.14 PowerShell 工具 danger-full-access
- **验证方法**：grep `PowerShell` in `tools/src/lib.rs`；检查注册的 permission mode
- **预期行为**：PowerShell 注册为 `danger-full-access`（与 bash 相同）
- **通过判定**：permission mode 匹配

---

## G6 — 会话/状态/分支恢复验证

**来源文档**：`docs/g003-boot-session-verification-map.md`、`docs/g005-branch-recovery-verification-map.md`、`docs/g006-task-policy-board-verification-map.md`、`docs/g010-clone-disambiguation-metadata.md`、`docs/g010-session-hygiene-verification-map.md`、`ROADMAP.md`

### G6.1 `.claw/worker-state.json` 写入
- **验证方法**：grep `emit_state_file` in `runtime/src/worker_boot.rs` 或相关；运行 `claw prompt "hi"` 后检查文件存在
- **预期行为**：WorkerStatus transitions 时原子写入 `.claw/worker-state.json`
- **通过判定**：文件存在且 JSON 有效

### G6.2 Worker 7 状态生命周期
- **验证方法**：grep `spawning|trust_required|tool_permission_required|ready_for_prompt|running|finished|failed` in `runtime/src/worker_boot.rs`
- **预期行为**：7 个状态枚举值存在
- **通过判定**：7/7 状态值命中

### G6.3 trust_resolver allowlist
- **验证方法**：grep `trustedRoots` in `runtime/src/config.rs` 和 `trust_resolver.rs`
- **预期行为**：从 `.claw/settings.json` 读取 `trustedRoots`；allowlisted 仓库 auto-trust
- **通过判定**：配置 key 存在且 auto-trust 逻辑生效

### G6.4 SessionStore workspace_fingerprint 隔离
- **验证方法**：grep `workspace_fingerprint` in `runtime/src/session_control.rs`
- **预期行为**：sessions 存储在 `.claw/sessions/<workspace_fingerprint>/`；fingerprint 为 16 字符 FNV-1a digest
- **通过判定**：路径模式和算法匹配

### G6.5 SessionStore canonicalize 路径
- **验证方法**：运行 `cargo test --manifest-path rust/Cargo.toml -p runtime session_store_from_cwd_canonicalizes_equivalent_paths -- --nocapture`
- **预期行为**：测试通过；等价路径产生相同 fingerprint
- **通过判定**：测试 PASS

### G6.6 不同 workspace 隔离
- **验证方法**：运行 `cargo test --manifest-path rust/Cargo.toml -p runtime session_store_from_cwd_isolates_sessions_by_workspace -- --nocapture`
- **预期行为**：测试通过；不同 workspace 不互相可见
- **通过判定**：测试 PASS

### G6.7 legacy session workspace 校验
- **验证方法**：运行 `cargo test --manifest-path rust/Cargo.toml -p runtime session_store_rejects_legacy_session_from_other_workspace -- --nocapture`
- **预期行为**：测试通过；其他 workspace 的 legacy session 被拒绝（`WorkspaceMismatch`）
- **通过判定**：测试 PASS

### G6.8 `/session fork` 同 namespace
- **验证方法**：运行 `cargo test --manifest-path rust/Cargo.toml -p runtime session_store_fork_stays_in_same_namespace -- --nocapture`
- **预期行为**：测试通过；fork 留在同一 workspace partition；记录 parent id + branch
- **通过判定**：测试 PASS

### G6.9 `.gitignore` 忽略 sessions
- **验证方法**：运行 `git check-ignore .claw/sessions/example.jsonl rust/.claw/sessions/example.jsonl .claude/sessions/example.json`
- **预期行为**：3 个路径均被忽略
- **通过判定**：3/3 命中

### G6.10 stale-branch 检测
- **验证方法**：grep `branch.stale_against_main` in `runtime/src/stale_branch.rs` 或 `lane_events.rs`
- **预期行为**：检测到 stale 时 emit 事件
- **通过判定**：事件名存在且 emit 逻辑存在

### G6.11 `workspace_test_branch_preflight`
- **验证方法**：grep `workspace_test_branch_preflight` in `runtime/src/bash.rs` 或相关
- **预期行为**：broad-test gate；分支落后 main 时阻止 bash 工具
- **通过判定**：函数存在且行为正确

### G6.12 recovery_recipes 7 scenarios
- **验证方法**：grep `FailureScenario` in `runtime/src/recovery_recipes.rs`
- **预期行为**：7 个 scenarios（trust_prompt_unresolved/prompt_delivered_to_shell/stale_branch/compile_red_after_refactor/MCP_handshake_failure/partial_plugin_startup/tool_permission_required）
- **通过判定**：7 个 scenarios 存在

### G6.13 RecoveryLedger 状态
- **验证方法**：grep `RecoveryLedgerEntry|RecoveryAttemptState` in `runtime/src/recovery_recipes.rs`
- **预期行为**：ledger 记录 attempt count、state、timestamps、failure summary、escalation reason
- **通过判定**：字段齐全

### G6.14 deprecated `permissionMode` 迁移
- **验证方法**：构造 `.claw/settings.json` 含 `permissionMode: "dangerFullAccess"`；运行 `claw status --output-format json`
- **预期行为**：迁移到 `permissions.defaultMode: DangerFullAccess`；不降级到 `WorkspaceWrite`
- **通过判定**：迁移后值正确

---

## G7 — 安全/权限/沙箱验证

**来源文档**：`docs/g002-security-verification-map.md`、`docs/container.md`、`docs/enterprise-audit-module-design.md`、`SECURITY.md`

### G7.1 `PermissionEnforcer.check` 4 方法
- **验证方法**：grep `pub fn check|pub fn check_with_required_mode|pub fn check_file_write|pub fn check_bash` in `runtime/src/permission_enforcer.rs`
- **预期行为**：4 个方法存在
- **通过判定**：4/4 方法命中

### G7.2 `validate_workspace_boundary`
- **验证方法**：grep `validate_workspace_boundary` in `runtime/src/file_ops.rs`
- **预期行为**：函数存在；symlink following、`../` escapes 检测
- **通过判定**：函数存在

### G7.3 `is_within_workspace` 不对 Windows absolute paths early-return false
- **验证方法**：grep `is_within_workspace` in `runtime/src/permission_enforcer.rs` 或 `file_ops.rs`
- **预期行为**：Windows absolute paths 不被 early-return false（已修复 hazard）
- **通过判定**：逻辑匹配

### G7.4 `strip_verbatim_prefix` Windows-only
- **验证方法**：grep `strip_verbatim_prefix` in `runtime/src/file_ops.rs` 或 `permission_enforcer.rs`
- **预期行为**：Windows-only；剥离 `\\?\` 和 `\\?\UNC\` 前缀
- **通过判定**：函数存在且 cfg windows

### G7.5 多根 workspace boundary check
- **验证方法**：grep `validate_workspace_boundary_multi` in `runtime/src/file_ops.rs`
- **预期行为**：多根 boundary check；`WorkspacePathScope::from_roots()` builder
- **通过判定**：函数存在

### G7.6 sandbox 容器检测
- **验证方法**：grep `/.dockerenv|/.containerenv` in `runtime/src/sandbox.rs`
- **预期行为**：检测 `/.dockerenv`、`/run/.containerenv`、env vars、`/proc/1/cgroup`
- **通过判定**：4 类 marker 检测存在

### G7.7 `claw sandbox` 报告容器状态
- **验证方法**：在容器内运行 `claw sandbox --output-format json`（或在主机运行查看 `In container false`）
- **预期行为**：报告 `In container true/false` 并列出 markers
- **通过判定**：字段存在

### G7.8 Windows Job Object sandbox
- **验证方法**：grep `Job Object|assign_process` in `runtime/src/sandbox.rs`
- **预期行为**：Windows 用 Job Object；`platform_sandbox_builder().assign_process(pid)`
- **通过判定**：Windows 路径存在

### G7.9 ConfigLoader 合并优先级
- **验证方法**：grep `ConfigLoader::discover` in `runtime/src/config.rs`；运行 `cargo test --manifest-path rust/Cargo.toml -p runtime loads_and_merges_claude_code_config_files_by_precedence -- --nocapture`
- **预期行为**：合并顺序 `~/.claw.json` → `~/.config/claw/settings.json` → `<repo>/.claw.json` → `<repo>/.claw/settings.json` → `<repo>/.claw/settings.local.json`
- **通过判定**：测试 PASS

### G7.10 `audit` crate 存在性
- **验证方法**：检查 `rust/crates/audit/` 目录是否存在；grep `AuditEvent|LocalJsonlSink` in `audit/src/`
- **预期行为**：（M1 milestone）audit crate 存在；LocalJsonlSink + hash chain 实现
- **通过判定**：crate 与 sink 存在（若 DEFER 需记录原因）

### G7.11 audit 6 hook 点
- **验证方法**：grep `audit.record|AuditEvent::permission_decision|AuditEvent::tool_call` in `tools/src/lib.rs`、`conversation.rs`、`bash.rs`、`compact.rs`、`streaming.rs`
- **预期行为**：6 个 hook 点埋入（tools/lib.rs:1476、conversation.rs:1320、bash.rs:299、compact.rs:525、streaming.rs:500、conversation.rs:1263）
- **通过判定**：6/6 命中（或 DEFER）

### G7.12 audit hash chain
- **验证方法**：grep `prev_hash|chain_seq` in `audit/src/`
- **预期行为**：`prev_hash = SHA256(prev.event_id || prev.payload)`；本地 `<workspace>/.claw/audit/audit-YYYYMMDD.jsonl`
- **通过判定**：字段与文件路径匹配

---

## G8 — 多 agent / DAG / Plan 子系统验证

**来源文档**：`docs/harness-engineering-optimization-plan.md`、`docs/harness-engineering-optimization-plan-phase2.md`、`docs/multi-agent-hardening-plan.md`、`docs/modules/dag-orchestration-detail.md`、`docs/ide-hooks-dag-implementation-plan.md`、`rust/.omc/plans/2026-07-20-code-review-fix-plan.md`

### G8.1 `MultiAgentCoordinator` 真实化（非空壳）
- **验证方法**：grep `MultiAgentCoordinator` in `runtime/src/multi_agent/mod.rs`；检查 `start()` 方法实际派生 runtime
- **预期行为**：`start()` 真实派生 `ConversationRuntime`；通过 `tokio::spawn` 异步执行
- **通过判定**：非空壳（修复 code-review-fix-plan P1-4）

### G8.2 `execute_dispatch_subagent` 存在
- **验证方法**：grep `execute_dispatch_subagent` in `runtime/src/conversation.rs`
- **预期行为**：函数存在；行号约 1656/1700
- **通过判定**：函数存在

### G8.3 `run_subagent_turn` 返回 result_ref
- **验证方法**：grep `run_subagent_turn` in `runtime/src/conversation.rs`
- **预期行为**：函数签名 `run_subagent_turn(&mut self, subagent_id, name, task) -> Result<String, String>`；返回 result_ref 相对路径
- **通过判定**：签名匹配

### G8.4 subagent 结果写入 `.claw/subagents/{id}.md`
- **验证方法**：触发 subagent 调用后检查文件
- **预期行为**：结果写入 `.claw/subagents/{id}.md`
- **通过判定**：文件存在

### G8.5 Subagent 字段扩展
- **验证方法**：grep `model|complexity|max_attempts|attempts|validated|notes|checkpoint_path|cost_limit|cost_accumulated` in `runtime/src/multi_agent/mod.rs`
- **预期行为**：8 个字段均存在；方法 `spawn_with_model`/`reset_for_retry`/`increment_attempts`/`record_cost`/`check_cost_limit`/`save_checkpoint` 存在
- **通过判定**：字段和方法齐全

### G8.6 `--enable-plan-mode` flag
- **验证方法**：grep `enable-plan-mode` in `rusty-claude-cli/src/commands_handler.rs` 或 `app.rs`
- **预期行为**：CLI flag 存在
- **通过判定**：flag 解析存在

### G8.7 `planMode` settings key
- **验证方法**：grep `planMode` in `runtime/src/config.rs` 和 `rusty-claude-cli/src/app.rs`
- **预期行为**：config key 存在
- **通过判定**：key 存在

### G8.8 `.claw/plans/<timestamp>.json` 持久化
- **验证方法**：grep `plans/` in `runtime/src/` 相关文件
- **预期行为**：Plan 持久化到该路径
- **通过判定**：路径写入逻辑存在

### G8.9 `PlanArtifact` steps 非空（修复 P1-5）
- **验证方法**：grep `PlanArtifact::new` in `runtime/src/conversation.rs` 和 `runtime/src/planner/mod.rs`
- **预期行为**：Complex 时 steps 非空（不再 `Vec::new()`）；或注入 `update_plan` 工具
- **通过判定**：非空或注入机制存在

### G8.10 DAG 模块存在性
- **验证方法**：检查 `rust/crates/runtime/src/dag/` 目录
- **预期行为**：目录存在；含 `node.rs`/`graph.rs`/`scheduler.rs`/`checkpoint.rs`/`yaml_loader.rs`
- **通过判定**：目录与文件存在（若 DEFER 需记录原因）

### G8.11 `dag_run`/`dag_status` 工具
- **验证方法**：grep `dag_run|dag_status` in `tools/src/lib.rs`
- **预期行为**：2 个工具 ToolSpec 存在
- **通过判定**：工具存在（或 DEFER）

### G8.12 LoopDetector.reset() 调用（修复 P2-7）
- **验证方法**：grep `loop_detector.reset` in `runtime/src/conversation.rs`
- **预期行为**：在 `run_turn` 入口调用 `reset()`
- **通过判定**：调用存在

---

## G9 — Hooks / Plugin / MCP 生命周期验证

**来源文档**：`docs/modules/hooks-system-detail.md`、`docs/g007-mcp-lifecycle-mapping.md`、`docs/g007-plugin-mcp-verification-map.md`、`docs/ide-hooks-dag-implementation-plan.md`

### G9.1 HookEvent 10 事件
- **验证方法**：grep `PreToolUse|PostToolUse|PostToolUseFailure|PostCustomToolCall|UserPromptSubmit|Notification|SessionStart|SessionEnd|Stop|SubagentStop|PreCompact` in `runtime/src/hooks.rs`
- **预期行为**：10/11 个事件枚举值存在
- **通过判定**：枚举值齐全

### G9.2 4 HookHandler 类型
- **验证方法**：grep `Command|Webhook|Inline|Prompt` in `runtime/src/hooks.rs`
- **预期行为**：4 个 handler 类型存在
- **通过判定**：4/4 类型存在

### G9.3 Hooks run_turn 7 集成点
- **验证方法**：grep hook 调用 in `runtime/src/conversation.rs` line ~834/841/1175/1281/1287/1490/2127
- **预期行为**：7 个集成点埋入
- **通过判定**：7/7 命中

### G9.4 HookDecision Allow/Deny/Continue
- **验证方法**：grep `HookDecision` in `runtime/src/hooks.rs`
- **预期行为**：3 个变体存在；PreToolUse 可返回 Deny 拦截
- **通过判定**：变体齐全

### G9.5 Hooks PreToolUse 覆盖 permission
- **验证方法**：grep `permission_override` in `runtime/src/conversation.rs` 约 line 1179-1219
- **预期行为**：PreToolUse hook 可返回 `permission_override` 覆盖 PermissionEnforcer
- **通过判定**：覆盖逻辑存在

### G9.6 `FailurePolicy::FailClose/FailOpen`
- **验证方法**：grep `FailClose|FailOpen` in `runtime/src/hooks.rs`
- **预期行为**：2 个 policy 存在
- **通过判定**：2/2 命中

### G9.7 `.claw/hooks.toml` 配置文件
- **验证方法**：grep `hooks.toml` in `runtime/src/config.rs` 或 `hooks.rs`
- **预期行为**：配置文件被读取
- **通过判定**：读取逻辑存在

### G9.8 Plugin lifecycle Init/Shutdown
- **验证方法**：grep `PluginLifecycle|Init|Shutdown` in `runtime/src/plugin_lifecycle.rs`
- **预期行为**：lifecycle 事件存在
- **通过判定**：枚举存在

### G9.9 `PluginSummary.lifecycle_state`
- **验证方法**：grep `lifecycle_state` in `runtime/src/plugin_lifecycle.rs`
- **预期行为**：`lifecycle_state` 字段值 `ready`/`disabled`/`load_failed`
- **通过判定**：字段与值匹配

### G9.10 MCP degraded startup 报告
- **验证方法**：grep `McpDegradedReport|McpFailedServer|McpErrorSurface` in `runtime/src/mcp_stdio.rs` 或 `mcp_lifecycle_hardened.rs`
- **预期行为**：degraded report 模型存在
- **通过判定**：3 个类型存在

### G9.11 `discover_tools_best_effort` 保留健康服务器
- **验证方法**：grep `discover_tools_best_effort` in `runtime/src/mcp_server.rs` 或相关
- **预期行为**：保留健康服务器工具 + 记录失败服务器
- **通过判定**：函数存在且行为正确

### G9.12 `mcpServers.<name>.required` config
- **验证方法**：grep `required` in `runtime/src/mcp.rs` 或 `config.rs`
- **预期行为**：`required: bool` 配置项；默认 false
- **通过判定**：配置项存在

### G9.13 `scoped_mcp_config_hash` 含 required
- **验证方法**：grep `scoped_mcp_config_hash` in `runtime/src/mcp.rs`
- **预期行为**：hash 含 `required:<bool>`
- **通过判定**：hash 计算包含 required

### G9.14 MCP env_keys redacted
- **验证方法**：grep `env_keys` in `runtime/src/mcp.rs` 或 `commands/src/lib.rs`
- **预期行为**：报告 `env_keys`/`Header keys` 但值不打印
- **通过判定**：脱敏逻辑存在

---

## G10 — 已知 BUG 复核（17 项）

**来源文档**：`rust/.omc/plans/2026-07-20-code-review-fix-plan.md`、`rust/.omc/plans/2026-07-20-tui-status-report-corrected.md`

> 这 17 项是已识别的 BUG，验证目的是确认是否已修复；若未修复则标 BUG 并附复现步骤。

### G10.1 [P0-1] StatusEmitter 错误路径沉默
- **验证方法**：grep `StatusEvent::StreamError` in `streaming.rs`；检查 9 处错误返回路径是否 emit
- **预期行为**：新增 `StreamError` variant；9 处 `return Err(...)` 前 emit；TUI 端处理 StreamError 调用 `finish_turn`
- **通过判定**：variant 存在且 9 处 emit

### G10.2 [P0-3] reactive_compact 跳过 Provider 恢复
- **验证方法**：grep `MicrocompactDone|FullCompactDone` in `runtime/src/conversation.rs` 约 L872-898
- **预期行为**：失败分支调用 `try_recover_or_record_fail`（preserve_recovery_state=true）
- **通过判定**：恢复路径存在

### G10.3 [P0-4] Worker panic 后 TUI 永久冻结
- **验证方法**：grep `Disconnected` 分支 in `tui/app.rs` 约 L339-348
- **预期行为**：Disconnected 分支追加 `[error] 对话线程已崩溃`；设置 `fatal_error` 标志
- **通过判定**：错误反馈存在

### G10.4 [P1-2] 鼠标点击显示行↔逻辑行映射错位
- **验证方法**：grep `tool_card_line_ranges` in `tui/output_view.rs` 约 L241-261
- **预期行为**：按显示行计算（累加每行 `ceil(width/area_width)`）；传入 area_width
- **通过判定**：计算逻辑改为显示行

### G10.5 [P1-3] response_to_events 非流式 fallback 不 emit
- **验证方法**：grep `response_to_events` in `streaming.rs` 约 L823-826
- **预期行为**：函数签名含 `Option<&StatusEmitter>` 参数；fallback 路径手动 emit
- **通过判定**：参数与 emit 存在

### G10.6 [P1-4] MultiAgentCoordinator 是空壳
- **验证方法**：见 G8.1
- **预期行为**：`start()` 真实派生 runtime
- **通过判定**：非空壳

### G10.7 [P1-5] Planner steps 永远为空
- **验证方法**：见 G8.9
- **预期行为**：Complex 时 steps 非空
- **通过判定**：非空

### G10.8 [P1-6] VerifierAgent / TraceAnalyzer / ContextAssembler 未注入
- **验证方法**：grep `with_verifier_agent|with_trace_analyzer|with_context_assembler` 调用点 in `rusty-claude-cli/src/app.rs` `build_runtime_with_plugin_state`
- **预期行为**：3 个 setter 在生产路径被调用（非仅测试）
- **通过判定**：生产调用点存在

### G10.9 [P2-1] `/effort` 被错误列入 STUB_COMMANDS
- **验证方法**：见 G3.14
- **预期行为**：`STUB_COMMANDS` 不含 `"effort"`
- **通过判定**：不含

### G10.10 [P2-2] slash_menu 不过滤 STUB_COMMANDS
- **验证方法**：见 G3.15
- **预期行为**：SlashMenu 加载时过滤 STUB
- **通过判定**：过滤逻辑存在

### G10.11 [P2-3] 状态栏 section 宽度用字节长度
- **验证方法**：grep `s.content.len()` in `tui/status_bar.rs` 约 L195
- **预期行为**：改用 `unicode_width::UnicodeWidthStr::width(s.content.as_ref())`
- **通过判定**：用 `UnicodeWidthStr`

### G10.12 [P2-4] Submit 时未立即设置 streaming=true
- **验证方法**：grep `Submit` 分支 in `tui/app.rs` 约 L685-695
- **预期行为**：Submit 后立即 `status_state.lock().reset_turn()` 或设 `streaming = true`
- **通过判定**：立即设置

### G10.13 [P2-5] MessageStart 多 content block 重复 emit Thinking
- **验证方法**：grep `block_has_thinking_summary` in `streaming.rs` 约 L417-438
- **预期行为**：emit 后立即 `block_has_thinking_summary = false`；或移到 for 循环外
- **通过判定**：去重逻辑存在

### G10.14 [P2-6] pricing_for_model 不覆盖非 Anthropic 模型
- **验证方法**：grep `pricing_for_model` in `runtime/src/usage.rs` 约 L59-81
- **预期行为**：覆盖 `gpt-5`/`grok-3`/`qwen-max` 等常见模型
- **通过判定**：覆盖范围扩展

### G10.15 [P2-7] LoopDetector.reset() 永不调用
- **验证方法**：见 G8.12
- **预期行为**：在 `run_turn` 入口调用
- **通过判定**：调用存在

### G10.16 [P1-1 Thinking 事件覆盖缺口] push_output_block 未 emit Thinking
- **验证方法**：grep `push_output_block` in `streaming.rs` 约 L786-793
- **预期行为**：TUI 模式下 `push_output_block` 处理 Thinking 块时 emit `StatusEvent::Thinking`
- **通过判定**：emit 调用存在

### G10.17 [P1-2 Markdown 渲染性能] 每次全量转换无缓存
- **验证方法**：grep `markdown_to_ansi` in `tui/app.rs` 约 L198-206
- **预期行为**：缓存上次 snapshot hash + Text；未变时跳过转换
- **通过判定**：缓存机制存在

---

## G11 — 测试套件基线验证

**来源文档**：`docs/g002-security-verification-map.md`、`docs/g004-events-reports-verification-map.md`、`docs/g005-branch-recovery-verification-map.md`、`docs/g006-task-policy-board-verification-map.md`、`docs/g007-plugin-mcp-verification-map.md`、`rust/PARITY.md`、`progress.txt`

### G11.1 `cargo test --workspace` 基线
- **验证方法**：在 `rust/` 运行 `cargo test --workspace 2>&1 | tail -50`
- **预期行为**：891+ tests passed；3 pre-existing failures（output_format_contract.rs 的 3 个 MCP/plugin 相关）
- **通过判定**：通过数 ≥ 891，失败数 = 3 且均为已知 pre-existing

### G11.2 `cargo test --features full-tui` 基线
- **验证方法**：运行 `cargo test --features full-tui -p rusty-claude-cli 2>&1 | tail -50`
- **预期行为**：324/327 passed；3 failed 与 MCP/plugin 相关
- **通过判定**：通过数 ≥ 324

### G11.3 `cargo test tui::` 基线
- **验证方法**：运行 `cargo test --features full-tui -p rusty-claude-cli tui:: 2>&1 | tail -20`
- **预期行为**：88 passed; 0 failed
- **通过判定**：88 passed

### G11.4 `cargo clippy --workspace --all-targets -- -D warnings`
- **验证方法**：在 `rust/` 运行 `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -50`
- **预期行为**：0 warnings（或仅 3 个 TUI manual_clamp/sort_by_key）
- **通过判定**：0 warnings 或仅已知 TUI warnings

### G11.5 `cargo fmt --all -- --check`
- **验证方法**：在 `rust/` 运行 `cargo fmt --all -- --check`
- **预期行为**：无 diff
- **通过判定**：无输出

### G11.6 `scripts/fmt.sh --check`
- **验证方法**：在仓库根运行 `bash scripts/fmt.sh --check`
- **预期行为**：无 diff
- **通过判定**：无输出

### G11.7 Mock parity harness 12 scenarios
- **验证方法**：在 `rust/` 运行 `./scripts/run_mock_parity_harness.sh` 或 `cargo test -p rusty-claude-cli --test mock_parity_harness`
- **预期行为**：12 scenarios 全过
- **通过判定**：12/12 PASS

### G11.8 Mock parity diff
- **验证方法**：在 `rust/` 运行 `python3 scripts/run_mock_parity_diff.py --no-run`
- **预期行为**：12/12 scenarios `[MAPPED]`；所有 `parity_refs` 在 `PARITY.md` 中存在
- **通过判定**：12/12 mapped

### G11.9 无 `#[ignore]` 隐藏失败
- **验证方法**：grep `#\[ignore\]` in `rust/**/*.rs`
- **预期行为**：最多 1 个（`live_stream_smoke_test`）
- **通过判定**：≤ 1 个 ignore

### G11.10 cc2 board validation
- **验证方法**：运行 `python3 scripts/validate_cc2_board.py`
- **预期行为**：PASS；729 items；124/124 ROADMAP headings；542/542 actions
- **通过判定**：PASS

---

## G12 — 文档/构建/发布产物验证

**来源文档**：`docs/g009-windows-docs-release-verification-map.md`、`docs/g012-final-release-readiness-report.md`、`docs/pr-issue-resolution-gate.md`、`docs/anti-slop-triage.md`、`docs/container.md`、`README.md`、`CONTRIBUTING.md`

### G12.1 `cargo build --release --workspace`
- **验证方法**：在 `rust/` 运行 `cargo build --release --workspace`
- **预期行为**：exit 0；生成 `target/release/claw.exe`
- **通过判定**：exit 0 且二进制存在

### G12.2 `cargo build --release --bin claw`
- **验证方法**：运行 `cargo build --release --bin claw`
- **预期行为**：exit 0；`target/release/claw.exe` ~16-20 MB
- **通过判定**：exit 0 且大小合理

### G12.3 `claw.exe --help` Windows smoke
- **验证方法**：运行 `.\target\release\claw-plus.exe --help`
- **预期行为**：渲染 help；无 warning
- **通过判定**：help 输出干净

### G12.4 Windows smoke 4 命令无凭据可用
- **验证方法**：依次运行 `claw.exe help`、`claw.exe status`、`claw.exe config env`、`claw.exe doctor`（无凭据环境）
- **预期行为**：4 个命令均无 panic；exit 0 或 helpful error（缺凭据时）
- **通过判定**：4/4 无 panic

### G12.5 `check_doc_source_of_truth.py`
- **验证方法**：运行 `python3 .github/scripts/check_doc_source_of_truth.py`
- **预期行为**：PASS；doc source-of-truth check passed
- **通过判定**：PASS

### G12.6 `check_release_readiness.py`
- **验证方法**：运行 `python3 .github/scripts/check_release_readiness.py`
- **预期行为**：PASS；release-readiness check passed
- **通过判定**：PASS

### G12.7 `workspace.package.license = "MIT"`
- **验证方法**：grep `^license` in `rust/Cargo.toml`
- **预期行为**：`license = "MIT"`
- **通过判定**：值匹配

### G12.8 CONTRIBUTING/SECURITY/SUPPORT 存在
- **验证方法**：检查 `CONTRIBUTING.md`、`SECURITY.md`、`SUPPORT.md` 文件存在
- **预期行为**：3 个文件均存在
- **通过判定**：3/3 存在

### G12.9 Containerfile 存在
- **验证方法**：检查仓库根 `Containerfile` 存在
- **预期行为**：文件存在；Docker/Podman 兼容
- **通过判定**：存在

### G12.10 `.github/workflows/` CI 配置
- **验证方法**：检查 `.github/workflows/rust-ci.yml`、`.github/workflows/release.yml` 存在
- **预期行为**：2 个 workflow 文件存在；rust-ci 运行 `cargo test --workspace` + fmt/clippy
- **通过判定**：2/2 文件存在

---

## 验证执行说明（给 claw.exe 内 LLM 的指令）

1. **执行顺序**：按 G1 → G2 → ... → G12 顺序逐项验证；每组内按编号顺序
2. **每项记录**：
   - **结果**：PASS / FAIL / BUG / SKIP / DEFER
   - **实际观测**：命令输出关键摘要、文件存在性、grep 命中行数等
   - **证据**：附 command output 截图或文本片段
   - **根因**（仅 BUG/FAIL）：定位到具体文件:行号 + 问题描述
3. **BUG 报告格式**：
   ```
   ### BUG-G<组号>.<项号>
   - **来源文档**：<文件路径>
   - **预期**：<预期行为>
   - **实际**：<实际观测>
   - **根因**：<文件:行号> <问题描述>
   - **修复建议**：<具体修改方向>
   ```
4. **进度更新**：每完成一组后更新汇总表（PASS/FAIL/BUG/SKIP 计数 + 进度百分比）
5. **优先级**：P0 项（G10.1-G10.3、G1.1-G1.5）必须最先验证；P1 项（G10.4-G10.17、其余 G1/G2/G3）次之
6. **环境约束**：
   - 默认在 `d:\claw-code-src\rust` 目录运行 cargo 命令
   - 验证 TUI 相关项需 `--features full-tui`
   - 验证 provider 路由需配置对应环境变量
   - 验证 mock parity 需先启动 `cargo run -p mock-anthropic-service`
7. **完成标准**：189 项中 ≥ 90% 标注 PASS 或 DEFER（附原因）；所有 P0 项必须有明确结论；所有 BUG 必须附根因和修复建议

---

## 附录：文档来源索引

| 文档 | 主要覆盖验证组 |
|---|---|
| `README.md` | G1, G12 |
| `USAGE.md` | G1, G3, G4 |
| `PHILOSOPHY.md` | （参考） |
| `ROADMAP.md` | G1, G3, G6 |
| `PARITY.md` | G5, G11 |
| `SECURITY.md` | G7 |
| `CONTRIBUTING.md` | G11, G12 |
| `CLAUDE.md` | G11 |
| `prd.json` | G4 |
| `progress.txt` | G4, G11 |
| `docs/harness-engineering-optimization-plan.md` | G8 |
| `docs/harness-engineering-optimization-plan-phase2.md` | G8 |
| `docs/harness-engineering-optimization-progress.md` | G8 |
| `docs/multi-agent-hardening-plan.md` | G8 |
| `docs/windows-install-release.md` | G1, G12 |
| `docs/modules/hooks-system-detail.md` | G9 |
| `docs/modules/ide-integration-detail.md` | G9 |
| `docs/modules/dag-orchestration-detail.md` | G8 |
| `docs/ide-hooks-dag-implementation-plan.md` | G8, G9 |
| `docs/enterprise-audit-module-design.md` | G7 |
| `docs/g002-security-verification-map.md` | G7, G11 |
| `docs/g003-boot-session-verification-map.md` | G6 |
| `docs/g004-events-reports-contract.md` | G6 |
| `docs/g004-events-reports-verification-map.md` | G6, G11 |
| `docs/g005-branch-recovery-verification-map.md` | G6, G11 |
| `docs/g006-task-policy-board-verification-map.md` | G6, G11 |
| `docs/g007-mcp-lifecycle-mapping.md` | G9 |
| `docs/g007-plugin-mcp-verification-map.md` | G9, G11 |
| `docs/g009-windows-docs-release-verification-map.md` | G12 |
| `docs/g010-clone-disambiguation-metadata.md` | G6 |
| `docs/g010-session-hygiene-verification-map.md` | G6, G11 |
| `docs/g011-acp-json-rpc-status-contract.md` | G1 |
| `docs/g011-ecosystem-ops-ux-verification-map.md` | G3 |
| `docs/g012-final-release-readiness-report.md` | G12 |
| `docs/navigation-file-context.md` | （参考） |
| `docs/local-openai-compatible-providers.md` | G4 |
| `docs/container.md` | G7, G12 |
| `docs/anti-slop-triage.md` | G12 |
| `docs/MODEL_COMPATIBILITY.md` | G4 |
| `docs/pr-issue-resolution-gate.md` | G12 |
| `docs/roadmap-pr-goals.md` | G12 |
| `rust/README.md` | G1, G5 |
| `rust/USAGE.md` | G1 |
| `rust/PARITY.md` | G5, G11 |
| `rust/MOCK_PARITY_HARNESS.md` | G11 |
| `rust/CLAUDE.md` | G11 |
| `rust/TUI-ENHANCEMENT-PLAN.md` | G2 |
| `rust/.omc/plans/2026-07-19-tui-phase0-refactor.md` | G2 |
| `rust/.omc/plans/2026-07-19-tui-phase1-ratatui.md` | G2 |
| `rust/.omc/plans/2026-07-20-tui-phase2-realtime-streaming.md` | G2 |
| `rust/.omc/plans/2026-07-20-tui-slash-command-output-migration.md` | G3 |
| `rust/.omc/plans/2026-07-20-tui-status-report-corrected.md` | G2, G10 |
| `rust/.omc/plans/2026-07-20-code-review-fix-plan.md` | G10 |
