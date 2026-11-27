# Web 热升级断点续跑实施计划

> 目标：任务执行中调用 `HotUpgrade`，进程重建后由新版本补上“升级完成”的工具结果，模型带着完整上下文继续同一个任务，不需要用户停任务、重新下指令。
>
> 边界：续跑发生在**工具调用边界**，不在模型流式输出中途。进程内函数栈不迁移，迁移的是“会话消息 + 挂起现场”。

## 1. 设计口径（已确认）

| 决策 | 口径 |
|---|---|
| 续跑粒度 | 同一轮断点续跑：已执行工具不重跑，HotUpgrade 之后未执行的工具标记为“未执行”，由模型按需重发 |
| 切换时机 | 模型调用 `HotUpgrade` 即挂起；构建、重启完成后继续 |
| 页面体验 | 允许自动刷新；刷新后能看到任务继续 |
| 停机容忍 | 接受分钟级中断；P0 沿用“先退出再构建”，P1 再优化为“构建成功才切换” |
| 适用范围 | Web（`claw serve`）。CLI/TUI 保持现有“turn 收尾后升级”语义，避免破坏冻结面 |

## 2. 目标消息序列

```text
assistant:
  text: "已修复 X，准备热升级"
  tool_use  id=call_1  name=HotUpgrade
旧进程:
  已执行工具结果已逐条落盘；call_1 不写 tool_result；进程退出
新进程启动:
  tool  id=call_1  name=HotUpgrade  output="热升级完成：版本 2026.11.27，耗时 95s"
  tool  id=call_2  name=...         output="未执行：热升级中断，请重新发起"  is_error=true
  user  [runtime] "热升级完成，请继续原任务"
assistant:
  继续下一步
```

约束：`assistant(tool_use...)` 之后必须先补齐**全部** tool_result，才能出现 user 消息，否则接口拒绝整段历史。

## 3. 阶段划分

| 阶段 | 交付 | 目的 |
|---|---|---|
| P0 | 挂起 → 重启 → 补结果 → 自动续跑 | 打通主链路，构建失败暂时沿用现状 |
| P1 | 构建成功才切换 + 失败返回工具错误 + 队列持久化 + 自动回滚 | 把失败路径做稳，接近真正“无感” |

实施顺序严格按 P0-1 → P0-6，P1 在 P0 端到端验证通过后开始。

---

## 4. P0-1：运行时挂起点

### 改动文件

- `rust/crates/runtime/src/conversation.rs`
- `rust/crates/runtime/src/lib.rs`（导出新类型）

### 改动点

