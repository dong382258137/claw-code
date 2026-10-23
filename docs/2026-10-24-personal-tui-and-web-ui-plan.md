# 单人自用：TUI 修复（A）与薄 Web UI（B）实施方案

> 2026-10-24 · 目标读者：项目所有者（单人使用）· 范围：`D:\claw-code-src`
> 前提：不做多用户、不做鉴权体系、不移植 DSH 的 UI 代码。保持"单二进制、无 Node 依赖、离线可用"。

---

## 0. 目标与约束

**目标**

1. 让现有的终端体验不再"承诺与实现不符"：vim 模式真的能用，桩命令有去处，终端能力补齐。
2. 在 A 稳定后，提供一个**可选的**本地 Web 视图，用于看长输出、diff、多会话，而**不牺牲**现有 TUI 与单二进制分发。

**硬约束**

| 约束 | 含义 |
|---|---|
| 单人 | 不做登录、权限、多租户；Web UI 默认只绑 `127.0.0.1` |
| 单二进制 | 前端资源用 `include_str!` 内嵌，不引入 Node 构建链 |
| 离线可用 | 不引入需要联网下载的运行期资源；新依赖仅限已在 `Cargo.lock` 中的（axum/tower-http 已在） |
| 可回滚 | 每个阶段独立提交、独立开关；B 阶段默认不启动 |
| 不碰铁律 | `BLUEPRINT.md` 第 4 节（缓存前缀、状态可恢复、不得假报完成）在改动中保持 |

**当前事实（实测）**

- TUI 已相当完整：`?` 帮助浮层、J/K 回复跳转、F2 侧栏、F4 语音、Ctrl+T 工具卡、PgUp/PgDn、End 跟随、F12 调试浮层（`tui/input_line.rs:11-60`）。
- 输入行硬编码 Emacs（`input.rs:111`），TUI 的 `InputLine` 无模式概念（`tui/input_line.rs:79-204`）。
- 31 条斜杠命令落入统一"not yet implemented"分支（`app.rs:1751-1788`）。
- 无任何终端图片协议（sixel/kitty/iterm 均未实现），也没有 `read_image` 类工具 —— 所以"图片渲染"不是当前的真实缺口。
- TUI 已启用鼠标捕获（`tui/app.rs:162/174`）。
- Web 侧地基齐全：`rusty-claude-cli` 已有 `tokio`；`im-bridge` 已在用 `axum 0.8.9` + `tower-http 0.6.8`（`Cargo.lock` 中已存在，无需新依赖）。

---

## A 阶段：TUI 修复（时间盒 1–2 周，单人约 3–5 个工作日）

### A1. vim 模式真实可用

**问题**：`/vim` 在帮助里写着"切换 vim 编辑模式"（`tui/slash_menu.rs:811`），但实现是空壳；输入行永远是 Emacs。

**方案**

| 层 | 改动 |
|---|---|
| TUI 输入行 | `InputLine` 增加 `VimState { enabled, mode: Insert/Normal, pending: Option<char>, register: String }`；`handle_key` 在 Normal 模式路由到 `handle_normal_key` |
| 最小键位 | Normal：`i a A I` 进入 Insert（光标位置不同）；`Esc` 回 Normal；`h l 0 ^ $ w b e`；`x X`；`dd dw D cw`；`u`（单步撤销，可选）；`p`（粘贴 register，可选） |
| Insert | 保持现有行为；`Esc` 不再清空/退出，而是切 Normal（仅当 vim 开启） |
| REPL 回退 | `LineEditor` 支持 `set_edit_mode(Vi)`，让非 TUI 场景一致 |
| 持久化 | `~/.claw/settings.json`（新文件，`{"vim": true}`），启动时读取，`/vim` 写回 |
| 状态栏 | 显示 `-- INSERT --` / `-- NORMAL --`（`tui/status_bar.rs`） |

**验收**

- `cargo test -p rusty-claude-cli` 全绿（新增 vim 单元测试：模式切换、h/l/x/dd、Esc 语义、CJK 光标）。
- 手工：TUI 下 `/vim` → 状态栏提示 → `Esc` 后 `h/l` 移动光标 → `i` 插入 → `dd` 清行。
- 关闭 `/vim` 后行为与现在完全一致（回归测试保证）。

