# Harness Engineering Optimization — Phase 2 计划方案

| 项 | 值 |
|---|---|
| 基于文档 | `docs/harness-engineering-optimization-plan.md` v1.6 + Phase 1 完成状态 |
| Phase 1 完成日期 | 2026-07-22 |
| Phase 2 目标 | 从"smoke test 激活"升级到"生产路径真实闭环" |
| 执行模型 | GLM-5.2 (Trae IDE) |

---

## 1. Phase 1 回顾与 Phase 2 定位

### Phase 1 成果(已完成)

- **Epic 0-6 全部完成**:19 个模块已至少接入 doctor smoke test 层
- **死代码清零**:从 3 个未接入 → 0 个未接入
- **文档对齐**:plan.md §9 接入路径章节 + progress.md 接入状态对照表

### Phase 1 局限

smoke test 只证明"模块 API 可被调用",**未让模块在生产路径真正发挥作用**。具体表现:

| 模块 | smoke test 状态 | 生产路径缺口 |
|---|---|---|
| MultiAgentCoordinator | ✅ 可实例化 | `start()` 只标记 Running,**不实际派发子 agent**(known issue:无限轮询) |
| TaskRegistry | ✅ 可注入 runtime | `spawn_subagent_for_task` 依赖 coordinator.start(),同样空转 |
| team_cron_registry | ✅ API 可调用 | Cron 调度从未接入 Teammate 模式 |
| policy_engine | ✅ evaluate 可调用 | run_turn 决策门未接入 |
| mcp_tool_bridge | ✅ registry 可操作 | McpToolRegistry 全局单例空转(从不 set_manager) |
| branch_lock | ✅ 碰撞检测可用 | 真正接入点(fork/worktree)未接入 |

### Phase 2 定位

**从"激活死代码"升级到"生产路径真实闭环"** — 让模块在真实 CLI 工作流中发挥作用,而非仅通过 doctor 自检。

---

## 2. 业界研究要点(2025-2026)

基于 web research(Anthropic 官方 + 业界实践):

### 2.1 Subagent 架构核心原则

1. **独立 LLM 请求 + 独立 prompt cache**(Anthropic 官方强调)
   - 子 agent 走独立 API 请求,不污染主 agent 的 prompt cache
   - Claude Code "从第一天起就围绕 Prompt 缓存构建"
   - 项目 plan.md §5.2 "Subagent as Tool" 模式与此一致

2. **独立上下文,互不干扰,并行处理**(Anthropic 2025-09 Subagent 功能)
   - 每个子 agent 有自己的上下文窗口
   - 多个子 agent 可并行执行
   - 结果返回主 agent 汇总

3. **subagent-as-tool 模式**(项目已采用)
   - 主 agent 通过 `dispatch_subagent` tool 派发子 agent
   - 子 agent 完成后结果作为 tool result 返回
   - 避免主 agent 上下文膨胀

### 2.2 Cron 调度子 agent 的业界评估

- **非主流模式**:业界主流 subagent 是同步派发-等待,cron 调度较少见
- **风险**:cron 触发的子 agent 需要独立会话状态,与当前会话隔离复杂度高
- **建议**:Phase 2 优先做同步 subagent 真实化,cron 调度作为 P2 延后

### 2.3 Polling Loop 问题解决方案

当前 `start()` 空转导致 `join_all()` 无限轮询。业界解决方案:

1. **async spawn**(推荐):子 agent 在 tokio task 中真实执行,`join_all` 等待 task 完成
2. **callback 通知**:子 agent 完成时通知 coordinator,避免轮询
3. **超时 + 状态检查**:设置最大等待时间,超时后返回 Partial

---

## 3. Phase 2 Epic 分解

### 设计原则

- **pilot-based**:Epic 7(试点)先行,验证流程后再推进 Epic 8-9
- **风险递增**:Epic 7(低)→ Epic 8(中)→ Epic 9(高)→ Epic 10(低收尾)
- **epic-level confirmation**:每个 Epic 开始前征求用户确认
- **不破坏现有契约**:所有变更通过 `cargo test` + `cargo clippy` 验证

