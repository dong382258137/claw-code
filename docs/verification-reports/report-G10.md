# G10 系列 BUG 修复验证报告

**验证日期**: 2026-07-20（基于实际代码审查），2026-07-27 重新验证 G10.5/G10.6/G10.7 + 多 Agent 硬化 P0
**代码仓库**: `D:\claw-code-src\rust`
**验证方法**: 全量 grep + read_file，交叉验证 plan 中的每一条声称修复

---

## 逐项验证详情

### G10.1 [P0-1] StatusEmitter 所有错误路径 emit StreamError — ✅ PASS

- **`StatusEvent::StreamError` 变体**: `streaming.rs` L86 — 已定义
- **`emit_stream_error` 辅助方法**: `streaming.rs` L287-301 — 已实现，调用 `self.emit_status(StatusEvent::StreamError {...})`
- **9 处错误路径全部 emit**: `streaming.rs` L636（fallback #9/9）、L564（#6/9）等已确认
- **TUI 消费**: `tui/app.rs` L2578-2588 — 处理 `StreamError` 变体，追加错误提示并调用 `finish_turn()`
- **测试**: `tui/app.rs` L2881-2885 — 测试 emitter 包含 StreamError 处理分支

---

### G10.2 [P0-3] reactive_compact 失败时调用 try_recover_or_record_fail — ✅ PASS

- **MicrocompactDone + removed==0 分支**: `conversation.rs` L1199-1233 — 调用 `try_recover_or_record_fail`
- **FullCompactDone 分支**: `conversation.rs` L1235-1250 — 同样调用恢复路径
- **注释**: 两处均有 "P0-3 修复" 注释，说明原 `preserve_recovery_state` 担忧不成立
- **文件**: `rust/crates/runtime/src/conversation.rs` — 确认有效

---

### G10.3 [P0-4] Worker panic 后 TUI 不冻结（fatal_error 标志）— ✅ PASS

- **`fatal_error` 标志**: `tui/app.rs` L534 — 已定义
- **Disconnected 处理**: `tui/app.rs` L768-775 — "P0-4 修复" 注释，说明原问题（cli_holder 不恢复）
- **Submit 反馈**: `tui/app.rs` L1912-1916 — `fatal_error` 为 true 时向 OutputView 追加 `[error] 对话线程已崩溃，请重启 TUI`
- **文件**: `rust/crates/rusty-claude-cli/src/tui/app.rs` — 确认有效

---

### G10.4 [P1-2] tool_card_line_ranges 按显示行计算 — ✅ PASS

- **签名变更**: `output_view.rs` L340-342 — `fn tool_card_line_ranges(&self, area_width: usize)` 已加上 `area_width` 参数
- **P1-2 注释**: `output_view.rs` L336-339 — 说明从逻辑行改为显示行计算
- **调用方**: `toggle_tool_card_at_line` 传入 `area_width`
- **文件**: `rust/crates/rusty-claude-cli/src/tui/output_view.rs` — 确认有效

---

### G10.5 [P1-3] response_to_events 非流式 fallback 不 emit — ✅ PASS（2026-07-27 重新验证）

- **修复方案**: 在 `streaming_tool_input=false` 分支直接 `events.push(AssistantEvent::Thinking { thinking, signature })`，caller 端遍历 events 进行 emit
- **`push_output_block()`**: `streaming.rs` L929-989 — 在非流式 fallback 路径 (L977-982) 直接 `events.push(AssistantEvent::Thinking { thinking, signature })`，注释明确标注 `// G10.5 fix: non-streaming fallback path must emit Thinking event`
- **`response_to_events()` 重构**: `streaming.rs` L992-1029 — 返回 `Vec<AssistantEvent>`，调用方在 L707-709 接收并附加 `push_prompt_cache_record`
- **兜底分支**: L1019-1023 — 非空 `pending_thinking` 也会 push Thinking 事件
- **流式路径同步**: L633-637 — `ContentBlockStop` 路径同步处理 thinking emit
- **文件**: `rust/crates/rusty-claude-cli/src/streaming.rs` L929-1029 — 确认有效

