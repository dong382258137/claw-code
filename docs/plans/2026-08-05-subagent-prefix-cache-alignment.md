# 子智能体前缀缓存对齐实施计划

**Goal:** 统一子智能体 system prompt(静态化前缀)+ 独立缓存统计 session,使 DeepSeek 缓存命中率从 92% 回升、本地统计不再被污染。

**Architecture:** 3a — 把唯一内容(id/name/task)从 system prompt 移入 user message,`build_subagent_system_prompt` 静态化;新增 `build_subagent_request` 公共函数消除 `execute_subagent_llm` 与 `SubagentDispatcher` 两份重复构造;DRY。3b — `ApiRequest` 增加 `request_kind` 字段,cli 侧 `AnthropicRuntimeClient` 持有双 `CacheBreakDetector`(主 session + `subagent-{session}`),按 kind 路由统计。

**Tech Stack:** Rust workspace;crates:`runtime`(conversation.rs / multi_agent::dag::subagent_dispatcher.rs / lib.rs)、`rusty-claude-cli`(streaming.rs)、`api`(CacheBreakDetector,不改)。

---

## 文件结构

| 文件 | 改动 | 职责 |
|---|---|---|
| `rust/crates/runtime/src/conversation.rs` | 修改 | `RequestKind` 枚举、`ApiRequest.request_kind` 字段、`build_subagent_system_prompt` 静态化、新增 `build_subagent_request`、`execute_subagent_llm` 改造、测试重写/新增 |
| `rust/crates/runtime/src/lib.rs` | 修改 | re-export `RequestKind`(streaming.rs 消费) |
| `rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs` | 修改 | `dispatch_impl` 改用 `build_subagent_request`(DRY),清理 3 个不再使用的 import |
| `rust/crates/rusty-claude-cli/src/streaming.rs` | 修改 | 双 `CacheBreakDetector` 字段 + `detector_for` 路由 + `consume_stream`/`record_cache_break` 传递 `request_kind` |
| `docs/2026-08-05-subagent-prefix-cache-alignment-design.md` | 已建 | 设计文档(本计划的 spec) |

---

## Task 1:RequestKind 类型 + ApiRequest 字段

**Files:**
- Modify: `rust/crates/runtime/src/conversation.rs:237`(ApiRequest struct 附近)
- Modify: `rust/crates/runtime/src/lib.rs:162-169`(conversation re-export)
- Test: `rust/crates/runtime/src/conversation.rs`(`mod tests` 内,`api_request_carries_system_prompt_split` 附近)

- [ ] **Step 1: 写失败测试**

在 `rust/crates/runtime/src/conversation.rs` 测试模块中,`api_request_carries_system_prompt_split` 测试后追加:

```rust
#[test]
fn api_request_carries_request_kind() {
    let request = ApiRequest {
        system_prompt: SystemPromptSplit::from_sections(vec!["static".to_string()]),
        messages: Vec::new(),
        request_kind: RequestKind::Main,
    };
    assert_eq!(request.request_kind, RequestKind::Main);
}
```

- [ ] **Step 2: 运行验证编译失败**

Run: `cd rust && cargo test -p runtime api_request_carries 2>&1 | tail -20`
Expected: FAIL — `no field request_kind` / `cannot find type RequestKind`

- [ ] **Step 3: 实现类型与字段**

在 `rust/crates/runtime/src/conversation.rs`,`ApiRequest` struct 定义(当前 `pub struct ApiRequest { pub system_prompt: SystemPromptSplit, pub messages: Vec<ConversationMessage> }`)上方新增:

```rust
/// 请求来源分类 — 用于缓存统计隔离。
///
/// 子智能体请求经 cli 侧路由到独立的 `subagent-{session}` 统计,
/// 不再污染主 agent 的缓存 break 检测(见设计文档 §4.2)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Main,
    Subagent,
}
```

