# Parity Status — claw-code Rust Port

Last updated: 2026-07-27

## Mock parity harness — milestone 1

- [x] Deterministic Anthropic-compatible mock service (`rust/crates/mock-anthropic-service`)
- [x] Reproducible clean-environment CLI harness (`rust/crates/rusty-claude-cli/tests/mock_parity_harness.rs`)
- [x] Scripted scenarios: `streaming_text`, `read_file_roundtrip`, `grep_chunk_assembly`, `write_file_allowed`, `write_file_denied`

## Mock parity harness — milestone 2 (behavioral expansion)

- [x] Scripted multi-tool turn coverage: `multi_tool_turn_roundtrip`
- [x] Scripted bash coverage: `bash_stdout_roundtrip`
- [x] Scripted permission prompt coverage: `bash_permission_prompt_approved`, `bash_permission_prompt_denied`
- [x] Scripted plugin-path coverage: `plugin_tool_roundtrip`
- [x] Behavioral diff/checklist runner: `rust/scripts/run_mock_parity_diff.py`

## Harness v2 behavioral checklist

Canonical scenario map: `rust/mock_parity_scenarios.json`

- Multi-tool assistant turns
- Bash flow roundtrips
- Permission enforcement across tool paths
- Plugin tool execution path
- File tools — harness-validated flows

## Completed Behavioral Parity Work

Hashes below come from `git log --oneline`. Merge line counts come from `git show --stat <merge>`.

| Lane | Status | Feature commit | Merge commit | Diff stat |
|------|--------|----------------|--------------|-----------|
| Bash validation (9 submodules) | ✅ complete | `36dac6c` | — (`jobdori/bash-validation-submodules`) | `1005 insertions` |
| CI fix | ✅ complete | `89104eb` | `f1969ce` | `22 insertions, 1 deletion` |
| File-tool edge cases | ✅ complete | `284163b` | `a98f2b6` | `195 insertions, 1 deletion` |
| TaskRegistry | ✅ complete | `5ea138e` | `21a1e1d` | `336 insertions` |
| Task tool wiring | ✅ complete | `e8692e4` | `d994be6` | `79 insertions, 35 deletions` |
| Team + cron runtime | ✅ complete | `c486ca6` | `49653fe` | `441 insertions, 37 deletions` |
| MCP lifecycle | ✅ complete | `730667f` | `cc0f92e` | `491 insertions, 24 deletions` |
| LSP client | ✅ complete | `2d66503` | `d7f0dc6` | `461 insertions, 9 deletions` |
| Permission enforcement | ✅ complete | `66283f4` | `336f820` | `357 insertions` |

## Tool Surface: 40/40 (spec parity)

### Real Implementations (behavioral parity — varying depth)