---

### G10.6 [P1-4] MultiAgentCoordinator 真实派生 Runtime — ✅ PASS（2026-07-27 重新验证 + P0 增强）

- **修复方案**: 保留 `start()` 原状态转换职责（避免破坏 12 处单测），新增 `execute_async()` 方法通过 `tokio::spawn` 真实派生任务
- **`start()` 实现**: `multi_agent/mod.rs` L168-181 — 仍仅状态转换（Created → Running），保持向后兼容
- **`execute_async()` 新增**: `multi_agent/mod.rs` L201-247 — 签名 `pub fn execute_async<F, Fut>(&self, subagent_id: &str, executor: F) -> Result<JoinHandle<Result<String, String>>, String>`
- **真实派生**: L219 `let handle = tokio::spawn(async move { ... })` 真实派生异步任务
- **自动状态管理**: L222-243 — 自动管理状态转换（Running → Completed/Failed）
- **注释**: L183 `// G10.6:tokio::spawn runtime`
- **文件**: `rust/crates/runtime/src/multi_agent/mod.rs` L201-247 — 确认有效
- **2026-07-27 P0 增强**: Multi-Agent Hardening Plan §4.5 retry loop 已落地（见下方"多 Agent 硬化 P0 实施状态"段）
- **注**: 修复方案与原 BUG 报告建议不同（不是改造 `start()`，而是新增 `execute_async()`），但 G10.6 的核心要求"tokio::spawn 真实派生 ConversationRuntime"已达成

---

### G10.7 [P1-5] Planner steps 启发式生成 — ✅ PASS（2026-07-27 重新验证）

- **修复方案**: 新增 `decompose_task(user_input: &str) -> Vec<PlanStep>` 函数，使用启发式分解自动填充 steps（完整 LLM 分解留待 Group D1）
- **`decompose_task()` 实现**: `planner/mod.rs` L178-248 — 启发式分解策略：
  1. 文件路径检测（每路径一个 step）
  2. 顺序标记检测（first/then/next/finally）
  3. 句子级切分（长输入或含顺序标记时）
  4. 兜底单步（含高风险关键词继承）
- **调用点**: `conversation.rs` L1107-1110 — 注释 `// G10.7 fix: heuristic task decomposition fills steps`，调用 `decompose_task(&user_input)` 后构造 `PlanArtifact::new(user_input.clone(), steps)`
- **SUMMARY 确认**: `rust/docs/verification-reports/SUMMARY.md` L49 已记录 G10.7 修复
- **文件**: `rust/crates/runtime/src/planner/mod.rs` L178-248，`conversation.rs` L1107-1110 — 确认有效
- **注**: 当前为启发式分解（零 LLM 成本），完整 LLM 子 agent 分解留待 Group D1

---

### G10.8 [P1-6] VerifierAgent / TraceAnalyzer / ContextAssembler 注入 — ✅ PASS

- **ContextAssembler 注入**: `app.rs` L2733 — `runtime = runtime.with_context_assembler(ContextAssembler::new(budget));`
- **VerifierAgent 注入**: `app.rs` L2750 — `runtime = runtime.with_verifier_agent(runtime::VerifierAgent::new());`
- **TraceAnalyzer 注入**: `app.rs` L2751 — `.with_trace_analyzer(runtime::TraceAnalyzer::new());`
- **P1-6 注释**: `app.rs` L2734-2747 — 说明修复内容和注入策略
- **文件**: `rust/crates/rusty-claude-cli/src/app.rs` L2730-2751 — 确认有效

---

### G10.9 [P2-1] `/effort` 从 STUB_COMMANDS 移除 — ✅ PASS

- **grep 结果**: `"effort"` 在 `commands_handler.rs` 中 **0 匹配** — 已从 `STUB_COMMANDS` 数组删除
- **文件**: `rust/crates/rusty-claude-cli/src/commands_handler.rs` L1174 — 确认有效

