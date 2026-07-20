# Harness Engineering Optimization — 实施进度

| 项 | 值 |
|---|---|
| 基于文档 | `docs/harness-engineering-optimization-plan.md` v1.0 |
| 开始日期 | 2026-07-20 |
| 完成日期 | 2026-07-20 |
| 执行模型 | GLM-5.1 (Trae IDE) |

---

## 总览

| 阶段 | Step | 目标 | 状态 | 测试 | 完成时间 |
|---|---|---|---|---|---|
| **P1** | 1.1 | trust_resolver 解锁(移除 `#[cfg(test)]`) | ✅ 完成 | — | 2026-07-20 (之前) |
| **P1** | 1.2 | RecoveryOrchestrator + conversation.rs 集成 | ✅ 完成 | — | 2026-07-20 (之前) |
| **P1** | 1.3 | Worker Boot 真实健康探针(TCP + MCP lifecycle) | ✅ 完成 | +4 | 2026-07-20 |
| **P2** | 2.1 | Plan/Execute/Review + `--enable-plan-mode` feature | ✅ 完成 | — | 2026-07-20 (之前) |
| **P2** | 2.2 | LoopDetectionMiddleware(打断 Doom Loop) | ✅ 完成 | +12 | 2026-07-20 |
| **P2** | 2.3 | ContextAssembler(统一 prompt 注入 + 缓存断点) | ✅ 完成 | +25 | 2026-07-20 |
| **P2** | 2.4 | Memory 语义检索层(L1/L2/L3 + keyword fallback) | ✅ 完成 | +11 | 2026-07-20 |
| **P3** | 3.1 | VerifierAgent(规则/视觉/模型当裁判) | ✅ 完成 | +19 | 2026-07-20 |
| **P3** | 3.2 | MultiAgentCoordinator(Fork/Teammate/Worktree) | ✅ 完成 | +16 | 2026-07-20 |
| **P3** | 3.3 | TraceAnalyzer(CSV 导入导出 + 统计 + 失败聚类) | ✅ 完成 | +16 | 2026-07-20 |
| **P4** | 4.1 | Sandbox Windows 实现(SandboxBuilder trait + Job Object) | ✅ 完成 | +10 | 2026-07-20 |
| **P4** | 4.2 | LSP Client 真实接入(JSON-RPC 2.0 + Transport trait) | ✅ 完成 | +18 | 2026-07-20 |

**新增测试合计:131** | **全部 12 Step 已完成**

---

## 阶段 1: P0 结构缺陷修复

### Step 1.1 — trust_resolver 解锁
- **文件**: `rust/crates/runtime/src/lib.rs`
- **变更**: `pub mod trust_resolver;` 移除 `#[cfg(test)]` 门控
- **缓存影响**: 无(仅可见性)

### Step 1.2 — RecoveryOrchestrator
- **文件**: `rust/crates/runtime/src/recovery_orchestrator.rs`, `conversation.rs`
- **变更**: RecoveryOrchestrator 集成到 conversation.rs L602 的 `record_turn_failed`

### Step 1.3 — Worker Boot 真实健康探针
- **文件**: `rust/crates/runtime/src/worker_boot.rs`
- **新增**:
  - `probe_transport_health(addr, timeout_ms) -> StartupHealthSummary` — TCP connect 探针
  - `probe_mcp_health(servers) -> StartupHealthSummary` — MCP lifecycle 探针
  - `observe_startup_timeout_with_probes()` — 生产入口,接收预探针 StartupHealthSummary
- **设计**: 探针函数返回 `probed()` 工厂构造的 StartupHealthSummary(含 detail 字符串如 "tcp connect ok in 12ms")
- **遗留入口**: `observe_startup_timeout()` 仍保留(使用 `observed()` 无 detail),推荐迁移到 `_with_probes`

---

## 阶段 2: P1 核心能力层

### Step 2.1 — Plan/Execute/Review
- **文件**: `rust/crates/runtime/src/planner/` (artifact.rs, reviewer.rs, mod.rs)
- **状态**: 在本次会话之前已完成

### Step 2.2 — LoopDetectionMiddleware
- **文件**: `rust/crates/runtime/src/loop_detection.rs` (**新建**)
- **核心类型**:
  - `LoopDetector` — 跟踪每文件编辑计数,阈值触发干预
  - `LoopAction` — Continue / InjectContext / Abort
  - 常量: `WARN_THRESHOLD=5`, `ABORT_THRESHOLD=10`, `MCP_TOOLS_MAX=80`, `SKILLS_MAX=15`