| Tool | Rust Impl | Behavioral Notes |
|------|-----------|-----------------|
| **bash** | `runtime::bash` 283 LOC | subprocess exec, timeout, background, sandbox — **strong parity**. 9/9 requested validation submodules are now tracked as complete via `36dac6c`, with on-main sandbox + permission enforcement runtime support |
| **read_file** | `runtime::file_ops` | offset/limit read — **good parity** |
| **write_file** | `runtime::file_ops` | file create/overwrite — **good parity** |
| **edit_file** | `runtime::file_ops` | old/new string replacement — **good parity**. Missing: replace_all was recently added |
| **glob_search** | `runtime::file_ops` | glob pattern matching — **good parity** |
| **grep_search** | `runtime::file_ops` | ripgrep-style search — **good parity** |
| **WebFetch** | `tools` | URL fetch + content extraction — **moderate parity** (need to verify content truncation, redirect handling vs upstream) |
| **WebSearch** | `tools` | search query execution — **moderate parity** |
| **TodoWrite** | `tools` | todo/note persistence — **moderate parity** |
| **Skill** | `tools` | skill discovery/install — **moderate parity** |
| **Agent** | `tools` | agent delegation — **moderate parity** |
| **TaskCreate** | `runtime::task_registry` + `tools` | in-memory task creation wired into tool dispatch — **good parity** |
| **TaskGet** | `runtime::task_registry` + `tools` | task lookup + metadata payload — **good parity** |
| **TaskList** | `runtime::task_registry` + `tools` | registry-backed task listing — **good parity** |
| **TaskStop** | `runtime::task_registry` + `tools` | terminal-state stop handling — **good parity** |
| **TaskUpdate** | `runtime::task_registry` + `tools` | registry-backed message updates — **good parity** |
| **TaskOutput** | `runtime::task_registry` + `tools` | output capture retrieval — **good parity** |
| **TeamCreate** | `runtime::team_cron_registry` + `tools` | team lifecycle + task assignment — **good parity** |
| **TeamDelete** | `runtime::team_cron_registry` + `tools` | team delete lifecycle — **good parity** |
| **CronCreate** | `runtime::team_cron_registry` + `tools` | cron entry creation — **good parity** |
| **CronDelete** | `runtime::team_cron_registry` + `tools` | cron entry removal — **good parity** |
| **CronList** | `runtime::team_cron_registry` + `tools` | registry-backed cron listing — **good parity** |
| **LSP** | `runtime::lsp_client` + `tools` | registry + dispatch for diagnostics, hover, definition, references, completion, symbols, formatting — **good parity** |
| **ListMcpResources** | `runtime::mcp_tool_bridge` + `tools` | connected-server resource listing — **good parity** |
| **ReadMcpResource** | `runtime::mcp_tool_bridge` + `tools` | connected-server resource reads — **good parity** |
| **MCP** | `runtime::mcp_tool_bridge` + `tools` | stateful MCP tool invocation bridge — **good parity** |
| **ToolSearch** | `tools` | tool discovery — **good parity** |
| **NotebookEdit** | `tools` | jupyter notebook cell editing — **moderate parity** |
| **Sleep** | `tools` | delay execution — **good parity** |
| **SendUserMessage/Brief** | `tools` | user-facing message — **good parity** |
| **Config** | `tools` | config inspection — **moderate parity** |
| **EnterPlanMode** | `tools` | worktree plan mode toggle — **good parity** |
| **ExitPlanMode** | `tools` | worktree plan mode restore — **good parity** |
| **StructuredOutput** | `tools` | passthrough JSON — **good parity** |
| **REPL** | `tools` | subprocess code execution — **moderate parity** |
| **PowerShell** | `tools` | Windows PowerShell execution — **moderate parity** |

### Stubs Only (surface parity, no behavior)

| Tool | Status | Notes |
|------|--------|-------|
| **TestingPermission** | stub | test-only, low priority (`tools/src/lib.rs` L2605-2611 硬编码 `permitted: true`) |

### Recently Promoted Out of Stub (2026-07-27 audit)

| Tool | Status | Notes |
|------|--------|-------|
| **AskUserQuestion** | ✅ real | `tools/src/lib.rs` L1963 `run_ask_user_question` — 真实 stdin/stdout 交互 + TUI handler 注入 (L1970-1981) + 全局 `ASK_USER_QUESTION_HANDLER` OnceLock (L2047-2056) + 2 个回归测试 (L11888-11928) |
| **McpAuth** | ✅ real (lightweight) | `tools/src/lib.rs` L2421 `run_mcp_auth` — 从 `global_mcp_registry()` 查询服务器状态并返回 `status`/`server_info`/`tool_count`/`resource_count` |
| **RemoteTrigger** | ✅ real | `tools/src/lib.rs` L2440 `run_remote_trigger` — 真实 HTTP client (reqwest)，支持 GET/POST/PUT/DELETE/PATCH/HEAD，含 SSRF 防护 (L2446-2448)、自定义 headers、body |

## Slash Commands: 67/141 upstream entries

- 27 original specs (pre-today) — all with real handlers
- 40 new specs — parse + stub handler ("not yet implemented")
- Remaining ~74 upstream entries are internal modules/dialogs/steps, not user `/commands`

### Behavioral Feature Checkpoints (completed work + remaining gaps)

**Bash tool — 9/9 requested validation submodules complete:**
- [x] `sedValidation` — validate sed commands before execution
- [x] `pathValidation` — validate file paths in commands
- [x] `readOnlyValidation` — block writes in read-only mode
- [x] `destructiveCommandWarning` — warn on rm -rf, etc.
- [x] `commandSemantics` — classify command intent
- [x] `bashPermissions` — permission gating per command type
- [x] `bashSecurity` — security checks
- [x] `modeValidation` — validate against current permission mode
- [x] `shouldUseSandbox` — sandbox decision logic

