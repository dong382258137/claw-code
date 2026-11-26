# Web-first 瘦身与性能优化方案（2026-11-25）

> 目标读者：项目所有者 · 范围：`D:\claw-code-src` · 前提：以 Web UI 为唯一产品界面，放弃 TUI 与 CLI。

---

## 0. 结论摘要

**可行，但“放弃 TUI/CLI”不是删目录，而是先解耦再删除。**

当前 Web 端在编译期依赖 TUI 特性：用 `--no-default-features` 关闭 TUI 后，`rusty-claude-cli` 有 5 处编译错误。因此实施顺序必须是：

1. 先把 Web 需要的类型和入口从终端渲染栈里抽出来（不改行为）；
2. 再删 TUI；
3. 再删 CLI 界面层；
4. 最后做能力搬迁、会话读取索引、定时任务与中文嵌入模型。

**已确认的决策**

| 决策项 | 结论 |
|---|---|
| CLI 边界 | 只保留 `serve` 启动入口，删除 REPL、TUI 和全部子命令 |
| 依赖裁剪 | 分阶段实测；deepseek-web 已放弃，直接移除 |
| 性能目标 | Web 交互延迟、启动与内存、分发体积、构建速度四项都做 |
| 改造方式 | 原地解耦再删，每阶段独立可编译、可回滚 |
| 必须搬迁的能力 | 导出会话、项目初始化说明、压缩与用量视图、后台/定时任务 |
| 向量检索 | 保留，模型换成 `bge-small-zh-v1.5`（512 维） |
| 会话读取 | SQLite 做索引与缓存，JSONL 仍是正文真相源 |
| 定时任务 | 真正到点执行；每个任务一个专属会话；停机期间只补最近一次 |
| 二进制体积 | 目标 ≤ 25 MB |
| 旧二进制名 | 保留 `claw-plus` 一个版本周期 |

**预期结果（需 Phase 0 实测确认）**

| 指标 | 当前 | 目标 |
|---|---|---|
| CLI crate 代码量 | 59,208 行 | 约 18,000–22,000 行 |
| 工作区总代码量 | 约 248,000 行 | 约 210,000 行 |
| release 二进制 | 71.5 MB | Phase 3 后 ≤ 60 MB；Phase 8 后 ≤ 25 MB |
| 服务启动到监听 | 579 ms | ≤ 300 ms |
| 空闲内存 | 28.9 MB | ≤ 22 MB |
| 会话流轮询 | 每 0.9 秒全量重读 JSONL | 未变化时只读文件元数据，变化时增量解析 |
| 会话列表 | 每工作区最多 50 条、每次重新扫描 | 索引查询，支持全量分页 |
| 构建 | 默认包含 TUI + ONNX + wasmtime | 默认不含 TUI + wasmtime |

---

## 1. 现状与证据

### 1.1 代码分布

| crate | 行数 | 与 Web 的关系 |
|---|---:|---|
| `runtime` | 124,417 | 引擎主体，Web 与 CLI 共用 |
| `rusty-claude-cli` | 59,208 | Web 服务 + TUI + CLI 全在这里 |
| `tools` | 19,040 | 工具实现，共用 |
| `commands` | 12,960 | 斜杠命令、技能/插件市场；市场部分与 Web 共用 |
| `api` | 11,220 | Provider 客户端 |
| `im-bridge` | 4,509 | 独立二进制，进程内直连引擎，不依赖 CLI |
| `plugins` | 4,469 | 插件系统 |
| `web-bridge` | 2,892 | DeepSeek 网页登录后端，仅 `--model deepseek-web` 使用 |
| 其他 | 6,000+ | ACP、shell、telemetry、mock、eval |

`rusty-claude-cli` 内部：

| 模块 | 行数 | 判定 |
|---|---:|---|
| `tui/` | 11,820 | 纯 TUI，可删 |
| `tui-ports/` | 2,316 | 纯 TUI，可删 |
| `serve.rs` + `serve/scheduler.rs` | 7,513 | Web 主体，保留 |
| `app.rs` | 4,966 | 混合：`LiveCli` 是 Web 的执行内核，其余是 REPL/TUI 命令分发 |
| `commands_handler.rs` | 2,706 | 斜杠命令实现；少量函数需搬到 Web |
| `doctor.rs` | 2,788 | CLI 专属，可删 |
| `format.rs` | 2,477 | CLI 帮助与输出格式，可删 |
| `render.rs` | 1,570 | 终端渲染，可删 |
| `streaming.rs` | 1,959 | 终端流式渲染，可删；但 `StatusEmitter` 类型要保留 |
| `tool_display.rs` | 869 | 终端工具卡片，可删 |
| `session_mgr.rs` | 2,433 | 会话管理，Web 共用 |
| `plugin_state.rs` | 1,512 | Web 市场共用 |
| `voice.rs` | 936 | Web 语音共用 |
| `upgrade_helper.rs` | 830 | Web 热升级共用 |
| `checkpoint.rs` | 817 | Web 检查点共用 |
| `llm_clients.rs` | 804 | 共用 |
| `init.rs` | 568 | `/init` 逻辑，搬到 Web 后保留 |
| `ultraplan.rs` | 314 | CLI 专属，可删 |
| `input.rs` / `paste.rs` / `suggestion.rs` / `locale.rs` / `settings.rs` | 974 | CLI 输入层，可删 |
| `lib.rs` | 1,770 | 参数解析与子命令分发，只保留入口 |
| `tests.rs` | 5,429 | 随模块删除同步收缩 |

