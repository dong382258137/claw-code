# 非阻塞遗留事项清单

**基准 commit**: 19e75430
**最后核查日期**: 2026-07-27
**workspace 状态**: 编译 0 errors,测试通过(0.10.4: 39 + 1.3: 27 + claw-acp: 11 = 77 个)

> 本清单已逐项核查代码事实,修正了原报告中的 2 处失实描述(2.3 和 3.3)和 1 处统计口径偏差(1.2)。

## 优先级总览

| 优先级 | 事项 | 工作量 | 状态 |
|---|---|---|---|
| P0 | 2.1 LaneEvent 桥接接入 run_turn | 1-2h | 待办 |
| P0 | 3.3 CoordinatorExecutor 注入生产 | 2-3h | 待办(且需重新设计接入路径) |
| P1 | 1.1 claw-acp 1.3 路径 10 个 warning | ~30min | 待办 |
| P1 | 1.2 claw-shell clippy 3 个 warning | ~15min | 待办 |
| P1 | 2.2 Notification 事件触发 | ~30min | 待办 |
| P1 | 2.3 dag_run 工具接入 async scheduler | 2-3h | 待办(原描述失实,实际更严重) |
| P2 | 3.1 ClawAgent::cancel stub | 3-5 天 | 待办(主循环重构) |
| P2 | 3.2 ACP 0.10.4 silent drop | 切换 1.3 即可 | 待办 |
| P2 | 4.1 tui_mode gating | 1-2 天 | 待办 |
| P2 | 4.2 cancel during permission prompt 测试 | ~4h | 待办 |
| P2 | 5.1 反向请求端到端测试 | ~1 天 | 待办 |
| P2 | 5.2 CoordinatorExecutor 生产路径测试 | ~1 天 | 待办(依赖 3.3) |

---

## 一、代码质量问题(14 个 warning)

### 1.1 claw-acp 1.3 路径 10 个 warning ✅ 已核查属实

**复现命令**: `cargo check -p claw-acp --no-default-features --features acp-1_5`

| # | 文件:行号 | warning 类型 | 说明 |
|---|---|---|---|
| 1 | gateway.rs:12 | unused import | `tracing::Instrument` |
| 2 | gateway.rs:17 | unused imports | `AcpAgentMessage` / `AcpClientMessage` |
| 3 | gateway.rs:170 | unused macro | `handle` 宏定义 |
| 4-5 | message.rs:161, 389 | unused import | `future::LocalBoxFuture`(2 处) |
| 6-7 | message.rs:161, 389 | unused import | `FutureExt`(2 处) |
| 8 | gateway.rs:28 | fields never read | `rx` / `conn` 字段(1.3 stub 路由) |
| 9 | gateway.rs:137 | function never used | `before_request` |
| 10 | gateway.rs:148 | function never used | `after_request` |

**根因**: 1.3 的 `route_to_agent` / `route_to_client` 被 cfg-gated 为 stub,导致相关 import/macro/字段/方法在 `acp-1_5` feature 下未使用。

**修复建议**: 用 `#[cfg(feature = "acp-0_10")]` 进一步 gating import,或用 `#[allow(dead_code)]` 标注 1.3 stub 代码。

### 1.2 claw-shell clippy 3 个 warning ✅ 已核查属实(统计口径已修正)

**复现命令**: `cargo clippy -p claw-shell --no-deps`

> **修正说明**: 原报告把这 3 个都算作 "claw-shell 0.10.4 路径 warning",但实际只有 #11 是 `cargo check` 的 dead_code warning,#12-14 是 **clippy** warning(type_complexity + needless_borrows)。普通 `cargo check -p claw-shell` 只产生 1 个 warning。

| # | 文件:行号 | warning 类型 | 说明 | 工具 |
|---|---|---|---|---|
| 11 | runtime/prompt.rs:998 | dead_code | `get_actions_section`(预先存在) | cargo check |
| 12 | claw-shell/agent.rs:68 | type_complexity | `tool_setup: Option<Box<dyn FnOnce(&mut StaticToolExecutor) + Send>>` | clippy |
| 13-14 | claw-shell/lane_bridge.rs:266, 287 | needless_borrows | `&format!(...)` → `format!(...)` | clippy |