Harness note: milestone 2 validates bash success plus workspace-write escalation approve/deny flows; dedicated validation submodules landed in `36dac6c`, and on-main runtime also carries sandbox + permission enforcement.

**File tools — completed checkpoint:**
- [x] Path traversal prevention (symlink following, ../ escapes)
- [x] Size limits on read/write
- [x] Binary file detection
- [x] Permission mode enforcement (read-only vs workspace-write)

Harness note: read_file, grep_search, write_file allow/deny, and multi-tool same-turn assembly are now covered by the mock parity harness; file edge cases + permission enforcement landed in `a98f2b6` and `336f820`.

**Config/Plugin/MCP flows:**
- [x] Full MCP server lifecycle (connect, list tools, call tool, disconnect)
- [ ] Plugin install/enable/disable/uninstall full flow (`plugin_lifecycle.rs` 8 态状态机完整，但 trait 仅 4 方法 validate_config/healthcheck/discover/shutdown，缺 install/enable/disable/uninstall 动作)
- [x] Config merge precedence (user > project > local) — `config.rs` L385-412 `discover()` + L421-441 `load()` 已实现 user→project→local 累积合并，测试 L1733 `loads_and_merges_claude_code_config_files_by_precedence`

Harness note: external plugin discovery + execution is now covered via `plugin_tool_roundtrip`; MCP lifecycle landed in `cc0f92e`; config merge precedence landed in `config.rs` L385-441. Plugin install/enable/disable/uninstall remain open.

## Runtime Behavioral Gaps

- [x] Permission enforcement across all tools (read-only, workspace-write, danger-full-access)
- [x] Output truncation (large stdout/file content) — `bash.rs` L726-747 `MAX_OUTPUT_BYTES=64KiB` + `truncate_output()` + 测试 L751-779；`file_ops.rs` L14-17 `MAX_READ_SIZE=10MB`/`MAX_WRITE_SIZE=10MB`，grep 结果 L350 `truncated: bool`，截断逻辑 L936-947/L1207-1211
- [ ] Session compaction behavior matching — `compact.rs` 框架完整（CompactBoundary/CompactTrigger/CompactionConfig/should_compact/compact_session），但与上游行为对齐无证据
- [x] Token counting / cost tracking accuracy — `usage.rs` L13-77 `ModelPricing` + `EstimatedCost::total_cost_usd()` + `pricing_for_model()`；G10.14 验证已覆盖非 Anthropic 模型 (gpt-5/gpt-4o、grok-3/grok-2、qwen-max/plus/turbo、deepseek-chat/reasoner)，测试 L390-433
- [x] Streaming response support validated by the mock parity harness

Harness note: current coverage now includes write-file denial, bash escalation approve/deny, and plugin workspace-write execution paths; permission enforcement landed in `336f820`; output truncation landed in `bash.rs`/`file_ops.rs`; token/cost tracking扩展 landed in `usage.rs` (G10.14).

## Multi-Agent Hardening (v3 MVP — P0 全部完成)

依据 `docs/multi-agent-hardening-plan.md` §10.3 MVP 实施清单（9 步），P0 全部 6 项（步骤 1-5 + 步骤 9 端到端验证）已落地。详细验证见 `docs/verification-reports/report-G10.md` "多 Agent 硬化 P0 实施状态" 段。

### P0 已完成项（步骤 1-5 + 步骤 9）