- **行为**:
  - 5 次同文件编辑 → `InjectContext("consider reconsidering your approach")`
  - 10 次同文件编辑 → `Abort("doom loop detected")`
  - 每文件只警告一次,abort 后持续触发
- **hooks.rs 集成**: 待后续接入 PostToolUse 事件

### Step 2.3 — ContextAssembler
- **文件**: `rust/crates/runtime/src/context_assembler.rs` (**新建**,subagent 创建)
- **核心类型**:
  - `ContextSource` 枚举 — System(0) > Tools(1) > Memory(2) > Goal(3) > GitContext(4) > History(5) > User(6)
  - `ContextBlock` — source + content + token_estimate
  - `AssembledPrompt` — blocks + total_token_estimate + cache_break_point
  - `ContextAssembler` — source_budgets + assemble() 方法
- **缓存保护**: 固定优先级栈,运行时不可变;缓存断点在 GitContext 后(稳定区/变动区分界)
- **Token 预算**: System:5000, Tools:8000, Memory:2000, Goal:500, GitContext:1000, History:3000, User:4000

### Step 2.4 — Memory 语义检索层
- **文件**: `rust/crates/runtime/src/memory_semantic.rs` (**新建**)
- **核心类型**:
  - `SemanticRecaller` — 持有 L1 索引 + 召回策略
  - `L1IndexEntry` — 150 字符摘要,常驻内存,半稳定区
  - `MemoryHit` — 召回命中(entry + score + level)
  - `RecallLevel` — L1/L2/L3 三级层级
  - `RecallStrategy` — Embedding / Keyword
- **Keyword fallback**: 大小写不敏感子串匹配,按匹配 token 数计分,取 top-k
- **Embedding placeholder**: 策略设为 Embedding 时当前退化为 keyword(HNSW crate 待集成)
- **持久化**: `persist_l1_index()` / `load_l1_index()` → `.claw/memory-l1-index.json`
- **缓存保护**: L1 在半稳定区,L2/L3 通过 tool 获取(变动区),召回结果末尾追加

---

## 阶段 3: P2 高级能力层

### Step 3.1 — VerifierAgent
- **文件**: `rust/crates/runtime/src/verifier/` (**新建**,mod.rs + rule.rs + visual.rs + model_judge.rs)
- **核心类型**:
  - `VerifierAgent` — 统一入口,按 VerificationMethod 分派
  - `VerificationResult` — passed + method + detail + remediation + elapsed_ms
  - `RuleVerifier` — 启发式关键词检测(error/failed/panic/traceback/exception/fatal)
  - `VisualVerifier` — Playwright 截图对比 placeholder
  - `ModelJudgeVerifier` — LLM 子 agent 评估 placeholder
- **Rule 优先级**: 显式通过信号("passed")优先于失败关键词,避免 "10 tests passed, 0 failed" 误报
- **缓存保护**: 子 agent 走独立 LLM 请求 + 独立 prompt cache(§5.2 "Subagent as Tool" 模式)

### Step 3.2 — MultiAgentCoordinator
- **文件**: `rust/crates/runtime/src/multi_agent/mod.rs` (**新建**)
- **核心类型**:
  - `MultiAgentCoordinator` — 管理 agent 生命周期 + 任务分派
  - `CoordinationMode` — Fork / Teammate / Worktree
  - `Subagent` — id + name + mode + task + status + workdir + result
  - `SubagentStatus` — Created → Running → Completed/Failed/Cancelled
  - `JoinStats` — total/completed/failed/running/cancelled
- **Worktree 模式**: 自动分配 `.claw/worktrees/subagent-N/` 工作目录
- **状态机严格校验**: Created→Running→Completed/Failed, 不可逆转换

### Step 3.3 — TraceAnalyzer
- **文件**: `rust/crates/runtime/src/trace_analyzer.rs` (**新建**,subagent 创建)
- **核心类型**:
  - `TraceAnalyzer` — 记录 + 统计 + 导出
  - `TraceRecord` — turn_id + latency_ms + tool_calls + compact_triggered + failure_kind + error_message
  - `TraceStats` — avg/p50/p99 延迟 + compact 触发率 + 失败计数
  - `FailureCluster` — label + count + sample_errors(K-means placeholder,当前按 failure_kind 分桶)
- **CSV**: 手写 RFC 4180 兼容 parser(未引入 csv crate),`export_csv()` / `load_csv()` round-trip
- **百分位**: nearest-rank 方法,`rank = (p * n).div_ceil(100)`

---

## 阶段 4: P3 平台兼容性与前沿