struct 改为:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: SystemPromptSplit,
    pub messages: Vec<ConversationMessage>,
    pub request_kind: RequestKind,
}
```

- [ ] **Step 4: 更新全部 4 个构造点**(否则编译失败)

| 位置 | 文件:行号 | 加字段 |
|---|---|---|
| 主 agent 路径 | `conversation.rs:1909`(`ApiRequest { system_prompt: system_split, messages }`) | `request_kind: RequestKind::Main,` |
| 子智能体路径 | `conversation.rs:3467`(`execute_subagent_llm` 内) | `request_kind: RequestKind::Subagent,` |
| 测试 | `conversation.rs:5883`(`api_request_carries_system_prompt_split`) | `request_kind: RequestKind::Main,` |
| DAG 路径 | `subagent_dispatcher.rs:98` | `request_kind: RequestKind::Subagent,` |

- [ ] **Step 5: re-export**

`rust/crates/runtime/src/lib.rs:162-169` 的 `pub use conversation::{ ... }` 列表中加入 `RequestKind`(字母序:插在 `PromptCacheEvent` 与 `RuntimeError` 之间)。

- [ ] **Step 6: 运行验证**

Run: `cd rust && cargo test -p runtime api_request_carries 2>&1 | tail -10`
Expected: 2 tests PASS(`api_request_carries_system_prompt_split` + `api_request_carries_request_kind`)

- [ ] **Step 7: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs rust/crates/runtime/src/lib.rs
git commit -m "feat(runtime): ApiRequest 增加 request_kind 字段用于缓存统计隔离"
```

---

## Task 2:system prompt 静态化 + build_subagent_request + execute_subagent_llm 改造

**Files:**
- Modify: `rust/crates/runtime/src/conversation.rs:3362`(`build_subagent_system_prompt`)
- Modify: `rust/crates/runtime/src/conversation.rs:3435`(`execute_subagent_llm`)
- Test: `rust/crates/runtime/src/conversation.rs:7605/7645/7671`(3 个现有测试重写)+ 新增 2 个测试

- [ ] **Step 1: 重写 3 个现有测试(红:签名不匹配编译失败)**

**测试 1** `build_subagent_system_prompt_injects_diagnostic_sop`(现 7605 行)整体替换为:

```rust
/// §4.6 验收:Diagnostic 复杂度时 system_prompt 含诊断 SOP;且不含唯一内容(3a 静态化)
#[test]
fn build_subagent_system_prompt_injects_diagnostic_sop() {
    let prompt = build_subagent_system_prompt(crate::multi_agent::TaskComplexity::Diagnostic);
    // 3a:静态化后不得包含 id/name/task 唯一内容
    assert!(!prompt.contains("# Subagent:"), "unique header must move to user message");
    assert!(!prompt.contains("定位 wizard 闪退"), "task must move to user message");
    // 诊断 SOP 五条规则
    assert!(prompt.contains("## 诊断任务执行规范"), "missing SOP header");
    assert!(prompt.contains("CLAW_DIAG=1"), "missing rule 1: diag log first");
    assert!(prompt.contains("panic vs Err vs 配置错误"), "missing rule 2: confirm error type");
    assert!(prompt.contains("cargo build"), "missing rule 3: verify compilation");
    assert!(prompt.contains("复现验证证据"), "missing rule 4: reproduce evidence");
    assert!(prompt.contains("catch_unwind / panic hook"), "missing rule 5: no defensive code");
}
```

**测试 2** `build_subagent_system_prompt_skips_sop_for_simple_task`(现 7645 行)整体替换为:

```rust
/// §4.6 验收:Simple 复杂度时 system_prompt 不含诊断 SOP(避免污染简单任务);且为纯静态
#[test]
fn build_subagent_system_prompt_skips_sop_for_simple_task() {
    let prompt = build_subagent_system_prompt(crate::multi_agent::TaskComplexity::Simple);
    assert!(!prompt.contains("# Subagent:"), "unique header must move to user message");
    assert!(!prompt.contains("## 诊断任务执行规范"), "Simple task should NOT have SOP");
    assert!(!prompt.contains("CLAW_DIAG=1"), "Simple task should NOT contain diag rule");
}
```

**测试 3** `build_subagent_system_prompt_injects_architectural_sop`(现 7671 行)整体替换为:

