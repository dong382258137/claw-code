# Claw Code Harness Engineering 分阶段优化方案

| 项 | 值 |
|---|---|
| 文档版本 | v1.2 |
| 创建日期 | 2026-07-19 |
| 文档类型 | 设计方案 / 实施指南 |
| 适用项目 | claw-code-src (Rust 实现的 Claude Code 克隆) |
| 当前模型 | DeepSeek V4 PRO(上下文缓存命中率 95%) |
| 参考学术 | CMU/Yale/JHU/NEU/Tulane/UAB/OSU/Virginia Tech/Amazon《Agent Harness Engineering: A Survey》 |

---

## 一、背景与目标

### 1.1 Harness Engineering 定义

**核心公式**:`Agent 能力 = Model × Harness`(倍乘关系,非相加)

Harness 是包裹在模型之外的**所有管控、调度、校验、反馈基础设施**,用来"驯服"原始大模型,让 AI 自主工作稳定、可控、可验证。模型决定能力天花板,Harness 决定能力地板。

**LangChain 实证**:同模型不动,只改 harness(系统提示词 + 中间件 + 上下文注入),TerminalBench 2.0 从 52.8% → 66.5%(+13.7 分),Top 30 外冲进 Top 5。

### 1.2 本文档目标

将本项目从"单 agent harness 成熟"升级到"Plan/Verify/Recover 闭环 + 上下文工程编排 + 语义记忆 + Multi-Agent 协作"的工业级 harness,在**不破坏现有 95% 缓存命中率基线**的前提下完成。

### 1.3 核心约束

| 约束 | 说明 |
|---|---|
| 缓存保护 | DeepSeek V4 PRO 当前 95% 命中率,优化后预期 88-92% |
| 最小修改 | 优先改 1 个文件,避免新建多组件 |
| 复用现有桥 | `recovery_recipes`/`worker_boot` 已有桥,只需接线 |
| 可独立验证 | 每步骤 `cargo test` + `cargo clippy` 验收 |
| 向后兼容 | 不破坏 `run_turn` API 和 PARITY.md |

---

## 二、学术基础

### 2.1 ETCLOVG 七层架构

来自 CMU 等机构联合综述,Agent Harness 分为 7 层:

| 层 | 名称 | 职责 |
|:-:|---|---|
| E | 执行环境 | Sandbox、容器、Job Object |
| T | 工具接口 | Tool Schema、MCP、注册/校验 |
| C | 上下文管理 | 压缩、注入、检索 |
| L | 生命周期编排 | Plan/Execute/Recover 循环 |
| O | 可观测性 | Trace、Metrics、Telemetry |
| V | 验证 | Verifier、Critic、回归测试 |
| G | 治理 | Trust、Permission、Guardrail |

### 2.2 Harness 12 大核心模块

1. 编排循环(Orchestration Loop)
2. 工具(Tools)
3. 记忆(Memory)
4. 上下文管理(Context Management)
5. 提示词组装(Prompt Assembly)
6. 结构化输出(Structured Output)
7. 状态与检查点(State & Checkpointing)
8. 错误处理(Error Handling)
9. 护栏(Guardrails)
10. 验证与反馈(Verification & Feedback)
11. 子 Agent 编排(Subagent Orchestration)
12. 初始化与环境搭建(Initialization & Environment Setup)

### 2.3 关键参考论文/资料

| 资料 | 来源 |
|---|---|
| Agent Harness Engineering: A Survey | https://openreview.net/pdf/f358711a95aaaf61fdeffd4ef3fc60fba9b8da57.pdf |
| awesome-agent-harness | https://github.com/Picrew/awesome-agent-harness |
| LangChain: Improving Deep Agents | https://blog.langchain.com/ |
| LangChain: Your Harness, Your Memory | https://blog.langchain.com/your-harness-your-memory/ |
| SEAGym(Self-Evolving Agent 评测) | https://arxiv.org/abs/2606.17546 |
| Claude Code 源码泄露分析 | https://www.theregister.com/2026/03/31/anthropic_claude_code_source_code/ |
| Self-Harness 论文(上海 AI Lab) | 三阶段循环:Weakness Mining → Proposal → Validation |

---

## 三、现状分析

### 3.1 已实现模块(生产级)

| 模块 | 文件路径 | 核心功能 |
|---|---|---|
| Agent Loop 主循环 | `rust/crates/runtime/src/conversation.rs` (99K) | `ConversationRuntime::run_turn` 完整 LLM↔Tool 循环、microcompact 三段恢复 |
| 上下文压缩 | `rust/crates/runtime/src/compact.rs` (65K) + `summary_compression.rs` | auto/manual/reactive 三触发器 |
| 会话管理 | `rust/crates/runtime/src/session.rs` + `session_control.rs` | JSONL 持久化、rotate 256KB/3 文件 |
| 权限策略 | `rust/crates/runtime/src/permissions.rs` + `permission_enforcer.rs` | 5 级 PermissionMode |
| MCP 全栈 | `rust/crates/runtime/src/mcp*.rs` | 11 阶段 lifecycle、6 种传输 |
| 策略引擎 | `rust/crates/runtime/src/policy_engine.rs` | 13 种 PolicyCondition |
| 任务管理 | `rust/crates/runtime/src/task_registry.rs` + `task_packet.rs` | 6 态 TaskStatus |
| 后台任务 | `rust/crates/runtime/src/bg.rs` + `lane_events.rs` | 19 种 LaneEvent 完整事件总线 |
| Approval Tokens | `rust/crates/runtime/src/approval_tokens.rs` | 5 态状态机 |
| Hooks | `rust/crates/runtime/src/hooks.rs` | PreToolUse/PostToolUse 三事件 |
| 插件生命周期 | `rust/crates/runtime/src/plugin_lifecycle.rs` | 8 态 PluginState |
| 恢复食谱 | `rust/crates/runtime/src/recovery_recipes.rs` | 7 种 FailureScenario(有 recipe 无 orchestrator) |
| Worker Boot | `rust/crates/runtime/src/worker_boot.rs` | 7 态 WorkerStatus、14 字段证据 |
| Memory 架构 | `rust/crates/runtime/src/memory.rs` + `memory_store.rs` | Letta 风格 3 block、rule-based |
| Prompt Cache 监控 | `rust/crates/api/src/prompt_cache.rs` | 4 维指纹、cache_break 检测 |

### 3.2 薄弱模块(部分实现)

| 模块 | 缺失点 |
|---|---|
| Bootstrap 引导 | `bootstrap.rs` 仅 3K,只有 enum 无 phase runner |
| Worker Boot 健康探针 | L138 `format!("{name}_{status}_placeholder")`,transport/MCP 探针标注 "future" |
| LSP Client | L285 注释 "Return structured placeholder",未真正调用 LSP |
| Sandbox | 仅 Linux 实现,Windows/macOS 缺失 |
| Ultraplan | 仅 progress reporter,非真正的 Plan/Execute/Review |
| Team/Cron Registry | 自注释 "to replace stub",in-memory only |

### 3.3 缺失模块

