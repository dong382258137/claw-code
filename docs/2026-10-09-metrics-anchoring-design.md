# 性能/质量指标口径锚定设计（对齐行业标准）

> 日期：2026-10-09
> 目的：为「提高 claw 执行任务的质量与性能」提供**可比较、非自定义**的度量口径。
> 原则：**不发明指标名**。凡行业已有被一致认定的语义约定（OTel GenAI / agent 评测基准），一律复用。

## 1. 为什么必须锚定标准

自行定义的指标（"感觉快了""输出变短了"）有三个致命缺陷：

1. **不可跨版本比较** —— 改了实现后无法判断是改善还是回退；
2. **可被伪造** —— 例如"收敛声明通过率"可被模型刷高（多输出声明段即可），度量失效；
3. **无法与业界对话** —— 无法回答"我们的 TTFT 相对业界基线如何"。

行业已有两套被一致认定的基准，本方案全部复用：

| 来源 | 覆盖 | 权威性 |
|---|---|---|
| **OpenTelemetry GenAI Semantic Conventions** | 延迟 / token / 调用次数 / 工具调用 | CNCF 官方，2026 年已成 LLM 可观测事实标准 |
| **Agent 评测基准（tau-bench / SWE-bench）** | 任务成功率 / 可靠性 | 学界 + 工业界一致引用的 pass@k / pass^k / resolved rate |

---

## 2. 五项指标的权威定义与映射

### M1 首响应时间 → OTel `gen_ai.client.inference.time_to_first_chunk`

**标准定义（逐字引用 OTel GenAI semconv）：**

> `gen_ai.client.inference.time_to_first_chunk` | Histogram | `s` |
> Time to receive the first chunk, measured from when the client issues the
> generation request to when the first chunk is received in the response stream.

**建议 bucket**（OTel 规定）：
`[0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92]`

**本仓库口径**：从客户端发出一次 inference 请求，到收到**流式响应首 chunk** 的墙钟。
注意：**不是**「turn 开始 → 第一次工具调用」（那包含本地预处理）。要的是纯网络 TTFT。

**落地点**：`telemetry` crate 需新增 `first_chunk` 事件（当前只有 `assistant_iteration_completed`，是整轮结束才记，测不到首 chunk）。测量点应在 SSE 流解析层。

---

### M2 总时长 → OTel `gen_ai.invoke_agent.duration`

**标准定义（逐字引用）：**

> `gen_ai.invoke_agent.duration` | Histogram | `s` |
> The end-to-end duration of a single in-process agent invocation, from the
> moment the invocation starts until the agent emits the last chunk of its
> final response or terminates with an error.

**建议 bucket**：`[0.1, 0.2, 0.4, 0.8, 1.6, 3.2, 6.4, 12.8, 25.6, 51.2, 102.4, 204.8, 409.6]`

**语义边界（OTel 明确）**：一个 "agent invocation" = 一次用户 turn 的端到端。若只测单次 provider 调用，则用 `gen_ai.client.operation.duration`，**不可混用**。

**本仓库口径**：`turn_started` → `turn_completed|turn_failed`（等价于现有 `latency_ms`）。这是「一个用户 turn」的端到端，符合 invoke_agent 语义。

**落地点**：已存在（`record_turn_completed` 的 `latency_ms`）。只需补 OTel bucket 边界以对齐直方图口径。

---

### M3 迭代数 → OTel `gen_ai.invoke_agent.inference_calls`

**标准定义（逐字引用）：**

> `gen_ai.invoke_agent.inference_calls` | Histogram | `{inference_call}` |
> The number of inference (model) calls a GenAI agent makes during a single invocation.

**建议 bucket**：`[1, 2, 4, 8, 16, 32, 64, 128]`

**关键语义（OTel 明确，直接击中本仓库既有盲区）：**

> The distribution is scoped to a single agent invocation and SHOULD include
> only the inference calls the agent itself issued including failed ones;
> calls made by sub-agents or transferred-to agents are recorded against
> those agents' own invocations so that each inference call is counted
> **exactly once** across the call tree.

**本仓库口径**：一个 turn 内 `assistant_iteration_completed` 的次数 = 模型调用数。

**落地点**：现有 `assistant_iteration_completed` 事件可直接聚合计数。但需修正一点：**分诊调用（`RequestKind::Subagent`）当前不记入主 session**，按 OTel 语义它应记到自己的 invocation 上——即分诊应被单独计一次 inference_call，而不是被隐藏。这正是上一刀修复的可观测性理由。

---

### M4 $/task → OTel token 计数器 × 定价

**标准定义（逐字引用 OTel token metrics）：**

> `gen_ai.client.inference.usage.input_tokens` | Counter | `{token}` |
> The number of input (prompt) tokens used, **including cached tokens**.
>
> `gen_ai.client.inference.usage.output_tokens` | Counter | `{token}` |
> The number of output (completion) tokens used, **including reasoning tokens**.
>
> `gen_ai.client.inference.usage.cache_read.input_tokens` — subset of input_tokens
> `gen_ai.client.inference.usage.cache_write.input_tokens` — subset of input_tokens
> `gen_ai.client.inference.usage.reasoning.output_tokens` — subset of output_tokens