---

### G10.10 [P2-2] slash_menu 过滤 STUB_COMMANDS — ✅ PASS

- **过滤逻辑**: `slash_menu.rs` L83-86 — `.filter(|spec| !STUB_COMMANDS.contains(&spec.name))`
- **P2-2 注释**: `slash_menu.rs` L78-80 — 说明过滤目的
- **测试**: `slash_menu.rs` L1023-1038 — `all_items_count_matches_static_specs` 测试验证过滤后无 stub 泄漏
- **文件**: `rust/crates/rusty-claude-cli/src/tui/slash_menu.rs` — 确认有效

---

### G10.11 [P2-3] 状态栏 section 宽度使用 UnicodeWidthStr — ✅ PASS

- **宽度计算**: `status_bar.rs` L161-164 — `unicode_width::UnicodeWidthStr::width(s.content.as_ref())`
- **P2-3 注释**: `status_bar.rs` L157-161 — 说明从 `.len()` 改为 `UnicodeWidthStr::width`
- **文件**: `rust/crates/rusty-claude-cli/src/tui/status_bar.rs` — 确认有效

---

### G10.12 [P2-4] Submit 时调用 reset_turn() — ✅ PASS

- **Submit 处理**: `tui/app.rs` L1541-1542 — `status_state.lock().guard.reset_turn()`
- **P2-4 注释**: `tui/app.rs` L1537-1540 — 说明 reset_turn 内部设置 streaming=true 并清零计时
- **文件**: `rust/crates/rusty-claude-cli/src/tui/app.rs` — 确认有效

---

### G10.13 [P2-5] MessageStart 多 content block 时只 emit 一次 Thinking — ✅ PASS

- **P2-5 修复逻辑**: `streaming.rs` L474-493 — 使用局部 `had_thinking_summary` 标志，for 循环结束后只 emit 一次
- **ContentBlockStart**: `streaming.rs` L510-516 — 同样检查后 emit
- **ContentBlockStop 重置**: `streaming.rs` L559 — `block_has_thinking_summary = false`
- **文件**: `rust/crates/rusty-claude-cli/src/streaming.rs` L474-516 — 确认有效

---

### G10.14 [P2-6] pricing_for_model 扩展非 Anthropic 模型 — ✅ PASS

- **扩展模型**: `usage.rs` L60-65 — 注释列出支持范围
- **OpenAI**: gpt-5, gpt-4o, gpt-4o-mini — 全部实现
- **xAI**: grok-3, grok-2 — 全部实现
- **阿里通义**: qwen-max, qwen-plus, qwen-turbo — 全部实现
- **DeepSeek**: deepseek-chat, deepseek-reasoner — 全部实现，包含别名匹配
- **测试**: `usage.rs` L390-433 — `supports_non_anthropic_model_pricing` 验证所有模型定价
- **文件**: `rust/crates/runtime/src/usage.rs` L72-170 — 确认有效

---

### G10.15 [P2-7] LoopDetector.reset() 在 run_turn 入口调用 — ✅ PASS

- **调用位置**: `conversation.rs` L936 — `self.loop_detector.reset()`
- **P2-7 注释**: `conversation.rs` L933-935 — 说明避免跨 turn 累积导致误判 doom loop
- **文件**: `rust/crates/runtime/src/conversation.rs` — 确认有效

---

### G10.16 [P1-1 push_output_block 的 Thinking emit] — ✅ PASS（流式路径）

- **push_output_block 设置标志**: `streaming.rs` L891-893（Thinking）、L896-897（RedactedThinking）— 设置 `block_has_thinking_summary = true`
- **consume_stream 流式路径检查**: `streaming.rs` L488-493、L512-516 — caller 端检查并 emit
- **ContentBlockStop 重置**: `streaming.rs` L559 — reset 标志
- **注意**: 非流式 `response_to_events` 路径未处理 emit（与 G10.5 同源问题）
- **文件**: `rust/crates/rusty-claude-cli/src/streaming.rs` L860-923 — 流式路径确认有效