| 模块 | 应具备能力 |
|---|---|
| **Trust Resolver(生产)** | `trust_resolver.rs` 29K 完整实现被 `#[cfg(test)]` 锁死 |
| **Recovery Orchestrator** | `recovery_recipes` 有 recipe 无调用方 |
| **Plan/Execute/Review** | 无独立 Planner、Reviewer、plan artifact 持久化 |
| **Verifier Agent** | 无 critic/verifier 子 agent |
| **ContextAssembler** | 各模块零散注入 system prompt,无统一编排 |
| **Memory 语义检索** | 仅 rule-based,无 embedding/HNSW |
| **Multi-Agent Coordinator** | task_registry 是数据结构,无消息总线 |
| **Telemetry 导出** | 仅 `SessionTracer::record`,无 OTLP/CSV 导出 |

### 3.4 结构性缺陷(三处断裂)

1. **G(治理)断裂**:`trust_resolver.rs` 被 `#[cfg(test)]` 锁死,生产构建完全缺失信任层
2. **L(生命周期)断裂**:`recovery_recipes` 有 recipe 无 orchestrator,失败时无人调用
3. **V(验证)缺失**:无 verifier/critic 闭环

### 3.5 整体评价

**广度优先、深度不均**。核心 Agent Loop 质量高,MCP 11 阶段生命周期和 Lane 19 事件总线是亮点。但存在三处结构性断裂,且缺失 Plan/Verify/Memory 语义/Multi-Agent 四大块。**当前处于人工设计成熟期,尚未进入 Meta-Harness 阶段**。

---

## 四、分阶段优化方案

### 设计原则

1. **源头修复**:每阶段先解决结构性缺陷,再做增量增强
2. **最小修改**:优先改 1 个文件解决
3. **可独立验证**:每步骤 `cargo test` + `cargo clippy` 验收
4. **复用现有桥**:`recovery_recipes`/`worker_boot` 桥已存在,只需接线
5. **保持向后兼容**:不破坏 `run_turn` API

---

### 阶段 1:P0 结构性缺陷修复(2-3 天)

#### Step 1.1 — 解锁 trust_resolver 生产构建 ✅

| 项 | 内容 |
|---|---|
| **目标** | 让 G(治理)层在生产构建可用 |
| **改动文件** | `rust/crates/runtime/src/lib.rs` L57-58、`rust/crates/runtime/src/worker_boot.rs` |
| **实现要点** | 1. 移除 `#[cfg(test)]` 前缀,改为 `pub mod trust_resolver;` <br> 2. `pub use` 导出 `TrustPolicy`/`TrustEvent`/`TrustResolution`/`TrustAllowlistEntry` <br> 3. `worker_boot.rs` 的 `WorkerFailureKind::TrustGate` 分支接入 `TrustEvent::TrustRequired` |
| **验证** | `cargo build --workspace --release` 通过;新增 `trust_resolver_production_smoke` 测试 |
| **风险** | 低 — 内部类型已完整,只是 cfg gate 解锁 |
| **缓存影响** | 无 — 不进入 prompt 构造路径 |

#### Step 1.2 — 补齐 recovery_recipes Orchestrator ✅

| 项 | 内容 |
|---|---|
| **目标** | L(生命周期)层失败分支自动恢复 |
| **改动文件** | 新增 `rust/crates/runtime/src/recovery_orchestrator.rs`;改 `conversation.rs` L425、`lib.rs` |
| **实现要点** | 1. `RecoveryOrchestrator` 结构,持有 `RecoveryRecipe` 查询表 <br> 2. `attempt_recovery(failure_kind: WorkerFailureKind) -> RecoveryOutcome` 入口 <br> 3. `conversation.rs` L425 `record_turn_failed` 之前注入 orchestrator 查询 <br> 4. 最多 1 次自动恢复,失败后 `EscalationPolicy::AlertHuman` <br> 5. `RecoveryStep` 执行器:RebaseBranch 接 `stale_branch.rs`,RestartPlugin 接 `plugin_lifecycle.rs` |
| **验证** | 5 个单测覆盖 7 种 FailureScenario;模拟 `PromptMisdelivery` 端到端 |
| **依赖** | Step 1.1 |
| **风险** | 中 — `max_attempts=1` 硬上限防止无限循环 |
| **缓存影响** | 低 — 仅在 `run_turn` 失败时触发 |

#### Step 1.3 — 修复 Worker Boot 健康探针 ✅

| 项 | 内容 |
|---|---|
| **目标** | O(可观测性)层探针落地 |
| **改动文件** | `rust/crates/runtime/src/worker_boot.rs` L126-178 |
| **实现要点** | 1. 移除 `format!("{name}_{status}_placeholder")`,改为 `Option<StartupHealthSummary>` 或 `HealthState::Unknown/Observed(bool)` <br> 2. `transport_health` 真实探针:TCP `connect()` + 100ms 超时 <br> 3. `mcp_health` 真实探针:复用 `mcp_lifecycle_hardened.rs` 11 阶段 validator,查询 final phase 是否 `Running` |
| **验证** | 启动 mock-anthropic-service 后 `worker_boot_health_probe` 集成测试 |
| **风险** | 低 — L750 起的 `StartupHealthSummary::observed()` 调用模式已存在 |
| **缓存影响** | 无 — 只改 worker_boot 内部状态 |

---

### 阶段 2:P1 核心架构升级(1-2 周)

#### Step 2.1 — Plan/Execute/Review 三段循环 ✅ 完成

> **真实状态（2026-07-21 v1.2 校正）**：`/ultraplan` CLI 命令已对接 runtime planner
> (commit 083f4a9)。`run_ultraplan` 调用 `runtime.set_plan_mode_enabled(true)` +
> `set_workspace_root(cwd)`,复杂任务(>200 字符或匹配关键词)触发 PlanArtifact 创建,
> 末尾追加到 system_prompt 变动区。`SlashCommand::Ultraplan` 分支返回 `true` 以
> persist plan mode 状态变更。`planner/` 子模块(PlanArtifact 持久化 +
> `assess_complexity` + `PreCompletionChecklistMiddleware`)已落地。

| 项 | 内容 |
|---|---|
| **目标** | 从单段 ReAct 升级为 Plan→Build→Verify→Fix 闭环 |
| **改动文件** | 新增 `rust/crates/runtime/src/planner/` 目录(mod.rs, artifact.rs, reviewer.rs);改 `conversation.rs` `run_turn`;改 `rusty-claude-cli/src/ultraplan.rs` |
| **实现要点** | 1. `PlanArtifact`:`Vec<PlanStep>`,每步含 `acceptance_criteria`、`verification_method`、`status` <br> 2. 复杂任务检测(用户输入 > 200 字符或多文件预期),触发 planner 子调用 <br> 3. `PreCompletionChecklistMiddleware` 在 Agent 退出前拦截,强制跑 verification <br> 4. Replan 触发:plan step 失败 → 重新规划剩余步骤 <br> 5. Plan artifact 持久化到 `<workspace>/.claw/plans/<timestamp>.json` <br> 6. **缓存保护**:PlanArtifact 必须**末尾追加**(详见 §5.2) |
| **验证** | SWE-bench 风格端到端测试;plan artifact schema 校验 |
| **依赖** | 阶段 1 全部完成 |
| **风险** | 高 — 主循环改动大,需 feature flag `--enable-plan-mode` 默认关闭 |
| **缓存影响** | **严重** — 必须采用末尾追加策略,预期从 95% 降至 88-92% |

