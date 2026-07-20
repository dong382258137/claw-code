# 代码审查与修复计划（commit ed2965a）

> 基于对 TUI / CLI / Runtime 三个模块的深度分析 + 实际代码核查后的修正版报告。
> 所有结论均通过实际读取代码验证，已剔除 1 条不属实问题（原 P1-1），将 1 条 P0 问题降级为潜伏缺陷（原 P0-2）。

## 1. 核查结论总览

| 模块 | 核查数 | 属实 | 部分属实 | 不属实 |
|:-:|:-:|:-:|:-:|:-:|
| TUI | 6 | 5 | 0 | 1（原 P1-1 已修复） |
| Runtime | 8 | 8 | 0 | 0 |
| CLI/Verifier | 3 | 2 | 1（原 P0-2 降级） | 0 |
| **合计** | **17** | **15** | **1** | **1** |

## 2. 重大修正项

### 修正-1：原 P1-1 Help 浮层非真正模态 — **不属实（bug 已修复）**

`tui/app.rs` L607 调用 `route_key(&mut input, key, help_visible)` 传入 help_visible 参数；
`tui/app.rs` L951-L977 `route_key` 函数开头即 short-circuit 返回 `InputAction::Ignore`，
字符不会写入 buffer，Enter 也不会触发 `self.reset()`。代码注释明确说明这是"Bug L4 修复"。
**此条从 BUG 列表删除。**

### 修正-2：原 P0-2 VerifierAgent placeholder 导致 plan 永远失败 — **部分属实（潜伏缺陷，生产环境不触发）**

- placeholder 总返回 `passed: false` — 属实（`verifier/visual.rs` L47-61 / `verifier/model_judge.rs` L50-68）
- Review 调用链存在 mark_failed — 属实（`conversation.rs` L1081-L1120）
- **关键修正**：VerifierAgent 从未注入主流程。`with_verifier_agent` / `set_verifier_agent` 在整个 `rust/crates` 代码库中无任何外部调用方，仅在 `conversation.rs` L498/L504 定义处出现。`app.rs` L748-780 `prepare_turn_runtime` 只调用 `build_runtime(...).with_hook_abort_signal(...)` + `set_tool_verbosity`，未注入 VerifierAgent。
- `self.verifier_agent` 永远为 `None`，`if let Some(verifier) = ...` 分支永远不进入，BUG 不会触发。
- **应从 P0 降级为潜伏缺陷（P3），与 P1-6 合并处理**。

## 3. 待修复问题清单（修正后）

### P0 级（3 条）

#### P0-1 StatusEmitter 在所有错误路径完全沉默 — **属实**

- **位置**：`rusty-claude-cli/src/streaming.rs` L368-374 / L392-400 / L402-406 / L465-468 / L496-499 / L506-509 / L523-526 / L559-562
- **现象**：`consume_stream` 共 9 处 `emit_status` 调用全部在成功路径，所有 **9 处**（原报告说 8 处，漏算 L403-405）错误返回路径都没有 emit 任何事件。流式失败时 StatusBar 收不到 `MessageStop` 或错误信号，`streaming: true` 一直保留，UI 永久假死。
- **修复**：在 `StatusEvent` 新增 `StreamError { message: String, recoverable: bool }` 变体；在每个 `return Err(...)` 前调用 `self.emit_status(StatusEvent::StreamError { ... })`；在 `tui/app.rs` StatusEvent 消费分支增加 `StreamError` 处理，调用 `state.finish_turn()` 并显示错误。

#### P0-3 reactive_compact 在 MicrocompactDone → 全 compaction 失败时跳过 Provider 恢复 — **属实**

- **位置**：`runtime/src/conversation.rs` L872-898
- **现象**：`MicrocompactDone` + `removed==0` 分支和 `FullCompactDone` 分支都直接 `record_turn_failed + return Err`，跳过 `try_recover_or_record_fail`。注释"避免 reactive_state 重置导致 API 调用翻倍"在 L886-888。
- **修复**：在 `MicrocompactDone` + `removed==0` 分支尝试 `preserve_recent=0` 更激进 microcompact；如果仍失败，调用 `try_recover_or_record_fail` 但不重置 `reactive_state`（修改函数签名加 `preserve_recovery_state: bool`）。

#### P0-4 Worker 线程 panic 后 TUI 永久冻结 — **属实（触发条件比原报告窄）**