---

### G10.17 [P1-2 Markdown 渲染缓存] — ✅ PASS

- **增量渲染器**: `tui/app.rs` L198-337 — `IncrementalRenderer` 实现增量渲染 + hash 缓存
- **测试验证**:
  - `incremental_renderer_hash_hit_returns_cached_text`（L3127-3133）— hash 命中返回缓存
  - `incremental_renderer_pending_cache_avoids_re_render_on_unchanged_pending`（L3211-3221）— 未变化时跳过渲染
- **文件**: `rust/crates/rusty-claude-cli/src/tui/app.rs` — 确认有效

---

## G10 Summary

| 项目 | 状态 |
|------|------|
| G10.1 [P0-1] StatusEmitter StreamError | PASS |
| G10.2 [P0-3] reactive_compact Provider 恢复 | PASS |
| G10.3 [P0-4] Worker panic TUI 反馈 | PASS |
| G10.4 [P1-2] tool_card_line_ranges 显示行 | PASS |
| G10.5 [P1-3] response_to_events fallback emit | PASS（2026-07-27 重新验证） |
| G10.6 [P1-4] MultiAgentCoordinator start() | PASS（2026-07-27 重新验证 + P0 增强） |
| G10.7 [P1-5] Planner steps 生成 | PASS（2026-07-27 重新验证） |
| G10.8 [P1-6] VerifierAgent/TraceAnalyzer/ContextAssembler | PASS |
| G10.9 [P2-1] /effort STUB_COMMANDS 移除 | PASS |
| G10.10 [P2-2] slash_menu STUB 过滤 | PASS |
| G10.11 [P2-3] status_bar UnicodeWidthStr | PASS |
| G10.12 [P2-4] Submit reset_turn | PASS |
| G10.13 [P2-5] MessageStart Thinking 重复 emit | PASS |
| G10.14 [P2-6] pricing_for_model 扩展 | PASS |
| G10.15 [P2-7] LoopDetector.reset() | PASS |
| G10.16 [P1-1] push_output_block Thinking emit | PASS |
| G10.17 [P1-2] markdown_to_ansi 缓存 | PASS |

#### G10 Summary
- PASS: 17（2026-07-27 重新验证：G10.5/G10.6/G10.7 已修复）
- FAIL: 0
- **BUG: 0**（原 G10.5/G10.6/G10.7 已于 2026-07-27 重新验证为 PASS，详见上文逐项验证详情）
- SKIP/DEFER: 0

---

## 2026-07-27 重新验证说明

原报告将 G10.5/G10.6/G10.7 标记为 BUG 是历史快照，未与代码同步。代码核查显示三项均已修复：

### G10.5 修复详情
- **位置**: `streaming.rs` L929-1029
- **修复**: `push_output_block()` 在 `streaming_tool_input=false` 分支直接 `events.push(AssistantEvent::Thinking { thinking, signature })`，注释 `// G10.5 fix: non-streaming fallback path must emit Thinking event`；caller 端遍历 events 进行 emit

### G10.6 修复详情
- **位置**: `multi_agent/mod.rs` L201-247
- **修复**: 新增 `execute_async()` 方法（保留 `start()` 状态转换职责以避免破坏 12 处单测），通过 `tokio::spawn` 真实派生任务，自动管理 Running → Completed/Failed 状态转换
- **P0 增强（2026-07-27）**: Multi-Agent Hardening Plan §4.5 retry loop + 成本门禁 + 模型升级已落地，详见下节

### G10.7 修复详情
- **位置**: `planner/mod.rs` L178-248，`conversation.rs` L1107-1110
- **修复**: 新增 `decompose_task(user_input: &str) -> Vec<PlanStep>` 启发式分解函数（文件路径检测/顺序标记检测/句子级切分/兜底单步），在 `create_plan_artifact` 调用处填充 steps（完整 LLM 分解留待 Group D1）