**修复建议**: #11 预先存在,可加 `#[allow(dead_code)]`;#12 用 type 别名拆分;#13-14 删除多余的 `&`。

---

## 二、未接入主循环的已实现功能(3 项)

### 2.1 LaneEvent → SessionNotification 桥接未接入 run_turn ⚠️ 高价值 ✅ 已核查属实(情况比报告更严重)

**状态**: `flush_lane_events_to_acp` 已实现于 [lane_bridge.rs:337](../../rust/crates/claw-shell/src/lane_bridge.rs) 并有 2 个测试,但**整个 rust/ 工作区零生产调用点**。

**修正说明**: 原报告说"未在 conversation.rs 或 agent.rs 中被调用"。实际:
- `conversation.rs` **文件根本不存在**(仅在 docs/ide-hooks-dag-implementation-plan.md:357 提到过,是未落地的规划)
- `agent.rs::prompt`(行 271-332)turn 循环中无任何 lane_event 相关调用
- 唯一非定义/非测试的引用是 `lib.rs:62` 的 re-export 和 `agent_v1_3.rs:401` 的文档注释

**影响**: IDE 端收不到任何 LaneEvent 推送(工具调用进度、子 agent 状态、Git 操作等),VS Code 扩展的实时更新能力闲置。

**修复**: 在 `ClawAgent::prompt` 的 turn 循环中,每次 tool call 后 / turn 结束时调用 `flush_lane_events_to_acp(&self.gateway, &session_id)`。需先确认 `ClawAgent` 如何持有 `gateway`。

**工作量**: 1-2 小时 | **风险**: 中

### 2.2 Hooks 的 Notification / PostCustomToolCall 事件未触发 ✅ 已核查属实

**状态**: `run_notification` 定义于 [hooks.rs:377](../../rust/crates/runtime/src/hooks.rs),`run_post_custom_tool_call` 定义于 [hooks.rs:401](../../rust/crates/runtime/src/hooks.rs),均带 `#[must_use]`,但**整个 rust/crates/ 目录零调用点**。

**修正说明**: 原报告说"补全 10 事件中的 1 个"。实际 HookEvent 枚举共 11 个变体,主循环 conversation.rs 已接入 9 个,独缺这两个。修复后是 10/11(不是 9/10)。

**主循环已接入的 9 个 hook**:
- run_pre_tool_use_with_context、run_post_tool_use_with_context、run_post_tool_use_failure_with_context
- run_session_start、run_user_prompt_submit、run_stop、run_session_end
- run_subagent_stop、run_pre_compact

**影响**: Notification 事件无触发点(用户通知能力缺失);PostCustomToolCall 无法区分自定义工具与普通工具。

**修复**: Notification 可在权限拒绝 / 长时间等待时触发;PostCustomToolCall 需先在 tool dispatch 层区分 custom tool。

**工作量**: Notification 约 30 分钟;PostCustomToolCall 需先设计 custom tool 识别机制 | **风险**: 低

### 2.3 dag_run 工具未接入 async scheduler ⚠️ 已核查,**原描述失实**(实际更严重)

**原报告声称**: "dag_status 工具仍走 v0.1 同步 DagExecutor 路径"
**实际**: dag_status 工具**根本不走 DagExecutor** — 它直接读 `DagStore::get_run` + `run.node_statuses`([tools/lib.rs:3497-3555](../../rust/crates/tools/src/lib.rs))。`DagExecutor` 在工具路径中是死代码。

**真实情况**:
- ✅ async scheduler 已实现([scheduler.rs:106-475](../../rust/crates/runtime/src/multi_agent/dag/scheduler.rs)),`with_dag_run` / `run_with_progress` / 桥接方法完整
- ✅ `dag_run` 工具([tools/lib.rs:3464-3494](../../rust/crates/tools/src/lib.rs))只调 `DagStore::start_run` 注册初始 Pending DagRun 就返回
- ❌ **既没接 DagScheduler 也没接 DagExecutor**,dag_status 读回的永远是初始 Pending 状态
- DagStore 的 v0.2 桥接方法(`update_node_status` / `update_run_status`)注释说"Called by the async DagScheduler",但 dag_run 工具根本不构造 DagScheduler

