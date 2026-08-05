# 子智能体前缀缓存对齐设计(2026-08-05)

> 目标:修复 DeepSeek 官网统计缓存命中率 92% vs 本地磁盘统计 98.54% 的差异
> 根因:子智能体 system prompt 含唯一 id/name/task + 存在两份重复构造,导致每请求全量 miss,且子智能体请求污染主 agent 的缓存统计
> 范围:3a(统一 system prompt)+ 3b(统计隔离)+ 公共函数提取(DRY)

---

## 1. 背景

- 本地 stats 体检:命中率 98.54%(34 session / 2504 请求),回归分析 R²=0.96,
  `creation ≈ 12,179×breaks + 848×req + 20,574`(每次 microcompact 缓存断裂约浪费 12K token)。
- 已确认机制:`conversation.rs` 每 turn 末尾 microcompact,只有旧工具结果老化被摘要时才"原地改写中部"→ 破掉其后整个尾巴的缓存。
- 主 agent 路径无 system prompt 变动(静态/动态段结构稳定),故命中率高。
- **子智能体路径**每次请求都注入唯一内容到 system prompt(见 §2),导致:
  1. 每请求全量 miss(前缀共享失效);
  2. 子智能体请求经主 client 记录,污染主 agent 的缓存统计(92% vs 98.5% 差异)。

## 2. 现状分析(行号以本设计定稿时 main 分支为准)

| # | 组件 | 位置 | 现状 |
|---|---|---|---|
| 1 | `build_subagent_system_prompt` | `rust/crates/runtime/src/conversation.rs:3362` | system 注入 `# Subagent: {name} ({id})` + task;按复杂度 3 变体追加 SOP |
| 2 | `execute_subagent_llm` | `conversation.rs:3435` | 调用 ①;user message **重复** task(`请执行以下任务:\n\n{enhanced_task}`);单次 `stream_async` |
| 3 | `SubagentDispatcher::dispatch_impl` | `rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs:56` | 内联**第二份** system prompt(含 id/name/task,无 SOP);同步 `stream` |
| 4 | 现有测试 | `conversation.rs:7605/7645/7671` | 断言 system 含 name/task → 需重写 |
| 5 | `ApiRequest` | `conversation.rs:237` | `{system_prompt, messages}`,无 kind 字段;4 个构造点(1909 主 / 3467 子 / 5883 测试 / dispatcher:98) |
| 6 | `ApiClient::with_model` | `conversation.rs:319`(trait)/ `streaming.rs:469`(impl) | 构造新 client,但**同 session_id** |
| 7 | `AnthropicRuntimeClient::new` | `streaming.rs:192-334` | session_id → `CacheBreakDetector::new(session_id)` |
| 8 | `record_cache_break` | `streaming.rs:275` | 无条件 `record_usage`(单一 detector) |
| 9 | `CacheBreakDetector` | `api/cache_break_detection.rs:151` | 单 session 状态机,指纹对比 + persist |
| 10 | DAG 子 client 构造 | `app.rs:3333` | `AnthropicRuntimeClient::new(session_id, model_for_subagent, ...)` 用**主 session** |
| 11 | DeepSeek 缓存 | 官方文档 | 跨请求全局共享、公共前缀自动检测落盘、完整单元匹配命中、自动开启 |

## 3. 根因

1. **前缀破坏**:system prompt 以唯一内容开头(`# Subagent: {name} ({id})` + task),唯一片段之前的静态前缀也可能被 id 长度差异影响对齐;且 task 在 system 与 user 中重复出现,浪费 token。
2. **双份实现漂移**:`execute_subagent_llm` 与 `SubagentDispatcher::dispatch_impl` 各有一份 prompt 构造,内容不同(DAG 版无 SOP),无法共享前缀。
3. **统计污染**:子智能体请求经主 `ApiClient` 发出 → `record_cache_break` 写入主 detector → 主 session 的 `previous`/`break_reasons` 被踩脏,本地命中率统计失真。

## 4. 设计

### 4.1 3a — 统一 system prompt(前缀共享)

**A1. `build_subagent_system_prompt(complexity)` 静态化**
- 签名:`build_subagent_system_prompt(subagent_id, name, task, complexity)` → `build_subagent_system_prompt(complexity)`
- 返回**纯静态**模板,移除:
  - `# Subagent: {name} ({subagent_id})`
  - `## 任务 {task}`(原 §2 中 task 注入部分)
- 保留:基础身份说明(不指名)/ 约束 / 输出格式;按复杂度追加 SOP(3 个静态变体,Simple / Diagnostic / Architectural)。

**A2. `execute_subagent_llm` user message 承载唯一内容**
```text
# Subagent: {name} ({subagent_id})

请执行以下任务:

{enhanced_task}
```
- id/name/task 移入 user message;task 从重复两次 → 一次;
- `gate_task` 的 `enhanced_task` 只进 user,不碰 system;
- system 完全静态 → 同一复杂度的所有子智能体请求共享同一公共前缀。

**A3. 公共函数提取(DRY)**
- 新增 `pub fn build_subagent_request(subagent_id, name, task, complexity) -> ApiRequest`
  (conversation.rs 公共函数,供 cli crate 经 runtime crate 引用);
- `execute_subagent_llm` 与 `subagent_dispatcher.rs` 均调用它 → 消除两份重复构造;
- DAG 路径无 complexity → 传 `Complexity::Simple`(与现状一致:无 SOP);
- `ApiRequest` 构造时带上 `request_kind: RequestKind::Subagent`(见 4.2)。