#### Step 2.2 — LoopDetectionMiddleware ✅

| 项 | 内容 |
|---|---|
| **目标** | 打断 Doom Loop(同一文件 10+ 次编辑) |
| **改动文件** | 改 `hooks.rs` PostToolUse 事件;新增 `rust/crates/runtime/src/loop_detection.rs` |
| **实现要点** | 1. `LoopDetector`:`HashMap<FilePath, EditCount>`,每轮 tool call 后更新 <br> 2. 阈值:同文件 5 次编辑触发 `inject_context("consider reconsidering your approach")` <br> 3. 10 次触发 `abort_with_reason("doom loop detected")`,走 Step 1.2 RecoveryOrchestrator <br> 4. **经验法则**:MCP Tools ≤ 80,Skills ≤ 15,注册时校验 |
| **验证** | 构造 fixture 会话强制 10 次同文件编辑,断言 abort 信号 |
| **依赖** | Step 1.2 |
| **风险** | 低 — 增量中间件 |
| **缓存影响** | 低 — 仅异常路径 |

#### Step 2.3 — ContextAssembler 编排器 ✅

| 项 | 内容 |
|---|---|
| **目标** | 统一管理 system prompt 注入,实现 C(上下文管理)层 |
| **改动文件** | 新增 `rust/crates/runtime/src/context_assembler.rs`;改 `prompt.rs`、`conversation.rs` |
| **实现要点** | 1. `ContextAssembler`:持有 memory/goal/history_search/repomap/git_context 注入优先级 <br> 2. `assemble(system_prompt, context_sources) -> AssembledPrompt` 统一入口 <br> 3. 优先级栈:system > tools > memory > goal > git_context > history > user <br> 4. Token 预算:每源上限,超出触发该源 microcompact <br> 5. 与 2026-07-18 完成的 11 项内存架构优化协同 <br> 6. **缓存保护**:固定优先级栈,运行时不可变(详见 §5.1) |
| **验证** | Token 预算单元测试;端到端验证注入顺序符合优先级 |
| **依赖** | 无(可与 Step 2.1/2.2 并行) |
| **风险** | 中 — 先 dry-run mode 对比新旧 prompt diff |
| **缓存影响** | **正向** — 固定注入顺序后,半稳定区更稳定,可能 +1-2% |

#### Step 2.4 — Memory 语义检索层 ✅ 完成

> **真实状态（2026-07-21 v1.2 校正）**：`EmbeddingProvider` trait +
> `HashEmbeddingProvider`(默认,无外部依赖)+ `FastembedProvider`(BGE-small-en-v1.5,
> 启用 `embedding` feature,基于 ONNX Runtime)已落地(commit 876f577)。
> `cosine_similarity` 向量余弦相似度已实现。`SemanticRecaller::with_embedding()`
> 支持注入 provider,无 provider 时自动退化到 keyword 搜索。L1 常驻 / L2 按需 /
> L3 仅搜索三级层级已落地。fastembed-rs 5.17.3 通过 `Arc<Mutex>` 保护 `&mut self`。
> HNSW 向量索引未集成(用线性扫描 + cosine similarity 替代,384 维下足够),
> 留到阶段 4 性能优化时再评估。

| 项 | 内容 |
|---|---|
| **目标** | 从 rule-based 升级到 embedding-based 语义检索 |
| **改动文件** | 改 `memory.rs`、`memory_store.rs`;新增 `rust/crates/runtime/src/memory/semantic.rs` |
| **实现要点** | 1. 集成 HNSW 向量索引 <br> 2. 三级层级(参考 Claude Code 源码泄露): <br> &nbsp;&nbsp; L1 索引:150 字符/条,常驻内存 <br> &nbsp;&nbsp; L2 主题文件:按需加载 <br> &nbsp;&nbsp; L3 原始记录:仅搜索访问 <br> 3. `semantic_recall(query: &str, k: usize) -> Vec<MemoryHit>` 入口 <br> 4. 嵌入模型:先支持 OpenAI text-embedding-3-small,后扩展本地模型 <br> 5. 与 Step 2.3 ContextAssembler 集成 <br> 6. **缓存保护**:L1 在半稳定区,L2/L3 通过 tool 获取(详见 §5.2) |
| **验证** | 记忆写入后语义召回测试;latency < 100ms |
| **依赖** | Step 2.3 |
| **风险** | 中 — 嵌入模型需 API key,设计 fallback 到 rule-based |
| **缓存影响** | **高** — 必须末尾注入,阈值控制 0.85 |

---

### 阶段 3:P2 验证反馈与 Multi-Agent(2-3 周)

#### Step 3.1 — Verifier Agent ⚠️ 部分

> **真实状态（2026-07-21 v1.2 校正）**：`verifier/` 目录与 `VerifierAgent` 骨架已落地，
> 规则反馈（cargo test / clippy / fmt）已接入，但**视觉反馈与模型当裁判为 placeholder**：
> - `verifier/visual.rs`：Playwright 截图对比未实现，函数返回占位结果
> - `verifier/model_judge.rs`：子 agent 调用 LLM 评估 tool result 未实现，返回占位结果
> `VerifierAgent::verify` 当前只走规则路径，且未注入主循环 `run_turn`（latent defect）。
> **v1.2 未对本步骤做新工作** — 规则反馈路径已满足 Step 3.2 Multi-Agent 的基础验证需求,
> 视觉/模型裁判留到阶段 4 或独立 PR。

| 项 | 内容 |
|---|---|
| **目标** | 实现 V(验证)层,产出质量 ×2-3 |
| **改动文件** | 新增 `rust/crates/runtime/src/verifier/` 目录;改 `task_registry.rs` |
| **实现要点** | 1. 三种验证反馈: <br> &nbsp;&nbsp; 规则反馈:`cargo test` / `cargo clippy` / `scripts/fmt.sh --check` <br> &nbsp;&nbsp; 视觉反馈:Playwright 截图对比(可选,先做规则) <br> &nbsp;&nbsp; 模型当裁判:子 agent 调用 LLM 评估 tool result <br> 2. `VerifierAgent::verify(tool_result) -> VerificationResult` <br> 3. 与 Step 2.1 PlanArtifact 的 `acceptance_criteria` 对接 <br> 4. 失败时触发 replan |
| **验证** | 模拟失败 tool result,断言 VerifierAgent 检测到 |
| **依赖** | Step 2.1 |
| **风险** | 低 — 子 agent 独立 |
| **缓存影响** | 无 — 子 agent 独立 LLM 请求,独立 prompt cache |

#### Step 3.2 — 子 Agent 编排与 Multi-Agent Coordinator ✅ 完成