| 步骤 | 任务 | 状态 | 关键文件 |
|---|---|---|---|
| 1 | `runtime::diag` 模块（提取 panic hook + 推广 paste_diag_log） | ✅ DONE | [diag.rs](../crates/runtime/src/diag.rs) L29-293（`DiagLevel`/`DiagEntry`/`global()`/`install_panic_hook`），`lib.rs` L22 `pub mod diag` |
| 2 | `api::model_tier`（`tier_for_model` + `upgrade_map` + `UpgradeEntry.cost_multiplier`） | ✅ DONE | [model_tier.rs](../crates/api/src/providers/model_tier.rs) L14-208（`ModelTier`/`TaskComplexity`/`UpgradeEntry`），测试 L214-250 覆盖 deepseek-v4-pro/flash、Claude/GPT/Grok/o 系列 |
| 3 | Subagent 字段扩展 + `spawn_with_model` + `reset_for_retry` + `record_cost` + `check_cost_limit` + `save_checkpoint` | ✅ DONE | [multi_agent/mod.rs](../crates/runtime/src/multi_agent/mod.rs) L86-156（v3 扩展字段：`model`/`complexity`/`max_attempts`/`attempts`/`validated`/`notes`/`checkpoint_path`/`cost_limit`/`cost_accumulated`），L304 `spawn_with_model`，L366 `reset_for_retry`，L424 `save_checkpoint`，L546 `check_cost_limit`，L559 `add_cost` |
| 4 | `validation.rs`（`CommandValidationGate` + `rust_compile_gate` + `LlmJudgeGate` trait 预留） | ✅ DONE | [validation.rs](../crates/runtime/src/multi_agent/validation.rs) L48 `ValidationGate` trait，L85 `CommandValidationGate`，L202 `rust_compile_gate`，L244 `LlmJudgeGate`（MVP stub 返 Ok，v2 实现 `call_judge_model`） |
| 5 | `execute_dispatch_subagent` retry loop（成本门禁 + checkpoint 保存） | ✅ DONE | [conversation.rs](../crates/runtime/src/conversation.rs) L2199-2497，retry loop 含模型升级 + 成本门禁 + 诊断日志 + checkpoint 保存 |
| 9 | 端到端 MVP 验证（§10.4 验收标准，场景 1-5 全覆盖） | ✅ DONE | [conversation.rs](../crates/runtime/src/conversation.rs) 测试 `dispatch_subagent_scenario1_simple_task_flash_succeeds` / `scenario2_diagnostic_task_pro_succeeds` / `scenario3_upgrade_retry_succeeds` / `scenario4_upgrade_still_fails` / `scenario5_cost_limit_blocks_upgrade` + 补充 `scenario5_high_cost_limit_allows_upgrade` / `no_retry_when_max_attempts_is_one` |

### P0 核心能力清单

- [x] **模型能力分级**：`tier_for_model` 识别 Budget/Standard/Flagship 三层（覆盖 deepseek-v4-pro/flash、Claude/GPT/Grok/o 系列）
- [x] **任务复杂度匹配**：`model_meets_complexity` 拒绝 Budget 模型执行 Diagnostic/Architectural 任务
- [x] **模型升级链**：`upgrade_model` + `upgrade_cost_multiplier` 实现 flash → pro 单跳升级（cost_multiplier=10.0）
- [x] **成本门禁**：`Subagent.cost_limit`/`cost_accumulated` + `check_cost_limit` 升级前校验，超限直接 fail 而非浪费 pro 调用费用
- [x] **验证门禁**：`CommandValidationGate` 执行 `cargo build` 类命令验证，`LlmJudgeGate` trait 预留（MVP stub）
- [x] **重试循环**：`execute_dispatch_subagent` retry loop 含 max_attempts 上限、模型升级、状态重置（支持 Failed/Completed 重置）、诊断日志
- [x] **统一诊断**：`runtime::diag` 模块 + `install_panic_hook()` + `DiagLog::global()` 记录 retry 链路
- [x] **Checkpoint 预留**：`save_checkpoint` 每轮 turn 后落盘（P1 restore 留待 v2）
- [x] **端到端验证**：场景 1-5 全部通过（简单成功/诊断成功/升级重试成功/升级仍失败/成本超限拒绝升级）

### §10.4 验收标准对应

**模型路由(P0)** — ✅ `cargo test -p api providers::model_tier` 9 passed:
- [x] `tier_for_model("deepseek-v4-flash") == ModelTier::Budget`
- [x] `tier_for_model("deepseek-v4-pro") == ModelTier::Flagship`
- [x] `upgrade_model("deepseek-v4-flash") == Some("deepseek-v4-pro")`（API 简化为返回 `Option<String>`，见下方"API 差异说明"）
- [x] `upgrade_model("deepseek-v4-pro") == None`（已顶级）
- [x] `model_meets_complexity("deepseek-v4-flash", Diagnostic) == false`
- [x] `model_meets_complexity("deepseek-v4-pro", Diagnostic) == true`