**风险**：Esc 语义冲突（现在 Esc 清空/退出）。对策：仅在 vim 开启时改变；有专门测试。

### A2. 31 条桩命令治理

**原则**：单人自用场景下，企业/社交类命令直接隐藏；能低成本实现的实现。

| 分类 | 命令 | 处置 |
|---|---|---|
| 实现（低成本、日常用） | `/exit` `/copy` `/keybindings` `/files` `/tasks` `/fast` `/add-dir` `/feedback` | 见下 |
| 保留但标注 | `/theme` `/plan` `/review` `/branch` `/rewind` `/rename` `/tag` `/hooks` `/color` `/ide` | 统一回复"未启用：原因 + 替代做法"，并从 `/help` 折叠到"未实现"分组 |
| 隐藏（不适用单人自用） | `/login` `/logout` `/share` `/desktop` `/advisor` `/stickers` `/insights` `/thinkback` `/release-notes` `/security-review` `/privacy-settings` `/brief` | 从补全与 `/help` 移除；直接输入时给一句"本构建未启用" |

**低成本实现清单**

- `/exit`：TUI 内真正退出（当前 TUI 里落到桩分支，是 bug）。
- `/copy [n]`：把最近 n 条 assistant 文本用 OSC52 写入系统剪贴板（SSH 也可用）；无终端支持时降级为写临时文件并提示路径。
- `/keybindings`：打印/打开现有 `?` 帮助浮层（复用 `ToggleHelp`）。
- `/files [limit]`：列出当前工作区文件（复用 `file_ops` 的遍历，忽略 `.git`/`target`/`.claw`）。
- `/tasks`：读 `runtime::task_registry::TaskRegistry::global()` 输出任务表（id/状态/owner/更新时间）。
- `/fast`：在 `flash` 与当前模型间切换（写 session 级 override，不动全局配置）。
- `/add-dir <path>`：把目录加入本会话的额外工作根（多根 scoping 已存在，见 `tools` 的多根校验）。
- `/feedback <text>`：追加写入 `~/.claw/feedback.jsonl`（本地，不外发）。

**验收**：`/help` 中不再出现"未实现"分组里的命令；31 条逐一有确定去向（实现/标注/隐藏），并有一张 `docs/command-status.md` 对照表。

### A3. 终端能力补齐（按真实缺口，不做图片协议）

- **OSC52 复制**：选中/最近回复 → `\x1b]52;c;base64\x07`；提供 `/copy` 与 `Ctrl+Y` 两个入口。
- **超长会话性能**：确认 `output_view` 是否有全量重排；若有，改为按可视区切片渲染（先用 `F12` 浮层里的 FPS 观测，再决定是否改）。
- **错误可见性**：TUI 下 `eprintln!` 会被 `stderr_guard` 吞掉；把桩命令/错误提示统一走 `tui_println`，避免"命令无反应"。

**验收**：SSH 会话中 `/copy` 后本地终端可粘贴；`F12` 显示稳定帧率；无"命令无输出"的情况。

### A 阶段里程碑

| 里程碑 | 内容 | 完成判据 |
|---|---|---|
| A1 | vim 模式 + 持久化 + 状态栏 | 单测 + 手工验收通过 |
| A2 | 31 条命令去向明确，8 条可用 | `docs/command-status.md` 全表 + 手测 |
| A3 | OSC52、错误可见性、（可选）渲染优化 | 手测通过 |
| 收口 | `fmt` / `clippy --workspace` / `test --workspace` 全绿 | 与 `BLUEPRINT.md` I8 一致 |

---

## B 阶段：薄 Web UI（时间盒 4–8 周，单人约 8–15 个工作日）

> 明确不做的：移植 DSH 的 `ui-chat`/`ui-conversation`（16.4 万行 + Cordis 客户端运行时 + 11 包注入链）。

### B0. 设计原则

1. **一个真相源，两个视图**：TUI 与 Web 都从同一份会话事件读取；Web 不引入第二套状态机。
2. **只读优先**：B1 只做"看"，验证价值后再做"写"。
3. **薄协议**：4 个端点起步，不追求通用 RPC 框架。
4. **默认离线**：前端无 CDN、无外部字体、无遥测。

### B1. 只读镜像（第一刀，约 3–5 天）