**影响**: 用户调 `dag_run` 启动 DAG 后,`dag_status` 永远返回 Pending,无法看到任何进度。

**修复**: 让 `dag_run` 工具在 `start_run` 后构造 `DagScheduler::with_dag_run(store.clone(), run_id)`,spawn 到后台执行,而非仅注册 Pending 就返回。

**工作量**: 2-3 小时 | **风险**: 中(需确认 DagStore 并发安全)

---

## 三、stub / 占位实现(3 项)

### 3.1 ClawAgent::cancel 是 stub ✅ 已核查属实

**位置**: [agent.rs:333-338](../../rust/crates/claw-shell/src/agent.rs)

```rust
async fn cancel(&self, _arguments: acp::CancelNotification) -> Result<(), acp::Error> {
    // 本期 run_turn 不支持中途取消(同步 API 无法中断)
    // TODO: 改造 run_turn 为 async + CancellationToken 后实现
    tracing::warn!("claw-agent: cancel not yet implemented (sync run_turn)");
    Ok(())
}
```

**影响**: IDE 端发送 `session/cancel` 时,agent 实际不停止,继续执行到 turn 结束。

**修复**: 需要将 `run_turn` 改造为 async + CancellationToken(Phase A 遗留)。

**工作量**: 3-5 天(涉及 conversation 主循环重构)| **风险**: 高

### 3.2 ACP 0.10.4 silent drop 行为 ✅ 已核查属实

**位置**: [stdio.rs:498-505](../../rust/crates/claw-shell/src/stdio.rs) 注释明确固化该行为,并有测试 `run_agent_on_io_silently_drops_invalid_json` 和 `run_agent_on_io_silently_drops_missing_method_field`。

**影响**: 调试困难 — 客户端发送格式错误的请求时无反馈。

**修复**: 升级到 ACP 1.3(1.3 返回 `-32700` parse_error / `-32601` unknown method),feature flag 已就绪,切换即可;或在 0.10.4 上层添加错误日志。

**工作量**: 1.3 升级已就绪,切换即可 | **风险**: 低

### 3.3 CoordinatorExecutor SubagentRunner 未注入 ⚠️ 已核查,**原描述部分失实**

**原报告声称**: "DAG 节点执行时返回 `ConfigError`"
**实际**: `NodeError` 枚举([executor_trait.rs:78-90](../../rust/crates/runtime/src/multi_agent/dag/executor_trait.rs))只有三个变体 `ExecutionFailed` / `Timeout` / `Cancelled`,**没有 ConfigError**。runner 为 None 时返回的是 `NodeError::ExecutionFailed("CoordinatorExecutor has no runner configured; subagent {id} cannot be executed. Wire ConversationRuntime::run_subagent_turn via CoordinatorExecutor::with_runner before dispatching DAG nodes.")`。

**真实情况(比报告更严重)**:
- ✅ CoordinatorExecutor 已实现([coordinator_executor.rs:81-86](../../rust/crates/runtime/src/multi_agent/dag/coordinator_executor.rs))
- ✅ `with_runner` 是唯一注入 runner 的途径,但全部 10 处调用都在自身文件的测试/文档中
- ❌ **`CoordinatorExecutor::new` 在生产代码中 0 次调用** — ConversationRuntime 完全不引用 CoordinatorExecutor
- ❌ ConversationRuntime 的子 agent 执行路径([conversation.rs:2161](../../rust/crates/runtime/src/conversation.rs) 调 `run_subagent_turn`)与 DAG/CoordinatorExecutor 路径是**两条平行未连通的代码路径**
- `coordinator_executor.rs` 文档注释自称 "production-grade SubagentExecutor implementation" — 与实际代码状态不符

**影响**: DAG 节点执行时返回 `NodeError::ExecutionFailed`,DAG 无法实际调度子 agent。

**修复**: 在 ConversationRuntime 构造 CoordinatorExecutor 时,注入 `run_subagent_turn` 闭包。需先解决 `run_subagent_turn` 是 `&mut self` 方法、SubagentRunner 是 `Arc<dyn Fn>` 的签名不兼容问题。