```rust
/// §4.6 v2 验收:Architectural 复杂度注入架构决策 SOP(非诊断 SOP);且为纯静态
#[test]
fn build_subagent_system_prompt_injects_architectural_sop() {
    let prompt = build_subagent_system_prompt(crate::multi_agent::TaskComplexity::Architectural);
    assert!(!prompt.contains("# Subagent:"), "unique header must move to user message");
    // 架构决策 SOP 六条规则
    assert!(prompt.contains("## 架构决策执行规范"), "missing architectural SOP header");
    assert!(prompt.contains("候选方案"), "missing rule 1: alternatives required");
    assert!(prompt.contains("trade-off"), "missing rule 2: trade-off evaluation");
    assert!(prompt.contains("rationale"), "missing rule 3: rationale for rejecting alternatives");
    assert!(prompt.contains("向后兼容"), "missing rule 4: backward compatibility impact");
    assert!(prompt.contains("NOTEBOOK.md"), "missing rule 5: decisions persistence");
    assert!(prompt.contains("禁止凭直觉"), "missing rule 6: no intuition-based decisions");
    assert!(!prompt.contains("## 诊断任务执行规范"), "Architectural should NOT have diagnostic SOP");
}
```

- [ ] **Step 2: 运行验证编译失败**

Run: `cd rust && cargo test -p runtime build_subagent 2>&1 | grep -E "error|FAILED" | head`
Expected: FAIL — `cannot find function build_subagent_system_prompt`(模块级函数尚未存在)

- [ ] **Step 3: 静态化 `build_subagent_system_prompt`(从 impl 块移至模块级自由函数)**

将 `impl<C, T> ConversationRuntime<C, T>` 块内的 `build_subagent_system_prompt`(现 3362 行,含 doc 注释与 3 变体 match)整体移出 impl 块,改为模块级私有自由函数(建议放在 impl 块之后、`execute_subagent_llm` 之前),新实现:

```rust
/// 构造子智能体 system_prompt — §4.6 诊断 SOP 注入。
///
/// 3a 静态化:不再接收 id/name/task — 唯一内容移入 user message(见
/// [`build_subagent_request`]),保证同一复杂度的所有子智能体请求共享
/// 同一前缀,命中 DeepSeek prefix cache。
///
/// - `complexity == Diagnostic`:追加诊断任务执行规范
/// - 其他复杂度:仅基础 prompt,不污染简单任务
fn build_subagent_system_prompt(complexity: crate::multi_agent::TaskComplexity) -> String {
    let base_prompt = "你是一个子智能体,由主智能体派发执行独立任务。\n\
             \n\
             ## 约束\n\
             - 你拥有独立的工作上下文,不共享主智能体的对话历史\n\
             - 你的响应将被写入文件,主智能体会后续读取\n\
             - 请提供完整、自包含的分析结果\n\
             - 不需要调用工具,直接给出你的分析和结论\n\
             \n\
             ## 输出格式\n\
             请直接输出你的分析结果,使用 Markdown 格式。包含:\n\
             1. 任务理解(简要复述)\n\
             2. 分析过程\n\
             3. 关键发现\n\
             4. 结论和建议";

    match complexity {
        crate::multi_agent::TaskComplexity::Diagnostic => format!(
            "{base_prompt}\n\n\
             ## 诊断任务执行规范\n\
             1. 遇到崩溃/闪退类问题,第一动作是写文件诊断日志(CLAW_DIAG=1 或调用 diag! 宏),\
             而非凭直觉堆砌防御代码\n\
             2. 先用可靠信号确认错误类型(panic vs Err vs 配置错误),再决定修复方向\n\
             3. 修改后必须运行 `cargo build` 验证编译通过\n\
             4. 声称修复后必须提供复现验证证据(重新运行原场景确认不崩溃)\n\
             5. 禁止在未验证根因的情况下堆砌 catch_unwind / panic hook 等防御性代码"
        ),
        crate::multi_agent::TaskComplexity::Architectural => format!(
            "{base_prompt}\n\n\
             ## 架构决策执行规范\n\
             1. 提出方案前必须列出至少 2 个候选方案(alternatives),\
             禁止只给出单一方案就拍板\n\
             2. 每个候选方案需评估 trade-off:优势 / 劣势 / 适用场景 / 风险\n\
             3. 推荐方案必须给出否决其他方案的理由(rationale),\
             而非仅陈述推荐方案的优势\n\
             4. 涉及向后兼容/迁移成本的决策,必须评估现有用户/代码的影响范围\n\
             5. 架构决策写入 NOTEBOOK.md `<decisions>` 段(context/decision/rationale/alternatives),\
             供后续 compaction 后回溯\n\
             6. 禁止凭直觉或习惯拍板:任何架构决策必须有可复现的论证依据\
             (benchmark / 代码引用 / 论文 / 既有项目实践)"
        ),
        crate::multi_agent::TaskComplexity::Simple => base_prompt.to_string(),
    }
}
```