---

### Epic 7 — MultiAgentCoordinator 真实化(试点,P0)

**目标**:解决 `start()` 空转的 known issue,让 subagent 真实派发执行。

**风险**:中(涉及 async runtime + LLM 请求,但隔离在 multi_agent 模块内)

**研究结论**:基于 §2.1 业界原则,采用 async spawn 模式。

**调研后状态汇总**(2026-07-22):

| 任务 | 调研结论 | 执行决策 |
|---|---|---|
| P0-1 start() 真实派发 | **已完成** — `execute_dispatch_subagent`(conversation.rs:1745)调用 `run_subagent_turn`(conversation.rs:1859)走真实 LLM 请求,独立 system_prompt + 独立 user_message + 结果写 `.claw/subagents/{id}.md` | ✅ 标记完成 |
| P0-2 TaskRegistry 闭环 | **未完成** — `spawn_subagent_for_task`(task_registry.rs:155)只调用 `coord.spawn()`,不调用 `coord.start()`,也不调用真实 LLM | ✅ 执行 |
| 修复剩余测试 | `dispatch_subagent_fails_gracefully_without_workspace_root` — 静态分析应通过,需实跑验证 | ✅ 执行 |

---

#### P0-1: start() 真实派发 — ✅ 已完成

**调研结论**:已在之前工作中完成,无需额外执行。

- `execute_dispatch_subagent`(conversation.rs:1745-1844)— 派发入口
  - `coordinator.spawn()` + `coordinator.start()` 标记状态
  - 发布 `SubagentHandoff` lane event
  - 调用 `run_subagent_turn()` 同步阻塞执行真实 LLM 请求
  - 根据结果调用 `coordinator.complete()` 或 `coordinator.fail()`
  - 发布终态 `SubagentResult` lane event
- `run_subagent_turn`(conversation.rs:1859-1958)— 子智能体独立 LLM 请求
  - 独立 system_prompt(`SystemPromptSplit::from_sections`,子智能体专用提示)
  - 独立 user_message(task 作为唯一输入,不共享主 agent 历史)
  - 复用 `self.api_client.stream(request)` 真实发起 LLM 请求
  - 结果原子写入 `.claw/subagents/{id}.md`(先写 .tmp 再 rename)
  - 返回相对路径作为 result_ref
- `join_all()`(multi_agent/mod.rs:247-279)— 同步快照实现
  - 当前为同步实现(无实际异步等待),返回当前快照
  - 在当前同步阻塞架构下自洽(主 agent 派发时已等待子 agent 完成)
- **不引入 `SubagentExecutor` trait** — 设计简化,直接在 ConversationRuntime 上实现

---

#### P0-2: TaskRegistry 闭环验证

**当前问题**:
- `spawn_subagent_for_task`(task_registry.rs:155-187)只调用 `coord.spawn()`,不调用 `coord.start()`
- 也不调用真实 LLM — 与 `execute_dispatch_subagent` 是两条独立的派发路径
- TaskRegistry 仅做登记,需要调用方后续手动调用 `start_subagent`/`complete_subagent`/`fail_subagent`

**方案**:评估 TaskRegistry 的设计定位

**执行步骤**:

1. **评估 TaskRegistry 与 execute_dispatch_subagent 的关系**
   - `execute_dispatch_subagent` 是主 agent 通过 tool 派发子 agent 的路径(已真实化)
   - `spawn_subagent_for_task` 是 TaskRegistry 程序化派发路径
   - 两者职责不同:前者是 LLM 驱动,后者是代码驱动

2. **决策:TaskRegistry 是否需要真实化**
   - 选项 A:保持现状(程序化派发只登记,由调用方驱动状态机)— 低风险
   - 选项 B:在 `spawn_subagent_for_task` 中调用 `coord.start()` + 真实 LLM — 中风险,但与 execute_dispatch_subagent 重复
   - **推荐选项 A**:TaskRegistry 的设计定位是"登记 + 状态机管理",真实派发由 `execute_dispatch_subagent` 负责