> **真实状态（2026-07-21 v1.2 校正）**：三个子步骤全部落地:
> - **3.2-a** (commit 8322e88):`lane_events.rs` 新增 `SubagentHandoff` /
>   `SubagentResult` 事件 + 2 个 helper 构造器,消息总线扩展完成。
> - **3.2-b** (commit a46a3b5):`team_cron_registry.rs` 实现 `TeamRegistry` /
>   `CronRegistry` JSON 持久化,原子写入(`<path>.tmp` + `rename`)避免崩溃破坏。
> - **3.2-c** (commit 36d9721):`conversation.rs` 注入 `MultiAgentCoordinator`,
>   subagent-as-tool 路由 — `dispatch_subagent` / `check_subagent` 两个 tool spec,
>   主 agent 通过 tool call 调用子 agent。子 agent 走独立 LLM 请求 + 独立 prompt cache,
>   不污染主 agent 缓存。13 个测试覆盖 dispatch + check 全流程。
> `multi_agent/` 目录的 3 种 `CoordinationMode`(Fork / Teammate / Worktree)与
> `Subagent` 生命周期管理已落地。

| 项 | 内容 |
|---|---|
| **目标** | 从单 agent 升级到 agent 集群 |
| **改动文件** | 改 `task_registry.rs`、`team_cron_registry.rs`;新增 `rust/crates/runtime/src/multi_agent/` 目录 |
| **实现要点** | 1. 参考 Claude Code 三模式:Fork(同步)/ Teammate(异步)/ Worktree(独立 git worktree) <br> 2. Agent-to-agent tool call 路由:子 agent 作为 tool 暴露给主 agent <br> 3. 消息总线:复用 `lane_events.rs` 19 事件,扩展 `SubagentHandoff`/`SubagentResult` <br> 4. `team_cron_registry.rs` 补齐持久化和真实 cron 调度 |
| **验证** | 双 agent 协作场景测试 |
| **依赖** | Step 3.1 |
| **风险** | 中 — 主 agent 缓存可能在 handoff 点失效 |
| **缓存影响** | 中 — 只在 handoff 点触发,正常流程不变 |

#### Step 3.3 — Telemetry 导出与 Trace Analyzer 基础 ✅ 完成

> **真实状态（2026-07-21 v1.2 校正）**：CSV exporter + `TraceAnalyzer::load_csv` /
> `export_csv` / `stats` 基础统计已落地。K-means 失败聚类已实现(commit 23c7c72):
> `cluster_failures_kmeans(provider)` 方法基于 (failure_kind, error_message_embedding)
> 二次切分,`K = min(MAX_KMEANS_CLUSTERS_PER_KIND=3, 组内样本数)`,cosine similarity +
> 均值 update,确定性初始化(前 K 个点),`KMEANS_MAX_ITERATIONS=10` 轮。无 provider 时
> 退化为 `cluster_failures()`(按 failure_kind 简单分桶)。14 个新测试覆盖 K-means
> 聚类、embed 失败降级、K 上限封顶、确定性、边界条件等,全部 31 个 trace_analyzer
> 测试通过。OTLP exporter 仍未实现(标注为可选,留到阶段 4),CSV exporter 已足够
> 支撑阶段 4 Self-Improving Harness 入口。

| 项 | 内容 |
|---|---|
| **目标** | 为阶段 4 Self-Improving Harness 铺路 |
| **改动文件** | 改 `rust/crates/telemetry/src/lib.rs`;新增 `rust/crates/runtime/src/trace_analyzer.rs` |
| **实现要点** | 1. `SessionTracer::record` 扩展:OTLP exporter(可选) + CSV exporter(默认) <br> 2. Metrics histogram:turn latency / tool call count / compact 触发率 <br> 3. `TraceAnalyzer::load(csv_path) -> TraceStats` 基础统计 <br> 4. 失败聚类:K-means on (failure_kind, error_message_embedding) <br> 5. 不做完整 Self-Harness 闭环(留到阶段 4) |
| **验证** | 跑 100 turn 后导出 CSV,断言 trace 完整性 |
| **依赖** | Step 2.2 |
| **风险** | 低 |
| **缓存影响** | 无 — 纯观测 |

---

### 阶段 3.5:三层信息持久化架构(基于论文调研,2026-07-21)

> **背景**:2026-07-21 发现 AI 在 70+ tool calls 后重复 dispatch 子智能体分析"缠论线段定义",
> 导致任务 stall。根因:context 压缩导致 AI 忘记已 dispatch 的子智能体,叠加
> `MultiAgentCoordinator::start()` 只改状态不执行任务的"空壳"问题。经论文调研后
> 实施三层信息持久化架构,彻底修复长程任务中关键信息丢失问题。

#### 论文基础

- **Anthropic《Effective Context Engineering for AI Agents》(2025)** — 推荐
  "Structured Note-taking":agent 定期写笔记到 context window 外部,后续拉回注入
- **Anthropic《Multi-Agent Research System》(2025)** — 4 要素:
  "spawn fresh subagents with clean contexts" / "maintaining continuity through
  careful handoffs" / "Subagent output to a filesystem" / "pass lightweight
  references back to the coordinator"
- **CompactionRL (arXiv:2607.05378)** — summary 必须保留 5 字段:
  original goal / completed actions / unresolved errors / current state /
  plausible next steps;公式 (9): `h̄_t = s ⊕ u_resume(S_t) ⊕ (z_{t-k+1}, ..., z_t)`, k=2
- **MIRIX (arXiv:2507.07957)** — OS-inspired 分层内存,NOTEBOOK 作为"working context"

#### 架构:三层信息持久化

```text
Layer 1: Main Context (LLM 推理窗口)
         ↑↓ page in/out
Layer 2: NOTEBOOK.md (跨压缩持久化) — 5 段 XML 结构
         ↑↓ fetch/deref
Layer 3: External Storage (.claw/subagents/*, trace CSV, ...)
```

NOTEBOOK.md 5 段结构对应 CompactionRL 的 summary 必备字段:
- `<plan>` — original goal + current state
- `<subagents>` — dispatched subagents registry(防重复 dispatch)
- `<attempted>` — completed actions + unresolved errors
- `<preferences>` — user constraints
- `<key_files>` — plausible next steps 的文件引用

#### P0-1 — NOTEBOOK.md parse() 修复 + Structured Note-taking ✅

| 项 | 内容 |
|---|---|
| **目标** | 修复 parse() 误匹配 header 中 XML 字面引用 + 空段/单行 XML 处理失败;落地 Structured Note-taking 持久化工作记忆 |
| **改动文件** | 新增 `rust/crates/runtime/src/notebook.rs`;改 `rust/crates/runtime/src/lib.rs`、`rusty-claude-cli/src/plugin_state.rs` |
| **实现要点** | 1. `Notebook` 数据模型(`BTreeMap<String, String>`,5 段 XML 标签) <br> 2. parse() 逐字符扫描行首位置(open tag 严格要求行首,close tag 优先行首后退任意位置) <br> 3. 空段等价缺失段(与 set_section 语义一致,确保 round-trip) <br> 4. 原子写(`.tmp` + `rename`)+ `NOTEBOOK_MAX_CHARS=16_000` 上限 <br> 5. `execute_notebook_update` 工具暴露给 LLM,类似 TodoWrite 模式 <br> 6. 通过 system_prompt 变动区每个 turn 重新注入 |
| **验证** | 26 个测试全绿(parse/save/round-trip/execute_notebook_update) |
| **commit** | 59f1663 |
| **缓存影响** | 低 — NOTEBOOK 在 system_prompt 变动区(末尾追加) |