**注意**:原 base_prompt 的 `# Subagent: {name} ({subagent_id})` 头与 `## 任务\n{task}` 段已删除;`Simple` 分支原返回 `base_prompt`(`&str`),因 match 各分支返回 `String`,需改为 `base_prompt.to_string()`(否则编译错误,`base_prompt` 是 `&str` 变量)。

- [ ] **Step 4: 新增 `build_subagent_request` 测试(红)**

在 3 个重写测试之后追加:

```rust
/// 3a:子智能体请求 — system 纯静态,id/name/task 移入 user message 且只出现一次
#[test]
fn build_subagent_request_moves_unique_fields_to_user_message() {
    let req = build_subagent_request(
        "s-1",
        "diag-agent",
        "定位 wizard 闪退",
        crate::multi_agent::TaskComplexity::Diagnostic,
    );
    let system = req.system_prompt.static_sections.join("\n");
    assert!(!system.contains("Subagent"), "id/name must not be in system");
    assert!(!system.contains("定位 wizard 闪退"), "task must not be in system");
    assert_eq!(req.request_kind, RequestKind::Subagent);
    assert_eq!(req.messages.len(), 1);
    let user_text = match &req.messages[0].blocks[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("user message must be text"),
    };
    assert!(user_text.contains("# Subagent: diag-agent (s-1)"), "id/name header in user");
    // task 只出现一次
    assert_eq!(user_text.matches("定位 wizard 闪退").count(), 1, "task must appear exactly once");
}

/// 3a:同一复杂度的不同子智能体共享完全相同的 system prompt(前缀缓存可命中)
#[test]
fn build_subagent_request_shared_prefix_for_same_complexity() {
    let a = build_subagent_request("s-1", "agent-a", "任务 A", crate::multi_agent::TaskComplexity::Diagnostic);
    let b = build_subagent_request("s-2", "agent-b", "任务 B", crate::multi_agent::TaskComplexity::Diagnostic);
    assert_eq!(a.system_prompt.static_sections, b.system_prompt.static_sections);
    // 三个复杂度变体互不相同(各自前缀)
    let simple = build_subagent_request("s-3", "agent-c", "任务 C", crate::multi_agent::TaskComplexity::Simple);
    assert_ne!(simple.system_prompt.static_sections, a.system_prompt.static_sections);
}
```

- [ ] **Step 5: 实现 `build_subagent_request` + 改造 `execute_subagent_llm`(绿)**

在 `build_subagent_system_prompt` 之后新增公共自由函数:

```rust
/// 构造子智能体完整请求 — 3a/3b DRY 公共入口。
///
/// `execute_subagent_llm` 与 `SubagentDispatcher::dispatch_impl` 共用,
/// 消除两份重复 prompt 构造:
/// - system prompt 纯静态(见 [`build_subagent_system_prompt`])
/// - id/name/task 移入 user message,单次出现
/// - `request_kind = Subagent`,经 cli 侧路由到独立缓存统计 session
pub(crate) fn build_subagent_request(
    subagent_id: &str,
    name: &str,
    task: &str,
    complexity: crate::multi_agent::TaskComplexity,
) -> ApiRequest {
    let system_prompt = build_subagent_system_prompt(complexity);
    let user_message = ConversationMessage {
        role: MessageRole::User,
        blocks: vec![ContentBlock::Text {
            text: format!("# Subagent: {name} ({subagent_id})\n\n请执行以下任务:\n\n{task}"),
        }],
        usage: None,
    };
    ApiRequest {
        system_prompt: SystemPromptSplit::from_sections(vec![system_prompt]),
        messages: vec![user_message],
        request_kind: RequestKind::Subagent,
    }
}
```

