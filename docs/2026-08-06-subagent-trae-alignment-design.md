# 子智能体 TRAE 架构对齐与升级方案

> **持久化文档** — 本方案固化到文件以绕过会话上下文限制。所有改动点带 `文件:行号` 锚定，便于 subagent 独立执行时无需重新调研。

**Goal:** 对齐 TRAE 子智能体架构（能力分级 / 工具白名单 / 多轮 tool call / 上下文注入 / 无状态执行），并基于项目已有 DAG 调度 + validation gate + 知识新鲜度门控做升级优化。

**Architecture:** 引入 `SubagentCapability` 三级能力枚举（L0 Analyze / L1 ReadOnly / L2 Execute），按能力注入工具白名单与上下文前缀；将 `execute_subagent_llm` 单轮调用改造为多轮 tool call 循环；结果以结构化 handoff 协议落盘，主 agent 按需读取，避免污染主上下文。

**Tech Stack:** Rust, tokio, serde, 现有 `multi_agent/dag/` + `multi_agent/validation.rs` + `repomap.rs` + `prompt.rs::ProjectContext`

---

## 1. TRAE 架构反推映射

| TRAE 特征（反推） | 项目现状 | 差距 | 对应 Epic |
|---|---|---|---|
| 能力分级 L0/L1/L2 | 无 `SubagentCapability` 枚举 | 全新 | Epic 0 |
| 工具权限白名单 | `with_model` 硬编码 `enable_tools=false` ([streaming.rs:505](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L505)) | 需按能力启用 | Epic 2 |
| 多轮 tool call 循环 | `execute_subagent_llm` 单轮 ([conversation.rs:3461](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3461)) | 核心改造 | Epic 3 |
| 上下文注入（工具定义/repo_map/项目环境） | `build_subagent_system_prompt` 纯静态文本 ([conversation.rs:261](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L261)) | 需注入 | Epic 1 |
| 并行调度 | ✅ DAG scheduler 真并行 | 已有优势 | — |
| 无状态执行 | ✅ 结果写 `.claw/subagents/{id}.md` | 已有优势 | — |
| 独立缓存统计 | ✅ `subagent_cache_break_detector` ([streaming.rs:195](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L195)) | 已有优势 | — |
| validation gate | ✅ `multi_agent/validation.rs`（命令+LLM 双门） | 已有优势 | — |
| 禁止递归派发 | 未显式约束 | 需加 guard | Epic 3 |
| 禁止用户交互 | ✅ `TuiSilentPermissionPrompter` | 已有优势 | — |

## 2. 项目升级优化点（超越 TRAE 基线）