#### P0-3 — 压缩前 NOTEBOOK 刷新 trigger ✅

| 项 | 内容 |
|---|---|
| **目标** | 压缩发生时提醒 LLM 立即刷新 NOTEBOOK,防止关键信息丢失 |
| **改动文件** | 改 `rust/crates/runtime/src/conversation.rs` |
| **实现要点** | 1. `notebook_refresh_pending: bool` flag <br> 2. 三个压缩点设置 flag:每 turn microcompact(比较前后 `tool_result_output_len`)、`maybe_auto_compact`、Reactive compaction <br> 3. 下个 iteration 的 system_prompt 变动区注入刷新提醒 <br> 4. LLM 调用 `notebook_update` 后清除 flag(无论成功失败) <br> 5. `tool_result_output_len` 辅助函数统计 ToolResult block output 总长度 |
| **验证** | 3 个新测试全绿(flag 设置/清除/system_prompt 注入) |
| **commit** | 8ea0c67 |
| **缓存影响** | 无 — flag 只影响变动区 |

#### P0-2 — 子智能体真实化(同步阻塞 + 上下文隔离 + 文件持久化)✅

> **增强 Step 3.2**:修复 `MultiAgentCoordinator::start()` 只改状态不执行任务的"空壳"问题。
> 这是长程任务 stall 的直接根因 — 子 agent 永远停留在 Running,主 agent 无限轮询。

| 项 | 内容 |
|---|---|
| **目标** | 子智能体从"空壳"升级为真实执行(Anthropic Multi-Agent Research System 4 要素) |
| **改动文件** | 改 `rust/crates/runtime/src/conversation.rs` |
| **实现要点** | 1. `execute_dispatch_subagent` 改为 `&mut self`,spawn + start 后同步阻塞执行 <br> 2. `run_subagent_turn` 新方法:独立 system_prompt + task 作为 user message → `api_client.stream` → 解析 response → 原子写 `.claw/subagents/{id}.md` → 返回 result_ref 路径 <br> 3. 完全隔离:子智能体不共享主 agent 上下文,不污染主 session messages <br> 4. 根据结果调用 `coordinator.complete()` 或 `fail()` <br> 5. 发布终态 `SubagentResult` lane event <br> 6. 主 agent 只收到 result_ref 路径(轻量引用,非完整结果) |
| **验证** | 10 个测试全绿(7 原有 + 3 新增:无 workspace_root 优雅失败 / 上下文隔离不污染主 session / id 递增 + 文件持久化) |
| **commit** | c2e8f48 |
| **缓存影响** | 无 — 子智能体走独立 LLM 请求 + 独立 prompt cache |

#### P1 — microcompact 结构化保留(子智能体指针 + 多行预览)✅

| 项 | 内容 |
|---|---|
| **目标** | 修复旧版 `format_tool_result_summary` 只保留第一行导致关键信息丢失 |
| **改动文件** | 改 `rust/crates/runtime/src/compact.rs` |
| **实现要点** | 1. `dispatch_subagent` / `check_subagent` 加入 `CRITICAL_TOOLS`(保留完整 result_ref 指针) <br> 2. `format_tool_result_summary` 从"只保留第一行"改为"保留前 3 行 + 行数信息"(240 字符上限) <br> 3. `is_already_summarized` 检测逻辑保持兼容(避免重复摘要) <br> 4. 与 NOTEBOOK 协同:关键信息应已持久化到 NOTEBOOK.md,这里保留前 N 行作为"足够判断的指针" |
| **验证** | 37 个 compact 测试全绿(含 5 个新 P1 测试) |
| **commit** | a1ac0d1 |
| **缓存影响** | 无 — 只影响已压缩消息的摘要格式 |

#### P3 — streaming stall 事件间超时 ✅

| 项 | 内容 |
|---|---|
| **目标** | 修复 `consume_stream` 首事件后所有 `next_event` 调用无超时保护,导致网络静默挂起时无限等待 |
| **改动文件** | 改 `rust/crates/rusty-claude-cli/src/streaming.rs` |
| **实现要点** | 1. 新增 `INTER_EVENT_TIMEOUT = 60s` 常量 <br> 2. 统一 `tokio::time::timeout` 包装:post-tool 首事件 10s(严格),其他 60s(宽容,容纳 extended thinking) <br> 3. 区分两种 stall 错误消息:post-tool stall / inter-event stall <br> 4. 所有超时 `recoverable=true`,允许上层重试 <br> 5. 与 `http_client.rs` 故意不设 `.timeout()` 的设计协同(总请求超时会错误中止合法长流式) |
| **验证** | 337 个 rusty-claude-cli 测试全绿(0 failed) |
| **commit** | b7edada |
| **缓存影响** | 无 — 只影响网络 I/O 超时检测 |

#### 阶段 3.5 总结

| 维度 | 改进 |
|---|---|
| **长程任务稳定性** | P0-1 + P0-3 + P0-2 联合修复"AI 忘记关键信息导致重复 dispatch"stall 问题 |
| **上下文质量** | P1 保留子智能体指针 + 多行预览,LLM 可判断是否需要 re-read |
| **网络鲁棒性** | P3 事件间超时保护,避免无限等待 |
| **测试覆盖** | 新增 37 个测试(P0-1: 26 / P0-3: 3 / P0-2: 3 / P1: 5),总 918 passed / 12 pre-existing Windows failures |
| **论文对标** | Anthropic Structured Note-taking + Multi-Agent 4 要素 + CompactionRL summary 5 字段 + MIRIX 分层内存 |

---

### 阶段 4:P3 平台兼容性与前沿(1 周)

#### Step 4.1 — Sandbox Windows 实现 ⚠️ 部分

> **真实状态(2026-07-21 v1.4 复核)**:`SandboxBuilder` trait + `LinuxSandboxBuilder` /
> `WindowsSandboxBuilder` / `MacOsSandboxBuilder` 三实现 structurally 存在(代码可编译,
> 11 个单元测试覆盖 builder API),但**运行时无效**:
> - `bg.rs::spawn()`(L89-124)完全绕过 `SandboxBuilder`,用自己的 `apply_detached_flags`
>   (L316-323),常量 `0x0800_0208` 重复硬编码,未引用 sandbox.rs 常量
> - `WindowsSandboxBuilder::assign_process_to_job_object(pid)`(sandbox.rs:471-494)
>   是**死代码** — 从未被任何调用方调用,Job Object 在运行时实际不创建
> - PowerShell + C# 内联的 Job Object 脚本(sandbox.rs:497-581)虽存在但从未执行
> - `lib.rs` 公共导出(L239-244)不含 `SandboxBuilder` trait 与新类型,外部 crate 无法使用
> - 无功能性测试 — Job Object 实际强制限制未验证、bg.rs spawn 走 SandboxBuilder 路径未验证
> - sandbox.rs:392 注释过时(声称"Job Object 限制待集成 Win32 API",实际已有 PowerShell 实现)
> - macOS 实现文档自述为 placeholder(sandbox.rs:315, 648)
>
> **要达到 ✅ 完整,至少需要**:
> 1. `bg.rs::spawn` 改用 `platform_sandbox_builder().build(...)`,删除 `apply_detached_flags` 硬编码
> 2. spawn 后取得 PID,调用 `WindowsSandboxBuilder::assign_process_to_job_object(pid)`
> 3. `lib.rs` 导出 `SandboxBuilder`/`SandboxCommand`/`WindowsSandboxBuilder`/`MacOsSandboxBuilder`/`platform_sandbox_builder`
> 4. 添加 Windows 集成测试,验证 Job Object 实际强制限制
> 5. 修正 sandbox.rs:392 过时注释