3. **补充测试验证现有行为**
   - 新增测试:`spawn_subagent_for_task` 后 subagent.status = Created(不是 Running)
   - 新增测试:`start_subagent` 后 subagent.status = Running
   - 新增测试:`complete_subagent` 后 subagent.status = Completed

**文件**:
- `runtime/src/task_registry.rs`(补充测试,不改实现)

**验证**:
- `cargo test -p runtime --lib task_registry`
- 修复 `dispatch_subagent_fails_gracefully_without_workspace_root`(实跑验证)

**风险**:低(只补测试,不改实现)

---

### Epic 8 — 策略与契约层生产接入(P1)

**目标**:policy_engine / green_contract / lane_events 接入 run_turn 决策链。

**风险**:中(涉及 run_turn 主循环改造,但有 flag-gated 保护)

**调研后状态汇总**(2026-07-22):

| 任务 | 调研结论 | 执行决策 |
|---|---|---|
| P1-1 policy_engine 接入 run_turn | **完全未接入** — run_turn 从未调用 PolicyEngine::evaluate;PolicyAction 没有 Allow/Deny 变体(实际是 MergeToDev/RecoverOnce/Block 等);不存在 `--enable-policy-engine` flag;PolicyEngine 未注入 ConversationRuntime | ⚠️ 需重新设计 |
| P1-2 green_contract 注入 PolicyEngine | **类型断层** — GreenContractOutcome 携带结构化信息,但 PolicyCondition 只接受 bool(green_contract_satisfied);GreenLevel 类型不统一(enum vs u8) | ⚠️ 需重新设计 |
| P1-3 lane_events + g004 校验 | **未接入** — try_publish 从未调用 validate_g004_contract_bundle | ✅ 执行 |

**关键发现**:PolicyAction 枚举没有 Allow/Deny 变体,实际语义是"策略动作"(MergeToDev/RecoverOnce/Block/RequireApprovalToken 等),不是"决策门"(Allow/Deny)。原计划 P1-1 的"决策门"设计需要调整。

---

#### P1-1: policy_engine 接入 run_turn(重新设计)

**当前问题**:
- `PolicyAction` 没有 Allow/Deny,实际是策略动作枚举(MergeToDev/RecoverOnce/Block/RequireApprovalToken 等)
- `PolicyEngine::evaluate(&LaneContext) -> Vec<PolicyAction>` 返回动作列表,不是单个决策
- PolicyEngine 的设计定位是"lane 完成时的策略决策",不是"run_turn 入口的决策门"

**重新设计方案**:调整接入点,从"run_turn 入口决策门"改为"lane 完成时策略决策"

**执行步骤**:

1. **评估接入点**
   - 原计划:run_turn 入口 → 不匹配(PolicyEngine 不是决策门)
   - 新方案:lane 完成时(lane_completion 工具内)→ 已部分接入(lane_completion.rs:12,77)
   - 或:PostToolUse 阶段,对特定工具调用做策略检查

2. **决策:是否需要重构 PolicyAction**
   - 选项 A:保持 PolicyAction 现有语义,接入点改为 lane 完成时 — 低风险
   - 选项 B:新增 Allow/Deny/RequireApproval 变体,run_turn 入口做决策门 — 高风险,改变 API
   - **推荐选项 A**:保持现有语义,确认 lane_completion 已接入即可

3. **补充 `--enable-policy-engine` flag**
   - 即使 PolicyEngine 已在 lane_completion 中接入,也补充 flag 控制是否启用
   - 默认关闭,向后兼容

**文件**:
- `rusty-claude-cli/src/commands_handler.rs`(新增 `--enable-policy-engine` flag)
- `rusty-claude-cli/src/app.rs`(flag-gated 注入,可选)

**验证**:
- `cargo test -p runtime --lib policy_engine`
- `cargo build -p rusty-claude-cli`