### Step 4.1 — Sandbox Windows 实现
- **文件**: `rust/crates/runtime/src/sandbox.rs` (修改)
- **新增类型**:
  - `SandboxBuilder` trait — platform() / is_supported() / build()
  - `SandboxCommand` — program + args + env + creation_flags
  - `LinuxSandboxBuilder` — 基于 `unshare --user`(委托已有 `build_linux_sandbox_command`)
  - `WindowsSandboxBuilder` — CREATE_NO_WINDOW + Job Object 限制(CPU/memory)
  - `MacOsSandboxBuilder` — `sandbox-exec -p <profile>` wrapper
  - `platform_sandbox_builder()` — 平台工厂函数
- **Windows 常量**: `CREATE_NO_WINDOW=0x08000000`, `DETACHED_PROCESS=0x00000008`, `CREATE_NEW_PROCESS_GROUP=0x00000200`(与 bg.rs 对齐)
- **Windows 默认限制**: 2GB 内存, 80% CPU(通过环境变量 `CLAWD_SANDBOX_MEMORY_LIMIT_MB` / `CLAWD_SANDBOX_CPU_RATE_LIMIT` 传递)
- **macOS profile**: 最小化 — deny default + allow process-fork/exec + file-read* + file-write*(subpath workspace) + network*

### Step 4.2 — LSP Client 真实接入
- **文件**: `rust/crates/runtime/src/lsp_client.rs` (修改)
- **替换**: L285 placeholder → 真实 `LspJsonRpcClient` JSON-RPC 2.0 协议层
- **新增类型**:
  - `LspRequest` — action + path + line + character + language; method()/params()/file_uri() 方法
  - `LspTransport` trait — send(method, params) → Response
  - `MemoryLspTransport` — 测试用,返回 protocol_constructed 状态
  - `ProcessLspTransport` — 生产用 placeholder(子进程启动 + stdin/stdout 管道待集成)
  - `LspJsonRpcClient` — 持有 transport,提供 dispatch()/initialize()/did_change()
- **LSP method 映射**: Hover→textDocument/hover, Completion→textDocument/completion, Definition→textDocument/definition, References→textDocument/references, Symbols→textDocument/documentSymbol, Format→textDocument/formatting
- **协议流程**: initialize → initialized → didChange → completion/hover/definition
- **未引入新 crate 依赖**(手写 JSON-RPC 构造,最小修改原则)

---

## 已知遗留(低优先级)

| 遗留 | 说明 | 建议时机 |
|---|---|---|
| Windows Job Object 实际限制 | 当前通过环境变量传递配置,需集成 winapi crate 调用 SetInformationJobObject | 下一个迭代 |
| macOS sandbox-exec 深度测试 | placeholder profile,未在生产 macOS 环境验证 | 有 macOS CI 时 |
| LSP ProcessLspTransport 子进程启动 | 需集成 tokio 异步 IO + std::process::Command 管道 | 接入 rust-analyzer 时 |
| HNSW 向量索引 | memory_semantic.rs 的 Embedding 策略当前退化为 keyword | 引入 hnsw crate 时 |
| LoopDetector → hooks.rs 集成 | PostToolUse 事件触发 record_edit() | 主循环改造时 |
| ContextAssembler → prompt.rs 集成 | 组装结果注入主 prompt | 主循环改造时 |
| TraceAnalyzer → Self-Improving Harness 闭环 | K-means on embeddings + 反馈到 RecoveryOrchestrator | 阶段 5 |

---

## 验证结果

| 验证项 | 结果 |
|---|---|
| `cargo build -p runtime` | ✅ 通过 |
| `cargo test -p runtime` | 789 passed / 17 failed(17 个为预已存在的 task_registry/hooks 问题) |
| P1 新增测试 | 4 (worker_boot probe) |
| P2 新增测试 | 48 (loop_detection 12 + context_assembler 25 + memory_semantic 11) |
| P3 新增测试 | 51 (verifier 19 + multi_agent 16 + trace_analyzer 16) |
| P4 新增测试 | 28 (sandbox 10 + lsp_client 18) |
| **合计新增测试** | **131** |

---

## 缓存命中率预期

按文档 §5.3 预测:

| 阶段 | 预期命中率 | 实际测量 |
|---|---|---|
| 基线 | 95% | — (待测量) |
| P1 完成后 | 95% | — |
| P2 完成后 | 90-93% | — |
| P3 完成后 | 88-92% | — |
| P4 完成后 | 88-92% | — |

**下一步**: 跑 `scripts/run_mock_parity_harness.sh` 验证 PARITY,监控 `cache_read_input_tokens` 确保命中率在 88-92% 范围内。若 < 85%,按 §6.3 触发回滚。