- **位置**：`rusty-claude-cli/src/tui/app.rs` L339-348
- **现象**：Disconnected 分支只清理 turn_rx / turn_start / streaming，没恢复 cli_holder。后续 Submit 检查 `cli_holder.is_some() && turn_rx.is_none()` 永远 false，Enter 键无任何反应。
- **重要补充**：worker 线程用 `catch_unwind` 包裹（L780-852），大多数 panic 通过 `tx.send(turn_result)` 返回 Err → 走 L322-L335 Ok 分支 → cli_holder 在 L329 恢复。**只有 catch_unwind 自身失败、thread::spawn 失败、tx.send 之前 panic 才触发**。
- **修复**：在 Disconnected 分支向 OutputView 追加 `[error] 对话线程已崩溃，请重启 TUI`，并标记 `fatal_error` 标志。

### P1 级（5 条）

#### P1-2 鼠标点击显示行↔逻辑行映射错位 — **属实**

- **位置**：`rusty-claude-cli/src/tui/app.rs` L901-908（click 计算）+ L441-L463（`last_scroll_y` 显示行）+ `tui/output_view.rs` L241-L261（`tool_card_line_ranges` 逻辑行）
- **现象**：`last_scroll_y` 基于显示行（考虑 wrap），但 `toggle_tool_card_at_line` 匹配 `[start, end)` 用 `lines().count()` 是逻辑行。
- **补充**：代码注释 L238-L240 主动承认此偏差，作者已知并接受。但仍应修复以避免长行场景下点击错位。
- **修复**：让 `tool_card_line_ranges` 按显示行计算（累加每行 `ceil(width/area_width)`），需要传入 area_width。

#### P1-3 response_to_events 非流式 fallback 完全不 emit — **属实**

- **位置**：`rusty-claude-cli/src/streaming.rs` L823-826（函数签名无 self 无 emitter）+ L553-565（fallback 路径）
- **修复**：给 `response_to_events` 加 `Option<&StatusEmitter>` 参数，或在 fallback 分支手动遍历 events 并 emit。

#### P1-4 MultiAgentCoordinator 是空壳 — **属实**

- **位置**：`runtime/src/multi_agent/mod.rs` L104-135（spawn）/ L138-151（start）/ L247-279（join_all）
- **修复**：在 `start` 时实际派生独立 `ConversationRuntime`，通过 `tokio::spawn` 异步执行。

#### P1-5 Planner steps 永远为空 — **属实（路径偏差已修正）**

- **位置**：`runtime/src/planner/mod.rs` L62-82（`assess_complexity` 函数，**runtime crate 中无 `Planner` 结构体**）+ `runtime/src/conversation.rs` L755（`PlanArtifact::new(user_input, Vec::new())`）
- **修复**：在 Complex 时调用子 agent LLM 生成 PlanStep 列表；或注入 `update_plan` 工具让主 agent 填充。

#### P1-6 VerifierAgent / TraceAnalyzer / ContextAssembler 未注入主流程 — **属实（路径偏差已修正）**

- **位置**：`rusty-claude-cli/src/app.rs` L748-780（`prepare_turn_runtime`）+ `app.rs` L2282-2347（`build_runtime_with_plugin_state`）
- **现象**：三个 setter 仅在 `conversation.rs` L2690 测试代码中调用，生产 0 调用点。
- **修复**：在 `build_runtime_with_plugin_state` 中根据 `feature_config` 注入这三个组件。

### P2 级（7 条）

#### P2-1 `/effort` 被错误列入 STUB_COMMANDS — **属实**

- **位置**：`rusty-claude-cli/src/commands_handler.rs` L1171（STUB_COMMANDS 包含 "effort"）+ L1259-1261（过滤）+ `app.rs` L1187-1219（`/effort` 已实现）
- **修复**：从 STUB_COMMANDS 数组删除 `"effort"` 字符串（1 行改动）。

#### P2-2 slash_menu 不过滤 STUB_COMMANDS — **属实**

- **位置**：`rusty-claude-cli/src/tui/slash_menu.rs` L36（`SlashMenu::new()` 全量加载 145 个 spec，无过滤）
- **数据**：`SLASH_COMMAND_SPECS` 共 145 个条目，`STUB_COMMANDS` 共 100 个条目，TUI 菜单实际显示 145 条（约 100 条是 stub，仅 45 条可用）。
- **修复**：在 `SlashMenu::new()` 中过滤 `STUB_COMMANDS.contains(&spec.name)` 的项。