| 项 | 内容 |
|---|---|
| **目标** | 让 E(执行环境)层在 Windows 可用(本项目实际运行平台) |
| **改动文件** | 改 `rust/crates/runtime/src/sandbox.rs`、`rust/crates/runtime/src/bg.rs`、`rust/crates/runtime/src/lib.rs` |
| **实现要点** | 1. Windows:`CREATE_NO_WINDOW` + Job Object 限制 CPU/memory <br> 2. macOS:`sandbox-exec` wrapper(可选,优先级低) <br> 3. 抽象 `SandboxBuilder` trait,Linux/Windows/macOS 三实现 <br> 4. 与 `bg.rs` 已有的 `CREATE_NO_WINDOW` flag 整合 |
| **验证** | Windows 上跑 `cargo test sandbox` |
| **缓存影响** | 无 |
| **实际状态** | ⚠️ 部分 — trait + 三实现存在(可编译),但 bg.rs 未整合、Job Object 是死代码、公共 API 未导出、无功能性测试 |

#### Step 4.2 — LSP Client 真实接入 ⚠️ 部分

> **真实状态(2026-07-21 v1.4 复核)**:协议层和传输层都已真实编码,但**生产 dispatch 路径
> 仍走 `MemoryLspTransport` placeholder**,`ProcessLspTransport::spawn()` 从未被生产代码调用,
> rust-analyzer 也从未被真实启动。
>
> **已实现部分**:
> - `LspRequest` 协议层(method/params/file_uri 构造)— 真实可用
> - `LspJsonRpcClient::initialize()` / `did_change()` 协议构造 — 真实可用
> - `ProcessLspTransport` 完整传输层实现(spawn / write_message / read_message / Drop 清理)— 真实可用但**未被生产使用**(死代码)
> - 40 个单元测试覆盖协议构造逻辑
>
> **缺失部分**:
> - ❌ **未引入 `lsp-types` / `tower-lsp` 官方 crate**,全部手搓 serde_json 易出错
> - ❌ **`LspRegistry::dispatch()`(L292)默认走 `MemoryLspTransport` placeholder** — 返回
>   固定响应 `"status": "protocol_constructed"`,不调用 `ProcessLspTransport::spawn()`
> - ❌ **生产代码无任何 `ProcessLspTransport::new()` / `with_transport()` 调用** — 该传输层
>   是死代码,仅被 3 个测试引用(且测试都没调用 `.spawn()`)
> - ❌ **`repomap.rs` 与 LSP 完全无关联** — lsp_client.rs:735 docstring 声称"协同"纯属愿景,
>   repomap.rs 中 0 个 LSP/symbol 引用
> - ❌ **无 `LspSymbol` 解析逻辑** — 从 `textDocument/documentSymbol` 响应到 `LspSymbol`
>   的转换未实现
> - ❌ **无 rust-analyzer 真实启动测试** — 没有 `#[ignore]` 集成测试或端到端测试
> - ❌ **`LspRegistry` 未提供 `spawn_server()` 之类的方法** — 注册的 server 状态是手动
>   `register("rust", LspServerStatus::Connected, ...)` 写入的,没有真实启动逻辑
>
> **要达到 ✅ 完整,至少需要**:
> 1. 在 `LspRegistry::dispatch()` 中改为使用 `ProcessLspTransport`(通过 `LspJsonRpcClient::with_transport`),
>    并先调用 `spawn()` 启动真实 LSP server
> 2. 提供 `LspRegistry::spawn_server(language, command, root_path)` 之类的 API
> 3. 实现 `textDocument/documentSymbol` 响应到 `LspSymbol` 的解析
> 4. 在 `repomap.rs` 中调用 `LspRegistry` 获取 symbol 信息(实现"协同")
> 5. 添加 `#[ignore]` 集成测试,真实启动 rust-analyzer 验证 initialize → didChange → hover/completion 全流程
> 6. 评估是否引入 `lsp-types` crate 替代手搓 serde_json

| 项 | 内容 |
|---|---|
| **目标** | 替换 `lsp_client.rs` L292 的 placeholder(LspJsonRpcClient::new 默认 MemoryLspTransport) |
| **改动文件** | 改 `rust/crates/runtime/src/lsp_client.rs`、`rust/crates/runtime/src/repomap.rs`、`rust/crates/runtime/Cargo.toml` |
| **实现要点** | 1. 集成 `tower-lsp` 或 `lsp-types` crate <br> 2. `dispatch` 方法真实调用 LSP JSON-RPC:initialize → didChange → completion/hover <br> 3. 与 `repomap.rs` 协同,LSP 提供 symbol 信息 |
| **验证** | 启动 rust-analyzer,断言 completion 返回非空 |
| **依赖** | 无 |
| **缓存影响** | 低 — LSP symbol 注入 repomap,但 repomap 已在变动区 |
| **实际状态** | ⚠️ 部分 — 协议层 + 传输层已编码,但生产 dispatch 走 placeholder、ProcessLspTransport 是死代码、repomap 协同未实现、无 LSP crate 依赖、无 rust-analyzer 集成测试 |

---

## 五、缓存命中率保护方案

### 5.1 背景

- DeepSeek V4 PRO 当前命中率:**95%**(超过 Claude Code 官方 92%)
- DeepSeek 缓存机制:**全自动、磁盘级、前缀字节级匹配**,从 prompt 开头到第一个不一致字节为止都命中
- DeepSeek V4 把 KV 缓存砍到 V3 的 10% → 命中收益更大但维护成本更高
- **关键结论**:任何在 prompt 中前部插入/修改,都会从该字节起全部断缓存

### 5.2 Prompt 四层物理隔离模型

把 prompt 划分为 4 层,每层有自己的缓存特性:

```
┌──────────────────────────────────────┐
│ 绝对稳定区 (~5K tokens)               │ ← 95%+ 命中,核心保护区
│ - system_prompt                      │
│ - tools_schema                       │
├──────────────────────────────────────┤
│ 半稳定区 (~2K tokens)                │ ← 80%+ 命中,跨轮稳定
│ - memory L1 索引(150字符×N)         │
│ - goal 当前状态                      │
│ - git_context                        │
├──────────────────────────────────────┤
│ 变动区 (~1-3K tokens)               │ ← 30-50% 命中,每轮可能变
│ - PlanArtifact(若触发 plan)         │
│ - 语义召回结果(若触发 recall)       │
│ - history_search 结果                │
├──────────────────────────────────────┤
│ 天然变动区 (动态)                    │ ← 0% 命中,每轮追加
│ - conversation_history               │
│ - 最新 user input                    │
└──────────────────────────────────────┘
```