1. **新增挂起描述结构**（放在 `conversation.rs`，公开导出）：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSuspension {
    pub tool_use_id: String,
    pub tool_name: String,
    /// 同批次中排在 HotUpgrade 之后、未执行的工具调用（按原顺序）
    pub pending_after: Vec<(String, String)>,
}
```

2. **运行时增加两个字段**：
   - `suspend_on_hot_upgrade: bool`（默认 `false`，仅 Web 的 `LiveCli` 打开）
   - `upgrade_suspension: Option<UpgradeSuspension>`
   - builder：`with_hot_upgrade_suspend(bool)`
   - 访问器：`take_upgrade_suspension() -> Option<UpgradeSuspension>`

3. **工具循环识别挂起点**（当前锚点 `conversation.rs:5049-5060`）：
   - 把消费式 `for (tool_use_id, tool_name, input) in pending_tool_uses` 改为带下标的遍历，保留整批清单，便于计算 `pending_after`。
   - 在执行权限检查/工具执行之前判断：`suspend_on_hot_upgrade && tool_name == "HotUpgrade"`。
   - 命中后：
     - `self.upgrade_pending.store(true, Ordering::SeqCst)`（复用现有“升级中跳过 plan stale”的收尾语义，见 `conversation.rs:6191-6204`）；
     - 写入 `upgrade_suspension`（当前 id + 后续未执行清单）；
     - **不执行工具、不 push tool_result**；
     - `break` 出工具批次。
   - 批次结束后若 `upgrade_suspension.is_some()`，在外层模型请求之前 `break`，不再发起下一轮 LLM 调用。
   - 批次后的收敛停滞统计、写入豁免、cost meter 等逻辑要加 `if suspension.is_none()` 保护，避免把“未执行”误判为失败。

4. **收尾路径**：
   - 不调用 `record_turn_failed`，返回 `Ok(TurnSummary)`。
   - `mark_plan_stale_if_content` 由现有 `upgrade_pending` 判断自动跳过。
   - `HookEvent::Stop` 在挂起时不触发（它不是会话结束，是进程切换）。

5. **新增续跑入口**（避免伪造用户消息）：
   - 把 `run_turn_async` 开头依赖 `user_input` 的初始化（`conversation.rs:3952-4026`：打回率、`push_user_text`、语义召回、goal 提取）抽成 `prepare_turn_input`。
   - 新增 `run_turn_continue(notice: String, prompter)`：
     - 不调用 `push_user_text`；
     - 注入一条 `runtime_note("upgrade", notice)`（复用现有的 `[runtime]` 注记通道）；
     - 复用同一套主循环。
   - 普通 `run_turn` 行为不变。

### 测试

- 同一批包含 `[工具A, HotUpgrade, 工具B]`：A 执行并落结果，HotUpgrade 与 B 无结果，`take_upgrade_suspension()` 返回 id 与 `pending_after=[B]`。
- 正常 turn 不受影响（`suspend_on_hot_upgrade=false` 时行为与现在一致）。
- 挂起 turn 不产生 `record_turn_failed`，不标记 plan stale。

---

## 5. P0-2：会话侧补结果能力

### 改动文件

- `rust/crates/runtime/src/session.rs`
- `rust/crates/runtime/src/lib.rs`（导出）

### 改动点

1. 新增 `Session::resolve_tool_use(tool_use_id, tool_name, output, is_error) -> bool`：
   - 在末尾 assistant 消息中查找该 tool_use；
   - 若已有对应 tool_result（幂等重入），返回 `false`；
   - 否则追加一条 `MessageRole::Tool` 结果消息（走现有 `push_message`，自动追加落盘）。
2. 新增 `Session::unresolved_tool_uses_after(tool_use_id) -> Vec<(String, String)>`：
   - 从末尾 assistant 消息提取顺序；
   - 只返回该 id 之后还没有 result 的调用，作为兜底（正常情况下直接用挂起现场里的 `pending_after`）。
3. 保持 `repair_dangling_tool_uses` 不变；**调用顺序**上必须先 `resolve_tool_use` 再走 `resume_from_path`，否则修补逻辑会把它写成“被中断”。
4. 幂等要求：新进程启动中途崩溃后再次启动，不能重复追加结果。

### 测试

- 补结果后消息序列为 assistant → tool(call_1) → tool(call_2)，顺序正确。
- 重复调用 `resolve_tool_use` 不产生第二条结果。
- 已补结果后再调用 `repair_dangling_tool_uses`，返回 0。

---

## 6. P0-3：服务端挂起与升级现场

### 6.1 升级请求结构扩展

文件：`rust/crates/claw-server/src/session_mgr.rs`（当前锚点 `39-61`）

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpgradePhase {
    Suspended,
    Building,
    Resuming,
}

pub(crate) struct UpgradeRequest {
    // 现有字段保持不变
    session_id: String,
    plan_id: Option<String>,
    requested_at_ms: i64,
    from_version: String,
    old_pid: u32,
    web_port: Option<u16>,
    web_token: Option<String>,

    // 新增（全部 #[serde(default)]，兼容旧标记）
    session_path: Option<String>,
    workspace: Option<String>,
    permission: Option<String>,
    model: Option<String>,
    suspended_tool_use_id: Option<String>,
    suspended_tool_name: Option<String>,
    pending_after: Vec<(String, String)>,
    phase: Option<UpgradePhase>,
}
```

新增辅助：`update_upgrade_phase`、`clear_upgrade_request_if`（避免误清新请求）。

### 6.2 调度器状态

文件：`rust/crates/claw-server/src/serve/scheduler.rs`

- `SessionState` 增加 `Suspended`（标签 `suspended`）。
- 新增：
  - `mark_suspended(session)`：清空该会话待执行队列（P0 直接拒绝新消息；P1 再持久化），状态置 `Suspended`。
  - `clear_suspended(session)`。
  - `is_quiet_except(session)`：除指定会话外全部 Idle 且队列为空。
- `enqueue` 命中 `Suspended` 返回新错误 `EnqueueError::UpgradePending`。
- `status` / `is_active` / `api_status` 支持新状态。
- 看门狗跳过 `Suspended` 会话（它没有运行中的 generation）。

### 6.3 LiveCli 结果透传

文件：`rust/crates/claw-server/src/app.rs`（当前锚点 `1425-1444`、`1451-1480`）