**风险**:低(确认现有接入,补充 flag 控制)

---

#### P1-2: green_contract 注入 PolicyEngine(重新设计)

**当前问题**:
- `GreenContractOutcome`(enum: Satisfied/Unsatisfied)与 `PolicyCondition::GreenAt`(bool)类型断层
- `GreenLevel` 在 green_contract.rs 是 enum,在 policy_engine.rs 是 u8

**重新设计方案**:桥接类型断层

**执行步骤**:

1. **新增 GreenLevel 类型统一**
   - 在 policy_engine.rs 中 `pub type GreenLevel = u8;` 改为引用 green_contract.rs 的 enum
   - 或在 green_contract.rs 中添加 `as_u8()` 方法

2. **新增 GreenContractOutcome → PolicyCondition 桥接**
   - 在 PolicyEngine 中新增方法 `with_green_contract_outcome(outcome: GreenContractOutcome)`
   - 内部将 outcome 转换为 `green_contract_satisfied: bool` + `green_level: u8`

**文件**:
- `runtime/src/policy_engine.rs`(桥接方法)
- `runtime/src/green_contract.rs`(as_u8 方法,如果需要)

**验证**:
- `cargo test -p runtime --lib policy_engine --lib green_contract`
- 新增测试:GreenContractOutcome → PolicyCondition 桥接

**风险**:低(新增桥接方法,不改变现有 API)

---

#### P1-3: lane_events + g004_conformance 契约校验闭环

**当前问题**:
- `try_publish`(lane_events.rs:1065-1088)从未调用 `validate_g004_contract_bundle`
- 仅 doctor smoke test 和单元测试在用

**方案**:在 try_publish 前调用 g004 校验

**执行步骤**:

1. **lane_events.rs:try_publish 内嵌 g004 校验**
   - 校验范围:仅对 `LaneEvent::ShipPrepared` 等关键事件做校验(不是所有事件)
   - 校验失败时:记录警告日志,不阻止发布(向后兼容)
   - 或:提供 `try_publish_with_g004_validation` 显式校验版本

2. **决策:校验失败行为**
   - 选项 A:校验失败阻止发布(严格) — 可能破坏现有流程
   - 选项 B:校验失败记录警告,仍发布(宽松) — 推荐,向后兼容
   - **推荐选项 B**:宽松模式,通过 flag 控制严格模式

**文件**:
- `runtime/src/lane_events.rs`(try_publish 内嵌 g004 校验,宽松模式)

**验证**:
- `cargo test -p runtime --lib lane_events`
- `cargo test -p runtime --lib g004_conformance`
- 新增测试:try_publish with g004 validation(合法/非法 bundle)

**风险**:低(宽松模式,不阻止发布)

---

### Epic 9 — 工具桥接生产接入(P2,高风险)

**目标**:mcp_tool_bridge + branch_lock 接入生产路径。

**风险**:高(涉及 RuntimeMcpState 重构 + MultiAgentCoordinator fork/worktree 改造)

#### P2-1: mcp_tool_bridge 重构 RuntimeMcpState(高风险)

**当前问题**:`McpToolRegistry` 全局单例空转,从不 `set_manager`,生产路径用不到。

**方案**:
- `RuntimeMcpState::new` 完成后,调用 `McpToolRegistry::set_manager(&manager)`
- `McpToolRegistry` 从全局单例读取,提供 `list_tools`/`call_tool` 的统一入口
- 风险:可能影响现有 MCP 工具调用路径,需充分回归测试

**文件**:
- `runtime/src/mcp_tool_bridge.rs`(set_manager 实现)
- `rusty-claude-cli/src/plugin_state.rs`(RuntimeMcpState::new 后调用 set_manager)

**验证**:
- 全量 `cargo test -p rusty-claude-cli`(回归)
- 新增测试:McpToolRegistry 全局单例能读取到已注册 server

#### P2-2: branch_lock 接入 MultiAgentCoordinator fork/worktree