```
claw serve --web [--port 4317] [--open]
  GET /                         → 内嵌单页（include_str!）
  GET /api/sessions             → [{id, cwd, title, updated_at, turn_count}]
  GET /api/session/:id          → 投影后的完整视图
  GET /api/session/:id/stream   → SSE：初帧 snapshot，之后增量
```

- 投影：从 `session.jsonl` 折叠出 `{messages[], tool_calls[], usage, status}`；复用 `runtime::session` 的解析与 `lane_events`。
- 实时：`session_bus` 增加一个 `broadcast::Sender<UiEvent>`（进程内），CLI/TUI 写入时同步推送；无订阅者时零开销。
- 前端：单文件 HTML + 原生 JS + SSE，三块区域（会话列表 / 消息流 / 工具卡与 diff）；Markdown 用极简自研渲染（只支持标题/列表/代码块/表格）。
- 安全：只绑 `127.0.0.1`；启动时生成一次性 token 写入 URL（`?t=`），后续请求校验；不提供 CORS。

**验收**：浏览器打开可看到历史会话并实时跟随当前 turn；断线 3 秒内自动重连且不重复消息。

### B2. 写入通道（约 5–7 天）

```
POST /api/session/:id/prompt   { text }
POST /api/session/:id/interrupt
POST /api/session/:id/approval { request_id, decision }
GET  /api/session/:id/pending  → 待审批/待回答
```

- 需要先把审批与提问从"终端回调"抽象成可多端消费的事件（`approval/request`、`question/request` 已有语义，需补一个统一出口）。
- 冲突策略：同一会话同时只允许一个前端写；TUI 与 Web 同时连时，后写者被拒并提示。

**验收**：Web 发起一轮对话、中途打断、Approve/Deny 生效；与 TUI 交替操作不串消息。

### B3. 打磨（可选，按使用反馈）

- diff 视图（复用 `tui_ports/diff_view.rs` 的 Myers 实现，搬到前端为文本 diff）
- 会话搜索、token/成本面板、任务面板
- 主题（浅/深）与键盘快捷键（`j/k`、`gg/G`、`/` 搜索）

### B 阶段里程碑

| 里程碑 | 判据 |
|---|---|
| B1 | 只读镜像可用，连续使用 3 天不崩 |
| B2 | 写通道可用，审批闭环跑通 |
| B3 | 按实际痛点增补，不做"功能对齐 DSH" |

---

## 5. 共同质量门

每次提交前（对齐 `BLUEPRINT.md` I8）：