### 4.2 3b — 统计隔离(request_kind 路由)

**B1.** `ApiRequest` 加字段:
```rust
pub request_kind: RequestKind   // #[derive(Default)] → RequestKind::Main
```
- `RequestKind { Main, Subagent }` 枚举(默认 `Main`);
- 已有 4 个构造点零改动(默认 Main);子智能体两处显式设 `Subagent`。

**B2.** `AnthropicRuntimeClient` 加 `subagent_cache_break_detector: Option<CacheBreakDetector>`
- lazy 构造于 `format!("subagent-{session_id}")`;
- `stream` / `stream_async` 把 `request.request_kind` 传入 `consume_stream`;
- `record_cache_break(kind)` 按 kind 选 detector;
- 效果:子智能体请求全部记入独立 `subagent-*` stats,主 agent 的 `previous` 与 `break_reasons` 不再被污染;本地统计可区分主/子两条曲线。

### 4.3 路径覆盖矩阵

| 路径 | 请求入口 | kind | system | user |
|---|---|---|---|---|
| 主 agent | conversation.rs:1909 | Main(默认) | 静态/动态分段(现状) | 现状 |
| `execute_subagent_llm`(model 指定 / model=None) | conversation.rs:3467 | Subagent | 纯静态变体 | id/name/task |
| DAG `SubagentDispatcher` | dispatcher.rs:98 | Subagent | 纯静态 Simple(无 SOP) | id/name/task |

## 5. 收益量化

- 静态 system(Simple ~300 tokens;带 SOP ~500-600 tokens)从每次全量 miss → 公共前缀命中;
- 按周 5000 次子智能体调用估算,节省 **1.5-3M creation tokens/周**;
- 官方命中率:子智能体 total 中 cache read 占比从 ~0 提升;
- 本地检测:主 agent 的 `break_reasons` 不再被 "system prompt changed" 污染,统计口径恢复可信。

## 6. 风险与验证

| 风险 | 缓解 |
|---|---|
| 模型行为变化 | user 内容与原来 system+user 语义等效(仅去重复 task);人工抽查 1-2 次子智能体输出 |
| DAG 路径行为漂移 | dispatcher 改用公共函数后 SOP 仍为 None(Simple),与现状一致 |
| 测试失效 | 重写 3 个现有测试 + 新增(静态性断言、user message 构造、kind 路由) |
| 回归 | `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` 全绿 |

端到端验证:派发 2 个同复杂度子智能体,观察第 2 个的 `cache_read > 0`(DeepSeek usage 字段)。

## 7. 验收标准

1. 同复杂度子智能体的 system prompt 完全一致(静态性断言);
2. 子智能体请求的 user message 含 `# Subagent: {name} ({id})` + 单次 task;
3. 子智能体请求记入 `subagent-*` stats,主 session stats 无子智能体请求;
4. 全部测试 + clippy 通过;
5. 端到端第 2 个同复杂度子智能体 cache_read > 0。

---

## 8. 实施记录(2026-08-05)

### Commit 清单

| Commit | 内容 |
|---|---|
| `c2d1b3f4` | feat(runtime): ApiRequest 增加 request_kind 字段用于缓存统计隔离(Task 1) |
| `6db09fa9` | refactor(runtime): 子智能体 system prompt 静态化 + build_subagent_request 公共构造(3a)(Task 2) |
| `827b7a92` | refactor(runtime): SubagentDispatcher 复用 build_subagent_request,消除双份 prompt 构造(DRY)(Task 3) |
| `44942a73` | feat(cli): 子智能体请求路由到独立 subagent-* 缓存统计 session(3b)(Task 4) |

### 落地要点

- `build_subagent_system_prompt(complexity)` 模块级自由函数,纯静态(无 id/name/task);
- `build_subagent_request(subagent_id, name, task, complexity)` 公共构造,`# Subagent: {name} ({id})` + 单次 task 移入 user message;
- `execute_subagent_llm` 与 `SubagentDispatcher::dispatch_impl` 共用同一公共函数(DAG 路径走 Simple,与现状无 SOP 一致);
- cli `AnthropicRuntimeClient` 持有双 `CacheBreakDetector`(`<session>` / `subagent-{session}`),`record_cache_break` 按 `request_kind` 路由。

### 验证结果(沙箱)

- `cargo test --workspace`:runtime 1489 passed;另有 3 个既有 flaky 测试(dispatch_subagent_fails_gracefully_without_workspace_root、memory::load_and_freeze_rebuilds_semantic_l1_index、spawn_blocking_block_on_panics),独立重跑均 PASS,与本次改动无关;
- clippy:本次改动的文件(conversation.rs / subagent_dispatcher.rs / streaming.rs)0 warning;workspace 存在既有 doc-lint 告警(build.rs、knowledge_freshness.rs 等,基线状态);
- 静态性抽查:`# Subagent:` 仅出现在 `build_subagent_request` 的 user message 格式串(conversation.rs:329),system prompt 无唯一内容。

### 待用户端到端验证(需真实 DeepSeek API key)

1. 启动 CLI,派发 2 个同复杂度(`Diagnostic`)子智能体;
2. 观察官方 usage:第 2 个请求 `cache_read_input_tokens > 0`;
3. `claw doctor --cache-stats`:存在独立 `subagent-{session}` 且其 tracked_requests 等于子智能体请求数;主 session 不再含子智能体请求。