改造 `execute_subagent_llm`(现 3435 行):把「构造 system_prompt + user_message + ApiRequest」三块(原 `let system_prompt = Self::build_subagent_system_prompt(...)` 到 `let request = ApiRequest {...}`)整体替换为:

```rust
        // 3a:统一构造 — system 静态,id/name/task 进 user message(DRY)
        let request = build_subagent_request(subagent_id, name, &enhanced_task, complexity);
```

删除原 `let system_prompt`、`let subagent_system_prompt`、`let user_message`、`let request = ApiRequest {...}` 四段代码(gate 逻辑 `let gated = ...; let enhanced_task = ...;` 保留在 `execute_subagent_llm` 内)。

- [ ] **Step 6: 运行验证**

Run: `cd rust && cargo test -p runtime build_subagent 2>&1 | tail -15`
Expected: 5 tests PASS(3 重写 + 2 新增)

Run: `cd rust && cargo test -p runtime conversation:: 2>&1 | tail -5`
Expected: 全 PASS(无回归)

- [ ] **Step 7: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "refactor(runtime): 子智能体 system prompt 静态化 + build_subagent_request 公共构造(3a)"
```

---

## Task 3:SubagentDispatcher 改用公共函数(DRY)

**Files:**
- Modify: `rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs:56-108`(`dispatch_impl` 请求构造段)
- Modify: `rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs:6-12`(imports)

- [ ] **Step 1: 替换请求构造段**

`dispatch_impl`(现 56 行起)中,把从 `// 构造请求(与 run_subagent_turn 完全一致)` 到 `let request = ApiRequest {...};`(现约 66-108 行)整体替换为:

```rust
        // 3a:与 execute_subagent_llm 共享同一公共构造(DRY)。
        // DAG 路径无 complexity 概念,与现状一致走 Simple(无 SOP)。
        let request = crate::conversation::build_subagent_request(
            subagent_id,
            name,
            &enhanced_task,
            crate::multi_agent::TaskComplexity::Simple,
        );
```

- [ ] **Step 2: 清理 imports**

`subagent_dispatcher.rs` 顶部(现 6-12 行):

```rust
use crate::conversation::{ApiClient, ApiRequest};
use crate::prompt::SystemPromptSplit;
use crate::session::{ContentBlock, ConversationMessage, MessageRole};
```

改为:

```rust
use crate::conversation::ApiClient;
use crate::session::ContentBlock;
```

(删除 `ApiRequest`、`SystemPromptSplit`、`ConversationMessage`、`MessageRole` — `ContentBlock` 在结果解析处仍在使用(现 142 行);`build_subagent_request` 经 `crate::conversation::` 路径调用。)

- [ ] **Step 3: 编译验证**

Run: `cd rust && cargo check -p runtime 2>&1 | tail -10`
Expected: 无 error、无 unused import 警告(该 crate 无 `#![allow(unused_imports)]`)

- [ ] **Step 4: Commit**

```bash
git add rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs
git commit -m "refactor(runtime): SubagentDispatcher 复用 build_subagent_request,消除双份 prompt 构造(DRY)"
```

---