```powershell
cd D:\claw-code-src\rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

B 阶段额外：`cargo build --release -p rusty-claude-cli` 后手工跑 `claw serve --web` 冒烟。

---

## 6. 顺序与决策点

1. **A1 → A2 → A3**（A1 是承诺兑现，A2 是信任修复，A3 是体验补齐）。
2. A 收口后**停下来用一周**，记录真实痛点（哪些操作还在回终端）。
3. 只有"Web 能显著减少切换成本"成立时才进入 B1；否则只做 A。
4. B1 上线后同样观察一周再决定 B2。

---

## 7. 明确不做

- 不移植 DSH UI 代码/运行时（会摧毁单二进制优势）。
- 不做图片协议渲染（没有图片工具，先有需求再有实现）。
- 不做多用户、鉴权体系、公网暴露。
- 不做"功能对齐清单"式的堆砌；所有新增都以"单人日常是否真的会按"为准。

---

## 8. 执行进展

### 2026-10-24 · A1 完成（vim 模式）

| 文件 | 改动 |
|---|---|
| `rusty-claude-cli/src/settings.rs`（新增） | `~/.claw/settings.json` 读写；`vim` 字段；损坏/缺失静默降级 |
| `rusty-claude-cli/src/lib.rs` | 声明 `pub mod settings` |
| `rusty-claude-cli/src/tui/input_line.rs` | `VimMode{Insert,Normal}` + 状态字段（pending/register/undo）；`handle_normal_key` 支持 `Esc i a A I h l 0 ^ $ w b e x X D dd dw d$ cw p u`；`prompt_label()`；Esc 不再清空（vim 开启时）；7 个单元测试 |
| `rusty-claude-cli/src/tui/app.rs` | 启动读设置；Submit 前拦截 `/vim`（就地切换 + 持久化）；输入框前缀改用 `prompt_label()` 并按显示宽度计算光标 |
| `rusty-claude-cli/src/input.rs` | `LineEditor::new_with_mode(…, vi)`：REPL 使用 rustyline Vi 键位 |
| `rusty-claude-cli/src/app.rs` | REPL 启动读取 `vi` 设置；REPL 内 `/vim` 写盘并提示"下次启动生效" |

**验证**

```
cargo fmt --all                     # 通过
cargo clippy -p rusty-claude-cli --all-targets -- -D warnings   # 通过
cargo test -p rusty-claude-cli --lib                            # 629 passed / 1 ignored
```

**手工验收路径**：启动 TUI → `/vim` → 输入框前缀变 `[I]>` → 输入 `abc` → `Esc` → 变 `[N]>` 且 buffer 保留 → `0` `l` `x` `u` → `dd` `p` → `i` 继续输入 → `Enter` 提交；再次 `/vim` 关闭后行为回到原样。

**已知限制**：REPL 的 vim 切换需重启生效（rustyline 15 无 `set_edit_mode`，键位在构造时固定）；TUI 为即时生效。

### 2026-10-24 · A2 完成（7 条实现 + 1 条推迟）

实现：`/vim`、`/exit`、`/keybindings`、`/files`、`/tasks`、`/copy`（OSC52 + 本地备份）、`/fast`、`/feedback`（本地 JSONL）。
推迟：`/add-dir`（额外工作根在启动期构造，会话内热加需要改 runtime 构造路径，记入下一阶段）。

同时完成发现面治理：`STUB_COMMANDS` 移除上述命令，使 `/help`、`/` 补全与 TUI 斜杠菜单只显示可用命令；新增 `docs/command-status.md` 逐条记录 31 条命令的去向。

**验证**：`cargo test -p rusty-claude-cli` 全绿（636 单元 + 8/16/12 集成）、`cargo clippy -p rusty-claude-cli --all-targets -- -D warnings` 通过、`cargo build -p rusty-claude-cli --bin claw` 通过。

实现明细：

| 命令 | 行为 | 位置 |
|---|---|---|
| `/exit` `/quit` `/q` | TUI 空闲时立即退出；turn 运行中忽略并记日志（避免线程仍持有 cli） | `tui/app.rs` Submit 拦截 |
| `/keybindings` | 打印实际键位表（输入/菜单/中断/vim/浏览/面板/语音） | `app.rs::keybindings_help` |
| `/files` | 列出当前目录文件（跳过隐藏项与 `target`/`node_modules`/`dist`/`build`，最多 100 条、深度 3） | `app.rs::list_workspace_files` |
| `/tasks [get <id>]` | 读 `runtime::task_registry::global()`，列表最多 30 条，`get` 显示单任务详情 | `app.rs::list_tasks` |
| `/copy [last\|all]` | OSC52 写剪贴板（SSH 可用）+ `~/.claw/last-copy.txt` 备份 | `app.rs::deliver_to_clipboard` |
| `/fast` | 在 `flash` 与 `pro` 之间切换模型（复用 `set_model`） | `app.rs` 分发 |
| `/feedback` | 追加 `~/.claw/feedback.jsonl`（不外发） | `app.rs::write_feedback_entry` |
| `/vim` | vim 模式（A1 已完成），并已从桩列表移出 | `tui/input_line.rs` |

未实现命令统一回复：`X 在本构建未启用（单人自用精简版）；/help 查看已支持命令。`
说明：非 TTY 的管道输入会进入 prompt 模式并真实调用模型，故斜杠命令只能靠单元测试与代码路径验证，不能用管道冒烟。

### 2026-10-24 · A3 收口

- `/model`、`/fast` 的输出改走新增的 `LiveCli::print_out`（TUI 面板优先，非 TUI 回落 stdout），修复 TUI 中直接 `println!` 破坏 alternate screen 的问题。
- 未实现命令统一文案与 `/help` 过滤在 A2 已覆盖；`docs/command-status.md` 记录 31 条命令去向。
- A 阶段质量门：`cargo fmt --all --check`、`cargo clippy -p rusty-claude-cli --all-targets -- -D warnings`、`cargo test -p rusty-claude-cli` 全绿。

**A 阶段手工验收清单**

1. 启动 TUI → `/vim` → 前缀 `[I]>` → 输入 → `Esc` 变 `[N]>` 且保留内容 → `h l 0 $ x dd p u i` 行为符合 vim 直觉。
2. `/help` 中不再出现 `/login` `/share` `/theme` 等未启用项；输入 `/theme` 得到明确"未启用"提示。
3. `/keybindings` `/files` `/tasks` 有输出；`/copy` 后粘贴得到最近回复（终端支持 OSC52 时），否则从 `~/.claw/last-copy.txt` 取。
4. `/fast` 切换模型后，提示出现在 TUI 面板而非刷屏；`/exit` 空闲时立即退出。
5. 长会话滚动、F2 侧栏、`?` 帮助浮层不受影响。

### 2026-10-24 · B1 完成（只读 Web 视图）

启动方式：

```powershell
claw serve --port 4317 --cwd D:\claw-code-src   # --no-open 可禁止自动开浏览器
```

- 只绑定 `127.0.0.1`；启动打印带一次性 token 的 URL，所有 API 校验 token（缺失/错误 → 401）。
- 端点：`/`（内嵌单页）、`/api/sessions`、`/api/session/{id}`、`/api/session/{id}/stream`（SSE，内容变化时推送 `snapshot`）。
- 会话目录按工作区隔离（`<workspace>/.claw/sessions/<fingerprint>/`），`--cwd` 决定查看哪个工作区。
- 列表先按 mtime 排序截断 50 条，再读前 96KB 提取标题与 `workspace_root`，避免完整解析大 JSONL（实测 50 条 ≈ 470ms）。
- `session_path` 拒绝 `..`、路径分隔符与非法字符；非法 id → 400，合法但不存在 → 404。

**验证（实测）**：`/api/sessions` 200 / 50 条；`/api/session/{id}` 200 / 121 条消息；SSE 首个事件 `event: snapshot`；无 token 401；`../etc` 与不存在会话 404。

### 2026-10-24 · B2 完成（写入通道，审批延后）

| 端点 | 行为 |
|---|---|
| `POST /api/session/{id}/prompt` `{text, permission?}` | 校验 token/文本/权限模式后，以子进程 `claw --permission-mode <mode> -p <text>` 在**同一会话**上跑一轮（子进程通过内部环境变量 `CLAW_RESUME_SESSION` 指回该 JSONL），返回 202 `{status,pid,session}` |
| `POST /api/session/{id}/interrupt` | 杀掉该会话正在运行的子进程；没有在跑 → 409 |

设计要点：

- **复用而非另造**：写入走真实 CLI turn 逻辑（工具、权限、持久化、压缩全部一致），服务端只做调度与鉴权。
- **单写者**：`running` 表按会话 id 去重，同一会话并发 prompt → 409；子进程退出由后台 reaper 回收。
- **参数安全**：prompt 作为 argv 传递，不经 shell；权限模式白名单 `read-only | workspace-write | danger-full-access`。
- **失败可见**：子进程 stdout/stderr 丢弃，结果通过 SSE 从会话文件回流；无 key 时子进程快速失败，不会挂住服务。

**验证（实测）**：prompt → 202 `{"status":"running","pid":...}`；并发第二次 → 409；空文本 → 400；子进程把 `ping` 写入会话 JSONL 后退出（无残留进程）；interrupt 在无运行时返回 409、在运行中返回 200。

**已知边界**

1. **审批/提问通道未接入**：`-p` 是非交互模式，审批由权限模式决定（默认 `workspace-write`），Web 端不能逐个 Approve/Deny。要支持需把 `PermissionPrompter` 抽象成可多端消费的事件，属于下一阶段。
2. **同一会话不要同时开 TUI**：TUI 持有内存态且不检查 Web 的写锁；服务端只保证自身单写者。
3. **失败 turn 会留下用户消息**：与 CLI 行为一致（会话记录输入），Web 端会在消息流里看到。
4. 真实模型链路需要 `DEEPSEEK_API_KEY`；本次验证只覆盖到"子进程写入用户消息并退出"，未做带真实 key 的端到端生成。

---

## 9. 收口结论（2026-10-24）

### 交付物

| 文件 | 作用 |
|---|---|
| `rusty-claude-cli/src/settings.rs` | `~/.claw/settings.json`（vim 偏好持久化） |
| `rusty-claude-cli/src/tui/input_line.rs` | vim 模式（模式状态机 + 动作 + 7 个单测） |
| `rusty-claude-cli/src/tui/app.rs` | `/vim` 就地切换、`/exit` 拦截、输入框模式前缀 |
| `rusty-claude-cli/src/app.rs` | `/copy`(OSC52) `/files` `/tasks` `/keybindings` `/fast` `/feedback`、`print_out` |
| `rusty-claude-cli/src/commands_handler.rs` | `STUB_COMMANDS` 收缩到真正未实现的 23 条 |
| `rusty-claude-cli/src/serve.rs` + `src/serve/index.html` | `claw serve`：只读视图 + SSE + prompt/interrupt |
| `docs/command-status.md` | 31 条斜杠命令的逐条去向 |
| `docs/2026-10-24-personal-tui-and-web-ui-plan.md` | 本方案与执行记录 |

### 验收证据

```
cargo fmt --all --check                                      # 通过
cargo clippy --workspace --all-targets -- -D warnings        # 通过（41.7s）
cargo test -p rusty-claude-cli                               # 640 单元 + 集成全绿
./target/debug/claw.exe serve --no-open --port 4399 --cwd D:\claw-code-src
  GET /                      → 200（含 composer）
  GET /api/sessions?t=       → 200，50 条（排除旁路 jsonl 与空会话）
  GET /api/session/{id}      → 200，122 条消息
  GET .../stream             → SSE 首帧 event: snapshot
  POST .../prompt            → 202 {"pid":...}
  POST .../prompt (并发)      → 409
  POST .../prompt (空文本)    → 400
  POST .../interrupt         → 200 / 409
  无 token                   → 401
  ../etc、不存在会话          → 404