1. **知识新鲜度门控**（`knowledge_freshness::gate_task`，[conversation.rs:3471](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3471)）— TRAE 无此机制，项目可在 Novel 任务时自动注入调研摘要。
2. **模型升级重试**（[conversation.rs:3184](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3184)）— validation 失败后自动升级模型重试，TRAE 反推未见。
3. **FailFast::Off + 并发限流 ≤5**（[project_memory L2](file:///c:/Users/38225/.trae-cn/memory/projects/-d-claw-code-src--p2-f32cb2afddcf071d9071/project_memory.md)）— 单点失败不连锁，TRAE 反推未见显式策略。
4. **静态前缀分层缓存**（`SystemPromptSplit::static_cache_breakpoints`）— 项目已有 3 级断点，子智能体注入上下文时复用此机制可保命中率（当前 94-95%）。

## 3. 架构设计

### 3.1 SubagentCapability 枚举

```rust
// multi_agent/mod.rs 新增
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentCapability {
    /// L0 分析型：只读 + 推理，无副作用。用于调研、方案设计、代码审查。
    #[default]
    Analyze,
    /// L1 只读型：可调用只读工具（read/grep/glob/repomap），禁止写入。
    ReadOnly,
    /// L2 执行型：可调用写入工具（edit/write/bash），受白名单约束。
    Execute,
}

impl SubagentCapability {
    /// 返回该能力允许的工具名白名单（按 tools::GlobalToolRegistry 注册名）。
    pub fn allowed_tools(self) -> &'static [&'static str] {
        match self {
            Self::Analyze => &[],  // 纯 LLM 推理，不启用工具
            Self::ReadOnly => &["read", "grep", "glob", "repomap", "lsp_diagnostics"],
            Self::Execute => &[
                "read", "grep", "glob", "repomap", "lsp_diagnostics",
                "edit", "write", "bash",
                // 注:`dispatch_subagent` / `spawn_parallel_subagents` 不放入白名单,
                // 递归派发禁止由 §3.3.1 guard 在 tool_use 提取阶段显式检查实现
                // (见 execute_subagent_llm 内 `if tu.name == "dispatch_subagent" ...` 分支)。
            ],
        }
    }
    pub fn enables_tools(self) -> bool {
        !matches!(self, Self::Analyze)
    }
    pub fn max_iterations(self) -> usize {
        match self {
            Self::Analyze => 1,
            Self::ReadOnly => 5,
            Self::Execute => 10,
        }
    }
}
```

### 3.2 上下文注入策略（保缓存命中率）

`build_subagent_system_prompt` 改造为分层结构，静态前缀进 `SystemPromptSplit::static_sections`（命中缓存），动态部分进 `dynamic_sections`：

| 层 | 内容 | 缓存策略 | 来源 |
|---|---|---|---|
| L0 静态指令 | 角色约束 + 输出格式 + SOP（按 complexity） | `static` + breakpoint | 现有 [conversation.rs:261](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L261) |
| L1 静态环境 | repo_map 摘要（限 1K token，见 §8.3） + ProjectContext（cwd/git_status） | `static` + breakpoint | `repomap.rs::RepoMap::render` + `prompt.rs::ProjectContext` |
| L2 静态工具 | capability 白名单对应的工具签名摘要（name+description，不含完整 schema） | `static` + breakpoint | `tool_registry` 过滤 |
| L3 动态 | task 文本 + 知识新鲜度摘要 | `dynamic`（不缓存） | 现有 user message |

**关键**：L0-L2 进静态前缀，三层 breakpoint 让"换 task 不换环境"时 L0/L1 命中，"换 capability"时仅 L2 失效。

**⚠ heading 对齐约束**：`SystemPromptSplit::static_cache_breakpoints`（[prompt.rs:134](file:///d:/claw-code-src/rust/crates/runtime/src/prompt.rs#L134)）通过**固定 heading 识别 tier 边界**，而非按数组位置。子智能体注入时必须复用以下 heading 才能命中现有 breakpoint 逻辑，否则缓存分层退化：

| Tier | 必须使用的 heading | 来源 |
|---|---|---|
| L0 指令 | 无固定 heading（base prompt） | 现有 `build_subagent_system_prompt` |
| L1 环境 | `# Environment context` + `## Repository Map` | 与主 agent 一致，[prompt.rs:147-153](file:///d:/claw-code-src/rust/crates/runtime/src/prompt.rs#L147) |
| L2 工具 | 无固定 heading（工具签名摘要） | capability 白名单过滤 |

若 subagent 使用自定义 heading 注入 repo_map/ProjectContext，breakpoints 不生效，缓存命中率会从 94-95% 退化。

### 3.3 多轮 tool call 循环

**⚠ 双路径差异**：项目有两条子智能体执行路径，执行模型不同，多轮循环必须分别适配：

| 路径 | 入口 | 执行模型 | 流式 API | 线程上下文 |
|---|---|---|---|---|
| A（主 agent retry） | `execute_subagent_llm` ([conversation.rs:3461](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3461)) | **async** | `stream_async().await` | tokio runtime 内 |
| B（DAG 调度） | `dispatch_impl` ([subagent_dispatcher.rs:53](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L53)) | **async 外壳 + 同步线程内 LLM 调用** ¹ | `stream()`（内部 block_on） | `std::thread::spawn` 独立 OS 线程（[subagent_dispatcher.rs:85](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L85)） |

> ¹ `dispatch_impl` 本身是 `async fn`，但其多轮循环体在 `std::thread::spawn` 的**同步 OS 线程**内执行（`client.stream()` 内部 `block_on`），不能在循环体中 `.await`。路径 B 刻意用独立 OS 线程规避嵌套 runtime panic（[subagent_dispatcher.rs:74-82](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L74) 注释）。若 B 照搬 A 的 async 循环会触发 `Cannot start a runtime from within a runtime` panic。

#### 3.3.1 路径 A（async，`execute_subagent_llm`）

```rust
async fn execute_subagent_llm(
    ...,
    tool_executor: &mut (dyn ToolExecutor + Send),  // 见 Epic 2.5（统一带 Send，3a/3b 共用）
) -> Result<String, String> {
    let capability = request.capability;
    let max_iter = capability.max_iterations();
    let mut messages = vec![initial_user_message];
    let mut iterations = 0;
    let mut tools_used = Vec::new();
    let mut changed_files = Vec::new();

    loop {
        iterations += 1;
        if iterations > max_iter {
            // 截断：落盘 Truncated handoff（含已完成工具/文件），返回 Err 触发升级重试
            write_handoff(&subagent_id, capability, iterations, tools_used, changed_files, "", HandoffStatus::Truncated)?;
            return Err(format!("subagent exceeded max_iterations ({max_iter})"));
        }
        let request = build_subagent_request(capability, &messages, ...);
        let events = client.stream_async(request).await?;
        let (assistant_msg, _usage, _cache) = build_assistant_message(events)?;

        // 提取 tool_use
        let tool_uses: Vec<_> = assistant_msg.blocks.iter()
            .filter_map(|b| if let ContentBlock::ToolUse{..} = b { Some(b) } else { None })
            .collect();

        messages.push(assistant_msg);

        if tool_uses.is_empty() { break; }  // 终止条件 1：无 tool_use

        // 工具调用处理（guard + 执行 + 记录）— 抽公共函数，3a/3b 共用
        process_tool_uses(capability, &tool_uses, tool_executor, &mut messages,
                          &mut tools_used, &mut changed_files)?;
    }

    // 结构化 handoff 落盘
    write_handoff(&subagent_id, capability, iterations, tools_used, changed_files, final_text, HandoffStatus::Completed)?;
    Ok(format!(".claw/subagents/{subagent_id}.md"))
}

/// 工具调用处理公共函数（3a/3b 共用，消除重复）。
/// 路径 A 传 `&mut dyn ToolExecutor`，路径 B 传 `&mut Box<dyn ToolExecutor + Send>`，
/// 用泛型 `E: ToolExecutor + ?Sized` 统一签名。
fn process_tool_uses<E: ToolExecutor + ?Sized>(
    capability: SubagentCapability,
    tool_uses: &[ContentBlock],
    tool_executor: &mut E,
    messages: &mut Vec<ConversationMessage>,
    tools_used: &mut Vec<String>,
    changed_files: &mut Vec<String>,
) -> Result<(), ToolError> {
    for tu in tool_uses {
        let (name, input, id) = match tu { ContentBlock::ToolUse{ name, input, id } => (name, input, id), _ => continue };

        // 禁止递归派发 guard
        if name == "dispatch_subagent" || name == "spawn_parallel_subagents" {
            return Err(ToolError::new("subagent recursion forbidden"));
        }
        // 白名单 guard
        if !capability.allowed_tools().contains(&name.as_str()) {
            return Err(ToolError::new(format!("tool {name} not allowed for capability {capability:?}")));
        }

        tools_used.push(name.clone());
        let result = tool_executor.execute(name, input)?;
        // changed_files 提取（edit/write 可能修改多文件，遍历所有工具输入）
        if matches!(name.as_str(), "edit" | "write") {
            changed_files.extend(extract_paths(input));  // extract_paths 返回 Vec<PathBuf>，规范化后转 String
        }
        messages.push(ConversationMessage::tool_result(id.clone(), result));
    }
    Ok(())
}
```

#### 3.3.2 路径 B（同步线程，`dispatch_impl`）

路径 B 在独立 OS 线程内执行，**不能直接 `.await`**。循环体改为同步 `client.stream()` 调用（[streaming.rs:365](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L365) 已有同步 `stream()` 方法，内部 `self.runtime.block_on()`，见 [streaming.rs:409](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L409)；独立 OS 线程内 block_on 安全，不触发嵌套 runtime panic）：

```rust
async fn dispatch_impl(...) -> Result<String, String> {
    // ... 知识新鲜度门控（不变）...

    let api_client = api_client.clone();
    let tool_executor: Box<dyn ToolExecutor + Send> = ...;  // Epic 2.5 方案 A，move 进线程
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let mut client = api_client.lock()...;
            let mut tool_executor = tool_executor;  // Box<dyn ToolExecutor + Send>
            let capability = ...;  // 从 SpawnRequest 传入
            let max_iter = capability.max_iterations();
            let mut messages = vec![initial_user_message];
            let mut iterations = 0;
            let mut tools_used = Vec::new();
            let mut changed_files = Vec::new();

            loop {
                iterations += 1;
                if iterations > max_iter {
                    write_handoff(..., HandoffStatus::Truncated)?;
                    return Err(format!("exceeded max_iterations ({max_iter})"));
                }
                let request = build_subagent_request(capability, &messages, ...);
                // ⚠ 同步 stream()（streaming.rs:365），不是 stream_async().await
                let events = client.stream(request)?;
                let (assistant_msg, _, _) = build_assistant_message(events)?;

                let tool_uses: Vec<_> = ...;  // 同路径 A
                messages.push(assistant_msg);
                if tool_uses.is_empty() { break; }

                // 工具调用处理（与 3a 共用 process_tool_uses）
                process_tool_uses(capability, &tool_uses, &mut *tool_executor,
                                  &mut messages, &mut tools_used, &mut changed_files)?;
            }
            write_handoff(..., HandoffStatus::Completed)?;
            Ok(format!(".claw/subagents/{subagent_id}.md"))
        })();
        let _ = tx.send(result);
    });
    rx.await??
}
```

**关键差异总结**：路径 A 用 `stream_async().await`，路径 B 用同步 `stream()`；两条路径共享相同的 guard 逻辑和 handoff 落盘，建议抽出 `fn process_tool_uses(...)` 公共函数消除重复。

#### 3.3.3 多轮 `subagent_cache_break_detector` 策略

多轮循环中 system prompt **不变**（仅 `messages` 数组增长），前缀缓存理论命中。但需明确 detector 如何处理多轮 `PromptCacheEvent`，避免误报 break：

**⚠ 代码事实**：当前 `CacheBreakDetector::record_usage`（[cache_break_detection.rs:193](file:///d:/claw-code-src/rust/crates/api/src/cache_break_detection.rs#L193)）通过 `TrackedPromptState::from_usage`（[cache_break_detection.rs:241](file:///d:/claw-code-src/rust/crates/api/src/cache_break_detection.rs#L241)）构建状态，其中 `RequestFingerprints::from_request`（[cache_break_detection.rs:264](file:///d:/claw-code-src/rust/crates/api/src/cache_break_detection.rs#L264)）计算 `messages_hash`（[cache_break_detection.rs:269](file:///d:/claw-code-src/rust/crates/api/src/cache_break_detection.rs#L269)）。`detect_cache_break`（[cache_break_detection.rs:311](file:///d:/claw-code-src/rust/crates/api/src/cache_break_detection.rs#L311)）**会检查 `messages_hash` 变化并归入 break 原因**。多轮循环中 messages 增长 → `messages_hash` 变化 → 触发 "message payload changed" break → 误报 `unexpected`。

**必需改动**（列入 Epic 3a/3b 改动点）：

为 `CacheBreakDetector` 新增方法 `record_usage_multi_turn`，多轮循环专用：
```rust
impl CacheBreakDetector {
    /// 多轮循环专用：仅比对 system/tools/model hash，忽略 messages_hash 变化。
    /// 用于子智能体多轮 tool call 循环（system prompt 不变，messages 增长是预期）。
    pub fn record_usage_multi_turn(&self, request: &MessageRequest, usage: &Usage) -> CacheBreakRecord {
        let mut inner = self.lock();
        let previous = inner.previous.clone();
        let current = TrackedPromptState::from_usage(request, usage);

        // 仅当 system/tools/model hash 变化时才视为 break
        let cache_break = detect_cache_break_multi_turn(previous.as_ref(), &current, &inner.config);
        // ... 统计更新同 record_usage ...
        inner.previous = Some(current);
        persist_state(&inner);
        // ...
    }
}

fn detect_cache_break_multi_turn(
    previous: Option<&TrackedPromptState>,
    current: &TrackedPromptState,
    config: &CacheBreakConfig,
) -> Option<CacheBreakEvent> {
    let previous = previous?;
    let token_drop = previous.cache_read_input_tokens.saturating_sub(current.cache_read_input_tokens);
    if token_drop < config.cache_break_min_drop { return None; }

    let mut reasons = Vec::new();
    if previous.system_hash != current.system_hash { reasons.push("system prompt changed"); }
    if previous.tools_hash != current.tools_hash { reasons.push("tool definitions changed"); }
    if previous.model_hash != current.model_hash { reasons.push("model changed"); }
    // ⚠ 不检查 messages_hash — 多轮循环中 messages 增长是预期行为
    if reasons.is_empty() { return None; }
    // ... 构造 CacheBreakEvent ...
}
```

| 事件 | detector 行为 | 理由 |
|---|---|---|
| 第 1 轮 `stream_async`/`stream` | `record_usage_multi_turn` 记录基线 | 首轮建立 `previous` |
| 第 N 轮（N>1，system prompt 不变，messages 增长） | `record_usage_multi_turn` 忽略 `messages_hash` 变化，不触发 `unexpected=true` | 前缀缓存命中是预期行为 |
| 第 N 轮（system prompt 变化，如 capability 切换） | `system_hash` 变化 → 触发 `unexpected=true` | system prompt 变化导致缓存失效是真实 break |

**实现要点**：
- `subagent_cache_break_detector`（[streaming.rs:195](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L195)）独立于主 session（见 [streaming.rs:190-194](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L190) 注释），多轮循环调用 `record_usage_multi_turn` 而非 `record_usage`
- 每轮的 `Usage` 累加到子智能体总 usage
- 主 agent 的 `CacheBreakDetector` 仍用原 `record_usage`（主 agent 的 messages 增长确实是 break 信号）

**测试**：
- 多轮循环 system prompt 不变：3 轮后 `unexpected` 始终 `false`，总 `cache_read_input_tokens` = 3 轮之和
- 多轮循环中第 2 轮切换 capability（system prompt 变化）：第 2 轮 `unexpected=true`，token 下降
- 多轮循环中 messages 增长但 `messages_hash` 变化：`unexpected` 仍为 `false`（验证不检查 messages_hash）

### 3.4 结构化 handoff 协议

替代纯文本结果文件，引入 Markdown + YAML frontmatter：

```markdown
---
subagent_id: <id>
name: <name>
capability: execute
complexity: architectural
iterations: 3
tools_used: [read, edit, bash]
changed_files: [src/foo.rs, src/bar.rs]
status: completed
timestamp: <unix>
---

# Subagent Result: <name>

## Summary
<最终文本>

## Details
<完整输出>
```

主 agent 读取时先解析 frontmatter，`changed_files` 直接喂给 validation gate（[validation.rs:234 detect_changed_files](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/validation.rs#L234) 可复用），`summary`（含 `changed_files` 列表，见 §8.4）用于主上下文压缩时保留关键信息。

**summary 截断逻辑**：summary 不做硬截断（硬截断会导致语义断裂）。通过 system prompt 约束 LLM 输出格式：`"输出以 ## Summary 开头，不超过 500 字符，包含 changed_files 列表"`。LLM 生成后若超长，仅在 `serde_yaml` 序列化时截断到 500 字符并追加 `…`（省略号标记），确保 frontmatter 解析安全。

**changed_files 提取**：遍历所有 edit/write 工具调用输入（`process_tool_uses` 中 `extract_paths(input)` 返回 `Vec<PathBuf>`），**支持单次 edit 修改多文件**（如批量编辑场景）。提取后 `canonicalize` + `dunce::simplified` 规范化（与 Epic 4 路径规范化一致），去重后存入 frontmatter。

**frontmatter 转义方案**：`summary` / `details` 字段可能含 `:`、`---`、换行等 YAML 特殊字符，直接写入会破坏 frontmatter 解析。采用 **YAML 块标量**序列化：

- `summary`（单行，≤500 字符）：用 YAML 双引号字符串 + JSON 转义（`\"`、`\\`、`\n`），确保所有特殊字符安全
- `details`（多行）：用 YAML literal block scalar（`|`），保留换行原样

示例（summary 含 `:` 和 `---`）：
```yaml
---
subagent_id: abc123
name: fix-tool
capability: execute
iterations: 3
tools_used: [read, edit, bash]
changed_files: ["src/foo.rs", "src/bar.rs"]
status: completed
timestamp: 1786015370
summary: "修复了 edit 工具的路径问题：相对路径未 join workspace_root --- 已验证"
details: |
  完整分析过程...
  多行内容...
  含 --- 分隔符也不影响解析
---

# Subagent Result: fix-tool
```

序列化时用 `serde_yaml`（而非手拼字符串），`summary` 标注 `#[serde(serialize_with = "...")]` 强制双引号风格，`details` 用 `serde_yaml::to_string` 自动选择块标量。反序列化时 `serde_yaml` 自动处理转义还原。

## 4. Epic 分解（低风险 → 高风险）

### Epic 0: SubagentCapability 枚举与 SpawnRequest 扩展（低风险，纯类型）

**目标**：引入能力分级类型，扩展派发请求字段，保持向后兼容。

**改动点**：
- 新增 `SubagentCapability` 枚举 → `multi_agent/mod.rs`（在 `TaskComplexity` 定义后，约 [mod.rs:48](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L48) 附近）
- `SpawnRequest` 增加字段 `capability: SubagentCapability`（默认 `Analyze`）→ [multi_agent/mod.rs:187-198](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L187)
- `Subagent` 结构同步增加 `capability` 字段（同文件查找 `struct Subagent`）
- `parse_spawn_parallel_input` 解析新增 `capability` 字段（[conversation.rs:3632](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3632)），缺失时默认 `Analyze`
- `dispatch_subagent` 工具 JSON schema 增加 `capability` 枚举字段

**测试**：
- `SubagentCapability::allowed_tools()` 三个变体返回值正确
- `SpawnRequest` 反序列化：缺 `capability` 字段时默认 `Analyze`（向后兼容）
- `parse_spawn_parallel_input` 解析含/缺 `capability` 的 JSON

**风险**：低。纯类型新增，默认值保证现有调用方零改动。

**验收**：`cargo test -p runtime multi_agent` 通过，`cargo clippy` 无警告。

---

### Epic 1: 上下文注入到 build_subagent_system_prompt（低风险，纯字符串）

**目标**：注入 repo_map + ProjectContext + 工具签名摘要到子智能体 system prompt，提升能力与缓存命中率。

**改动点**：
- `build_subagent_system_prompt` 签名扩展为 `build_subagent_system_prompt(complexity, capability, ctx: &SubagentContext)` → [conversation.rs:261](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L261)
- 新增 `SubagentContext` 结构（同文件或 `multi_agent/mod.rs`）。**采用 owned 字段而非 `&'a` 引用**，原因：`execute_subagent_llm` 是 async，持有引用跨 `.await` 点会引入严苛生命周期约束；repo_map 由 `RepoMap::render()` 生成 owned `String`，调用方持有所有权后传值（非引用）给 `SubagentContext`，避免 borrow 跨 await：
  ```rust
  pub struct SubagentContext {
      pub repo_map: Option<String>,              // 限 1K token 的摘要(owned,见 §8.3)
      pub project_context: Option<ProjectContext>, // owned(实现了 Clone)
      pub tool_summaries: Vec<ToolSummary>,       // capability 白名单对应(owned)
  }
  pub struct ToolSummary { pub name: String, pub description: String }
  ```
  - `SubagentContext` 本身 `Clone`（字段均 Clone），调用方构造后 `clone()` 传入 `build_subagent_*`，或传 `&SubagentContext`（函数签名用引用，生命周期仅限同步构造阶段，不跨 `.await`）
  - repo_map 所有权归属：`ConversationRuntime` 持有 `RepoMap` 实例（已有 `cache/cache_time` 字段做新鲜度缓存），`execute_subagent_llm` 调用前 `render()` 取 owned `String` 存入 `SubagentContext`，避免在 async 循环中持有 `&mut RepoMap`
- `build_subagent_request` 签名扩展，接收 `&SubagentContext` → [conversation.rs:319](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L319)
- 返回 `SystemPromptSplit::from_sections` 改为多层 static + dynamic（[conversation.rs:334](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L334)）
- 调用方 `execute_subagent_llm` ([conversation.rs:3475](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3475)) 与 `subagent_dispatcher.rs::dispatch_impl` ([subagent_dispatcher.rs:53](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L53)) 构造 `SubagentContext` 传入
- repo_map 生成：复用 `ConversationRuntime` 持有的 `RepoMap` 实例（默认 1K token，见 §8.3），调用 `render()` 取 owned `String`（[repomap.rs:72](file:///d:/claw-code-src/rust/crates/runtime/src/repomap.rs#L72)）。**不独立 `RepoMap::new`**，避免重复扫描。`cache_time` 控制新鲜度（已有字段），子智能体调用复用同一实例
- **heading 对齐**（§3.2 约束）：注入 repo_map 时 section heading 必须为 `## Repository Map`，ProjectContext 为 `# Environment context`，与 `static_cache_breakpoints`（[prompt.rs:147-153](file:///d:/claw-code-src/rust/crates/runtime/src/prompt.rs#L147)）识别逻辑一致。使用自定义 heading 会导致 breakpoint 失效、缓存分层退化。
- **知识新鲜度门控融入**：子智能体构造 `SubagentContext` 时，若任务为 Novel 类型，应调用 `knowledge_freshness::gate_task`（[conversation.rs:3471](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3471) 已有调用）获取摘要并注入动态层（L3，与主 agent 一致）。当前 `execute_subagent_llm` 已在 task 文本中注入 `gate_task` 摘要（[conversation.rs:3472](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3472) `enhanced_task`），无需额外改动——确认 Epic 1 不破坏此路径即可

**测试**：
- 快照测试：三个 capability 的 system prompt 结构（static/dynamic 分层数量、breakpoint 位置）
- **breakpoint 单元测试**：`SystemPromptSplit::from_sections` 返回的 split 中，`# Environment context` 和 `## Repository Map` heading 被 `static_cache_breakpoints`（[prompt.rs:147](file:///d:/claw-code-src/rust/crates/runtime/src/prompt.rs#L147)）正确识别为 breakpoint，断言 breakpoint 数量与期望一致（回归测试，防止 heading 拼写错误导致缓存分层退化）
- repo_map 超 1K token 时正确截断（复用主 agent `RepoMap` 实例，`DEFAULT_MAX_TOKENS=1024`）
- `Analyze` capability（无工具）不注入工具签名层

**风险**：中低。需确保静态前缀顺序稳定（否则缓存失效）。repo_map 生成有性能开销，需缓存（`RepoMap` 已有 `cache/cache_time` 字段）。

**验收**：`cargo test -p runtime build_subagent` + breakpoint 单元测试通过。缓存命中率回归：CI 中增加 breakpoint 数量断言（模拟 `SystemPromptSplit` 并断言 breakpoint 数量与期望一致），防止 heading 拼写错误导致缓存分层退化。手动验证用 `claw doctor --cache-stats` 对比改动前后命中率。

---

### Epic 2: with_model 工具白名单启用（中风险，API 请求构造）

**目标**：按 capability 启用工具并设置白名单，替代硬编码 `enable_tools=false`。

**改动点**：
- `ApiClient` trait 新增方法 `with_model_and_capability`（默认委托 `with_model`）→ [conversation.rs:417](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L417)
  ```rust
  fn with_model_and_capability(
      &self, model: &str, capability: SubagentCapability,
  ) -> Result<Box<dyn ApiClient>, String> {
      let _ = capability;
      self.with_model(model)
  }
  ```
- `AnthropicRuntimeClient` 重写 `with_model_and_capability` → [streaming.rs:501](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L501)：
  ```rust
  fn with_model_and_capability(&self, model: &str, capability: SubagentCapability) -> Result<Box<dyn ApiClient>, String> {
      let allowed: AllowedToolSet = capability.allowed_tools().iter().map(|s| s.to_string()).collect();
      let client = AnthropicRuntimeClient::new(
          &self.session_id, model.to_string(),
          capability.enables_tools(),  // false for Analyze, true for ReadOnly/Execute
          false, // emit_output
          Some(allowed),
          self.tool_registry.clone(),
          None,
      )?;
      Ok(Box::new(client))
  }
  ```
- `execute_dispatch_subagent_async` 调用点改用 `with_model_and_capability` → [conversation.rs:3184 附近](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3184)
- `subagent_dispatcher.rs` 同步改造（[dispatch_impl](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L53)）

**测试**：
- mock `ApiClient`：`Analyze` 请求体 `tools` 字段为空/absent
- `ReadOnly` 请求体 `tools` 仅含白名单 5 个
- `Execute` 请求体 `tools` 含白名单全集
- `AllowedToolSet` 过滤逻辑（[streaming.rs:470 filter_tool_specs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L470)）

**风险**：中。`AnthropicRuntimeClient::new` 当前是 `pub(crate)`，跨 crate 调用需确认可见性。`AllowedToolSet` 类型来自 `rusty-claude-cli`，runtime crate 不能直接依赖（已有 `with_model` 跨 crate 模式可参考）。

**验收**：`cargo test -p rusty-claude-cli streaming`，手动验证子智能体请求体（启用 `RUST_LOG=debug` 看 tools 字段）。

---

### Epic 2.5: ToolExecutor Send 审计与跨线程注入

**目标**：解决 `ToolExecutor` trait 无 `Send` 约束的问题，使多轮 tool call 循环能在路径 A（async）和路径 B（独立 OS 线程）中安全持有工具执行器。

**背景**：当前 [conversation.rs:423](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L423) 定义：
```rust
pub trait ToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;
}
```
无 `Send` supertrait。路径 B 用 `std::thread::spawn`（[subagent_dispatcher.rs:85](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L85)），闭包需 `Send`，而 `&mut dyn ToolExecutor`（非 Send）无法跨线程传递。

#### 2.5.1 审计结论（已完成）

全代码库 `impl ToolExecutor for` 共 3 处，字段 Send 性清单：

| 实现 | 位置 | 字段 | Send 性 | 用途 |
|---|---|---|---|---|
| `SubagentToolExecutor` | [tools/lib.rs:6312](file:///d:/claw-code-src/rust/crates/tools/src/lib.rs#L6312) | `BTreeSet<String>` + `Option<PermissionEnforcer>` | ✅ Send | 生产(子智能体工具) |
| `CliToolExecutor` | [tool_display.rs:56](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tool_display.rs#L56) | `TerminalRenderer`(纯数据:SyntaxSet/Theme/ColorTheme) + `Option<StatusEmitter>`(`Arc<dyn Fn + Send + Sync>`) + `Option<Arc<Mutex<…>>>` | ✅ Send | 生产(CLI 主 executor) |
| `StaticToolExecutor` | [conversation.rs:4777](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L4777) | `BTreeMap<String, Box<dyn FnMut>>`(handler 无 Send bound) | ❌ 不 Send | **仅测试用** |

辅助字段确认：
- `PermissionEnforcer`（[permission_enforcer.rs:27](file:///d:/claw-code-src/rust/crates/runtime/src/permission_enforcer.rs#L27)）= `PermissionPolicy`（[permissions.rs:99](file:///d:/claw-code-src/rust/crates/runtime/src/permissions.rs#L99)：`PermissionMode` 枚举 + `BTreeMap` + `Vec<PermissionRule>`，全部 Send）→ `PermissionEnforcer` Send ✅
- `StatusEmitter` = `Arc<dyn Fn(StatusEvent) + Send + Sync>`（[streaming.rs:44](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L44)）→ Send ✅
- `TerminalRenderer`（[render.rs:221](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/render.rs#L221)）= `SyntaxSet` + `Theme` + `ColorTheme`（均为纯数据结构，无 `Rc`/裸指针）→ Send ✅

**结论**：两个生产实现均已满足 Send，仅测试用 `StaticToolExecutor` 不满足。

#### 2.5.2 选定方案：方案 A（加 `Send` supertrait）

基于审计结论，方案 A 成本最低：

```rust
pub trait ToolExecutor: Send {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;
}
```
- `&mut dyn ToolExecutor` 自动满足 `Send`，路径 A/B 均可跨线程传递
- 生产实现零改动（已满足 Send）
- 仅需修改 `StaticToolExecutor`：handler 类型从 `Box<dyn FnMut(&str) -> Result<String, ToolError> + 'static>` 改为 `Box<dyn FnMut(&str) -> Result<String, ToolError> + Send + 'static>`，并同步修改 `ToolHandler` type alias 和 `register` 签名

**方案 B（通道模式）保留为 fallback**：若未来引入含非 Send 资源（如 `Rc`、裸文件句柄）的 ToolExecutor 实现，再启用 `RemoteToolExecutor` 通道包装。当前无需引入。

**改动点**：
- 修改 `ToolExecutor` trait 定义加 `Send` supertrait → [conversation.rs:423](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L423)
- 修改 `StaticToolExecutor` 的 `ToolHandler` type alias（[conversation.rs:4773](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L4773)）加 `+ Send`
- 路径 A（async）：`execute_subagent_llm` 签名增加 `tool_executor: &mut (dyn ToolExecutor + Send)`
- 路径 B（同步线程）：`dispatch_impl` 的 `std::thread::spawn` 闭包通过 move 语义持有 `Box<dyn ToolExecutor + Send>`（或 `&mut` 经由 channel 传递，见 §3.3.2）

**测试**：
- 编译通过（验证 Send 约束在所有实现处满足）
- `StaticToolExecutor` 测试用例的 handler 闭包需捕获 Send 资源（如 `String` 而非 `Rc`），修复后现有测试不退化
- 跨线程：路径 B 闭包编译通过（`Send` 约束满足）

**风险**：低。审计已确认生产实现满足 Send，唯一改动是测试用 `StaticToolExecutor` 的 handler 签名（`+ Send`），影响面限于测试代码。

**验收**：`cargo build` 全 crate 编译通过 + `cargo test -p runtime tool_executor` 不退化。

#### 2.5.3 与 Epic 3 的阻塞关系

**决策**：Epic 2.5 硬阻塞 Epic 3a 和 3b。采用"统一带 Send"方案，3a/3b 共用 `dyn ToolExecutor + Send` trait，简化泛型签名。

| 路径 | 是否被 Epic 2.5 阻塞 | 说明 |
|---|---|---|
| Epic 3a（async，`execute_subagent_llm`） | **硬阻塞** | 签名 `tool_executor: &mut (dyn ToolExecutor + Send)`，统一带 Send，与 3b 共用 trait 和 `process_tool_uses` 泛型签名 |
| Epic 3b（同步线程，`dispatch_impl`） | **硬阻塞** | `std::thread::spawn` 闭包需 `Send`，`Box<dyn ToolExecutor + Send>` move 进闭包 |

**理由**：Epic 2.5 改动极小（仅加 supertrait + 改 `StaticToolExecutor` 测试 handler 签名），先完成后 3a/3b 统一用带 Send 的 trait，避免条件阻塞的复杂排期。3a 即使顺序执行工具，带 Send 也不增加成本（生产实现已满足 Send）。

---

### Epic 3: 多轮 tool call 循环（高风险，核心执行路径）

**目标**：`execute_subagent_llm` 单轮 → 多轮，支持工具调用与结果回填。

**⚠ 前置依赖**：Epic 2.5（ToolExecutor Send 审计）必须先完成，否则路径 B 无法编译。

**拆分为 3a/3b**，因两条执行路径模型不同（见 §3.3 双路径差异表）：

#### Epic 3a：路径 A — async 多轮循环（`execute_subagent_llm`）

**改动点**：
- `execute_subagent_llm` 重构为 async 循环（[conversation.rs:3461-3530](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3461)），见 §3.3.1 伪代码
- `ToolExecutor` 注入：签名增加 `tool_executor: &mut (dyn ToolExecutor + Send)` 参数。**统一带 Send**（与 3b 共用 trait，简化泛型签名），依赖 Epic 2.5 先行完成
- 新增 `process_tool_uses` 公共函数（见 §3.3.1 签名），3a/3b 共用，消除重复
- 递归派发 guard / 白名单 guard / max_iterations 截断（见 §3.3.1）
- max_iterations 截断语义：返回 Err + 落盘 `Truncated` handoff（见 §8.1）
- `execute_dispatch_subagent_async` 传递主 runtime 的 `ToolExecutor`（[conversation.rs:2957](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L2957)）
- **`subagent_cache_break_detector` 适配**：多轮循环中调用 `record_usage_multi_turn`（见 §3.3.3），忽略 `messages_hash` 变化。需在 `api` crate 的 `CacheBreakDetector` 新增此方法

**测试**：
- 单轮无工具（Analyze）：行为与改造前一致
- 单轮带工具（ReadOnly 调 read）：1 轮 tool_use + 1 轮收尾
- 多轮（Execute 调 read→edit→bash）：3 轮 tool_use
- max_iterations 截断：ReadOnly 任务调 6 次工具，第 6 次后返回 Err + Truncated handoff 落盘
- 递归派发 guard：tool_use 为 `dispatch_subagent` 时立即 Err
- 白名单 guard：ReadOnly 调 `edit` 时立即 Err
- **多轮 cache detector**：3 轮循环 system prompt 不变，`unexpected` 始终 `false`（验证 `record_usage_multi_turn` 不检查 `messages_hash`）

#### Epic 3b：路径 B — 同步线程多轮循环（`dispatch_impl`）

**改动点**：
- `dispatch_impl` 重构为同步循环（[subagent_dispatcher.rs:53-140](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L53)），见 §3.3.2 伪代码
- **关键**：用同步 `client.stream()`（[streaming.rs:365](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L365)）而非 `stream_async().await`（独立 OS 线程内不能 await，否则嵌套 runtime panic）。`stream()` 内部 `self.runtime.block_on()`（[streaming.rs:409](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/streaming.rs#L409)），独立 OS 线程内安全
- `ToolExecutor` 注入：通过 Epic 2.5 选定的方案 A（`Send` supertrait），闭包 move `Box<dyn ToolExecutor + Send>` 进独立线程
- guard 逻辑与 3a 共享：调用 `process_tool_uses` 公共函数（见 §3.3.1 签名）
- `SpawnRequest` 的 `capability` 字段（Epic 0）传入 `dispatch_impl`
- handoff 落盘与 3a 共用 `write_handoff`
- **`subagent_cache_break_detector` 适配**：同 3a，多轮循环中调用 `record_usage_multi_turn`

**测试**：
- 同 3a 测试用例，但走 DAG 调度路径（`spawn_parallel_subagents` 触发）
- 嵌套 runtime 验证：确认无 `Cannot start a runtime from within a runtime` panic（通过 mock `client.stream()` 模拟多轮调用，验证多次 `block_on` 无副作用）
- 独立线程 `Box<dyn ToolExecutor + Send>` move：tool_executor 调用 round-trip

**风险（3a + 3b 共享）**：高。
1. `ToolExecutor` 跨线程传递 → Epic 2.5 已解决（方案 A，Send supertrait）
2. 工具执行副作用并发：多个子智能体并行调 `edit` 同一文件 → 冲突。Epic 4 解决
3. `subagent_cache_break_detector` 多轮适配 → §3.3.3 已给出 `record_usage_multi_turn` 方案，需在 `api` crate 实现
4. 路径 B 的 `client.stream()` 在 `std::thread::spawn` 内部仍调 `block_on`，多轮循环中多次 `block_on` 是否有副作用？需验证（现有单轮已验证安全，多轮理论无差异但需实测）

**验收**：`cargo test -p runtime execute_subagent_llm` + `cargo test -p runtime dispatch_impl`，集成测试：派发 ReadOnly 任务调 read 工具读取真实文件（两条路径各测一次）。

---

### Epic 4: 文件操作权限隔离（中风险，并发安全）

**目标**：基于 capability 限制文件写入，防止并行子智能体冲突。

**改动点**：
- 新增 `SubagentFileGuard`（`multi_agent/file_guard.rs`）：
  ```rust
  pub struct SubagentFileGuard {
      capability: SubagentCapability,
      workspace_root: PathBuf,
      locks: Arc<DashMap<PathBuf, Arc<Mutex<()>>>>,  // 每文件独立锁，DashMap 避免全局竞争
  }
  impl SubagentFileGuard {
      pub fn try_acquire(&self, path: &Path, write: bool) -> Result<LockHandle, String>;
  }
  ```
  - **锁粒度与并发安全**：用 `DashMap<PathBuf, Arc<Mutex<()>>>` 维护每文件独立锁，避免全局 `Mutex<HashSet>` 的竞争瓶颈（DashMap 分片锁，读写不互斥）
  - **超时等待实现**：路径 B 是同步 OS 线程，不能用 `tokio::time::timeout`。用 `Condvar` + `Mutex` 配合超时等待，避免忙等：
    ```rust
    let (lock, cvar) = self.locks.entry(path.clone())
        .or_insert_with(|| Arc::new((Mutex::new(()), Condvar::new())))
        .clone();
    let result = lock.lock().unwrap();
    let waited = cvar.wait_timeout(result, Duration::from_secs(timeout_secs))?;
    if waited.1.timed_out() {
        return Err(format!("file lock timeout: {}", path.display()));
    }
    ```
- `edit` / `write` 工具执行前调用 `try_acquire(path, write=true)`：
  - `Analyze` / `ReadOnly`：直接拒绝（capability 白名单已排除 edit/write，此为二次防护）
  - `Execute`：写入前获取锁，已锁定则等待（30s 超时后 Err，见 §8.2）
- `ToolExecutor` 包装层：`GuardedToolExecutor` 装饰主 executor，执行前检查权限
- 全局锁注册表：进程级 `OnceLock<Arc<DashMap<...>>>`，跨子智能体共享
- 超时时长：环境变量 `CLAW_SUBAGENT_FILE_LOCK_TIMEOUT`（默认 30s，见 §8.2）
- **路径规范化**：`edit` 的 `file_path` 可能是相对路径，用 `workspace_root.join` + `std::fs::canonicalize` 规范化后比较。`canonicalize` 可能因符号链接或权限失败，**优雅降级**：失败时用 `dunce::simplified(&workspace_root.join(path))` 去除 Windows `\\?\` 前缀作为 fallback key

**测试**：
- `Analyze` 调 `edit` → 拒绝
- `ReadOnly` 调 `edit` → 拒绝
- `Execute` 调 `edit` 未锁定文件 → 成功
- 两个 `Execute` 子智能体并发调 `edit` 同一文件 → 第二个等待，30s 超时后 Err
- 锁释放：`LockHandle` drop 后 `cvar.notify_one()`，文件可被其他子智能体获取
- 路径规范化：相对路径 `src/foo.rs` 和绝对路径 `/workspace/src/foo.rs` 规范化后命中同一锁

**风险**：中。
1. 死锁：子智能体 A 持有文件 X 锁等待文件 Y，子智能体 B 持有 Y 等待 X。缓解：**仅实现 30s 超时释放，不实现循环等待检测**（Wait-for 图 + 拓扑判定复杂度高，单进程内文件锁场景死锁概率低，超时已足够兜底）。超时后 `try_acquire` 返回 `Err`，子智能体将该工具调用记为失败并继续下一轮（或触发 max_iterations 截断）。
2. 性能：DashMap 分片锁缓解竞争，每文件独立锁避免互斥。
3. 路径规范化：`canonicalize` 失败时用 `dunce::simplified` 降级，需测试符号链接和权限场景。

**验收**：`cargo test -p runtime file_guard`，集成测试：2 个 Execute 子智能体并行修改不同文件成功，修改同一文件时第二个等待后超时失败。

---

### Epic 5: 结构化 handoff 协议（低风险，纯数据结构）

**目标**：替代纯文本结果文件，引入结构化 handoff 便于主 agent 解析与上下文压缩。

**改动点**：
- 新增 `SubagentHandoff` 结构（`multi_agent/handoff.rs`）：
  ```rust
  #[derive(Serialize, Deserialize)]
  pub struct SubagentHandoff {
      pub subagent_id: String,
      pub name: String,
      pub capability: SubagentCapability,
      pub complexity: TaskComplexity,
      pub iterations: usize,
      pub tools_used: Vec<String>,
      pub changed_files: Vec<String>,
      pub status: HandoffStatus,  // Completed/Failed/Truncated
      pub timestamp: u64,
      pub summary: String,   // <= 500 字符
      pub details: String,   // 完整输出
  }
  ```
- 落盘格式：YAML frontmatter + Markdown body（见 §3.4）
- `execute_subagent_llm` 末尾构造 `SubagentHandoff` 并写盘（替代 [conversation.rs:3507-3525](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3507)）
- 主 agent tool result 解析：`dispatch_subagent` 返回结果时，主 agent 读取 `.claw/subagents/{id}.md`，解析 frontmatter，`summary`（含 `changed_files` 列表）进主上下文，`details` 按需 Read（见 §8.4）
- validation gate 补充子智能体变更集：当前 [validation.rs:234 detect_changed_files](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/validation.rs#L234) 跑全局 `git diff --name-only HEAD`（含主 agent 的所有变更），而 `handoff.changed_files` 仅记录子智能体的。两者语义不同，**不能直接替换**。改为在 `ValidationContext` 新增 `subagent_changed_files: Vec<PathBuf>` 字段，`detect_changed_files` 仍保留全局检测，validation gate 同时检查两份列表（子智能体声称改了但 git diff 没有的 → 需排查；git diff 有但子智能体没声称的 → 主 agent 并发修改，不归此子智能体 validation）

**测试**：
- `SubagentHandoff` 序列化/反序列化 round-trip
- frontmatter 解析：含特殊字符（`:`、`---`、换行）的 summary/details 经 `serde_yaml` 序列化/反序列化后正确还原（见 §3.4 转义方案）
- summary 截断：LLM 输出超 500 字符时序列化截断 + `…` 标记，frontmatter 解析安全
- changed_files 多文件提取：单次 edit 修改多文件（如批量编辑）时，`extract_paths` 正确提取全部路径并去重
- 路径规范化：changed_files 中的相对路径经 `canonicalize` + `dunce::simplified` 规范化后与 git diff 比对一致
- 主 agent 解析 handoff：summary 提取正确（含 `changed_files`），details 按需 Read
- 向后兼容：旧格式纯文本文件能被降级解析（无 frontmatter 时整体作为 details）

**风险**：低。纯数据结构，向后兼容降级解析保证旧结果文件可读。

**验收**：`cargo test -p runtime handoff`，手动验证：派发子智能体后查看 `.claw/subagents/{id}.md` 含 frontmatter。

---

## 5. 9 项实施可行性清单

| # | 检查项 | 状态 | 说明 |
|---|---|---|---|
| 1 | 类型一致性（跨 Epic 枚举/结构体字段名） | ✅ | `SubagentCapability` 在 Epic 0 定义，Epic 1-5 复用；`allowed_tools()` / `enables_tools()` / `max_iterations()` 方法名统一 |
| 2 | 向后兼容（默认值保证现有调用零改动） | ✅ | `SubagentCapability` 默认 `Analyze`，`SpawnRequest.capability` 缺失时默认 `Analyze`，`with_model_and_capability` 默认委托 `with_model` |
| 3 | 跨 crate 可见性 | ⚠️ | `AnthropicRuntimeClient::new` 是 `pub(crate)`，runtime crate 不能直接调；Epic 2 通过 trait 方法 `with_model_and_capability` 跨 crate（已有 `with_model` 模式可参考） |
| 4 | Send/Sync 约束（多线程并行） | ✅(Epic 2.5) | Epic 2.5 审计已闭合：3 个 `ToolExecutor` 实现中 2 个生产实现已 Send，1 个测试用 `StaticToolExecutor` 需加 `+ Send`；选定方案 A（`Send` supertrait），风险低。**统一带 Send**：硬阻塞 Epic 3a/3b（见 §2.5.3），3a/3b 共用 `dyn ToolExecutor + Send` trait 和 `process_tool_uses` 泛型签名；Epic 4 `SubagentFileGuard` 用 `DashMap<PathBuf, Arc<Mutex<()>>>` |
| 5 | 缓存命中率不下降 | ✅ | Epic 1 静态前缀分层 + breakpoint，repo_map 进 L1 静态层；Epic 3 多轮循环中 system prompt 不变（仅 messages 增长），前缀缓存命中 |
| 6 | 并发安全（文件锁） | ✅ | Epic 4 全局锁注册表 + 每文件独立锁 + 超时释放 |
| 7 | 错误处理（不吞错） | ✅ | Epic 3 递归 guard / 白名单 guard / max_iterations 均返回 Err；Epic 4 锁获取失败返回 Err |
| 8 | 测试覆盖（每 Epic 有测试） | ✅ | 每 Epic 列出测试用例，Epic 3 含集成测试 |
| 9 | 回归风险（现有 94-95% 命中率不退化） | ⚠️ | Epic 1 注入上下文增大请求体，需对比 `claw doctor --cache-stats`；Epic 3 多轮循环改变请求模式，需监控命中率 |

## 6. 绕过上下文限制策略

本方案从两个层面绕过上下文限制：

### 6.1 方案设计层面（本会话）
- **文档持久化**：本文件固化设计，会话压缩不丢失关键信息
- **行号锚定**：所有改动点带 `文件:行号`，subagent 执行时无需重新调研
- **Epic 独立性**：每 Epic 独立可验证，可由独立 subagent 实现（fresh context），主上下文只做 review
- **代码事实锚定**：基于 search subagent 调研结果（非凭记忆），避免幻觉

### 6.2 运行时层面（子智能体执行）
- **结构化 handoff**（Epic 5）：主 agent 读取 summary 进上下文，details 按需读取，避免子智能体完整输出污染主上下文
- **独立缓存统计**（已有）：`subagent_cache_break_detector` 与主 session 互不污染
- **静态前缀缓存**（Epic 1）：repo_map/ProjectContext 进静态层，多轮循环中复用，避免重复注入
- **结果落盘**（已有）：子智能体结果写 `.claw/subagents/{id}.md`，主 agent 按需读取
- **max_iterations 截断**（Epic 3）：防止子智能体无限循环消耗上下文

## 7. 执行顺序建议

```
Epic 0 (类型基础) ──┬─→ Epic 1 (上下文注入) ──┐
                    ├─→ Epic 2 (工具白名单) ──┤
                    └─→ Epic 2.5 (Send 审计) ─┤
                                             ├─→ Epic 3a/3b (并行,统一带 Send) ──┬─→ Epic 4 (文件隔离)
                                             │                                    └─→ Epic 5 (handoff)
```

- **Epic 0** 先行（纯类型，无副作用，验证类型系统兼容性）
- **Epic 1 / Epic 2 / Epic 2.5** 并行（均仅依赖 Epic 0 的 `SubagentCapability` 类型，互不依赖）。Epic 2.5 改动极小，快速完成
- **Epic 3a / Epic 3b** 均依赖 0/1/2/**2.5**（统一带 Send），3a/3b 可并行（分别改 async 路径与同步路径，共用 `process_tool_uses` 公共函数）
- **Epic 4 / Epic 5** 并行（均依赖 3；Epic 5 纯数据结构不依赖文件锁）

**Pilot 建议**：先做 Epic 0 + Epic 1 的 `Analyze` 路径（无工具，零风险），验证静态前缀分层缓存命中率不下降。然后 Epic 2 + Epic 2.5 并行推进（2.5 快速完成），再 Epic 3a/3b 并行，最后 Epic 4/5。

## 8. 已决策点

### 8.1 max_iterations 截断语义:Err + Truncated handoff

**决策**：返回 Err + 落盘 `status: Truncated` 的 handoff（记录 `tools_used` / `changed_files`）。

**依据**：
- max_iterations 截断是执行超限，非正常完成。返回 Ok 会让 validation gate 验证一个明知不完整的结果，语义混乱；Err 明确表达"未正常完成"，让主 agent 决策更清晰
- **当前升级链已关闭**：[upgrade_lookup](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L1199) 全注释（V4-Flash 正式版上线后关闭），`upgrade_model_for_subagent` 对所有模型返回 None。当前 Err 路径走到 [conversation.rs:3250](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3250) 后走"已是旗舰，无法升级 — 立即失败"分支（[conversation.rs:3196-3204](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3196)）。**当前 Err = 立即失败，无自动重试**
- Truncated handoff 的当前价值：主 agent 收到失败消息后可 Read `.claw/subagents/{id}.md` 获取部分成果，决定是否重新派发（而非自动重试）

**⚠ 升级重开时的前置约束**：
- [reset_for_retry](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L487) 会 `agent.result = None`（[mod.rs:523](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L523)），retry loop（[conversation.rs:3136-3260](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3136)）是自动的，主 agent 不参与。**当前重试子智能体从全新状态开始，不会自动读取 Truncated handoff**
- 若未来重新启用升级链（取消 [mod.rs:1199-1213](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L1199) 注释），必须同时修改 `reset_for_retry` 或 retry loop，在重试的 task 文本中注入上次 Truncated handoff 的 `summary` + `tools_used` + `changed_files`，否则重试子智能体会重复执行已完成的工具调用
- **Epic 3 实施时**：max_iterations 截断的 Err 返回值应包含 Truncated handoff 路径（如 `"subagent exceeded max_iterations (10); partial result at .claw/subagents/{id}.md"`），便于主 agent 在失败消息中直接拿到路径

### 8.2 文件锁竞争策略:等待 + 30s 可配置超时 + 超时 Err

**决策**：Execute 子智能体间等待文件锁，30s 超时后返回 Err。超时时长通过环境变量 `CLAW_SUBAGENT_FILE_LOCK_TIMEOUT` 可配置（默认 30s）。

**依据**：
- **升级链关闭使拒绝=永久失败**：拒绝策略会导致任务失败率上升，与 project_memory L2"FailFast::Off 单点失败不连锁"理念冲突
- 路径 B 独立 OS 线程等待 30s 仅阻塞该线程，不影响主 runtime 或其他子智能体（各自独立线程）
- **30s 可配置**：Execute capability `max_iterations=10`（§3.1），若每轮含一次 edit，10 次 edit 可能超过 30s（大文件或慢磁盘）。长任务场景需调高超时
- **超时返回 Err 而非 Ok**：文件锁超时是冲突信号，主 agent 应知晓并决策（重新派发、改用串行、或人工介入）

**不选拒绝的理由**：升级关闭背景下拒绝=立即失败且无重试机会，代价过高。等待给第一个子智能体完成机会，大多数 edit 操作 <30s（单文件修改），等待可化解冲突。

### 8.3 repo_map token 上限:1K token，复用主 agent RepoMap 实例

**决策**：子智能体 repo_map 上限 1K token（与主 agent 一致），复用 `ConversationRuntime` 持有的 `RepoMap` 实例，不独立扫描。

**依据**：
- **主 agent repo_map 默认 1K token**：[repomap.rs:15](file:///d:/claw-code-src/rust/crates/runtime/src/repomap.rs#L15) `DEFAULT_MAX_TOKENS = 1024`，是主 agent 验证过的合理值
- **子智能体任务范围通常窄于主 agent**（单任务 vs 全局），1K token 足够；信息不足时通过 read/grep/glob 工具补充（ReadOnly/Execute capability 已启用，见 §3.1），这比增大 repo_map 更精准
- **复用 RepoMap 实例**：已有 `cache/cache_time` 字段做新鲜度缓存（[repomap.rs:72](file:///d:/claw-code-src/rust/crates/runtime/src/repomap.rs#L72) `render()` 调 `refresh_cache_if_stale()`），复用避免独立扫描开销，与 Epic 1 风险"repo_map 生成有性能开销，需缓存"一致
- **缓存命中率**：1K token 变化概率低于 2K，L1 静态层缓存失效更少（§3.2 heading 对齐约束）

**调整机制**：实测后若 1K 不足（表现为子智能体频繁调 read 探索文件结构），通过 `RepoMap::with_max_tokens(2048)` 调到 2K。需对比 `claw doctor --cache-stats` 确认命中率不下降。

### 8.4 handoff details 进主上下文:默认仅 summary，summary 含 changed_files

**决策**：默认仅 `summary` 进主上下文，`details` 通过 `result_ref` 路径按需 Read。`summary` 必须包含 `changed_files` 列表（从 frontmatter 提取）。

**依据**：
- **现有模式已是"路径进上下文，内容不进"**：[conversation.rs:3147-3149](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3147) 主 agent 收到 `"Result written to: {result_ref}\nUse Read tool to inspect"`，明确"did not pollute your context window"。handoff 的 summary 是增量改进（给主 agent 足够信息决策是否 Read），不改变基本模式
- **auto_compaction 阈值 100K token**：[conversation.rs:71](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L71) `DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD = 100_000`。多个子智能体 details 累计快速逼近阈值，触发 auto_compaction 丢失历史。summary ≤500 字符，10 个子智能体仅 5K token，远低于阈值
- **summary 含 changed_files 的必要性**：主 agent 据此判断是否需要 Read details 审查具体改动；validation gate 双列表检查（Epic 5）标记异常时，主 agent 应 Read details 排查

**主 agent Read details 的时机**：
- validation gate 标记"子智能体声称改了但 git diff 没有的"或"git diff 有但子智能体没声称的"（Epic 5 双列表检查）
- 主 agent 需审查具体代码改动（如 code review 场景）