## Task 4:cli 双 detector 统计隔离(3b)

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/streaming.rs`
  - imports(现 22-29 行 `use runtime::{...}`)
  - struct 字段(现 186 行 `cache_break_detector` 附近)
  - `new()`(现 192 行起)
  - `record_cache_break`(现 275 行)
  - `stream()`(现 347 行起,`consume_stream` 调用在 386 行)
  - `stream_async()`(现 406 行起,`consume_stream` 调用在 453 行)
  - `consume_stream`(现 488 行)
  - 778/797 行 `record_cache_break` 调用

- [ ] **Step 1: imports + 字段 + 构造**

`use runtime::{ ... }`(现 22-29 行)中加入 `RequestKind`(字母序:`PermissionPolicy` 与 `RuntimeError` 之间):

```rust
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, ContentBlock, ConversationMessage, MessageRole,
    PermissionMode, PermissionPolicy, RequestKind, RuntimeError, SystemPromptSplit,
    TokenUsage,
};
```

struct 字段(现 186-189 行)改为:

```rust
    cache_break_detector: api::CacheBreakDetector,
    /// 子智能体请求的独立缓存统计 session(`subagent-{session_id}`)。
    ///
    /// 3b:子智能体 system prompt 唯一内容多(历史上曾注入 id/name/task),
    /// 若与主 agent 共用同一 detector,会踩脏主 session 的 `previous`
    /// 指纹,导致主 agent 的 break_reasons 被 "system prompt changed"
    /// 污染、本地命中率统计失真。独立 session 后两条曲线互不干扰。
    subagent_cache_break_detector: api::CacheBreakDetector,
```

`new()` 构造(现 334 行附近)改为:

```rust
            cache_break_detector: api::CacheBreakDetector::new(session_id),
            subagent_cache_break_detector: api::CacheBreakDetector::new(format!(
                "subagent-{session_id}"
            )),
```

- [ ] **Step 2: `detector_for` + `record_cache_break` 改造**

`record_cache_break`(现 275 行)整体替换为:

```rust
    /// 按请求来源选择缓存统计 detector:主 agent → 主 session;子智能体 → `subagent-{session}`。
    fn detector_for(&self, kind: RequestKind) -> &api::CacheBreakDetector {
        match kind {
            RequestKind::Main => &self.cache_break_detector,
            RequestKind::Subagent => &self.subagent_cache_break_detector,
        }
    }

    /// 记录 cache break 检测数据。
    ///
    /// 从 TokenUsage 还原 api::Usage,调用 detector 记录本次请求的 prompt 指纹
    /// 和 cache 命中情况。usage 为 None 时(流式未收到任何 usage 事件)跳过。
    /// 3b:按 `kind` 路由到主/子独立 detector,互不污染。
    fn record_cache_break(
        &self,
        kind: RequestKind,
        request: &MessageRequest,
        usage: Option<TokenUsage>,
    ) {
        let Some(usage) = usage else {
            return;
        };
        let api_usage = api::Usage {
            input_tokens: usage.input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            output_tokens: usage.output_tokens,
        };
        let _ = self.detector_for(kind).record_usage(request, &api_usage);
    }
