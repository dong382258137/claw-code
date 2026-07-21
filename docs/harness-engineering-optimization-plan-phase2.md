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

#### P0-1: start() 真实派发(核心)

**当前问题**:
```rust
// multi_agent/mod.rs:138 — 只标记状态,不执行
pub fn start(&self, subagent_id: &str) -> Result<(), String> {
    agent.status = SubagentStatus::Running;
    Ok(())  // ← 没有实际派发
}
```

**方案**:
- `start()` 改为 `spawn(subagent_id, executor)` — 接收一个 `SubagentExecutor` trait
- `SubagentExecutor` trait 定义 `execute(task: &str) -> Result<String, String>`
- 生产实现:`LlmSubagentExecutor` — 走独立 LLM 请求(独立 prompt cache)
- 测试实现:`MockSubagentExecutor` — 返回固定结果
- 子 agent 在 `tokio::spawn` 中执行,完成后更新 status + result
- `join_all()` 改为 async,等待所有 tokio task 完成(带超时)

**文件**:
- `runtime/src/multi_agent/mod.rs`(修改 start/spawn/join_all)
- `runtime/src/multi_agent/executor.rs`(**新建**,SubagentExecutor trait + Llm/Mock 实现)
- `runtime/src/multi_agent/mod.rs` 测试更新

**验证**:
- `cargo test -p runtime --lib multi_agent`
- 新增测试:子 agent 真实执行后 status=Completed + result 非空
- 新增测试:join_all 带超时不无限轮询

#### P0-2: TaskRegistry 闭环验证

**目标**:`spawn_subagent_for_task` 调用真实 start(),任务能完成。

**文件**:
- `runtime/src/task_registry.rs`(测试更新,验证 spawn_subagent_for_task 真实完成)

**验证**:
- `cargo test -p runtime --lib task_registry`
- 新增测试:spawn_subagent_for_task 后 task.status = Completed

---

### Epic 8 — 策略与契约层生产接入(P1)

**目标**:policy_engine / green_contract / lane_events 接入 run_turn 决策链。

**风险**:中(涉及 run_turn 主循环改造,但有 flag-gated 保护)

#### P1-1: policy_engine 接入 run_turn 决策门

**方案**:
- 在 `conversation.rs` 的 `run_turn` 入口,调用 `PolicyEngine::evaluate`
- 根据 `PolicyAction`(Allow/Deny/RequireApproval)决定是否继续 turn
- 通过 `--enable-policy-engine` flag-gated(默认关闭,向后兼容)

**文件**:
- `runtime/src/conversation.rs`(run_turn 入口插入 policy 检查)
- `rusty-claude-cli/src/app.rs`(flag-gated 注入)

#### P1-2: green_contract 注入 PolicyEngine

**方案**:
- `GreenContract::evaluate` 产出 `GreenContractOutcome`
- outcome 作为 `PolicyCondition` 注入 PolicyEngine
- merge_ready 级别作为 Allow 条件,低于此级别触发 RequireApproval

**文件**:
- `runtime/src/green_contract.rs`(产出 GreenContractOutcome)
- `runtime/src/policy_engine.rs`(接受 GreenContractOutcome 作为 condition)

#### P1-3: lane_events + g004_conformance 契约校验闭环

**方案**:
- `try_publish` 前调用 `validate_g004_contract_bundle`
- 校验失败时返回 Err,阻止事件发布
- 校验通过后正常发布 + drain

**文件**:
- `runtime/src/lane_events.rs`(try_publish 内嵌 g004 校验)

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

#### P3-1: Plan/Execute/Review 默认启用评估

**当前**:flag-gated,未传 flag 时 planner 不激活。
**方案**:评估是否改为 settings.json 配置项(默认关闭,用户可开启)。

#### P3-2: Memory 语义检索层直接接入

**当前**:经 PersistentMemory 间接调用。
**方案**:在 `run_turn` 末尾调用 `SemanticRecaler::recall`,将召回结果注入下一轮 prompt。

#### P3-3: LSP Client 直接接入

**当前**:经 RepoMap 间接调用。
**方案**:评估是否在 `read_file` 时直接调用 LSP 提供符号信息(风险:可能影响性能)。

#### P3-4: mcp_lifecycle_hardened 完整接入

**当前**:`McpLifecycleValidator`/`McpLifecycleState` 完全未引用。
**方案**:在 `RuntimeMcpState::new` 后构造 `McpLifecycleValidator`,定期 healthcheck。

#### P3-5: task_packet validate_packet bug 修复

**当前**:预存测试 `creates_task_from_packet` 失败,validate_packet 把 acceptance_tests 清空。
**方案**:修复 validate_packet 逻辑,不清空 acceptance_tests。

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