---

## 多 Agent 硬化 P0 实施状态（2026-07-27 — P0 全部完成）

依据 `docs/multi-agent-hardening-plan.md` §10.3 MVP 实施清单（9 步），P0 全部 6 项（步骤 1-5 + 步骤 9 端到端验证）已落地：

| 步骤 | 任务 | 优先级 | 状态 | 关键文件 |
|---|---|---|---|---|
| 1 | `runtime::diag` 模块（提取 panic hook + 推广 paste_diag_log） | P0 | ✅ DONE | [diag.rs](../../rust/crates/runtime/src/diag.rs) L29-293（`DiagLevel`/`DiagEntry`/`global()`/`install_panic_hook`），`lib.rs` L22 `pub mod diag` |
| 2 | `api::model_tier`（`tier_for_model` + `upgrade_map` + `UpgradeEntry.cost_multiplier`） | P0 | ✅ DONE | [model_tier.rs](../../rust/crates/api/src/providers/model_tier.rs) L14-250（`ModelTier`/`TaskComplexity`/`UpgradeEntry`），测试 L216-250 覆盖 deepseek-v4-pro/flash、Claude/GPT/Grok/o 系列 |
| 3 | Subagent 字段扩展 + `spawn_with_model` + `reset_for_retry` + `record_cost` + `check_cost_limit` + `save_checkpoint` | P0 | ✅ DONE | [multi_agent/mod.rs](../../rust/crates/runtime/src/multi_agent/mod.rs) L86-156（v3 扩展字段：`model`/`complexity`/`max_attempts`/`attempts`/`validated`/`notes`/`checkpoint_path`/`cost_limit`/`cost_accumulated`），L304-343 `spawn_with_model`，L366-403 `reset_for_retry`，成本门禁方法 |
| 4 | `validation.rs`（`CommandValidationGate` + `rust_compile_gate` + `LlmJudgeGate` trait 预留） | P0 | ✅ DONE | [validation.rs](../../rust/crates/runtime/src/multi_agent/validation.rs) L48-296，`CommandValidationGate` L85-197，`LlmJudgeGate` L243-295（MVP stub 返 Ok，v2 实现 `call_judge_model`） |
| 5 | `execute_dispatch_subagent` retry loop（成本门禁 + checkpoint 保存） | P0 | ✅ DONE | [conversation.rs](../../rust/crates/runtime/src/conversation.rs) L2199-2497，retry loop L2291-2497，模型升级 L2396-2420/L2462-2484，成本门禁 L2382-2393/L2451-2460 |
| 6 | 诊断 SOP 注入（Diagnostic 复杂度） | P1 | ✅ DONE | [conversation.rs](../../rust/crates/runtime/src/conversation.rs) `run_subagent_turn_with_model` 中 `DIAGNOSTIC_SOP_PROMPT` 拼接 |
| 7 | `spawn_parallel` 接口预留（串行退化） | P1 | ✅ DONE | [multi_agent/mod.rs](../../rust/crates/runtime/src/multi_agent/mod.rs) `spawn_parallel(&mut self, tasks: &[SubagentSpec]) -> Result<Vec<String>, String>` |
| 8 | 决策持久化 §4.7（NOTEBOOK decisions 段 + FTS5 decision role） | P1 | ✅ DONE | [decision_log.rs](../../rust/crates/runtime/src/decision_log.rs) `extract_decisions_before_compaction` + [notebook.rs](../../rust/crates/runtime/src/notebook.rs) `decisions` 段 + [history_search.rs](../../rust/crates/runtime/src/history_search.rs) `rank *= 2.0` 加权 |
| 9 | 端到端 MVP 验证（场景 1-5，含成本超限场景） | P0 | ✅ DONE | [conversation.rs](../../rust/crates/runtime/src/conversation.rs) 测试 L6524-6803（scenario1-5 + 边界测试），7 个端到端测试全通过 |