- `LiveCli` 增加字段 `last_upgrade_suspension: Option<runtime::UpgradeSuspension>`。
- `run_turn_silent_with` 在 `runtime.run_turn` 返回后读取 `runtime.take_upgrade_suspension()`，存入字段，再 `replace_runtime` / `persist_session`。
- 新增 `take_upgrade_suspension()`、`restore_active_plan(plan)`（透传到 runtime 现有方法）。
- `LiveCli::new` 增加参数或在 serve 创建后调用 `set_hot_upgrade_suspend(true)`，仅 Web 打开。

### 6.4 Worker 处理挂起

文件：`rust/crates/claw-server/src/serve.rs`（当前锚点 `2468`、`2560-2712`）

- `SessionWorker::run_turn` 返回 `Result<Option<UpgradeSuspension>, String>`。
- Worker 循环在 turn 结束后：
  - 若返回挂起：
    1. `scheduler.complete_turn(...)`（让本会话不再占 running）；
    2. `scheduler.mark_suspended(session)`；
    3. 计算 `plan_id = session_active_plan_id(&job.session_path)`（现有函数 `serve.rs:376`）；
    4. 写 `UpgradeRequest`（含挂起字段、模型、权限标签、`pending_after`）；
    5. 发出 SSE `{kind:"upgrading"}`；
    6. 启动升级协调线程，返回 worker 循环。
  - 若未挂起：保持现有收尾逻辑。
- 删除/停用现有 `is_quiet() && tools::take_hot_upgrade_request()` 路径（`serve.rs:2688-2698`），避免两条升级路径并存。CLI/TUI 仍走旧路径。

### 6.5 升级协调线程

文件：`rust/crates/claw-server/src/serve.rs`（`begin_web_upgrade` 附近，当前锚点 `6611-6662`）

流程：

1. 等待 `scheduler.is_quiet_except(session)`（轮询 500ms）。
2. 超时（建议 180s）：
   - 清挂起状态；
   - 补一条 HotUpgrade 错误结果（“其他会话仍在运行，升级未执行”）；
   - 重新入队续跑；
   - 发 SSE `{kind:"upgrade_aborted", message}`。
3. 就绪后：
   - 更新 request `phase=Building`；
   - spawn helper（沿用 `upgrade_helper::spawn`）；
   - 等 700ms 刷 SSE；
   - 写 exited 标记；
   - `std::process::exit(0)`。

注意：协调线程运行在 worker 之外，避免被看门狗当成卡死 turn。

---

## 7. P0-4：新进程启动恢复

### 7.1 新模块

文件：`rust/crates/claw-server/src/upgrade_resume.rs`（新增，`lib.rs` 注册）

职责：

1. `load_pending(workspace) -> Option<UpgradeRequest>`：只接受 `phase=Suspended` 且 `suspended_tool_use_id` 非空的请求。
2. `resolve_session(req) -> Result<ResumePlan>`：
   - 定位会话文件（`session_path`，缺失时用 `session_id` 在会话目录查找）；
   - `session_mgr::load_session_reference_in` 载入；
   - `resolve_tool_use(id, "HotUpgrade", "热升级完成：版本 X（构建耗时 Ys）", false)`；
   - 按 `pending_after` 顺序补 `is_error=true` 的“未执行”结果；
   - `save_to_path`；
   - 返回 `ResumePlan { session_id, session_path, workspace, permission, model, plan_id }`。
3. `enqueue(state, plan)`：构造 `PromptJob`，标记为续跑，入队并 `spawn_session_worker`。

### 7.2 PromptJob 扩展

文件：`rust/crates/claw-server/src/serve/scheduler.rs`（当前锚点 `95-105`）

```rust
pub struct PromptJob {
    // 现有字段...
    pub resume: Option<ResumeSpec>, // None = 普通用户消息
}

pub struct ResumeSpec {
    pub notice: String,
    pub plan_id: Option<String>,
}
```

- `SessionWorker::run_turn`：`job.resume` 为 `Some` 时走 `cli.run_turn_continue(notice, prompter)`，否则走现有 `run_turn_silent_with`。
- 进入 turn 前若 `plan_id` 有值，调用 `runtime::planner::load_plan_artifact` 加载并 `cli.restore_active_plan(plan)`。

### 7.3 启动接线

文件：`rust/crates/claw-server/src/serve.rs`（当前锚点 `1260-1300`）

- `AppState` 创建后、`axum::serve` 前调用 `upgrade_resume::schedule(state.clone())`。
- `schedule` 启动一个线程：
  1. 等 `CLAW_UPGRADE_READY_GRACE_MS`（默认 5000ms）或首个事件订阅/状态查询到达（见 7.4）；
  2. `load_pending`；
  3. `resolve_session`；
  4. `enqueue`；
  5. request 标记 `phase=Resuming`（不立即删除）；
  6. 续跑 turn 正常结束后再删除 request；若失败保留，下次启动可重试（补结果幂等保证安全）。