**方案**:
- `MultiAgentCoordinator::spawn` (Worktree 模式)前调用 `detect_branch_lock_collisions`
- 检测到碰撞时返回 Err,阻止 spawn
- 记录碰撞到 lane_events

**文件**:
- `runtime/src/multi_agent/mod.rs`(spawn 前插入碰撞检测)
- `runtime/src/branch_lock.rs`(复用现有 detect_branch_lock_collisions)

---

### Epic 10 — 部分接入模块补齐(P3,低风险收尾)

**目标**:5 个部分/间接接入模块补齐到直接接入。

**调研后状态汇总**(2026-07-22):

| 任务 | 调研结论 | 执行决策 |
|---|---|---|
| P3-1 planner | settings.json `"planMode"` 文档承诺但未实现 | ✅ 执行 |
| P3-2 Memory | **已接入** — run_turn 入口已调用 `semantic_recall(top-3)`,注入 dynamic_sections | ✅ 标记完成 |
| P3-3 LSP | 风险中-高(性能延迟/并发序列化),LSP 已作为独立工具接入 | ❌ 不接入 read_file |
| P3-4 McpLifecycle | **完全未引用(死代码)** — Validator/State 零生产引用 | ✅ 执行 |
| P3-5 task_packet | 测试断言已对齐 validate_packet 行为 | ✅ 标记完成 |

---

#### P3-1: Plan/Execute/Review 改为 settings.json 配置项

**当前问题**:
- planner 通过 CLI flag `--enable-plan-mode` 触发(默认关闭)
- `planner/mod.rs:17-18` 和 `conversation.rs:322-324` 注释声称支持 settings.json `"planMode": true`,但 `RuntimeFeatureConfig` 无此字段,无解析代码
- 属于"文档承诺但未实现"

**方案**:补齐 settings.json `planMode` 配置项,与 CLI flag 并行(CLI flag 优先级更高)

**执行步骤**:

1. **config.rs:新增 plan_mode 字段**
   - `RuntimeFeatureConfig` 结构体新增 `plan_mode: bool` 字段(默认 false)
   - 新增 `parse_optional_plan_mode(json) -> Option<bool>` 解析函数(参照 `parse_optional_poor_mode` 模式)
   - `RuntimeFeatureConfig::plan_mode()` getter

2. **config.rs:接入解析链**
   - 在 `parse_runtime_feature_config` 中调用 `parse_optional_plan_mode`
   - settings.json schema: `"planMode": true/false`

3. **app.rs:settings.json 驱动注入**
   - 在 `build_runtime_with_plugin_state` 或 app.rs 初始化路径中,读取 `config.features().plan_mode()`
   - 若为 true,调用 `runtime.set_plan_mode_enabled(true)` + `set_workspace_root(cwd)`
   - **优先级**:CLI flag `--enable-plan-mode` > settings.json `planMode` > 默认 false

4. **文档对齐**
   - 更新 `planner/mod.rs:17-18` 注释,确认 settings.json 配置已实现
   - 更新 `conversation.rs:322-324` 注释

**文件**:
- `runtime/src/config.rs`(新增字段 + 解析函数 + getter)
- `rusty-claude-cli/src/app.rs`(settings.json 驱动注入,约 L360-363 附近)

**验证**:
- `cargo test -p runtime --lib config`(新增测试:parse_optional_plan_mode)
- `cargo test -p runtime --lib planner`(确保现有测试不受影响)
- `cargo build -p runtime -p rusty-claude-cli`
- `cargo clippy -p runtime -p rusty-claude-cli --lib --tests`

**风险**:低(新增配置项,不改变现有 CLI flag 行为,向后兼容)

---

#### P3-2: Memory 语义检索层直接接入 — ✅ 已完成

**调研结论**:已在之前工作中接入,无需额外执行。