#### P2-3 状态栏 section 宽度用字节长度而非显示宽度 — **属实**

- **位置**：`rusty-claude-cli/src/tui/status_bar.rs` L195
- **现象**：`section.iter().map(|s| s.content.len()).sum()` 返回字节长度。状态栏含中文/emoji：「经由」「令牌」「穷人模式」「🔢💰🌿⏱🎯🪼」等。对比 `app.rs` L450 同类代码已正确用 `UnicodeWidthStr::width`。
- **修复**：改用 `unicode_width::UnicodeWidthStr::width(s.content.as_ref())`。

#### P2-4 Submit 时未立即设置 streaming=true — **属实**

- **位置**：`rusty-claude-cli/src/tui/app.rs` L685-695
- **修复**：Submit 分支后立即 `status_state.lock().reset_turn()` 或设置 `streaming = true`。

#### P2-5 MessageStart 多 content block 时重复 emit Thinking — **属实**

- **位置**：`rusty-claude-cli/src/streaming.rs` L417-438（for 循环每次迭代后检查并 emit）+ L811-818（push_output_block 只 set 不 reset）
- **修复**：emit 后立即 `block_has_thinking_summary = false`，或把 emit 检查移到 for 循环外（只 emit 一次）。

#### P2-6 pricing_for_model 不覆盖非 Anthropic 模型 — **属实**

- **位置**：`runtime/src/usage.rs` L59-81
- **现象**：只匹配 `haiku`/`opus`/`sonnet`，其他 Provider（xAI/OpenAI/OpenRouter/DashScope/Ollama）模型返回 `None`。
- **修复**：扩展 `pricing_for_model` 覆盖 gpt-5/grok-3/qwen-max 等常见模型。

#### P2-7 LoopDetector.reset() 永不调用 — **属实**

- **位置**：`runtime/src/loop_detection.rs` L103-107 + `conversation.rs` L628（只调 `record_edit`）
- **修复**：在 `run_turn` 入口调用 `self.loop_detector.reset()`，或改为基于时间窗口的滑动计数。

## 4. 修复批次计划

按"风险→收益→复杂度"排序，分 8 个批次：

| 批次 | 优先级 | 内容 | 预估改动 |
|:-:|:-:|:-|:-|
| 1 | 高 | P2-1 + P2-2（STUB_COMMANDS 删 effort + slash_menu 加 STUB 过滤） | ~10 行 |
| 2 | 高 | P2-3 + P2-4 + P2-5（TUI UI 反馈类） | ~20 行 |
| 3 | 高 | P0-1（StatusEmitter 错误路径 + StreamError variant） | ~50 行 |
| 4 | 中 | P1-2（鼠标点击按显示行映射） | ~30 行 |
| 5 | 高 | P0-4（Worker panic 恢复反馈） | ~15 行 |
| 6 | 高 | P0-3（reactive_compact 恢复路径） | ~40 行 |
| 7 | 中 | P0-2/P1-4/P1-5/P1-6（runtime harness placeholder + 注入） | ~100 行 |
| 8 | 中 | P2-6 + P2-7（pricing 扩展 + LoopDetector reset） | ~30 行 |

每批修复后立即 `cargo build` 验证编译，并运行相关单元测试。

## 5. 已剔除项

| 原编号 | 描述 | 剔除原因 |
|:-:|:-|:-|
| 原 P1-1 | Help 浮层非真正模态，Enter 静默丢失输入 | Bug L4 已修复，route_key 在 help_visible 时短路返回 Ignore |
| 原 P0-2（部分） | "plan 永远失败"的生产后果 | VerifierAgent 未注入主流程，BUG 不会触发；与 P1-6 合并为潜伏缺陷 |

## 6. 已知预存在测试失败（与本次修复无关）

3 个失败测试位于 `rusty-claude-cli/tests/output_format_contract.rs`：
- L223-298：`plugins_json_surfaces_lifecycle_contract_when_plugin_is_installed`（Windows 不支持 .sh 脚本）
- L685-762：`mcp_json_reports_required_optional_and_redacts_secret_values`（MCP Http 解析 regression）
- L764-818：`mcp_degraded_config_and_failed_usage_are_distinct_json_contracts`（degraded config 处理变化）

不在本次修复范围。