DeepSeek 前缀缓存命中到第一个不一致字节。**只要绝对稳定区和半稳定区不变,前面那部分继续命中(占 prompt 70%+)**。变动区失效是预期内的(本来每轮都变),不影响前面缓存。

### 5.3 四大缓解原则

#### 原则 1:稳定区与变动区物理隔离

按 §5.2 四层模型组织 prompt,严禁跨层插入。

#### 原则 2:追加优于替换

每次注入新内容时:
- ✅ 正确:`prompt.append(new_content)` → 只扩展长度,前面不变
- ❌ 错误:`prompt.replace(old_block, new_block)` → 改变后续字节位置

#### 原则 3:强化本项目已有监控

`prompt_cache.rs` 已有 `unexpected_cache_breaks` 监控,建议:

1. **降低 `cache_break_min_drop` 阈值**:从 2000 → 500(优化阶段更敏感)
2. **新增"命中率指标"**:在 `PromptCacheStats` 增加 `cache_hit_rate: f32` 字段,< 0.85 时告警
3. **每步优化后跑 baseline 对比**:记录优化前后的 `cache_read_input_tokens` 均值
4. **复用 `PromptCacheEvent` 通道**:在 `conversation.rs` L174 已接入,扩展事件类型

#### 原则 4:子 agent 独立缓存

Verifier/Multi-Agent 的子 agent 走独立 LLM 请求,各自维护独立 prompt cache,**不污染主 agent 的缓存**。这是学术综述推荐的 "Subagent as Tool" 模式。

### 5.4 预期影响量化

| 阶段 | 完成后命中率 | 较 95% 基线 | 说明 |
|---|---|---|---|
| 阶段 1 完成 | 95% | 0% | 完全无影响 |
| Step 2.1(末尾追加策略) | 88-92% | -3 ~ -7% | PlanArtifact 在变动区,缓存断点前移约 1-3K tokens |
| Step 2.2 | 88-92% | 0% | 异常路径,正常流程不变 |
| Step 2.3(固定优先级栈) | 90-93% | +1-2% | ContextAssembler 统一注入顺序,半稳定区更稳定 |
| Step 2.4(语义召回末尾注入) | 88-92% | 0% | 召回结果在变动区,不影响前面 |
| 阶段 3 完成 | 88-92% | 0% | 子 agent 独立缓存 |
| 阶段 4 完成 | 88-92% | 0% | 平台层无影响 |

**最终预期:88-92%**(从 95% 下降 3-7 个百分点)

**关键权衡**:用 3-7% 缓存命中率换取 Plan/Verify/Recover + 语义记忆 + Multi-Agent,对应 LangChain TerminalBench +13.7 分,ROI 极高。

### 5.5 可选保守方案

如果对 95% 命中率有硬约束(成本 SLA),可采用保守方案:

| 阶段 | 保守方案 |
|---|---|
| 阶段 1 | ✅ 全做(零影响) |
| Step 2.1 | ⚠️ 只做 Side channel 模式 — PlanArtifact 通过 tool fetch,不进主 prompt |
| Step 2.2 | ✅ 做(异常路径) |
| Step 2.3 | ✅ 做(实际上提升命中率) |
| Step 2.4 | ⚠️ 只做 L1 索引(稳定),L2/L3 通过 tool 获取 |
| 阶段 3 | ✅ 全做(子 agent 独立缓存) |
| 阶段 4 | ✅ 全做 |

保守方案预期命中率:**93-95%**(几乎不降),但放弃 PlanArtifact 主 prompt 注入,Plan/Verify 能力弱化。

### 5.6 推荐决策

**建议采用标准方案**(预期 88-92%),理由:
1. 95% → 90% 的 5 个百分点,对应成本增加约 10-15%(DeepSeek 缓存读取价格通常是 input 的 10-14%)
2. 换来 Plan/Verify/Recover + 语义记忆 + Multi-Agent,综合能力提升显著大于成本增加
3. `prompt_cache.rs` 已有完善监控,任何优化导致的命中率下降都能立即发现并回滚

---

## 六、执行节奏与风险控制

### 6.1 执行节奏

参考本项目 2026-07-18 内存架构优化经验(5 并行 subagent + 4 项集成 gap + 重编译验证):

| 阶段 | 推荐执行模式 | 验收门 |
|---|---|---|
| 阶段 1(P0) | **串行**(Step 1.1→1.2→1.3,依赖链清晰) | `cargo build --release` + `cargo test --workspace` 全绿 |
| 阶段 2(P1) | **2.1 串行 + 2.2/2.3/2.4 并行 subagent** | mock_parity_harness 通过;PlanArtifact schema 稳定 |
| 阶段 3(P2) | **3.1 串行先,3.2 依赖 3.1,3.3 可并行** | 双 agent 协作 e2e;trace CSV 导出 |
| 阶段 4(P3) | **4.1/4.2 并行**(无依赖) | Windows sandbox test + LSP 集成 test |

### 6.2 风险与回滚

- **每步独立 commit**:出错可单步 revert
- **每阶段结束后重编译 `claw.exe`**(参考 topics.md 2026-07-18 0:42 重编译流程)
- **阶段 2.1 是最大风险点**:主循环改动大,建议先做 feature flag `--enable-plan-mode` 默认关闭
- **不破坏 PARITY.md**:每个改动后跑 `scripts/run_mock_parity_harness.sh` 确保与上游 Claude Code 行为对齐

### 6.3 缓存命中率监控纪律

- 每个优化步骤 PR 必须附带 `cache_read_input_tokens` 前后对比
- `unexpected_cache_breaks` 单调递增超过 3 次 → 阻断合并
- 命中率 < 85% → 触发告警,启动回滚
- `cache_break_min_drop` 阈值降为 500(优化阶段更敏感)

### 6.4 验收标准

| 阶段 | 验收点 |
|---|---|
| 阶段 1 | `cargo build --release` + `cargo test --workspace` 全绿;trust_resolver 在生产构建可用;RecoveryOrchestrator 端到端测试 |
| 阶段 2 | mock_parity_harness 通过;PlanArtifact schema 稳定;ContextAssembler 注入顺序测试;Memory 语义召回 latency < 100ms |
| 阶段 3 | 双 agent 协作 e2e;trace CSV 导出;VerifierAgent 检测失败 tool result |
| 阶段 4 | Windows 上 `cargo test sandbox`;rust-analyzer completion 非空 |

---

## 七、预期影响

### 7.1 能力提升

完成阶段 1-2 后,本项目将从"单 agent harness 成熟"升级到"**Plan/Verify/Recover 闭环 + 上下文工程编排 + 语义记忆**"的工业级 harness。对照 LangChain TerminalBench 2.0 数据(同模型只改 harness +13.7 分),预期在 SWE-bench 风格任务上显著提升通过率。

阶段 3-4 完成后,具备多 agent 协作 + 自优化基础设施,进入学术界的 Meta-Harness 阶段。

### 7.2 成本影响