**成本门禁(P0 v3 新增)** — ✅ `dispatch_subagent_scenario5_*` 2 tests:
- [x] `record_cost` 正确累加 `cost_accumulated`（scenario5 验证 flash $0.001 累计）
- [x] `check_cost_limit` 在 accumulated > limit 时返回 false（拒绝升级）
- [x] `check_cost_limit` 在 accumulated ≤ limit 时返回 true（允许升级，scenario5_high 验证）
- [x] 场景 5：cost_limit=0.0005 时，flash 失败后升级 pro 被成本门禁拒绝，不浪费 pro 调用

**端到端流程(P0)** — ✅ 场景 1-5 + 边界测试全通过:
- [x] 简单任务路由到 flash，一次成功（scenario1）
- [x] 诊断任务路由到 pro，一次成功（scenario2）
- [x] 诊断任务误派 flash，validate 失败后自动升级 pro 重试成功（scenario3）
- [x] 升级后仍失败，正确返回 fail 而非无限重试（scenario4）
- [x] 成本超限时，正确返回 fail 而非强行升级（scenario5）
- [x] max_attempts=1 时不重试，第一次 validate 失败直接 fail（边界测试）

**诊断层(P0)** — ✅ `cargo test -p runtime diag::` 4 passed:
- [x] `DiagLog::global()` 记录完整 retry 链路（attempt/model/cost 字段，见 `execute_dispatch_subagent` L2301-2325 诊断日志）
- [x] `install_panic_hook()` 在 panic 时生成 crash log（`diag::tests::install_panic_hook_does_not_panic` 验证）

**LlmJudgeGate trait 预留(P0)** — ✅ `cargo test -p runtime multi_agent::validation` 12 passed:
- [x] `LlmJudgeGate` 实现 `ValidationGate` trait，编译通过（`llm_judge_gate_implements_validation_gate_trait`）
- [x] `LlmJudgeGate::diagnostic_default` rubric 含根因定位/方案可行性/完整性/副作用评估四维（`llm_judge_gate_diagnostic_default_rubric_contains_four_dimensions`）
- [x] MVP 阶段不注册 `LlmJudgeGate`（诊断任务用人工验收 + rust_compile_gate，stub 返 Ok）

### P1 已完成项（MVP 阶段,2026-07-27）

- [x] 步骤 6：诊断 SOP 注入（Diagnostic 复杂度时注入系统提示） — `conversation.rs::run_subagent_turn_with_model` 在 `complexity == Diagnostic` 时拼接 `DIAGNOSTIC_SOP_PROMPT`,要求"先诊断后修复、写诊断日志、`cargo build` 验证、提供复现证据、根因未定位前不堆砌防御代码"
- [x] 步骤 7：`spawn_parallel` 接口预留（MVP 串行退化,v2 接入 tokio 真并行） — `MultiAgentCoordinator::spawn_parallel(&mut self, tasks: &[SubagentSpec]) -> Result<Vec<String>, String>` 接口就位,MVP 内部循环调用 `spawn_with_model` 串行执行,接口签名已为 v2 tokio 并行预留
- [x] 步骤 8：决策持久化 §4.7 — 三个子步骤全部落地:
  - [x] 步骤 8c:`decision_log.rs::extract_decisions_before_compaction` 启发式提取(关键词检测 "决定/decided/采用/否决/alternatives" 等 + 上下文/决策/理由/替代方案四元组 + 200/300/500 字符截断)
  - [x] 步骤 8d:`notebook.rs` SECTION_TAGS 新增 `"decisions"` 段,`persist_decisions_to_notebook` 将决策点追加写入 NOTEBOOK.md(跨 compaction 持久化)
  - [x] 步骤 8e:`history_search.rs::search` 对 `role="decision"` 命中 `rank *= 2.0`(FTS5 BM25 越负越相关,× 2.0 让 rank 更负 = 排名提前),`conversation.rs` 在 compaction 前自动提取并写入 FTS5 索引