- `conversation.rs:894` — run_turn 入口调用 `memory.semantic_recall(&user_input, 3)`(top-3 召回)
- 召回结果存入 `pending_semantic_context`,注入 prompt 的 `dynamic_sections`(变动区,符合缓存保护)
- `conversation.rs:1548-1550` — turn 结束时清空,下一轮重新召回
- 默认策略:`RecallStrategy::Keyword`(关键词匹配),启用 `embedding` feature 后走向量搜索

---

#### P3-3: LSP Client 直接接入 — ❌ 评估结论不接入

**调研结论**:风险中-高,保持 LSP 作为独立工具,不侵入 read_file 高频路径。

**风险分析**:
- **性能延迟**:LSP 请求同步阻塞(10s 超时),read_file 是高频工具,每次触发 `documentSymbol` 显著拖慢
- **并发序列化**:`ProcessLspTransport::send_lock` 强制串行化,多线程并发 read_file 被阻塞
- **错误传播**:LSP 失败需明确降级策略,不能导致 read_file 失败
- **生命周期耦合**:file_ops 是无状态 IO 模块,引入 LSP 会依赖全局状态

**当前状态**:LSP 已作为独立工具 `LSP` 接入(`tools/src/lib.rs:1188`),由模型按需调用,这是更合理的架构。

---

#### P3-4: mcp_lifecycle_hardened 完整接入

**当前问题**:
- `McpLifecycleValidator` / `McpLifecycleState` 完全未引用(死代码)
- 同模块的 4 个数据型类型(`McpDegradedReport`/`McpFailedServer`/`McpErrorSurface`/`McpLifecyclePhase`)已在 `plugin_state.rs` 中手工拼装
- phase 转移未走 FSM 校验,错误手工构造

**方案**:在 `RuntimeMcpState::new` 中构造 `McpLifecycleValidator`,用 FSM 驱动 phase 转移,替代手工拼装

**执行步骤**:

1. **plugin_state.rs:RuntimeMcpState 新增 lifecycle 字段**
   - 结构体新增 `lifecycle: McpLifecycleValidator` 字段
   - `RuntimeMcpState::new` 中构造 `McpLifecycleValidator::new()`

2. **plugin_state.rs:用 Validator 驱动 phase 转移**
   - 当前 `RuntimeMcpState::new`(L36-126)手工构造 `McpFailedServer` / `McpErrorSurface` / `McpDegradedReport`
   - 改为通过 Validator 顺序驱动:
     - `run_phase(ConfigLoad)` → 配置加载
     - `run_phase(ServerRegistration)` → server 注册
     - `run_phase(SpawnConnect)` → spawn + connect
     - `run_phase(InitializeHandshake)` → initialize 握手
     - `run_phase(ToolDiscovery)` → `discover_tools_best_effort()`
     - `run_phase(Ready)` → 就绪
   - 失败时调用 `record_failure(error)` 或 `record_timeout(...)`,替代手工拼装
   - 最终从 `validator.state()` 生成 `McpDegradedReport`

3. **plugin_state.rs:call_tool / shutdown 接入**
   - `call_tool`(L145)前调用 `run_phase(Invocation)`
   - `shutdown`(L128)前调用 `run_phase(Shutdown)` → `run_phase(Cleanup)`

4. **保留向后兼容**
   - `McpDegradedReport` 的外部 API 不变(只是内部构造方式从手工改为 Validator 驱动)
   - 现有测试不应受影响

**文件**:
- `rusty-claude-cli/src/plugin_state.rs`(RuntimeMcpState 结构体 + new + call_tool + shutdown)

**验证**:
- `cargo test -p rusty-claude-cli`(全量回归)
- `cargo test -p runtime --lib mcp_lifecycle_hardened`(确保 Validator 自测通过)
- `cargo test -p rusty-claude-cli --test output_format_contract`(doctor smoke test)
- `cargo build --release`(release 构建无误)
- `cargo clippy -p rusty-claude-cli --lib --tests`

**风险**:低-中(重构 plugin_state.rs 内部构造逻辑,外部 API 不变;需充分回归测试)

---