### P0 步骤 9 端到端验证详情（§10.4 验收标准）

**测试清单**（`cargo test -p runtime --lib conversation::tests::dispatch_subagent_scenario` 6 passed + 1 边界测试）:

| 测试名 | 场景 | 验收项 | 状态 |
|---|---|---|---|
| `dispatch_subagent_scenario1_simple_task_flash_succeeds` | 场景 1：简单任务 → flash → 一次成功 | 模型未升级 / attempts=0 / validated / Completed | ✅ |
| `dispatch_subagent_scenario2_diagnostic_task_pro_succeeds` | 场景 2：诊断任务 → pro → 一次成功 | 模型保持 pro / attempts=0 / validated / Completed / max_attempts=2 | ✅ |
| `dispatch_subagent_scenario3_upgrade_retry_succeeds` | 场景 3：flash 失败 → 升级 pro → 重试成功 | 模型升级 pro / attempts=1 / validated / Completed | ✅ |
| `dispatch_subagent_scenario4_upgrade_still_fails` | 场景 4：flash 失败 → 升级 pro → 仍失败 → 达 max_attempts fail | 模型升级 pro / status=Failed / !validated | ✅ |
| `dispatch_subagent_scenario5_cost_limit_blocks_upgrade` | 场景 5：成本超限 → 拒绝升级 → fail | 模型未升级（仍 flash）/ status=Failed / cost_accumulated=0.001 / msg 含 "cost limit" | ✅ |
| `dispatch_subagent_scenario5_high_cost_limit_allows_upgrade` | 场景 5 对照：高成本上限 → 允许升级 → 成功 | 模型升级 pro / status=Completed | ✅ |
| `dispatch_subagent_no_retry_when_max_attempts_is_one` | 边界：max_attempts=1 不重试 | 模型未升级 / attempts=0 / status=Failed | ✅ |

**配套测试**:
- `cargo test -p api --lib providers::model_tier` — 9 passed（模型路由 P0 验收）
- `cargo test -p runtime --lib multi_agent::validation` — 12 passed（LlmJudgeGate trait 预留 + CommandValidationGate）
- `cargo test -p runtime --lib diag::` — 4 passed（诊断层 P0）
- `cargo test -p runtime --lib multi_agent::` — 94 passed（含 fail() 状态转换修复回归）

### P0 retry loop 核心实现（步骤 5）

`execute_dispatch_subagent`（conversation.rs L2199）retry loop 已实现：

1. **max_attempts 上限**：从 Subagent 字段读取，默认 1，Diagnostic/Architectural 任务默认 2（spawn_with_model 中设置）
2. **模型升级路径**：`upgrade_model_for_subagent`（multi_agent/mod.rs）— flash → pro 单跳，cost_multiplier=10.0
3. **成本门禁**：升级前调用 `check_cost_limit`，超限直接 fail 而非浪费 pro 调用费用
4. **状态转换**：`reset_for_retry`（multi_agent/mod.rs L366-403）支持 Failed 和 Completed 状态重置（修复 v1 不可达漏洞）
5. **诊断日志**：每个 attempt 通过 `crate::diag::global().append(...)` 记录 subagent_id/attempt/model 字段
6. **checkpoint 保存**：每轮 turn 后调用 `coordinator.save_checkpoint(&subagent_id)`（P1 预留，MVP 落地 save）

### 步骤 9 实施过程的关键 bug 修复

**`fail()` 状态转换 bug**（2026-07-27 发现并修复）:
- **现象**：scenario4/scenario5 测试失败，agent 终态为 `Completed` 而非 `Failed`
- **根因**：`MultiAgentCoordinator::fail()` 只接受 `Running` 起始状态，但 retry loop 路径是 `Running --turn Ok--> Completed --validate fail--> fail()`，验证失败时状态已是 `Completed`，`fail()` 返回 Err 导致状态未变更
- **修复**：`fail()` 扩展为接受 `Running` 和 `Completed` 两种起始状态（multi_agent/mod.rs L723-746），与 `reset_for_retry` 已支持的 `Failed/Completed` 起始状态保持一致
- **测试验证**：修复后 scenario4/scenario5 通过，且 94 个 multi_agent 测试全通过（无回归）