### 7.4 页面就绪闸门（建议 P0 一起做）

文件：`serve.rs`（`api_events` / `api_status`，当前锚点 `2792`、`2975`）

- `AppState` 增加 `resume_ready: Arc<AtomicBool>`。
- 新进程启动时若存在待恢复请求，`resume_ready=false`。
- `api_events` 建立会话订阅或 `api_status` 首次查询该会话时置 `true`。
- `upgrade_resume` 线程等待 `resume_ready` 或 30s 超时后再入队。
- 目的：避免模型已经输出一段、页面才刷新加载，过程看起来像卡住。

### 7.5 Helper 调整

文件：`rust/crates/claw-server/src/upgrade_helper.rs`（当前锚点 `325-382`）

- Web 模式下**不要**在 spawn 新进程后 `clear_upgrade_request`；由新 serve 消费。
- 只清理 `upgrade-exited.json`。
- 构建失败时：
  - P0：记录 `upgrade-helper.log`，保留 `.old`，不启动新进程（现状）；
  - P1：见第 9 节。
- CLI/TUI 分支保持现有清理行为。

### 7.6 前端

文件：

- `web/src/chat/useSessionEvents.js`（当前锚点 `184`）
- `web/src/App.jsx`（当前锚点 `252-271`、`705`）
- `web/src/chat/ToolCard.jsx`（可选）

改动：

1. 收到 `tool_use` 且 `name == "HotUpgrade"` 时立即显示“正在热升级，完成后自动继续”，不必等 `upgrading` 事件。
2. `/api/meta` 增加 `version` / `build_id`；页面轮询只有检测到版本变化才 `location.reload()`，避免旧进程存活时提前刷新（P1 必需，P0 先加字段不影响）。
3. 刷新后拉 `/api/status`：`suspended/upgrading` 显示等待横幅，`running` 恢复正常界面。
4. 续跑完成后 SSE 增加 `{kind:"upgrade_resumed"}`，前端清横幅。
5. 输入框草稿写 `localStorage`，刷新后恢复（P0 可做，成本低）。

---

## 8. P0 验收标准

1. 任务中调用 HotUpgrade：1 秒内页面出现升级提示，模型停止继续输出，进程退出。
2. 构建成功后：同端口、同 token 起新进程；会话出现“热升级完成”的工具结果；模型**无需用户输入**继续下一个动作。
3. 升级前已执行的工具不重跑；HotUpgrade 之后的工具标记“未执行”，由模型重发。
4. 计划、待办、历史消息完整；页面自动刷新后能看到续跑状态。
5. 同时有其它会话运行时：升级等待它们空闲；超时能回退到“放弃升级并继续任务”，不丢失当前轮。
6. 重复启动/重复消费升级标记不会产生重复工具结果（幂等）。

### 验证命令

```powershell
scripts/fmt.sh --check
cd rust
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd crates/claw-server/web
npm run build
```

### 手工端到端

1. `claw serve --port 4399 --cwd D:\claw-code-src --no-open`
2. 在 Web 会话里让模型改一处无风险源码，然后调用 HotUpgrade。
3. 观察：升级横幅 → 服务重启 → 页面刷新 → 会话里出现“升级完成”工具结果 → 模型继续。
4. 检查 `.claw/upgrade-helper.log`、`.claw/web-server.log`、会话 JSONL 的消息顺序。

---

## 9. P1：失败路径与无感切换

### 9.1 构建成功才切换

文件：`upgrade_helper.rs`、`serve.rs`

- 挂起后不立即退出，进入“等待构建”状态（会话保持 Suspended）。
- Helper 在旧进程存活时构建：
  - 方案 A：先把运行中的 exe 改名为 `.old` 释放路径，再 `cargo build` 写回原路径；Windows 允许重命名运行中的镜像。
  - 方案 B：新增次级 bin 目标（如 `claw-next`），构建到同目录不同文件名，成功后再替换。
- 构建成功后 helper 调 `POST /api/upgrade/commit?t=<token>`；旧进程收到后停止接单、写 exited、退出。
- 构建失败：helper 写 `.claw/upgrade-result.json`（`{ok:false, stderr_tail}`）；旧进程轮询到后：
  - 补一条 HotUpgrade 错误结果（“编译失败，错误见下…”）；
  - 清挂起状态；
  - 重新入队续跑，模型当场修代码重试。
- 这条路径彻底消除“构建失败任务死掉”的问题。