**关键约束**：`cache_read` / `cache_write` / `reasoning` 是 **input/output 的「子集」**，不是并列项。计费时按各自单价算，但**总量口径不得重复计数**。

**$/task 定义为推导量**（非独立指标）：
```
$/task = Σ(各 provider 各类 token × 该类单价)
       = input×p_in + output×p_out + cache_write×p_cw + cache_read×p_cr
```

**本仓库口径**：`TokenUsage { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens }` 已逐字段对齐 OTel 语义（`cache_creation`↔`cache_write`）。定价用 `usage.rs::UsageCostEstimate`。

**落地点**：已存在（`UsageCostEstimate::total_cost_usd`）。**缺口**：分诊等 Subagent 路由的 usage 未计入主 session，导致 $/task 低估——需按 OTel「每调用计一次」汇总到 invocation 树的根。

---

### M5 一次做对率 → tau-bench `pass^1`（可靠性）

**标准定义（逐字引用 tau-bench 官方）：** 基准同时报告 `pass@k` 与 `pass^k`：

| 指标 | 语义 | 回答的问题 |
|---|---|---|
| `pass@k` | **能力**：k 次尝试中**至少一次**成功 | "它能不能做对？" |
| `pass^k` | **可靠性**：k 次尝试**全部**成功 | "它是不是每次都做对？" |

tau-bench 报告 `Pass^1 … Pass^4` 列（例：TC/claude-3.5 Pass^1=0.460、Pass^2=0.326，可靠性随 k 单调下降）。

**本仓库口径**：用户所述「一次做对率」= **`pass^1`**（单次尝试成功，即可靠性基线），
应与 SWE-bench 的 `resolved rate`（resolve 率，二值成功判定）同口径。

**关键诚实性要求（防止指标被刷）**：
- `pass^1` 的分母必须是**独立任务样本**，成功判定必须是**客观终态**（测试通过 / 目标达成），**不能**用模型自己的「收敛声明通过」当判据——那正是可被刷高的伪指标（模型多输出声明段即可通过）。
- 本仓库现有可用代理量：`convergence::classify_punchback` 的打回率 → `pass^1 ≈ 1 − punchback_rate`。**但这是弱代理**，只反映"用户是否打回"，非客观成功。**结论：pass^1 必须配合外部任务集（如 SWE-bench 式）才有意义，不能仅靠会话内信号。**

**落地点**：需接入外部任务集。会话内先上「punchback 率」作临时代理，并在文档中明确标注其为弱代理。

---

## 3. 标准 → 本仓库 落地对照表（含缺口）

| 指标 | OTel/基准名 | 本仓库现状 | 缺口 |
|---|---|---|---|
| M1 首响应 | `gen_ai.client.inference.time_to_first_chunk` | ❌ 无（只有整轮完成事件） | **需新增首 chunk 埋点**（SSE 层） |
| M2 总时长 | `gen_ai.invoke_agent.duration` | ✅ `latency_ms` | 补 OTel bucket 边界 |
| M3 迭代数 | `gen_ai.invoke_agent.inference_calls` | ✅ `assistant_iteration_completed` 计数 | 分诊/子代理调用需归入调用树，各计一次 |
| M4 $/task | `gen_ai.client.inference.usage.*` × 定价 | ⚠️ 部分（Subagent 路由不计入主 session） | **补全调用树 token 汇总** |
| M5 一次做对率 | tau-bench `pass^1` / SWE-bench resolved | ⚠️ 弱代理（punchback 率） | **需外部客观任务集** |

---

## 4. 立即可以做的（按依赖顺序）

1. **M2/M3 零成本**：现有事件已可聚合，先出基线（不改代码）。
2. **M1 补埋点**：SSE 首 chunk 事件 —— 这是回答"首轮为什么慢"的直接度量，也是上一刀优化的验收信号。
3. **M4/M3 归口**：把 Subagent 路由（含分诊）的 token/调用计入 invocation 树，修正 $/task 与迭代数低估。
4. **M5 任务集**：选定一个外部任务集（SWE-bench 子集 / 自建 ground-truth turn 集）作为 pass^1 的客观分母。

## 5. 明确不做（防自欺）

- ❌ 不用「收敛声明通过率」当质量指标 —— 可被模型自行刷高。
- ❌ 不把"输出变短"当性能改善 —— 长度非延迟，且可能牺牲质量。
- ❌ 不发明新指标名 —— 全部复用 OTel / tau-bench / SWE-bench 既有口径，保证可跨版本、可跨业界比较。

## 参考来源

- OpenTelemetry GenAI Semantic Conventions — https://github.com/open-telemetry/semantic-conventions-genai （`docs/gen-ai/gen-ai-metrics.md`、`client-inference.md`、`gen-ai-token-metrics.md`）
- tau-bench（Sierra Research）— https://github.com/sierra-research/tau-bench （pass^k / pass@k 定义）
- SWE-bench — resolved rate 口径