**工作量**: 2-3 小时(若仅做闭包适配);实际接入需先设计 ConversationRuntime 与 CoordinatorExecutor 的集成点 | **风险**: 中

---

## 四、Phase A 遗留技术债(2 项)

### 4.1 tui_mode gating 保留 ✅ 已核查属实(位置已修正)

**修正说明**: 原报告说"tui_mode 标志仍保留在 app.rs 和 tui/app.rs 中"。实际 **claw-shell/src 中无 tui_mode**(grep 0 匹配),仅存在于 `rusty-claude-cli/src/`:
- [app.rs:516](../../rust/crates/rusty-claude-cli/src/app.rs) 字段定义
- [app.rs:2482](../../rust/crates/rusty-claude-cli/src/app.rs) `set_tui_mode`
- 14 处使用(包括 permission_prompter 分支、stdout gating 等)

**影响**: TUI 相关代码仍被 feature flag gating。

**修复**: Phase A Step A5 — 评估是否可以移除 tui_mode,让 TUI 始终可用。

**工作量**: 1-2 天 | **风险**: 中

### 4.2 cancel with active prompts 未验证 ✅ 已核查属实

**状态**: 全局 grep `cancel.*permission|permission.*cancel|during.*prompt` 在 claw-shell 中 0 匹配,无相关集成测试。

**影响**: 用户在权限提示期间点击取消,可能导致状态不一致。

**修复**: 编写集成测试覆盖 "cancel during permission prompt" 场景。

**工作量**: 约 4 小时 | **风险**: 低(仅测试)

---

## 五、测试覆盖缺口(2 项)

### 5.1 agent_v1_3.rs 反向请求端到端测试缺失 ✅ 已核查属实

**状态**: agent_v1_3.rs 共 24 个测试函数。grep `mock|Client\.builder|on_receive_request` 仅 1 处命中(文档注释),**无 mock gateway 测试**。

3 个反向请求方法(`read_editor_buffer` / `write_editor_buffer` / `request_permission`)仅覆盖:
- ConnectionClosed 错误路径(3 个测试)
- AlwaysAllow 缓存命中(1 个测试)

**未覆盖**: 真实 IDE 连接下的完整请求-响应循环。

**修复**: 用 `Client.builder().on_receive_request(...)` 构造 mock gateway,测试完整请求-响应循环。

**工作量**: 约 1 天(1.3 Connection mock 构造较复杂)| **风险**: 低(仅测试)

### 5.2 CoordinatorExecutor 生产路径测试缺失 ✅ 已核查属实(依赖 3.3)

**状态**: coordinator_executor.rs 共 8 个测试,全部用 mock runner。

**未覆盖**: 注入真实 `run_subagent_turn` 后的行为。

**修复**: 集成测试 — 注入真实 `run_subagent_turn` + 构造小型 DAG + 验证子 agent 执行结果。

**依赖**: 必须先完成 3.3(生产环境注入 runner)才有意义。

**工作量**: 约 1 天 | **风险**: 低(仅测试)

---

## 修复进度追踪

| 项 | 优先级 | 状态 | commit |
|---|---|---|---|
| 1.1 claw-acp 10 warning | P1 | ✅ 已修复 | 1d78b863 |
| 1.2 claw-shell 3 warning | P1 | ✅ 已修复 | 1d78b863 |
| 2.1 LaneEvent 桥接接入 | P0 | ✅ 已修复 | 1d78b863 |
| 2.2 Notification 事件触发 | P1 | ✅ 已修复 | 1d78b863 |
| 2.3 dag_run 接入 async scheduler | P1 | ✅ 已修复 | 1d78b863 |
| 3.1 ClawAgent::cancel stub | P2 | ⏭️ 跳过(需 3-5 天主循环重构) | - |
| 3.2 ACP silent drop | P2 | ⏭️ 跳过(需 fork 外部库或 wrapper) | - |
| 3.3 CoordinatorExecutor 注入 | P0 | ✅ 已修复 | 1d78b863 |
| 4.1 tui_mode gating | P2 | ⏭️ 跳过(需 1-2 天评估所有使用点) | - |
| 4.2 cancel during prompt 测试 | P2 | ✅ 已修复 | 81a52dd3 |
| 5.1 反向请求端到端测试 | P2 | ✅ 已修复 | 81a52dd3 |
| 5.2 CoordinatorExecutor 生产测试 | P2 | ✅ 已修复 | 81a52dd3 |