### 9.2 其它加固

- 排队消息持久化：`mark_suspended` 时把队列序列化进 `UpgradeRequest`，启动恢复后排在续跑任务之后重新入队。
- 前端只在版本变化时刷新（9.1 必需，避免旧进程存活期间提前 reload）。
- 新进程健康检查：helper 启动后轮询 `/api/meta`，失败则恢复 `.old` 并重启旧版本。
- 前端改动自动构建：helper 检测 `web/src` 比 `web/dist` 新时先 `npm run build`。
- 升级循环保护：每会话记录连续升级次数，超过阈值要求用户确认。
- 退出前终止会话登记的后台子进程（`SessionProcesses::terminate_all`），避免文件锁。
- 更新 `docs/web-hot-upgrade.md` 的行为与限制说明。

---

## 10. 文件改动汇总

| 顺序 | 文件 | 改动 |
|---|---|---|
| 1 | `rust/crates/runtime/src/conversation.rs` | 挂起结构、工具循环挂起点、续跑入口、收尾跳过 |
| 2 | `rust/crates/runtime/src/lib.rs` | 导出 `UpgradeSuspension` |
| 3 | `rust/crates/runtime/src/session.rs` | `resolve_tool_use`、未解析工具查询 |
| 4 | `rust/crates/tools/src/lib.rs` | HotUpgrade 描述与返回文案（要求单独调用、说明会挂起） |
| 5 | `rust/crates/claw-server/src/session_mgr.rs` | `UpgradeRequest` 扩展、phase、幂等清理 |
| 6 | `rust/crates/claw-server/src/serve/scheduler.rs` | `Suspended` 状态、`is_quiet_except`、入队拒绝、`ResumeSpec` |
| 7 | `rust/crates/claw-server/src/app.rs` | LiveCli 透传挂起信息、续跑调用、计划恢复 |
| 8 | `rust/crates/claw-server/src/serve.rs` | 挂起处理、协调线程、启动恢复接线、状态接口 |
| 9 | `rust/crates/claw-server/src/upgrade_resume.rs` | 新增：启动恢复与续跑入队 |
| 10 | `rust/crates/claw-server/src/lib.rs` | 注册新模块 |
| 11 | `rust/crates/claw-server/src/upgrade_helper.rs` | 保留升级请求给新进程消费 |
| 12 | `rust/crates/claw-server/web/src/chat/useSessionEvents.js` | HotUpgrade 即时提示、恢复事件 |
| 13 | `rust/crates/claw-server/web/src/App.jsx` | 升级/恢复横幅、按版本刷新、草稿保留 |
| 14 | `rust/crates/claw-server/web/src/chat/ToolCard.jsx` | HotUpgrade 卡片状态（可选） |
| 15 | `docs/web-hot-upgrade.md` | 更新行为、限制、验证步骤 |

## 11. 风险与回滚

| 风险 | 处理 |
|---|---|
| 首次上线不可续跑 | 旧二进制不会写挂起现场；部署后第一次升级仍是旧行为，第二次起生效。文档中明确 |
| CLI/TUI 被改变 | 挂起行为由 Web 侧开关控制，运行时默认关闭 |
| 消息顺序非法 | 补结果必须先于 `resume_from_path`；顺序按 `pending_after`；单元测试覆盖 |
| 幂等失败导致重复结果 | `resolve_tool_use` 先检查已有结果；升级请求保留到续跑结束再删 |
| 其它会话被误杀 | 升级前 `is_quiet_except` 等待；超时走“放弃升级并继续”分支 |
| 新进程起不来 | 保留 `.old`；P0 手动恢复；P1 健康检查自动回滚 |
| 前端提前刷新 | `/api/meta` 带版本号，只有版本变化才 reload |
| Windows exe 被占用 | P0 先退出再构建；P1 用改名释放路径或次级产物名 |

回滚步骤：

1. 停新进程；
2. 把 `claw.exe.old` 复制回 `claw.exe`；
3. 删除 `.claw/upgrade-request.json`（旧版本不消费该文件，避免残留）；
4. 启动旧版本；会话中悬空的 HotUpgrade 调用会被旧版本修补为“被中断”，可继续。

## 12. 不做的事

- 不做模型流式输出中途的进程切换（重复生成、丢 token、成本不可控）。
- 不迁移运行中的子进程（cargo/test/browser）；挂起前必须没有前台工具在跑，后台进程在 P1 处理。
- 不引入常驻 supervisor 或双进程 worker 架构（当前收益不足以承担重构成本）。
- 不改变 CLI/TUI 的升级语义。