- 缓存命中率:95% → 88-92%(标准方案)/ 93-95%(保守方案)
- API 成本:增加约 10-15%(DeepSeek 缓存读取价格通常是 input 的 10-14%)
- 换得能力:Plan/Verify/Recover + 语义记忆 + Multi-Agent,ROI 极高

### 7.3 学术对标

- 当前:**人工设计成熟期**(单 agent harness 完整)
- 阶段 2 完成后:**工业级 harness**(对照 Claude Code 2.1.88 架构)
- 阶段 3 完成后:**Meta-Harness 基础设施**(具备 trace analyzer)
- 阶段 4 完成后:**跨平台工业级 harness**(Windows 完整支持)

---

## 八、附录

### 8.1 关键文件路径速查

| 模块 | 路径 |
|---|---|
| Agent Loop | `rust/crates/runtime/src/conversation.rs` |
| Prompt Cache 监控 | `rust/crates/api/src/prompt_cache.rs` |
| Trust Resolver(待解锁) | `rust/crates/runtime/src/trust_resolver.rs` |
| Recovery Recipes | `rust/crates/runtime/src/recovery_recipes.rs` |
| Worker Boot | `rust/crates/runtime/src/worker_boot.rs` |
| Memory | `rust/crates/runtime/src/memory.rs` |
| Compact | `rust/crates/runtime/src/compact.rs` |
| Hooks | `rust/crates/runtime/src/hooks.rs` |
| Task Registry | `rust/crates/runtime/src/task_registry.rs` |
| Lane Events | `rust/crates/runtime/src/lane_events.rs` |
| MCP Lifecycle | `rust/crates/runtime/src/mcp_lifecycle_hardened.rs` |
| Sandbox | `rust/crates/runtime/src/sandbox.rs` |
| LSP Client | `rust/crates/runtime/src/lsp_client.rs` |
| CLI REPL | `rust/crates/rusty-claude-cli/src/main.rs` |
| Ultraplan | `rust/crates/rusty-claude-cli/src/ultraplan.rs` |

### 8.2 验证命令速查

```bash
# 格式化检查
scripts/fmt.sh --check

# Clippy
cd rust && cargo clippy --workspace --all-targets -- -D warnings

# 全量测试
cd rust && cargo test --workspace

# Release 构建
cd rust && cargo build --release

# Mock parity 验证
cd rust && ./scripts/run_mock_parity_harness.sh

# 重编译 claw.exe
cd rust && cargo build && cp target/debug/claw.exe debug/claw.exe
```

### 8.3 关键接入点

| 接入点 | 位置 | 用途 |
|---|---|---|
| `record_turn_failed` | `conversation.rs` L425 | RecoveryOrchestrator 入口 |
| `PromptCacheEvent` | `conversation.rs` L174 | 缓存监控扩展 |
| `PostToolUse` hook | `hooks.rs` | LoopDetectionMiddleware |
| `WorkerFailureKind::TrustGate` | `worker_boot.rs` | TrustResolver 接入 |
| `FailureScenario::from_worker_failure_kind` | `recovery_recipes.rs` L46 | Recovery 桥 |

### 8.4 参考资料

- [Agent Harness Engineering: A Survey (OpenReview)](https://openreview.net/pdf/f358711a95aaaf61fdeffd4ef3fc60fba9b8da57.pdf)
- [awesome-agent-harness (GitHub)](https://github.com/Picrew/awesome-agent-harness)
- [LangChain: Improving Deep Agents with Harness Engineering](https://blog.langchain.com/)
- [LangChain: Your Harness, Your Memory](https://blog.langchain.com/your-harness-your-memory/)
- [SEAGym: An Evaluation Environment for Self-Evolving LLM Agents](https://arxiv.org/abs/2606.17546)
- [Claude Code Source Leak (The Register)](https://www.theregister.com/2026/03/31/anthropic_claude_code_source_code/)
- [Anthropic Managed Agents](https://www.anthropic.com/engineering/managed-agents)
- [DeepSeek Context Caching on Disk 设计规则](https://blog.csdn.net/wuShiJingZuo/article/details/160641455)
- [PromptCaching 4 家 LLM API 缓存规则排查清单](https://blog.csdn.net/cmzznet/article/details/161051934)
- [Anthropic 工程师:Claude Code 关键设计 — Prompt Caching Is Everything](https://m.toutiao.com/group/7612302476603163142/)

---

## 九、变更记录

| 日期 | 版本 | 变更 |
|---|---|---|
| 2026-07-19 | v1.0 | 初始版本,涵盖阶段 1-4 + 缓存保护方案 |
| 2026-07-20 | v1.1 | 校正 Step 状态：5 个原标 ✅ 的 Step 经代码核对实际为部分实现，改为 ⚠️ 部分（Step 2.1 / 2.4 / 3.1 / 3.2 / 3.3），并补充真实状态说明 |
| 2026-07-21 | v1.2 | 实施完成 4 个原 ⚠️ 部分 Step 并校正状态：Step 2.1 ✅(commit 083f4a9,/ultraplan 对接 runtime planner)、Step 2.4 ✅(commit 876f577,EmbeddingProvider trait + FastembedProvider)、Step 3.2 ✅(3.2-a/b/c 全部完成,commits 8322e88/a46a3b5/36d9721,subagent-as-tool 路由)、Step 3.3 ✅(commit 23c7c72,K-means 失败聚类)。Step 3.1 维持 ⚠️ 部分(规则反馈已够用,视觉/模型裁判留到阶段 4)。 |
| 2026-07-21 | v1.3 | 新增阶段 3.5:三层信息持久化架构(基于论文调研)。基于 Anthropic《Effective Context Engineering》《Multi-Agent Research System》、CompactionRL (arXiv:2607.05378)、MIRIX (arXiv:2507.07957) 实施 5 个改进:P0-1 NOTEBOOK.md parse() 修复 + Structured Note-taking(commit 59f1663,26 测试)、P0-3 压缩前 NOTEBOOK 刷新 trigger(commit 8ea0c67,3 测试)、P0-2 子智能体真实化(同步阻塞 + 上下文隔离 + 文件持久化,commit c2e8f48,10 测试,修复 MultiAgentCoordinator 空壳问题)、P1 microcompact 结构化保留(子智能体指针 + 多行预览,commit a1ac0d1,5 测试)、P3 streaming stall 事件间超时(commit b7edada,337 测试全绿)。新增 37 个测试,总 918 passed。修复长程任务中"AI 忘记关键信息导致重复 dispatch"stall 问题。 |
| 2026-07-21 | v1.4 | 代码核对复核 Step 4.1 / 4.2,校正状态从 ✅ 到 ⚠️ 部分。Step 4.1 Sandbox:SandboxBuilder trait + 三实现存在(可编译),但 bg.rs::spawn 完全绕过 SandboxBuilder、assign_process_to_job_object 是死代码、公共 API 未导出、无功能性测试。Step 4.2 LSP Client:协议层 + ProcessLspTransport 传输层已编码,但生产 dispatch 走 MemoryLspTransport placeholder、ProcessLspTransport 是死代码、repomap 协同未实现、无 lsp-types/tower-lsp 依赖、无 rust-analyzer 集成测试。两个 Step 都需要补齐"整合 + 功能性测试"才能达到 ✅ 完整。 |