```

- [ ] **Step 3: `consume_stream` 加参数 + 两入口传值 + 调用点更新**

`consume_stream` 签名(现 488 行)改为:

```rust
    async fn consume_stream(
        &self,
        request_kind: RequestKind,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
```

`stream()`(同步)中,在 `let mut split = request.system_prompt;` 之前(现约 352 行)插入:

```rust
        let request_kind = request.request_kind;
```

并更新其 `consume_stream` 调用(现 386 行):

```rust
            self.consume_stream(request_kind, &message_request, is_post_tool).await
```

`stream_async()` 中,在 `let mut split = request.system_prompt;` 之前(现约 416 行)插入同样一行 `let request_kind = request.request_kind;`,并更新其调用(现 453 行):

```rust
            self.consume_stream(request_kind, &message_request, is_post_tool).await
```

`consume_stream` 内部两处 `record_cache_break` 调用(现 778、797 行)改为:

```rust
            self.record_cache_break(request_kind, message_request, final_usage);
```

与

```rust
        self.record_cache_break(request_kind, message_request, Some(fallback_usage));
```

- [ ] **Step 4: 编译 + 现有测试验证**

Run: `cd rust && cargo check -p rusty-claude-cli 2>&1 | tail -10`
Expected: 无 error(streaming.rs 顶部有 `#![allow(dead_code, unused_imports, unused_variables)]`,新增字段不触发警告)

Run: `cd rust && cargo test --workspace 2>&1 | tail -15`
Expected: 全 PASS

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/streaming.rs
git commit -m "feat(cli): 子智能体请求路由到独立 subagent-* 缓存统计 session(3b)"
```

---

## Task 5:全量验证 + 端到端

**Files:** 无代码改动

- [ ] **Step 1: 全量测试 + clippy**

Run: `cd rust && cargo test --workspace 2>&1 | tail -15`
Expected: 全部 PASS

Run: `cd rust && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15`
Expected: 无 warning(exit 0)

- [ ] **Step 2: 静态性抽查(代码级)**

Run: `grep -n "Subagent:" rust/crates/runtime/src/conversation.rs | grep -v "build_subagent_request\|user message\|# Subagent: {name}"`
Expected: 无残留 system prompt 注入点;`# Subagent: {name}` 仅出现在 `build_subagent_request` 的 user message 格式串中。

- [ ] **Step 3: 端到端验证(需真实 DeepSeek API key)**

1. 启动 CLI,派发 2 个同复杂度(`Diagnostic`)子智能体(如 `dispatch_subagent` × 2);
2. 观察官方 usage:第 2 个请求的 `cache_read_input_tokens > 0`(命中静态前缀);
3. 运行 `claw doctor --cache-stats`,确认存在独立 session `subagent-{session_id}` 且其 `tracked_requests` 等于子智能体请求数;主 session 的 `tracked_requests` 不再包含子智能体请求。

- [ ] **Step 4: 文档收尾**

在 `docs/2026-08-05-subagent-prefix-cache-alignment-design.md` 末尾追加「实施记录」段,记录落地 commit 与端到端结果。

---

## 实现可行性评审(Self-Review)

### 代码事实核查表

| # | 声明 | 位置 | 验证 |
|---|---|---|---|
| 1 | `build_subagent_system_prompt(subagent_id, name, task, complexity)` 在 `impl<C,T> ConversationRuntime<C,T>` 块内(无 where 约束),system 以 `# Subagent: {name} ({subagent_id})` 开头并含 `## 任务` | conversation.rs:3362-3419,impl 块起于 592 | ✅ |
| 2 | `execute_subagent_llm` 无 self,user message 重复 task;`Self::build_subagent_system_prompt(...)` 调用 | conversation.rs:3435-3467 | ✅ |
| 3 | `SubagentDispatcher::dispatch_impl` 内联第二份 system prompt(无 SOP),同步 `stream` | subagent_dispatcher.rs:56-108 | ✅ |
| 4 | `ApiRequest { system_prompt, messages }` 无 Default derive,无 `..Default::default()` 构造点 | conversation.rs:237-241 | ✅ |
| 5 | 4 个 ApiRequest 构造点:1909(主)/ 3467(子)/ 5883(测试)/ dispatcher:98 | grep 全库 | ✅ |
| 6 | `CacheBreakDetector::new(session_id: impl Into<String>) -> Self` 不失败;`record_usage(&self, ...)` 内部 Mutex | api/src/cache_break_detection.rs:159/193 | ✅ |
| 7 | `record_cache_break(&self, request, usage)` 无条件写单一 detector;调用点 778/797 在 `consume_stream` 内 | streaming.rs:275/778/797 | ✅ |
| 8 | `stream()`(347)/`stream_async()`(406)都调 `consume_stream(&message_request, is_post_tool)`;`consume_stream` 是 async &self | streaming.rs:386/453/488 | ✅ |
| 9 | streaming.rs imports 走 `use runtime::{...}`;`RequestKind` 需经 lib.rs re-export(现 162-169 行列表) | streaming.rs:22-29 / lib.rs:162 | ✅ |
| 10 | dispatcher 剩余代码用 `ContentBlock::Text`(142)与 `crate::conversation::build_assistant_message`,不再需要 SystemPromptSplit/ConversationMessage/MessageRole | subagent_dispatcher.rs:95-181 | ✅ |
| 11 | `SystemPromptSplit::from_sections` 无 boundary 时全部进 static_sections(测试 `api_request_carries_system_prompt_split` 证实) | prompt.rs:52 / conversation.rs:5876 | ✅ |
| 12 | `TaskComplexity { Simple, Diagnostic, Architectural }` 在 `runtime/src/multi_agent/mod.rs:48`,conversation.rs 已通过 `crate::multi_agent::` 路径引用 | multi_agent/mod.rs:48 | ✅ |

### 逐项推演

**7. 签名兼容性**:`build_subagent_system_prompt(complexity) -> String` 同步自由函数;调用点 `execute_subagent_llm`(async 上下文,同步调用 OK)与测试(同步)均兼容。`build_subagent_request(...) -> ApiRequest` 同步;`execute_subagent_llm` 与 `dispatch_impl` 均为 async,同步调用 OK。`record_cache_break(&self, kind, ...)` 保持 `&self`(detector_for 返回 `&CacheBreakDetector`,`record_usage` 是 `&self`)。`consume_stream` 加 `request_kind` 参数,两入口同步更新。✅

**8. 参数来源**:complexity 来自 `execute_subagent_llm` 现有参数(由 `run_subagent_turn_with_model` 传入);dispatcher 无此参数 → 硬编码 `TaskComplexity::Simple`(与现状无 SOP 一致)。request_kind 提取自 `request`(Copy,在 `request.system_prompt` move 前取)。session_id 是 client 字段。✅

**9. 数据传递链**:`request_kind` 全程传递:ApiRequest 构造 → stream/stream_async 局部变量(提取) → `consume_stream` 参数 → `record_cache_break` 参数 → `detector_for` → 选 detector。无中间层丢失。✅

**10. 判定优先级**:detector 选择是 `match kind` 二选一,互斥枚举,无顺序问题。✅

**11. retry/重入**:`build_subagent_request` 纯函数无副作用。`CacheBreakDetector` 内部 Mutex + 幂等 record(每次独立请求哈希);subagent detector 在 `new()` 一次性构造,无重复创建。✅

**12. 冲突处理**:主/子请求交错时按 kind 精确路由,互不污染。`subagent-{session_id}` 命名空间与主 `<session_id>` 无冲突(极端情况下若用户 session 恰同名,仅 stats 合并,无害)。✅

**13. 与现有系统重叠**:CacheBreakDetector 机制未改,仅实例翻倍;doctor.rs 按 `~/.claude/cache/prompt-cache/<session>/` 目录枚举,自动识别新 session,无需改动。`with_model` 的 doc 注释("复用主 agent 的 session_id")语义变为「复用 session_id 前缀,但子请求走 subagent detector」,注释在 Task 4 Step 1 中随字段文档一并更新。✅

**14. 失败路径**:`CacheBreakDetector::new` 读 json 用 `unwrap_or_default`,不失败。API 请求失败时 `record_cache_break` 不调用(与现状一致,仅成功/fallback 路径记录)。子智能体构造失败 → 已有降级路径(回退主 client)不受影响。✅

**15. 构造点破坏扫描**:ApiRequest 共 4 个构造点,无 `..Default::default()` 用法,全部显式补 `request_kind` 字段(Task 1 Step 4 清单);测试构造点 5883 已列入。受影响文件:`conversation.rs`(3 处)+ `subagent_dispatcher.rs`(1 处),已在 Task 1 文件改动清单标注。✅

**16. 成本估算**:Task 1 ≈ 40 行(类型 + 字段 + 4 构造点 + re-export + 测试);Task 2 ≈ 130 行(函数静态化重写 + 公共函数 + execute_subagent_llm 改造 + 5 测试);Task 3 ≈ -30 行(删重复构造 + 清理 imports);Task 4 ≈ 30 行(字段/路由/传参);合计净增 ≈ 170 行,含完整测试。无隐藏 prompt 工程/边界 case。✅

### 风险登记

| 风险 | 等级 | 缓解 |
|---|---|---|
| 模型行为变化(唯一内容从 system 移到 user) | 低 | 语义等效(内容相同仅位置变化);Task 5 端到端抽查输出 |
| `Simple` 分支 `base_prompt` 类型(&str vs String) | 低 | Task 2 Step 3 已显式 `base_prompt.to_string()` |
| dispatcher unused import 触发 clippy 警告 | 低 | Task 3 Step 2 明确删除 4 个 import |
| cli 侧无 3b 单测(构造 client 需 API key) | 中 | 由 runtime 侧 `build_subagent_request` 的 `request_kind` 断言 + Task 5 端到端覆盖 |