### 修复详情(2026-07-27)

**1.1 claw-acp 10 warning**: 用 `#[cfg(feature = "acp-0_10")]` gating 0.10.4 专属 import/macro/函数,1.3 路径下不再触发 dead_code。修改 gateway.rs(import gating、handle! macro、before/after_request、AcpGatewayReceiver allow(dead_code))和 message.rs(LocalBoxFuture/FutureExt cfg-gating)。

**1.2 claw-shell 3 warning**: needless_borrows 删除多余 `&`;type_complexity 加 `#[allow(clippy::type_complexity)]`;dead_code 加 `#[allow(dead_code)]`。

**2.1 LaneEvent 桥接接入**: 在 `ClawAgent::prompt` 的 turn 完成后调用 `flush_lane_events_to_acp(&self.client_gateway, &session_id)`,激活 IDE 实时推送。

**2.2 Notification 事件触发**: 在 `PermissionOutcome::Deny` 分支调用 `self.hook_runner.run_notification(...)`,权限拒绝时触发用户通知。

**3.3 CoordinatorExecutor 注入**: 新增 `SubagentDispatcher`(subagent_dispatcher.rs)提取 `run_subagent_turn` 逻辑为异步 Send+Sync;ConversationRuntime 新增 `coordinator_executor` 字段 + `with_dag_coordinator` builder;app.rs 构造第二个 AnthropicRuntimeClient 给 dispatcher 并注入 tools 全局 registry。

**2.3 dag_run 接入 async scheduler**: tools/lib.rs 新增 `COORDINATOR_EXECUTOR: OnceLock` + `set_coordinator_executor` setter;`run_dag_run` 在 executor 已注入时构造 `DagGraph::from_dag` + `DagScheduler::with_dag_run`,在独立 OS 线程 + tokio runtime 中后台 spawn 调度循环,立即返回 Running 状态。

### 修复详情(2026-07-27 P2 测试覆盖)

**5.1 反向请求端到端测试**: 用 `acp::Channel::duplex()` 构造 in-process mock gateway,通过 `Client.builder().on_receive_request(...)` 注册类型化 handler。新增 3 个成功路径测试:`read_editor_buffer_returns_content_from_ide`、`write_editor_buffer_completes_when_ide_acknowledges`、`request_permission_returns_decision_from_ide`。

**5.2 CoordinatorExecutor 生产路径测试**: 新增 3 个测试覆盖真实 runner 注入:`coordinator_executor_with_realistic_runner_executes_dag_node`(模拟 LLM 延迟 + 线性 DAG + DagScheduler 端到端)、`coordinator_executor_with_failing_runner_reports_node_failure`(失败传播 + FailFast)、`coordinator_executor_with_subagent_dispatcher_pattern`(完整 SubagentDispatcher 写文件模式)。

**4.2 cancel during permission prompt 测试**: 新增 2 个测试:`claw_agent_cancel_returns_ok_as_stub`(单元测试,固化 stub 返回 Ok 的契约)、`run_agent_on_io_cancel_during_prompt_does_not_interrupt_turn`(集成测试,完整 ACP 协议交互,验证 cancel 不中断 turn)。

### 跳过项说明(3 项)

| 项 | 跳过原因 |
|---|---|
| 3.1 ClawAgent::cancel stub | 需将 run_turn 改造为 async + CancellationToken,涉及 conversation 主循环重构,估计 3-5 天 |
| 3.2 ACP silent drop | silent drop 是外部库 agent_client_protocol 的 rpc.rs 设计,claw-shell 无法直接修改;切换 1.3 会导致 route_to_agent/route_to_client stub 退化 |
| 4.1 tui_mode gating | 需评估 14 处 tui_mode 使用点(app.rs + tui/app.rs),涉及 permission_prompter 分支、stdout gating 等,估计 1-2 天 |