- [x] 死代码清理:`run_subagent_turn` 加 `#[allow(dead_code)]`(MVP 用 `run_subagent_turn_with_model` 替代,接口保留供 v2 调用)

**P1 验收** — ✅ `cargo test -p runtime --lib` 1336 passed / 0 failed / 2 ignored:
- `history_search::tests::*` 10 passed(含 4 个 decision role 加权测试)
- `decision_log::tests::*` 46 passed(含启发式提取/NOTEBOOK 持久化/截断/多字节 UTF-8 安全)
- `multi_agent::tests::*` 全部通过(无回归)
- `diag::tests::*` 4 passed(P0 诊断层无回归)

### P1 待办项（v2/v3 阶段落地）

- [x] **v2 Phase 1 Epic 1:Architectural SOP 注入**(2026-07-27)— `build_subagent_system_prompt` 新增 Architectural 复杂度的架构决策 SOP(六条规则:候选方案/trade-off/rationale/向后兼容/NOTEBOOK 持久化/禁止凭直觉拍板)
- [x] **v2 Phase 1 Epic 2:多 ValidationGate 注册**(2026-07-27)— `validation.rs` 新增 `npm_build_gate`/`pytest_gate` helper,`app.rs` 用 `command_exists` 探测 PATH 后注册 rust/npm/pytest gate,`file_filter` 正则隔离互不干扰
- [x] **v2 Phase 1 Epic 3:spawn_parallel 真并行路径文档化**(2026-07-27)— `spawn_parallel` 文档指向 DAG 模块(`DagScheduler`+`CoordinatorExecutor`+`SubagentDispatcher`)真并行路径,新增串行退化语义测试
- [x] **v2 Phase 2 Epic 4:checkpoint restore**(2026-07-27)— 实现 `restore_from_checkpoint`,读取 JSON → 反序列化 Subagent → Running 降级为 Created → 插入 registry。5 个测试(roundtrip/Running 降级/文件不存在/损坏 JSON/id 冲突)全通过
- [x] **v2 Phase 2 Epic 5:LlmJudgeGate 实现**(2026-07-27)— 引入 `JudgeClient` trait 依赖倒置(runtime 不依赖 api crate),实现 `parse_score`(正则提取 0.0-1.0 浮点数/整数回退)+ `build_judge_prompt` + `validate`(client 调用/分数解析/阈值比较)。无 client 时降级为 stub。9 个新测试全通过
- [x] **v2 Phase 2 Epic 6:决策持久化 LlmExtract**(2026-07-27)— 引入 `DecisionExtractorClient` trait + 全局 OnceLock 注册。实现 `extract_decisions_with_llm` + `build_llm_extract_prompt` + `parse_llm_decision_json`(剥离 markdown 代码块 + JSON 数组解析 + 字段缺失容错 + 截断)。三重降级策略(API 失败/JSON 解析失败/空数组 → Heuristic)。15 个新测试全通过
- [ ] 多 provider 升级链（Anthropic/OpenAI/xAI，v3 阶段）

### API 与文档差异说明

`multi-agent-hardening-plan.md` §10.4 验收标准中 `upgrade_model("deepseek-v4-flash", Diagnostic) == Some(UpgradeEntry{...})` 与实际 API `upgrade_model(model: &str) -> Option<String>` 不同（实际 API 不接 complexity 参数，返回 `Option<String>` 而非 `Option<UpgradeEntry>`）。这是设计文档草案与实现细节的正常差异，实际 API 已通过 `cargo test -p api model_tier` 验证。

## Migration Readiness

- [x] `PARITY.md` maintained and honest
- [ ] No `#[ignore]` tests hiding failures (允许 1 个：`live_stream_smoke_test`；实际有 3 个：1 个允许 + 2 个 lsp_client 测试 `rust/crates/runtime/src/lsp_client.rs` L3100/L3162，需真实 LSP server，非"隐藏失败"性质但超出允许数量)
- [ ] CI green on every commit (`.github/workflows/rust-ci.yml` 5 job 就绪：doc-source-of-truth/fmt/test-workspace/clippy-workspace/windows-smoke；运行时状态需核查)
- [ ] Codebase shape clean for handoff