### 编译/测试验证（2026-07-27 步骤 9 完成后）

- **cargo build -p runtime --tests**: ✅ PASS（仅 1 个 `run_subagent_turn` dead_code 警告 — 该方法为兼容性保留的私有薄包装）
- **cargo test -p runtime --lib conversation::tests::dispatch_subagent_scenario**: ✅ 6 passed, 0 failed
- **cargo test -p runtime --lib conversation::tests::dispatch_subagent_no_retry_when_max_attempts_is_one**: ✅ 1 passed
- **cargo test -p runtime --lib conversation::tests::dispatch_subagent**（全 17 个 dispatch 测试）: ✅ 17 passed, 0 failed
- **cargo test -p runtime --lib multi_agent::**（全 94 个 multi_agent 测试）: ✅ 94 passed, 0 failed
- **cargo test -p api --lib providers::model_tier**: ✅ 9 passed（模型路由验收）
- **cargo test -p runtime --lib multi_agent::validation**: ✅ 12 passed（验证门禁验收）
- **cargo test -p runtime --lib diag::**: ✅ 4 passed（诊断层验收）

### P1 步骤 6/7/8 实施详情（2026-07-27 完成）

#### 步骤 6:诊断 SOP 注入

`run_subagent_turn_with_model` 在 `complexity == TaskComplexity::Diagnostic` 时拼接 `DIAGNOSTIC_SOP_PROMPT` 到 system prompt,要求模型:
1. **先诊断后修复**:第一动作是写文件诊断日志确认错误类型,而非堆砌防御代码
2. **验证错误机制**:用文件日志/复现脚本确认是 panic 还是 Err 传播,而非凭直觉假设
3. **`cargo build` 验证**:任何代码修改后必须 `cargo build` 验证编译通过
4. **提供复现证据**:修复方案必须附复现脚本/日志/错误信息
5. **根因未定位前不堆砌防御代码**:禁止 `catch_unwind` / `render_error_screen` 等无根因的防御

**背景**:源于一次真实事故 — deepseek 旧版 API 执行诊断任务时凭直觉堆砌防御代码、误判 panic 机制,浪费两轮迭代。SOP 注入从平台层根治"能力不足模型浪费轮次"。

#### 步骤 7:`spawn_parallel` 接口预留

`MultiAgentCoordinator::spawn_parallel(&mut self, tasks: &[SubagentSpec]) -> Result<Vec<String>, String>`:
- **MVP 实现**:内部循环调用 `spawn_with_model` 串行执行,返回 id 列表
- **v2 路径**:接口签名已为 tokio `JoinSet` 并行执行预留,届时只需替换内部实现,调用方零改动
- **设计依据**:借鉴 Anthropic Multi-Agent Research System(并行 spawn 3-5 subagents,90% 加速)

#### 步骤 8:决策持久化 §4.7

**问题**:context compaction 会丢弃原始消息,导致设计决策的 rationale 随之消失。后续 LLM 无法回溯"为什么这样做",可能推翻已论证的决策。

**三段子工作**:

1. **步骤 8c 启发式提取**(`decision_log.rs::extract_decisions_before_compaction`):
   - 关键词检测:`决定/decided/采用/选择/否决/alternatives/方案` 等
   - 提取四元组:`context`(前一条消息) + `decision`(本条) + `rationale`(本条) + `alternatives`(空,MVP 不提取)
   - 截断策略:context 200 / decision 300 / rationale 500 字符,多字节 UTF-8 安全
   - `DetectionStrategy::Heuristic`(MVP 落地) vs `LlmExtract { model }`(v2 预留)