#### P3-5: task_packet validate_packet bug 修复 — ✅ 已完成

**调研结论**:validate_packet 清空 acceptance_tests 是设计行为(canonical vs legacy dual-track),测试断言已对齐。

---

### Epic 11 — 质量与稳定性(P3)

#### P11-1: microcompact 信息丢失修复

**已知问题**:microcompact summary format 只保留首行,导致子 agent 调用和 tool result 关键信息丢失。
**方案**:改为保留前 N 行(N=3),或按结构化字段保留关键字段。

#### P11-2: 预存测试修复

**已知问题**:17 个预存测试失败(task_registry/hooks 问题)。
**方案**:逐个分析修复。

---

## 4. 优先级与依赖关系

```
Epic 7 (试点,P0) ──┐
                    ├─→ Epic 8 (P1) ──→ Epic 9 (P2,高风险)
                    │
Epic 10 (P3,收尾) ─┘
                    
Epic 11 (P3,独立) — 可并行
```

- **Epic 7 是试点**:验证 async spawn + SubagentExecutor 模式可行后再推进
- **Epic 8 依赖 Epic 7**:policy_engine 决策门需要 subagent 真实化后才有意义
- **Epic 9 依赖 Epic 7**:branch_lock 接入 spawn 需要 spawn 真实化
- **Epic 10/11 独立**:可并行推进

---

## 5. 风险矩阵

| Epic | 风险等级 | 主要风险 | 缓解措施 |
|---|---|---|---|
| Epic 7 | 中 | async runtime 改造可能影响现有同步调用 | flag-gated + Mock executor 测试先行 |
| Epic 8 | 中 | run_turn 主循环改造 | flag-gated,默认关闭 |
| Epic 9 | **高** | RuntimeMcpState 重构可能破坏 MCP 调用 | 充分回归测试 + 逐 server 验证 |
| Epic 10 | 低 | 部分接入补齐,改动小 | 逐模块独立验证 |
| Epic 11 | 低 | bug 修复,不影响架构 | 逐个测试验证 |

---

## 6. 验证策略

每个 Epic 完成后必须通过:

1. `cargo build -p runtime -p rusty-claude-cli`
2. `cargo test -p runtime --lib <相关模块>`
3. `cargo test -p rusty-claude-cli --test output_format_contract`
4. `cargo clippy -p runtime -p rusty-claude-cli --lib --tests`
5. `cargo build --release`(确保 release 构建无误)
6. 更新 `docs/harness-engineering-optimization-progress.md`

---

## 7. Phase 2 完成标准

| 指标 | Phase 1 现状 | Phase 2 目标 |
|---|---|---|
| smoke test 接入 | 19/19 (100%) | 19/19 (保持) |
| 生产路径接入 | 0/19 (0%) | ≥10/19 (≥53%) |
| 已知缺陷 | start() 空转 + microcompact 丢失 | 全部修复 |
| 预存测试失败 | 17 个 | ≤5 个 |
| 死代码 | 0 | 0(保持) |

**生产路径接入**定义:模块在真实 CLI 工作流(非 doctor 自检)中被调用,且调用结果影响用户可见行为。

---

## 8. 执行节奏

- **每个 Epic 开始前**:用 NotifyUser 征求用户确认(epic-level confirmation)
- **每个 P 级任务完成后**:更新 progress.md + 跑全量验证
- **Epic 7 完成后**:暂停,向用户汇报试点结果,确认后再推进 Epic 8-9
- **Epic 9(高风险)开始前**:额外风险评估,可能需要单独的 RFC

---

## 9. 待用户决策项

1. **Epic 7 试点是否启动?** — 这是 Phase 2 的核心,解决 known infinite polling loop
2. **Epic 9(mcp_tool_bridge 重构)是否推进?** — 风险最高,可能需要更详细评估
3. **Epic 10/11 是否并行推进?** — 低风险收尾,可与其他 Epic 并行
4. **是否需要补充其他 Epic?** — 用户可能有额外需求