```

`cargo test --workspace` 有 2 个**与本次改动无关**的既有失败（runtime crate，均依赖本机状态/环境）：
`evolution_inbox::tests::archive_list_mark_processed_roundtrip`（`rust/.claw/evolution/inbox` 里已有历史文件导致重复归档判定）、
`source_root::tests::resolve_prefers_env_then_registry`（环境变量/注册表状态）。runtime 代码本次未改动。

### 使用方式

```powershell
# TUI：/vim 开关 vim；/help 只列可用命令；/copy 复制最近回复
# Web：只读浏览 + 在 Web 里发消息（需要在启动 claw serve 的环境里配置 DEEPSEEK_API_KEY）
claw serve --port 4317 --cwd D:\claw-code-src
```

### 明确未做（与第 7 节一致）

- 图片协议渲染（当前没有 `read_image` 类工具，先有需求再做）
- Web 端逐次审批（`-p` 非交互；需把 `PermissionPrompter` 抽象成事件）
- `/add-dir` 会话内热加工作根
- 多用户/鉴权体系/公网访问

---

## 10. 提交后的一步：Web 安全控制切片（2026-10-24）

在逐次审批可用之前，先补上单人使用真正需要的三个控制：

| 能力 | 实现 | 验证 |
|---|---|---|
| 每轮权限模式选择 | 输入框旁下拉：`workspace-write`（默认）/`read-only`/`danger-full-access`，随 prompt 发送；服务端白名单校验 | 非法模式 → 400；`read-only` → 202 |
| 运行状态 | `GET /api/session/{id}/status` → `{running}`；前端每 1.5s 轮询，运行中禁用发送并显示"运行中…（可点停止）" | 返回 `{"running":false}` 初始态；prompt 后由 `running` 表驱动 |
| 停止 | 复用 `POST /interrupt`（杀掉子进程） | 运行中 200 / 无运行 409 |

**为什么逐次审批仍推迟**：当前写入是子进程 `claw -p`，审批回调在子进程内部且是非交互的；要做逐次 Approve/Deny，必须二选一：
1. 把 turn 改为 serve 进程内执行，并注入自定义 `PermissionPrompter`（需要给 `LiveCli` 增加可注入 prompter 的入口 + 线程内运行 + 中断信号）；
2. 保留子进程，增加父子审批代理（本地 socket/HTTP 往返 + CLI 侧 prompter 改造）。

两条路都涉及 runtime/CLI 的接口改动，且需要真实 `DEEPSEEK_API_KEY` 才能端到端验证，因此单独作为下一步，不混进本次提交。