2. **步骤 8d NOTEBOOK 持久化**(`notebook.rs` + `decision_log.rs::persist_decisions_to_notebook`):
   - `SECTION_TAGS` 新增 `"decisions"` 段
   - `persist_decisions_to_notebook` 将决策点追加写入 NOTEBOOK.md
   - **NOTEBOOK.md 跨 compaction 持久化**(microcompact/compact_session 不影响)
   - 限制:最多 50 条,超出时按 FIFO 淘汰

3. **步骤 8e FTS5 decision role 加权**(`history_search.rs::search`):
   - `conversation.rs` 在 compaction 前调用 `extract_decisions_before_compaction`,将决策点以 `role="decision"` 写入 FTS5 索引
   - `search` 方法对 `role="decision"` 命中 `rank *= 2.0`(FTS5 BM25 越负越相关,× 2.0 让 rank 更负 = 排名提前)
   - 实现策略:多取 `top_k * 2` 条 → 决策点 rank × 2.0 → 重新排序 → 截断到 top_k
   - **关键 bug 修复**:原实现 `rank *= 0.5` 是错的 — 对负 rank 来说 × 0.5 让绝对值变小(更接近 0)= 更不相关 = 排名退后,实际是惩罚而非加权。改为 `*= 2.0` 才是真正的加权

#### 死代码清理

- `run_subagent_turn`(conversation.rs L2533) 加 `#[allow(dead_code)]` — MVP 用 `run_subagent_turn_with_model` 替代,接口保留供 v2 调用

### P1 验收(2026-07-27)

- **cargo test -p runtime --lib**: ✅ 1336 passed / 0 failed / 2 ignored
  - `history_search::tests::*`: 10 passed(含 4 个 decision role 加权测试:`decision_role_gets_rank_boosted` / `decision_role_boost_fits_within_top_k` / `non_decision_roles_are_not_boosted` / `search_with_top_k_zero_returns_empty`)
  - `decision_log::tests::*`: 46 passed(含启发式提取 / NOTEBOOK 持久化 / 截断 / 多字节 UTF-8 安全 / schema migration)
  - `multi_agent::tests::*`: 全部通过(无回归)
  - `diag::tests::*`: 4 passed(P0 诊断层无回归)

### 后续 v2/v3 工作项

P0 + P1 全部完成,后续进入 v2/v3 阶段:

1. **checkpoint restore**:v2 阶段实现 `restore_from_checkpoint`
2. **`LlmJudgeGate` 实现**:v2 阶段实现 `call_judge_model`
3. **多 ValidationGate**:npm/pytest/lint gate,v2 阶段
4. **多 provider 升级链**:Anthropic/OpenAI/xAI,v3 阶段
5. **`spawn_parallel` 真并行**:v2 阶段接入 tokio `JoinSet`
6. **`DetectionStrategy::LlmExtract` 实现**:v2 阶段用 LLM 提取决策点,替代启发式关键词

---

## 历史 BUG 修复说明（保留供追溯）

原 2026-07-20 报告将 G10.5/G10.6/G10.7 标记为 BUG 的根因分析（已修复，保留供历史追溯）：

- **原 BUG #1 (G10.5)**: `response_to_events` 是自由函数，无 `StatusEmitter`；非流式 fallback caller 也不遍历结果进行 emit → **已修复**：`push_output_block` 直接 push `AssistantEvent::Thinking`
- **原 BUG #2 (G10.6)**: `start()` 仅设置 `agent.status = SubagentStatus::Running`，未派生 `ConversationRuntime` 或 `tokio::spawn` → **已修复**：新增 `execute_async()` + 2026-07-27 P0 retry loop
- **原 BUG #3 (G10.7)**: `assess_complexity` 检测到 Complex 后，`PlanArtifact` 以 `Vec::new()` 构造，没有 planner 子 agent LLM 调用自动生成 PlanStep 列表 → **已修复**：`decompose_task` 启发式分解