### 1.2 三处硬耦合

1. **流式事件类型挂在终端渲染栈上**
   Web 每轮调用 [`set_status_emitter`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L2060)，该方法在 [`app.rs`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/app.rs#L3365-L3368) 被 `#[cfg(feature = "full-tui")]` 门控；`StatusEmitter` 定义在 `streaming.rs`，经 `render.rs` 依赖 `crossterm` 与 `syntect`。

2. **CLI 核心代码反向引用 TUI 模块**
   [`enforce_broad_cwd_policy`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/app.rs#L210-L248) 直接调用 `crate::tui_ports::project_picker`，没有特性门控。

3. **执行内核与界面层混在一个结构里**
   [`LiveCli`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/app.rs#L674-L742) 同时持有 Web 需要的运行态（`external_abort_signal`、`session_processes`、`steer_inbox`）和 TUI 专用的 `tui_output`、`status_emitter`、`diag_callback`、`tool_result_callback`。

**验证结果**：`cargo check -p rusty-claude-cli --no-default-features` 报 5 个错误，全部来自上述耦合。

### 1.3 二进制构成

release 二进制 71.5 MB（2026-11-26 复核 `target/release/claw.exe`，早期记录的 75.0 MB 已过时）。编译产物大小（rlib）可以反映链接权重：

| 依赖 | rlib 大小 | 说明 |
|---|---:|---|
| `ort_sys`（ONNX Runtime） | 292.6 MB | 本地向量检索，链接进二进制约 40–50 MB |
| `wasmtime` + `wasmtime_environ` + `cranelift` | 39.3 MB | 仅 DeepSeek 网页登录后端使用，链接约 10–15 MB |
| `ratatui` + `syntect` + `crossterm` + `pulldown-cmark` + `rustyline` | 15.8 MB | TUI/CLI 栈，链接约 6–10 MB |
| 内嵌 Web 前端 | 0.69 MB | `web/dist/index.html`，单文件内联 |

### 1.4 实测基线

| 指标 | 值 | 测法 |
|---|---|---|
| 启动到监听 | 579 ms | release 二进制，临时工作区，端口探测 |
| 空闲内存 | 28.9 MB | 监听后 500 ms 的 Working Set |

### 1.5 Web 端现有性能问题

1. **会话流每 0.9 秒全量重读**
   [`api_stream`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1768-L1783) 每个轮询周期调用 [`session_view`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L5558-L5560)，后者 `Session::load_from_path` 重新读盘并解析整个 JSONL。每个打开的浏览器标签各跑一份，长会话越用越慢。

2. **会话列表每次重新扫描，且有 50 条硬上限**
   `all=1` 聚合时每个工作区只取最近 50 条，`total` 也按截断后的数量计算。第 51 条以后的历史会话在侧栏里不存在。

3. **流式用量被丢弃**
   [`StatusEvent::Usage(_) => return`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L2150)，前端只能轮末从快照统计，流式期间没有上下文占用反馈。

4. **恢复会话没有挂全文索引**
   [`new_cli_session_in`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/session_mgr.rs#L1225-L1241) 会挂 `.claw/history.db`，但 Web 恢复会话走的 [`load_session_reference_in`](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/session_mgr.rs#L1370-L1384) 不挂。结果是 Web 会话里的 `session_search` 能力很可能一直失效。

5. **多会话并发受常驻 worker 数量影响**
   [`MAX_HOT_WORKERS = 8`、空闲 90 秒退出](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve/scheduler.rs#L34-L41)。每个常驻 worker 持有一份 `LiveCli`，包含系统提示、仓库拓扑与 MCP 客户端；并发会话多时内存会抬升。

### 1.6 已有的可复用基础设施

- **SQLite 已在用**：`rusqlite` bundled（[runtime Cargo.toml](file:///d:/claw-code-src/rust/crates/runtime/Cargo.toml#L16)）。
- **FTS5 全文索引已存在**：[`HistoryIndex`](file:///d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L54-L97)，含 v1→v4 迁移、CJK 分词、可选向量表 `history_vectors`。
- **记忆注入已存在**：每轮取 top-3 记忆渲染为 `# Relevant Memories` 注入请求（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3990-L4020)）。
- **调度器已存在**：全局并发 4、每工作区 1、每会话排队 5（[scheduler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve/scheduler.rs#L31-L83)）。

---

## 2. 目标架构

### 2.1 crate 结构

实施期间保留 `rusty-claude-cli` 名字，收尾时改名为 `claw-server`。

```
rust/crates/
  runtime/        引擎：会话、记忆、工具运行时、全文索引（不变）
  tools/          工具实现（不变）
  api/            Provider 客户端（不变）
  plugins/        插件系统（不变）
  commands/       技能/插件市场、需要搬迁的命令逻辑（收缩）
  telemetry/      遥测（不变）
  claw-server/    Web 服务、调度器、turn 内核适配、市场 API（由 CLI crate 收缩而来）
  im-bridge/      独立二进制（保留，不依赖主二进制）
```

`web-bridge`、`claw-shell`、`claw-acp` 从主二进制依赖图中移除；`im-bridge` 仍可使用 `claw-shell`/`claw-acp`，但不再参与主二进制的构建。

### 2.2 模块边界

`claw-server` 内部划分为四层，依赖方向单向：

```
http/      axum 路由、鉴权、SSE/WS
service/   turn 执行、调度、审批、市场、任务、cron
core/      LiveCli 抽出的执行内核（只依赖 runtime/tools/api/plugins）
infra/     会话索引、设置、热升级、路径解析
```

`core` 不允许引用任何 `http`、`tui`、终端输出相关类型。

### 2.3 删除与保留清单

| 删除 | 保留 |
|---|---|
| `tui/`、`tui-ports/` | `serve.rs`、`serve/scheduler.rs` |
| `format.rs`、`render.rs`、`streaming.rs`、`tool_display.rs` | `session_mgr.rs`、`checkpoint.rs`、`upgrade_helper.rs` |
| `doctor.rs`、`input.rs`、`paste.rs`、`suggestion.rs`、`locale.rs`、`settings.rs`、`ultraplan.rs` | `voice.rs`、`llm_clients.rs`、`plugin_state.rs`、`init.rs` |
| `lib.rs` 的参数解析与子命令分发 | `serve` 入口、`--upgrade-helper` 分支 |
| `ratatui`、`crossterm`、`ansi-to-tui`、`syntect`、`similar`、`rustyline`、`pulldown-cmark`、`unicode-width`、`sys-locale` | `axum`、`portable-pty`、`tokio`、`reqwest`（Web 语音与市场） |
| `web-bridge` 依赖、`--model deepseek-web` | 标准 API Provider |

### 2.4 入口

- 唯一二进制：`claw`（可保留 `claw-web.exe` 的文件名识别，双击等价于 `claw serve`）。
- `main_entry` 只处理三件事：`serve`、`--upgrade-helper`、`--version/--help`。
- 不再提供 `prompt`、`doctor`、`status`、`config`、`skills`、`mcp` 等子命令。

---

## 3. 实施阶段

每个阶段独立提交、独立验证、可回滚。前一阶段未通过验收，不进入下一阶段。

**实施进度**

| 阶段 | 状态 | 备注 |
|---|---|---|
| Phase 0 基线 | 部分完成 | 已有：二进制 71.5 MB、启动 579 ms、空闲 RSS 28.9 MB。待补：冷启动构建耗时、1 万条消息会话的打开与轮询耗时（Phase 5 前补齐） |
| Phase 1 解耦 | 已完成（2026-11-26） | `--no-default-features` 编译通过。`StatusEmitter` 早已定义在 `runtime`，本次只解除调用侧门控；`OutputVerbosity` 暂不搬动，随 `render.rs` 在 Phase 3 一并删除，现阶段搬动不改变依赖图 |
| Phase 2 删除 TUI | 已完成（2026-11-26） | 删除 `tui/`（12 文件）、`tui-ports/`（4 文件）与 `ratatui`/`ansi-to-tui`/`similar` 依赖；`full-tui` 特性移除。**保留 `status_emitter`**（Web SSE 依赖），**保留 `crossterm`**（`render.rs`/`format.rs`/`tool_display.rs` 仍用，Phase 3 随 CLI 界面层删除）。实测：二进制 70.59 MB（-0.9 MB）、启动到监听 690 ms、监听后 500 ms RSS 12.5 MB（单次采样，构建噪声未排除） |
| Phase 3 删除 CLI 界面层 | 已完成（2026-11-26） | 删除 `commands_handler.rs`、`doctor.rs`、`format.rs`、`input.rs`、`locale.rs`、`paste.rs`、`render.rs`、`settings.rs`、`streaming.rs`、`suggestion.rs`、`tool_display.rs`、`ultraplan.rs` 与 6 个 CLI 集成测试文件；`streaming.rs` 收敛为 `api_client.rs`（去终端渲染），`tool_display.rs` 收敛为 `tool_executor.rs`（仅保留执行器）。`session_mgr.rs` 收缩为会话生命周期 + 导出（供 Phase 4 复用）。依赖移除 `crossterm`/`rustyline`/`pulldown-cmark`/`syntect`/`sys-locale`/`unicode-width`/`web-bridge`。实测：二进制 58.7 MB（较 Phase 2 -11.9 MB），启动到监听 560–772 ms（3 次采样，**未达 400 ms**，待 Phase 5/8 继续），首页 200（718 KB）、`/api/sessions` 200、真实对话一轮成功（含工具权限拒绝与回复落盘） |
| Phase 4 能力搬迁 | 已完成（2026-11-26） | 新增 `GET /api/session/{id}/export?format=md\|json`（复用 `session_mgr` Markdown 渲染与 `Session::to_json`）、`POST /api/workspace/init`（复用 `init::initialize_repo`，返回逐文件状态与 CLAUDE.md 摘要）、`POST /api/session/{id}/compact`（手动压缩并落盘，返回前后 token/移除条数；压缩后 token 未减少则取消）、定时任务骨架 `GET/POST /api/workspace/crons` + `POST .../{id}/toggle` + `DELETE .../{id}` + `POST .../{id}/run`（返回 501，Phase 6 实装）。SSE 转发 `usage` 事件；会话快照 `stats` 增加 `estimated_tokens`/`context_window`。前端：会话行导出、工作区菜单初始化入口、输入框上方上下文占用条 + 压缩按钮、TasksPane 定时任务区。实测（debug serve + 临时工作区）：两轮真实对话成功、`usage` 事件 4 条、导出 md/json 200（带 `Content-Disposition`）、init 生成 `.claw.json`/`.gitignore`/`CLAUDE.md`、压缩 3991→1292 tok（移除 3 条并落盘）、cron 新建/启停/删除全通且立即执行 501。`cargo test -p rusty-claude-cli` 144 通过，cron 专项 27 通过 |
| Phase 5 会话读取索引 | 已完成（2026-11-26） | 新增 `runtime::session_index`（`<ws>/.claw/sessions.db`，WAL）：`sessions` / `messages` 双表（含 `workspace,jsonl_mtime_ms` 索引），`Session` 追加消息时写消息字节区间（失败静默），`save_to_path` 整份重写后重建全部区间，新增 `append_jsonl_tail_from` 只消费新增完整行。serve：`/api/sessions` 走索引（分页 `limit`≤500、`all=1` 聚合多工作区，去掉每工作区 50 条硬上限），首次访问按 size/mtime 对账懒建/清理索引，DB 不可用时回退旧扫描；`/api/session/{id}/stream` 以 `(size,mtime)` + 头 4 KiB/边界 128 B 指纹判定，未变发 keepalive、变长走增量解析，快照 LRU 8；`/api/block` 按索引字节区间只读单条消息。Web 恢复会话补挂 `history.db`。**偏差**：`running_state` 未落库（运行态只在调度器内存，仍在响应时装饰），避免双写。实测（debug serve，临时工作区，1000 小会话 + 1×1 万条消息）：列表首查 767 ms（建 1001 条索引）、热查 2.3–5.6 ms、500 条分页 16 ms；1 万条会话 `/api/session` 冷读 476 ms、SSE 首快照 1299 ms（含固定 900 ms 轮询间隔）、未变轮询增量 5–11 ms；运行中追加消息 2.06 s 内推送含新消息快照；删 `sessions.db*` 重启首查 452 ms 自动重建、二次 3.9 ms、block 仍取全文。`cargo test -p runtime session_index` 4 通过、`session::tests` 增量解析 2 通过，`cargo test -p rusty-claude-cli` 144 通过，clippy 无告警 |
| Phase 6 定时任务真实执行 | 已完成（2026-11-26） | 新增 `serve/cron_expr`（5 段 cron：`*`、`*/n`、`a-b`、列表，按本地时区求下次时间，日/周同时受限走标准「或」语义）与 `serve/cron_runner`（15 s tick、进程内下次到期表、启动按 `last_run_at`→`created_at` 重算只补跑最近一次）。`CronEntry` 增加 `session_id`/`last_status`/`last_finished_at`/`last_error`（`serde(default)` 兼容旧文件），新增 `bind_session`/`record_start`/`record_finish`；到期任务封装为 `PromptJob` 进入现有 `Scheduler`，不绕过并发门闸；专属会话首次执行时创建（标题 `定时任务 · 名称`，删除后下次执行自动重建，不提供改绑）；权限继承 Web 设置且最高 `workspace-write`；同一任务上次未结束时记 `skipped` 并推跳过事件。API：列表返回 `next_run_at`/`last_status`/`schedule_error`，新建时校验表达式（非法 400），`POST .../{id}/run` 实装为立即入队（运行中 409）；worker 收尾写回结果并向该会话事件流推送 `kind=cron` 的 `start`/`end`/`skipped`；TasksPane 显示下次执行/上次结果并支持立即执行。实测（debug serve + 真实模型）：每分钟任务连续 3 次 `success`，会话落盘 3 轮 user/assistant 且 `run_count=3`；重启后 8 s 内不重复，下一分钟继续第 4 次；全局并发 4 被占满时手动触发任务状态 `queued>running` 后 `success`；重叠触发返回 409 且 `run_count` 不增；SSE 收到 start/end 各 1 条；非法表达式 400。`cargo test -p rusty-claude-cli` 151 通过（新增 cron_expr 7）、`cargo test -p runtime cron` 29 通过，clippy 干净 |
| Phase 7 中文嵌入模型与 ONNX 动态加载 | 已完成（2026-11-26，体积目标未达） | `fastembed` 改 `default-features = false` + `ort-load-dynamic` + `hf-hub-native-tls`（不再静态链接 ONNX、去掉 image-models 依赖），模型切 `BGESmallZHV15`（512 维，`name()` = `bge-small-zh-v1.5`）。新增 `runtime::EmbeddingStatus` / `embedding_status()`：serve 启动后台预热，`/api/meta.embedding` 上报 `ready/keyword/loading/disabled`，设置页「关于」新增「语义检索」状态行（降级显示“关键词降级”并附原因）。初始化前预检 `onnxruntime.dll`（`ORT_DYLIB_PATH` → exe 同目录 → cwd），缺库/版本不兼容（ort 内部 panic 已兜住）统一返回 None 走关键词降级。`history_meta` 增加 `embedding_model`：与当前模型不一致时清空 `history_vectors` 并后台分批重建（独立连接 + busy timeout + EXISTS 守卫防孤儿向量）；记忆 L1 索引本就只存文本，加载时重算，未改。评测（25 条中文记忆查询对，top-3）：BGE-small-en-v1.5 11/25 = 44.0%，bge-small-zh-v1.5 20/25 = 80.0%，保留切换。实测：`claw.exe` 58.7 → **40.41 MB**（-18.3 MB，`onnxruntime.dll` 16.0 MB 独立分发）；移走 DLL 后服务正常启动、首页 200、`/api/meta` 返回 `state=keyword`；旧工作区（schema v4、384 维旧向量、无 `embedding_model`）首次对话后自动写入 `embedding_model` 并将 2 条旧向量重建为 2048 B（512×f32），日志 `[history] rebuilt 2 vector(s)`。**偏差**：主二进制未达 ≤25 MB，剩余体积（wasmtime/tiktoken 词表/axum/portable-pty 等）留 Phase 8 排查。`cargo test -p runtime history` 45 通过（新增模型切换重建回归测试）、`cargo test -p rusty-claude-cli` 151 通过，clippy 干净 |
| Phase 8 体积实测与动态加载收口 | 已完成（2026-11-26） | release profile 增加 `opt-level="z"` + `lto="fat"` + `codegen-units=1` + `strip="symbols"`：`claw.exe` 40.41 → **22.88 MB**（较 Phase 3 静态基线 58.7 MB 降 61%，≤25 MB 达成）。语义检索从“启动预热”改为“启动仅探测 DLL、首次检索加载模型”（新增 `runtime::probe_embedding`，状态可更新）：空闲 RSS 10.3–10.6 MB，加载模型后 ≈163 MB，首轮 `build_runtime` 1.07 s（含模型加载）。三组实测见下方对比表（静态基线 58.7 MB / 560–772 ms / 12.5 MB；动态+profile 22.88 MB / 503–516 ms / 10.3–10.6 MB；移走 DLL 同二进制 / 501–514 ms / 10.6 MB，`/api/meta` state=keyword，首页 200、737 KB）。`cargo bloat`：CLI 4.6 / tools 4.4 / runtime 4.2 / chromiumoxide 3.9 MiB，未再删功能依赖（wasmtime、tiktoken 已不在依赖图）。新增 `scripts/package-release.ps1`（exe×2 + DLL×2 + README，zip 实测 22.38 MB）。**遗留**：启动到监听 503–516 ms，仍未达 ≤300 ms 目标（Phase 9 前再评估）。`cargo test -p rusty-claude-cli` 151 通过、`cargo test -p runtime history` 45 通过，clippy 干净 |

### Phase 0：基线与测量脚本

**改动**

- 不修改业务代码。
- 建立三项基线：构建时间（`cargo check -p rusty-claude-cli`、`cargo build --release`）、运行指标（启动、空闲内存、首轮延迟）、长会话指标（1 万条消息的打开与轮询 CPU）。

**验收**

- 基线数据写入本文件第 4 节表格，后续每阶段用同一口径复测。

**回滚**

- 无代码改动。

### Phase 1：解耦（行为不变）

**改动**

1. 把 `StatusEmitter` 类型定义从 `streaming.rs` 移到 `runtime`（或 `core/status.rs`），只保留 `Arc<dyn Fn(StatusEvent) + Send + Sync>`，不依赖终端渲染。
2. `OutputVerbosity` 从 `render.rs` 抽到 `core`，或者从 `LiveCli::new` 签名中移除（Web 不使用）。
3. `diag_log` 从 `#[cfg(feature = "full-tui")]` 移出。
4. 删除 `app.rs` 里对 `tui_ports::project_picker` 的引用；broad-cwd 的交互式选择属于 CLI 行为，Web 不需要。
5. `LiveCli` 中 Web 需要的字段（`external_abort_signal`、`session_processes`、`steer_inbox`、`upgrade_signal`）去掉特性门控；TUI 专用字段保持门控。

**验收**

- `cargo check -p rusty-claude-cli --no-default-features` 通过（当前 5 个错误清零）。
- `cargo check -p rusty-claude-cli`（默认特性）通过。
- `cargo test -p rusty-claude-cli` 全绿。
- 启动 serve，完成一轮真实对话，前端流式输出与工具卡片正常。

**风险**

- `StatusEmitter` 移动会牵动 TUI 与 Web 两处调用点；用类型别名过渡，不改事件内容。

### Phase 2：删除 TUI

**改动**

1. 删除 `src/tui/`、`src/tui-ports/`。
2. 删除 `ratatui`、`crossterm`、`ansi-to-tui` 依赖与 `full-tui` 特性。
3. 删除 `LiveCli` 的 `tui_output`、`status_emitter`、`diag_callback`、`tool_result_callback` 字段与方法。
4. 删除 `run_tui_repl`、`run_tui_repl_entry`、`TuiSilentPermissionPrompter`、`run_turn_tui`。

**验收**

- 工作区 `cargo check` / `clippy -- -D warnings` / `test` 全绿。
- serve 冒烟：新建会话、流式对话、工具审批、停止、文件面板、终端面板。
- 记录二进制体积与启动时间变化。

**预期**

- 减少约 14,000 行。
- 二进制减少约 5–8 MB。
- 编译时间下降（不再编译 ratatui/syntect 及其依赖）。

### Phase 3：删除 CLI 界面层

**改动**

1. 删除 `lib.rs` 的参数解析与 `CliAction` 分发，只保留 `main_entry` 的 serve / upgrade-helper 分支。
2. 删除 `format.rs`、`render.rs`、`streaming.rs`、`tool_display.rs`、`doctor.rs`、`input.rs`、`paste.rs`、`suggestion.rs`、`locale.rs`、`settings.rs`、`ultraplan.rs`。
3. `app.rs` 只保留 `LiveCli` 与 turn 执行、运行时构建、系统提示装配；删除 `run_repl`、斜杠命令分发、`print_*` 系列、`tui_println`。
4. `commands_handler.rs` 中 Web 要用的函数（导出、初始化、压缩、状态）抽到 `commands` crate 或 `service` 层，其余删除。
5. `Cargo.toml` 移除 `rustyline`、`pulldown-cmark`、`syntect`、`similar`、`unicode-width`、`sys-locale`。
6. 删除 `web-bridge` 依赖与 `--model deepseek-web` 分支。
7. 二进制只保留 `claw`；`claw-plus` 保留一个版本周期作为文件名兼容。

**验收**

- 全部 Web 路由冒烟通过（会话、流式、审批、文件、变更、任务、附件、终端、市场、语音、设置、热升级）。
- `cargo build --release` 产物 ≤ 60 MB。
- 启动到监听 ≤ 400 ms。
- 仓库里的 CLI 使用文档与命令示例同步更新，不再宣传已删除的能力。

**回滚**

- 保留 Phase 3 前的 git tag；如需回退，恢复该 tag 并重新构建。

### Phase 4：能力搬迁

按以下顺序做，每项单独提交、单独验证。

#### 4.1 导出会话

- 新增 `GET /api/session/{id}/export?format=md|json`。
- 复用现有导出逻辑（`commands_handler` 中的 `/export` 实现）。
- 前端会话菜单增加“导出”。

#### 4.2 项目初始化说明

- 新增 `POST /api/workspace/init`。
- 复用 `init.rs` 的生成逻辑；返回生成的文件路径与内容摘要。
- 前端在设置页或工作区菜单增加入口。

#### 4.3 压缩与用量视图

- 后端转发 `StatusEvent::Usage`（当前被直接丢弃）。
- 新增 `POST /api/session/{id}/compact`，返回压缩前后 token 与保留消息数。
- 前端输入区上方增加上下文占用条；流式期间实时更新；压缩发生时显示提示。

#### 4.4 后台/定时任务

见 Phase 6。Phase 4 只负责前端入口与 API 骨架。

**验收**

- 四项都有可点击入口，且端到端可用（不依赖 CLI）。

### Phase 5：会话读取索引

**目标**：列表不再扫全盘，快照不再全量重解析，旧会话零迁移。

**数据模型**

新建 `.claw/sessions.db`（与 `history.db` 分开，避免搜索索引重建影响元数据）：

```sql
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  workspace TEXT NOT NULL,
  title TEXT,
  created_at_ms INTEGER,
  updated_at_ms INTEGER,
  message_count INTEGER,
  jsonl_size INTEGER,
  jsonl_mtime_ms INTEGER,
  running_state TEXT
);
CREATE TABLE messages (
  session_id TEXT NOT NULL,
  idx INTEGER NOT NULL,
  byte_start INTEGER NOT NULL,
  byte_end INTEGER NOT NULL,
  role TEXT,
  timestamp_ms INTEGER,
  PRIMARY KEY (session_id, idx)
);
```

**写入路径**

- 在 `Session` 追加消息的持久化路径上挂一个可选索引写入器，与现有 `history_index` 同构：写入失败静默降级，不影响会话主流程。
- 索引只做缓存：任何不一致都可以通过“删除并重建”修复。

**读取路径**

- `/api/sessions`：只查 `sessions` 表，支持分页与全量；不再有每工作区 50 条上限。
- `/api/session/{id}/stream`：每轮只 `stat` 文件；`(size, mtime)` 未变则直接返回 keepalive；变化时从上次解析到的字节偏移继续。
- 会话快照按 session 缓存在内存中，按索引版本失效。
- `/api/block` 的全文回读继续按消息序号定位，改为查索引里的字节区间。

**顺带修复**

- Web 恢复会话时挂上 `history.db`（与新建会话一致），恢复 `session_search`。
- 旧会话首次访问时懒建索引；后台线程可选地批量补齐历史。

**验收**

- 1 万条消息的会话：首次打开 ≤ 500 ms，之后每次轮询 ≤ 10 ms。
- 1000 个会话的工作区：列表接口 ≤ 50 ms。
- 删除 `sessions.db` 后重启，索引自动重建，会话内容不丢。
- 运行中的会话在 Web 与磁盘之间保持一致（写入后立即刷新）。

**风险**

- 双写不一致：索引必须可重建，且任何读取都要能回退到 JSONL。
- 并发写：同一会话只允许一个写者（现有调度器已保证每工作区并发 1）。

### Phase 6：定时任务真实执行

**现状**

`CronRegistry` 只有登记、列出、删除、禁用，没有触发器（[team_cron_registry.rs](file:///d:/claw-code-src/rust/crates/runtime/src/team_cron_registry.rs#L303-L426)）。Web 任务面板只是从工具调用里提取 `CronCreate`/`CronDelete` 展示。

**改动**

1. **依赖**：增加 `cron` 表达式解析库（体量很小），或在 `runtime` 内实现最小 cron 解析（支持 `分 时 日 月 周`）。优先用现成库，减少自测成本。
2. **持久化**：`CronRegistry` 持久化到 `.claw/cron.json`（现有 `with_persistence` 已支持）。
3. **触发器**：serve 进程内一个 tick 线程，每 15 秒扫描到期任务。
4. **执行**：到期任务封装为 `PromptJob` 进入现有 `Scheduler`，不绕过并发门闸与权限模式。
5. **会话归属**：每个定时任务绑定一个专属会话（首次执行时创建），会话标题带任务名；不提供改绑到已有会话的入口。
6. **错过策略**：服务停机期间到点的任务，启动时只补跑最近一次；补跑同样受并发限制。
7. **安全**：定时任务默认继承工作区的权限模式，最高 `workspace-write`；不允许定时任务默认使用 `danger-full-access`。
8. **UI**：任务面板新增“定时任务”区，显示下次执行时间、上次结果、启停、删除、立即执行。
9. **事件**：任务开始与结束时向该会话的事件流推送事件，侧栏显示运行状态。

**验收**

- 创建一个每分钟执行的任务，连续观察三次执行，会话记录与 UI 状态一致。
- 重启 serve 后任务继续执行，不错乱、不重复。
- 并发 4 个会话跑满时，定时任务按队列等待而不是抢占。

**风险**

- 时区：统一按本地时区解析与展示。
- 长任务重叠：同一任务上一次未结束时，本次跳过并记录一条“跳过”事件。

### Phase 7：中文嵌入模型与 ONNX 动态加载

**目标**

主二进制 ≤ 25 MB，同时保留中文语义检索。

**关键发现**

`fastembed 5.17.3` 自带 `ort-load-dynamic` 特性（映射到 `ort/load-dynamic`），可以让 ONNX 运行时在启动时从 `onnxruntime.dll` 动态加载，而不是静态链接进主程序。这样：

- 主二进制不再包含约 40–50 MB 的 ONNX 静态库；
- 中文模型照常可用；
- DLL 缺失或加载失败时，沿用现有降级路径退化为关键词匹配，不阻塞对话。

**改动**

1. `fastembed` 依赖改为 `default-features = false`，启用 `ort-load-dynamic` 与 `hf-hub-native-tls`。
2. 模型从 `BGESmallENV15`（384 维）切到 `BGESmallZHV15`（512 维），同步修改 `dim()`。
3. 维度变更处理：
   - `history_meta` 增加 `embedding_model` 键；启动时与当前模型比对，不一致则清空 `history_vectors` 并后台重建。
   - 记忆的 L1 索引只存文本、不存向量，加载时会重新计算，无需迁移。
4. 首次运行下载模型到缓存目录；下载失败沿用关键词降级。
5. **分发方式**：`onnxruntime.dll` 与主程序一起分发，但不内嵌；缺失时设置页明确提示“语义检索不可用，当前为关键词匹配”。版本要求 ≥ 1.24（ort `api-24`），可从 PyPI `onnxruntime==1.24.2` 的 `win_amd64` wheel 中提取 `onnxruntime/capi/onnxruntime.dll`；也可通过 `ORT_DYLIB_PATH` 指定任意可用路径。
6. 评测量化：准备 20–30 条中文记忆查询对，对比英文模型与中文模型的 top-3 命中率；只有明显提升才保留切换。

**验收**

- 主二进制 ≤ 25 MB，`onnxruntime.dll` 独立存在。
- 中文查询的 top-3 命中率相对英文模型有提升（记录数据）。
- 移走 DLL 后服务正常启动，语义检索退化为关键词，界面有提示。
- 旧工作区首次启动后向量索引自动重建，`session_search` 正常。

### Phase 8：体积实测与动态加载收口

**改动**

1. 对比三种构建：
   - 当前静态链接（基线）；
   - TUI/CLI 已删 + `ort-load-dynamic`；
   - 同上并移走 `onnxruntime.dll`（模拟未安装）。
2. 记录各自的二进制体积、`cargo build --release` 时间、`cargo check` 时间、启动时间、空闲内存、首轮延迟。
3. 确认主二进制 ≤ 25 MB；若未达到，继续排查剩余体积来源（`axum`、`portable-pty`、`tiktoken` 类词表等）。
4. 打包脚本同步：主程序 + `onnxruntime.dll` + 前端资源，文档写明 DLL 的作用与缺失行为。

**验收**

- 一张构建对比表，数据可复现。
- 主二进制 ≤ 25 MB。
- 移走 DLL 后服务正常启动，语义检索退化为关键词，界面有提示。

**实测（2026-11-26，Windows / release）**

| 构建 | 二进制 | release 构建 | cargo check（增量） | 启动到监听 | 空闲 RSS | 首轮 |
|---|---:|---:|---:|---:|---:|---:|
| 静态链接基线（Phase 3） | 58.7 MB | 未采集 | 未采集 | 560–772 ms | 12.5 MB | 未采集 |
| 动态加载 + 体积 profile | **22.88 MB** | 9m23s（全依赖）/ 6m13s（仅 CLI 改动） | 15.2 s（runtime）/ 17.8 s（CLI） | 503–516 ms（3 次） | 10.3–10.6 MB（未加载模型）；首次检索后 ≈163 MB | `build_runtime` 1.07 s（含模型加载；远端回复受网络影响未计入） |
| 同上，移走 `onnxruntime.dll` | 22.88 MB（同一二进制） | 同左 | 同左 | 501–514 ms | 10.6 MB | 同左（关键词降级） |

口径与复现：

- 启动到监听：`claw.exe serve --port <p> --cwd <临时目录> --no-open`，每 10 ms 尝试 TCP 连接，各 3 次。
- 空闲 RSS：监听后 500 ms 的 Working Set（未打开会话、未触发检索）。
- 首轮：`POST /api/sessions` 后 `POST /api/session/{id}/prompt`，从 serve 日志读取 `build_runtime` 耗时；远程模型回复受网络影响，未纳入对比。
- 构建/检查：`cargo build --release -p rusty-claude-cli`、`cargo check -p rusty-claude-cli`（依赖已缓存）。
- 体积来源：`cargo bloat --release -p rusty-claude-cli --bin claw --crates`，`.text` 29.6 MiB；最大项 `rusty-claude-cli` 4.6 / `tools` 4.4 / `runtime` 4.2 / `chromiumoxide` 3.9 / `std` 2.2 / `reqwest` 1.4 MiB。未再裁剪功能依赖（chromiumoxide 为浏览器工具所需；wasmtime、tiktoken 已不在依赖图中）。

体积收口手段：`[profile.release]` 增加 `opt-level = "z"`、`lto = "fat"`、`codegen-units = 1`、`strip = "symbols"`（40.41 → 22.88 MB，较 Phase 3 静态基线降 61%）。语义检索改为启动时只探测 DLL（`runtime::probe_embedding`），模型首次检索时加载，空闲进程不再常驻 ~120 MB 模型内存。

**打包**：`powershell -ExecutionPolicy Bypass -File scripts/package-release.ps1` 生成 `dist/claw-<version>-win-x64/`（`claw.exe`、`claw-plus.exe`、`onnxruntime.dll`、`onnxruntime_providers_shared.dll`、`README.txt`）与同名 zip（实测 22.38 MB）。前端资源已内嵌进 exe，无需单独分发。DLL 要求 ≥1.24（ort `api-24`），来源与缺失行为见脚本头注释与包内 README。

### Phase 9：收尾

1. crate 改名 `rusty-claude-cli` → `claw-server`。
2. 删除 README、USAGE、CLAUDE.md 中关于 TUI/CLI 的描述，改为 Web 启动、配置、排障说明。
3. 更新 CI：只构建 `claw` 与 `im-bridge`；增加 Web 前端构建校验。
4. 清理 `PARITY.md` 中已删除能力的条目。
5. 保留一个 git tag 作为回退点。

---

## 4. 性能目标与测量方法

### 4.1 指标表

| 指标 | 基线 | 目标 | 测量方法 |
|---|---:|---:|---|
| 启动到监听 | 579 ms | ≤ 300 ms | 启动 serve，轮询端口 |
| 空闲内存 | 28.9 MB | ≤ 22 MB | 监听后 500 ms 的 Working Set |
| release 二进制 | 71.5 MB | Phase 3 ≤ 60 MB；Phase 8 另定 | 文件大小 |
| `cargo check` 冷启动 | 待测 | 下降 30% 以上 | 清空 `target` 后计时 |
| 1 万条消息首次打开 | 待测 | ≤ 500 ms | 造数据 + 浏览器 Network 面板 |
| 会话流轮询（无变化） | 每 0.9 秒全量解析 | ≤ 10 ms | 后端计时日志 |
| 1000 会话列表 | 待测（每工作区 50 条截断） | ≤ 50 ms | 造数据 + 接口计时 |
| 4 会话并发内存 | 待测 | 记录并设上限 | 同时跑 4 个会话测 RSS |

### 4.2 测量纪律

- 先量后改：每个阶段开始前复测一次基线，结束后用同一口径复测。
- 结论粒度不超过证据粒度：没有测到的指标不写“已优化”。
- 任何“变快”的结论必须附命令与数据，不写主观判断。

---

## 5. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| `LiveCli` 拆分破坏 turn 行为 | 高 | Phase 1 只抽类型、不改逻辑；每步跑全量测试 + 真实对话冒烟 |
| 删除 CLI 后遗漏某个日常能力 | 中 | Phase 4 四项能力逐项验收；验收前不删对应 CLI 代码 |
| 会话索引与 JSONL 不一致 | 中 | 索引可重建；读取异常时回退 JSONL；写入失败静默降级 |
| 定时任务与手动会话争抢并发 | 中 | 复用现有调度器，不新增旁路；同任务重叠时跳过 |
| 中文模型切换导致向量失效 | 低 | `history_meta.embedding_model` 版本校验 + 自动重建 |
| 二进制体积未达预期 | 中 | Phase 8 用实测数据决定 embedding 是否可选，不提前承诺 |
| 热升级路径依赖旧参数 | 中 | Phase 3 冒烟覆盖热升级；保留 `--upgrade-helper` 分支 |

---

## 6. 明确不做

- 不做多用户、登录、公网暴露。
- 不重构会话正文存储（JSONL 继续作为真相源）。
- 不为了对齐其他产品新增功能面板。
- 不在本次引入新的前端框架或编辑器组件。
- 不删除 `im-bridge`、`claw-shell`、`claw-acp`（独立二进制或外部集成仍在使用）。
- 不改模型 Provider 的协议层。

---

## 7. 已确认决策

以下五项已于 2026-11-26 确认，作为执行依据：

1. **定时任务会话归属**：每个任务一个专属会话，不提供改绑入口。
2. **错过补跑**：服务停机期间只补最近一次。
3. **中文模型**：`bge-small-zh-v1.5`（512 维）。
4. **二进制体积**：目标 ≤ 25 MB。通过 `ort-load-dynamic` 把 ONNX 运行时改为独立 DLL，解决“小体积”与“中文语义检索”的冲突。
5. **旧二进制名**：`claw-plus` 保留一个版本周期。

**唯一的执行前提**：Phase 8 的实测必须证明“动态加载 ONNX”后主二进制确实 ≤ 25 MB。若实测不达标，回退方案是把 embedding 改为可选编译特性，并在设置页明确标注语义检索需要单独构建。除此之外不引入第三套方案。
