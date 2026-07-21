# Hooks 系统细化方案

- 文档版本: v0.1
- 创建日期: 2026-07-21
- 父文档: [ide-hooks-dag-implementation-plan.md](../ide-hooks-dag-implementation-plan.md)
- 焦点: 10 事件 × 4 Handler + HookRunner 异步引擎 + run_turn 7 集成点 + 配置示例
- 关联代码:
  - [rust/crates/runtime/src/hooks.rs](../../rust/crates/runtime/src/hooks.rs)
  - [rust/crates/runtime/src/conversation.rs](../../rust/crates/runtime/src/conversation.rs)
  - [rust/crates/runtime/src/config.rs](../../rust/crates/runtime/src/config.rs)

本章节是父文档第三章「Hooks 系统方案」的可实施细化版本。所有代码骨架以 `rust/crates/runtime/src/hooks.rs` 现有实现为基线,目标是把 3 事件 / 1 handler / 同步执行扩展为 10 事件 / 4 handler / 异步引擎,同时保持向后兼容与渐进迁移能力。

---

## 目录

1. [现状审计](#1-现状审计)
2. [HookEvent 完整设计](#2-hookevent-完整设计)
3. [HookHandler 4 类型详解](#3-hookhandler-4-类型详解)
4. [HookContext 数据结构](#4-hookcontext-数据结构)
5. [Hook trait + HookRegistry](#5-hook-trait--hookregistry)
6. [HookRunner 异步引擎](#6-hookrunner-异步引擎)
7. [run_turn 7 集成点](#7-run_turn-7-集成点)
8. [配置文件格式](#8-配置文件格式)
9. [与现有系统的协同](#9-与现有系统的协同)
10. [实施步骤分解](#10-实施步骤分解)
11. [测试矩阵](#11-测试矩阵)
12. [风险与缓解](#12-风险与缓解)

---

## 1. 现状审计

### 1.1 现有实现概览

文件: `rust/crates/runtime/src/hooks.rs`(1141 行,以下行号均相对该文件)。

| 组件 | 位置 | 现状 |
|---|---|---|
| `HookEvent` enum | line 22-26 | 仅 3 事件:`PreToolUse` / `PostToolUse` / `PostToolUseFailure` |
| `HookProgressEvent` | line 40-56 | `Started` / `Completed` / `Cancelled`,不支持 `Denied` / `Failed` 分辨 |
| `HookAbortSignal` | line 62-81 | `Arc<AtomicBool>`,正确,可直接复用 |
| `HookRunResult` | line 84-177 | 字段为 `denied` / `failed` / `cancelled` / `messages` / `permission_override` / `permission_reason` / `updated_input`,但无 `decision` 统一枚量、无 `additional_context` |
| `HookRunner` | line 180-335 | 同步 `&self`,函数签名带 `&mut reporter`,无法跨 `.await` 点持有 |
| `run_commands` | line 338-439 | 命令循环,exit code 0/2/其他契约完整,但无 timeout |
| `run_command` | line 442-549 | `std::process::Command` 同步阻塞,无法在异步上下文下让出线程 |
| `parse_hook_output` | line 563 | 解析 stdout JSON,提取 `decision` / `permissionDecision` / `updatedInput`,契约与 Claude Code 对齐 |
| `hook_payload` | line 657 | 构造 stdin JSON,目前仅工具类事件字段 |
| `format_hook_failure` | line 751 | 失败消息格式化 |
| `shell_command` | line 763 | shell 解析 |

### 1.2 配置层现状

文件: `rust/crates/runtime/src/config.rs` line 87-91:

```rust
pub struct RuntimeHookConfig {
    pre_tool_use: Vec<String>,
    post_tool_use: Vec<String>,
    post_tool_use_failure: Vec<String>,
}
```

每个事件绑定一组 shell 命令字符串。无法表达 matcher / priority / failure_policy / 多 handler 类型,需要被 `HookConfig`(详见第 8 章)替代。`RuntimeFeatureConfig::hooks()` 返回该结构的引用,迁移期需要保留兼容字段。

### 1.3 集成点现状

文件: `rust/crates/runtime/src/conversation.rs`,`run_turn` 起始于 line 824。当前接入的 3 个集成点:

| 集成点 | 行号 | 函数 | 现状 |
|---|---|---|---|
| PreToolUse | 1175 | `run_pre_tool_use_hook` | 在工具执行前调用,返回 `updated_input` 改写输入 |
| PostToolUse / PostToolUseFailure | 1280-1293 | `run_post_tool_use_hook` / `run_post_tool_use_failure_hook` | 工具完成后调用,根据 `is_error` 分支 |
| LoopDetector 注入 | 746-776 | PostToolUse 中检测 doom loop | BUG-2 修复,已合入 |

未接入的 7 个事件(均为 P0 待新增):`UserPromptSubmit` / `SessionStart` / `SessionEnd` / `Stop` / `SubagentStop` / `PreCompact` / `Notification`,以及 MCP 工具专用的 `PostCustomToolCall`。

### 1.4 缺失能力清单

- ❌ 缺 7 事件(对应 Claude Code 完整事件集合)
- ❌ 缺 3 handler(webhook / inline / prompt)
- ❌ 缺 matcher(regex 工具名过滤)
- ❌ 缺 timeout 控制(`run_command` 无超时,可能永久阻塞)
- ❌ 缺异步执行(`run_command` 阻塞调用线程,无法与 tokio runtime 协作)
- ❌ 缺 fail-open/fail-close 可配置
- ❌ `runtime/hooks.rs` 与 `plugins/hooks.rs`(若存在)重复实现,需要统一

---

## 2. HookEvent 完整设计

10 事件按所属层级分组,每个事件携带不同上下文,阻断语义不同。所有事件统一在 `rust/crates/runtime/src/hooks.rs` 中以 `#[serde(rename_all = "camelCase")]` 标注,序列化名与 Claude Code 官方实现对齐。

### 2.1 完整 enum 定义

```rust
// rust/crates/runtime/src/hooks.rs(扩展)
//
// 10 事件统一枚举,跨工具层 / 对话层 / 会话层 / Agent 层 / 上下文层。
// serde rename_all = "camelCase" 仅影响字段,事件名通过 #[serde(rename)]
// 强制首字母大写以保持与 Claude Code settings.json 兼容。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    // ── 工具层 ──
    /// 工具调用前,可阻断,可改写 tool_input(updatedInput)
    #[serde(rename = "PreToolUse")]
    PreToolUse,
    /// 工具成功后,可观察 tool_response,不可阻断
    #[serde(rename = "PostToolUse")]
    PostToolUse,
    /// Claw Code 特色:工具失败独立事件(优于 Claude Code 并入 PostToolUse)
    #[serde(rename = "PostToolUseFailure")]
    PostToolUseFailure,
    /// MCP 工具专用,可改 updatedOutput(对应 Claude Code 1.0+ 的 PostCustomToolCall)
    #[serde(rename = "PostCustomToolCall")]
    PostCustomToolCall,

    // ── 对话层 ──
    /// 用户提交 prompt 后、构造 LLM 请求前,可改写 prompt / 注入 additional_context
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit,
    /// 通知事件(权限请求、等待输入等),不可阻断,用于 webhook/通知场景
    #[serde(rename = "Notification")]
    Notification,

    // ── 会话层 ──
    #[serde(rename = "SessionStart")]
    SessionStart,
    #[serde(rename = "SessionEnd")]
    SessionEnd,

    // ── Agent 层 ──
    /// 主 agent 完成 turn 时,可阻断(让 Claude 继续),需防递归(stop_hook_active)
    #[serde(rename = "Stop")]
    Stop,
    /// 子 agent(dispatch_subagent)完成时,在 execute_check_subagent 中触发
    #[serde(rename = "SubagentStop")]
    SubagentStop,

    // ── 上下文层 ──
    /// 压缩前,可阻断 / 改写将被压缩的消息
    #[serde(rename = "PreCompact")]
    PreCompact,
}
```

### 2.2 事件属性辅助方法

```rust
impl HookEvent {
    /// 该事件是否支持阻断。可阻断事件在 hook 返回 Deny 时短路;
    /// 不可阻断事件必须执行完整 hook 链,即使某个 hook 返回 Deny。
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostCustomToolCall
                | Self::UserPromptSubmit
                | Self::Stop
                | Self::SubagentStop
                | Self::PreCompact
                | Self::SessionStart
        )
    }

    /// 该事件是否支持 matcher(工具名 regex)。仅工具类事件支持,
    /// 其他事件传入 matcher 字段时会被 HookRunner 忽略并发出警告。
    #[must_use]
    pub fn supports_matcher(&self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::PostCustomToolCall
        )
    }

    /// 序列化名(用于配置文件 key 与日志)
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PostCustomToolCall => "PostCustomToolCall",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Notification => "Notification",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
        }
    }
}
```

### 2.3 各事件详细语义

下表 9 字段含义:触发时机(代码位置)、上下文字段、blocking、是否支持 matcher、短路条件、典型用例。

#### 2.3.1 PreToolUse

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs::run_turn` line 1175,工具 dispatch 前 |
| 上下文 | `tool_name` / `tool_input` / `session_id` / `cwd` |
| blocking | 是 |
| supports_matcher | 是 |
| 短路条件 | `decision == Deny` 或 `failed && FailClose` |
| 典型用例 | 命令注入检测、文件路径白名单、Edit 前自动 lint |

#### 2.3.2 PostToolUse

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs` line 1287,工具成功执行后 |
| 上下文 | `tool_name` / `tool_input` / `tool_response` / `tool_result_is_error=false` |
| blocking | 否 |
| supports_matcher | 是 |
| 短路条件 | 不短路,但可改 `output`(经 `merge_hook_feedback`) |
| 典型用例 | 自动测试、生成 commit message、记录 audit log |

#### 2.3.3 PostToolUseFailure

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs` line 1281,工具执行返回错误后 |
| 上下文 | `tool_name` / `tool_input` / `tool_response` / `tool_result_is_error=true` |
| blocking | 否 |
| supports_matcher | 是 |
| 短路条件 | 不短路 |
| 典型用例 | 错误聚合、自动重试策略、PagerDuty 告警 |

#### 2.3.4 PostCustomToolCall

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs` line 1174 循环内,当 `tool_name.starts_with("mcp__")` 时分流 |
| 上下文 | `tool_name` / `tool_input` / `tool_response` |
| blocking | 是(MCP 工具支持改 `updatedOutput`) |
| supports_matcher | 是 |
| 短路条件 | `decision == Deny` |
| 典型用例 | MCP 工具响应改写、敏感字段脱敏 |

#### 2.3.5 UserPromptSubmit

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs::run_turn` line 829 之后,在 `session.push_user_text` 之前 |
| 上下文 | `prompt` / `session_id` / `cwd` |
| blocking | 是 |
| supports_matcher | 否 |
| 短路条件 | `decision == Deny`(整 turn 终止) |
| 典型用例 | prompt 重写、敏感词过滤、注入额外上下文(`additional_context`) |

#### 2.3.6 Notification

| 字段 | 值 |
|---|---|
| 触发时机 | 权限请求弹出 / 等待用户输入 / Idle 超时 |
| 上下文 | `message` / `session_id` |
| blocking | 否 |
| supports_matcher | 否 |
| 短路条件 | 不短路 |
| 典型用例 | Slack/钉钉 webhook、桌面通知 |

#### 2.3.7 SessionStart

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs::run_turn` line 876 之前,当 `is_first_turn == true` |
| 上下文 | `source`(`startup` / `resume` / `clear` / `compact`)/ `session_id` / `env_file` |
| blocking | 是 |
| supports_matcher | 否 |
| 短路条件 | `decision == Deny`(会话拒绝启动) |
| 典型用例 | 环境检查、加载 .env、注入项目说明 |

#### 2.3.8 SessionEnd

| 字段 | 值 |
|---|---|
| 触发时机 | 会话显式 `session_close` 或 idle timeout 触发 |
| 上下文 | `reason`(`user_exit` / `idle_timeout` / `error`)/ `session_id` |
| blocking | 否 |
| supports_matcher | 否 |
| 短路条件 | 不短路 |
| 典型用例 | 清理临时文件、汇总报告、归档 transcript |

#### 2.3.9 Stop

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs::run_turn` line 1490 创建 TurnSummary 之前 |
| 上下文 | `stop_hook_active`(防递归) / `session_id` |
| blocking | 是 |
| supports_matcher | 否 |
| 短路条件 | `decision == Continue && !stop_hook_active`(递归调用 `run_turn`) |
| 典型用例 | 完成度检查、自动 commit、prompt 评估(handler=prompt) |

#### 2.3.10 SubagentStop

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs::execute_check_subagent` line 1879,子 agent 进入终态时 |
| 上下文 | `subagent_id` / `subagent_status` / `session_id` |
| blocking | 是 |
| supports_matcher | 否 |
| 短路条件 | `decision == Deny`(子 agent 结果被丢弃,主 agent 重新 dispatch) |
| 典型用例 | 子 agent 结果验证、跨 agent 状态同步 |

#### 2.3.11 PreCompact

| 字段 | 值 |
|---|---|
| 触发时机 | `conversation.rs::maybe_auto_compact` line 2127 之前,或 reactive compaction 之前 line 1041 |
| 上下文 | `trigger`(`manual` / `auto` / `reactive`)/ `session_id` |
| blocking | 是 |
| supports_matcher | 否 |
| 短路条件 | `decision == Deny`(放弃本次压缩) |
| 典型用例 | 压缩前持久化关键消息、自定义摘要注入 |

---

## 3. HookHandler 4 类型详解

### 3.1 设计动机

| Handler | 动机 | 与 Claude Code 对比 |
|---|---|---|
| `Command` | 跨语言、隔离性好,已有实现 | Claude Code 唯一支持的 handler |
| `Webhook` | 远程审计 / Slack 通知 / SIEM 接入 | Claude Code 无,需通过 command + curl 间接实现 |
| `Inline` | 进程内 Rust trait,零开销,SDK 嵌入场景必需 | Claude Code 无,因为 Claude Code 不暴露 SDK |
| `Prompt` | LLM-based 评估,对应 Claude Code 的 `prompt` handler | Claude Code 1.0+ 支持,用于复杂语义判定 |

### 3.2 统一 enum 定义

```rust
// rust/crates/runtime/src/hooks.rs(扩展)
//
// HookHandler 是 HookEntry 中的核心字段,通过 serde tag = "type" 区分。
// 配置文件中 handler 字段必须带 "type" 子字段,例如:
//   { "handler": { "type": "command", "command": "..." } }
// 反序列化时根据 type 路由到对应 struct。

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookHandler {
    /// 子进程命令(已有,保留)。stdin 传 JSON payload,exit code 0/2/其他
    /// 作为 Allow/Deny/Failed 契约。
    Command(CommandHook),
    /// HTTP webhook POST。Body 为 HookContext 序列化的 JSON,
    /// 响应体可携带与 command 相同的 JSON schema。
    Webhook(WebhookHook),
    /// 进程内 Rust trait。通过 InlineHookRef.name 在 HookRegistry 中查找。
    /// SDK 嵌入场景下零开销,无需 fork 子进程。
    Inline(InlineHookRef),
    /// LLM-based 评估。调用快速模型(Haiku 级)评估当前上下文,
    /// 返回 JSON { decision, reason, continue }。
    Prompt(PromptHook),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandHook {
    /// shell 命令(支持管道、环境变量展开)
    pub command: String,
    /// 超时,默认 30s。超时后视为 Failed,触发 failure_policy。
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    /// 工作目录(默认继承 runtime cwd)
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// 额外环境变量
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookHook {
    /// 目标 URL。支持 `{session_id}` / `{event}` 模板变量。
    pub url: String,
    /// HTTP 方法,默认 POST
    #[serde(default = "default_http_method")]
    pub method: String,
    /// 自定义 headers(默认带 Content-Type: application/json)
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 超时,默认 30s
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    /// HMAC-SHA256 签名密钥(可选)。签名放入 X-Claw-Signature header。
    #[serde(default)]
    pub secret: Option<String>,
    /// 是否在失败时重试(默认 false,P1 实现)
    #[serde(default)]
    pub retry: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHook {
    /// prompt 模板。支持 $ARGUMENTS / $TOOL_NAME / $SESSION_ID 占位符。
    pub prompt: String,
    /// 模型名(默认 Haiku 级快速模型,通过 LlmRouter 解析)
    #[serde(default)]
    pub model: Option<String>,
    /// 超时,默认 60s(LLM 调用通常比 command 慢)
    #[serde(default = "default_prompt_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    /// 温度(默认 0,评估场景需要确定性)
    #[serde(default = "default_prompt_temperature")]
    pub temperature: f32,
    /// 最大 tokens(默认 1024,避免评估 prompt 反向消耗配额)
    #[serde(default = "default_prompt_max_tokens")]
    pub max_tokens: u32,
}

/// 进程内 Hook 引用。通过注册名在 HookRegistry 中查找。
/// 序列化时只存 name,实现代码不暴露给配置文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineHookRef {
    /// HookRegistry 中注册的 key(由 Hook::id() 返回)
    pub name: String,
}

fn default_timeout() -> Duration { Duration::from_secs(30) }
fn default_prompt_timeout() -> Duration { Duration::from_secs(60) }
fn default_http_method() -> String { "POST".to_string() }
fn default_prompt_temperature() -> f32 { 0.0 }
fn default_prompt_max_tokens() -> u32 { 1024 }

impl HookHandler {
    /// 返回该 handler 的超时。HookRunner 用此值包装 tokio::time::timeout。
    #[must_use]
    pub fn timeout(&self) -> Duration {
        match self {
            Self::Command(c) => c.timeout,
            Self::Webhook(w) => w.timeout,
            Self::Inline(_) => Duration::from_secs(60), // inline 默认 60s
            Self::Prompt(p) => p.timeout,
        }
    }

    /// 返回 handler 类型名字,用于日志与 metrics
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Command(_) => "command",
            Self::Webhook(_) => "webhook",
            Self::Inline(_) => "inline",
            Self::Prompt(_) => "prompt",
        }
    }
}
```

### 3.3 执行语义对比

| 维度 | Command | Webhook | Inline | Prompt |
|---|---|---|---|---|
| 同步/异步 | `tokio::process::Command::output_with_stdin`(异步) | `reqwest::Client`(异步) | `async_trait::async_trait`(异步) | `async_trait` 调用 LLM |
| 默认超时 | 30s | 30s | 60s | 60s |
| 失败语义 | exit code != 0 && != 2 → Failed | HTTP status >= 400 或网络错误 → Failed | `Err` 返回 → Failed | LLM 错误或 JSON 解析失败 → Failed |
| 可阻断 | 是(exit 2 → Deny) | 是(响应 `decision=deny`) | 是(`HookRunResult::denied()`) | 是(响应 `decision=deny`) |
| 可改输入 | 是(`updatedInput`) | 是(响应 `updatedInput`) | 是(直接返回 `updated_input`) | 否(LLM 不擅长结构改写) |
| 进程隔离 | 是 | 是 | 否(共享 runtime 地址空间) | 是 |
| 适用场景 | 跨语言 / 隔离 / CI 集成 | 远程审计 / 通知 | 高频低延迟 hook / SDK 嵌入 | 复杂语义判定 / 内容审核 |

### 3.4 配置 schema 示例

```toml
# .claw/hooks.toml(也可在 settings.json 中以 "hooks" key 嵌入)

[[PreToolUse]]
matcher = "Edit|Write|MultiEdit"
execution = "sequential"

  [[PreToolUse.hooks]]
  priority = 100
  failure_policy = "failClose"
  enabled = true

  [PreToolUse.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/lint.sh"
  timeout = "30s"

[[Stop]]
execution = "sequential"

  [[Stop.hooks]]
  priority = 100

  [Stop.hooks.handler]
  type = "prompt"
  prompt = "Evaluate if the task is complete: $ARGUMENTS"
  model = "claude-haiku"
  timeout = "60s"
  temperature = 0.0
  max_tokens = 1024

[[Notification]]
execution = "parallel"

  [[Notification.hooks]]
  priority = 100
  failure_policy = "failOpen"

  [Notification.hooks.handler]
  type = "webhook"
  url = "https://hooks.slack.com/services/T000/B000/XXX"
  timeout = "10s"
  secret = "slack-signing-secret"
```

### 3.5 与 Claude Code 官方实现对比

| 维度 | Claude Code | Claw Code(本方案) |
|---|---|---|
| Handler 类型 | command + prompt(1.0+) | command + webhook + inline + prompt |
| 事件数量 | 9 | 10(保留 PostToolUseFailure) |
| 异步执行 | 否(Node.js 单线程同步) | 是(tokio + async_trait) |
| Matcher | 仅 PreToolUse | 4 个工具类事件均支持 |
| 配置格式 | JSON settings.json | TOML 或 JSON |
| 进程内 hook | 不支持(无 SDK) | 支持(Inline hook) |

---

## 4. HookContext 数据结构

### 4.1 完整字段定义

```rust
// rust/crates/runtime/src/hooks.rs(扩展)
//
// HookContext 是传给所有 hook handler 的统一上下文。
// 设计原则:
// 1. 所有事件共用一个 struct,事件无关字段以 Option 表示
// 2. 借用生命周期 'a 避免大对象 clone(tool_input/tool_response 直接借用)
// 3. 不可变性保证:除 abort_signal 外所有字段均为 & 引用,hook 无法修改 runtime 状态
// 4. 序列化兼容:实现 Serialize,用于 command/webhook handler 的 stdin/body

use std::path::{Path, PathBuf};
use serde_json::Value;
use crate::permissions::ResolvedPermissionMode;

pub struct HookContext<'a> {
    // ── 公共字段(所有事件必有) ──
    pub event: HookEvent,
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub cwd: PathBuf,
    pub permission_mode: ResolvedPermissionMode,

    // ── 工具类事件字段(PreToolUse / PostToolUse / PostToolUseFailure / PostCustomToolCall) ──
    pub tool_name: Option<&'a str>,
    pub tool_input: Option<&'a Value>,
    pub tool_response: Option<&'a Value>,
    pub tool_result_is_error: bool,

    // ── 对话类事件字段 ──
    pub prompt: Option<&'a str>,           // UserPromptSubmit
    pub message: Option<&'a str>,           // Notification

    // ── Agent 类事件字段 ──
    pub stop_hook_active: bool,             // Stop(防递归)
    pub subagent_id: Option<&'a str>,       // SubagentStop
    pub subagent_status: Option<&'a str>,   // SubagentStop

    // ── 会话类事件字段 ──
    pub source: Option<&'a str>,             // SessionStart: startup/resume/clear/compact
    pub reason: Option<&'a str>,             // SessionEnd: user_exit/idle_timeout/error

    // ── 上下文类事件字段 ──
    pub trigger: Option<&'a str>,           // PreCompact: manual/auto/reactive
    pub custom_instructions: Option<&'a str>,

    // ── 运行时辅助 ──
    pub abort_signal: &'a HookAbortSignal,
    pub env_file: Option<&'a Path>,         // CLAUDE_ENV_FILE for SessionStart
}

impl<'a> HookContext<'a> {
    /// 构造 PreToolUse 上下文。
    /// 调用点: conversation.rs line 1175 之前
    #[must_use]
    pub fn for_pre_tool_use(
        tool_name: &'a str,
        tool_input: &'a Value,
        session_id: String,
        cwd: PathBuf,
        abort_signal: &'a HookAbortSignal,
    ) -> Self {
        Self {
            event: HookEvent::PreToolUse,
            session_id,
            transcript_path: PathBuf::new(),
            cwd,
            permission_mode: ResolvedPermissionMode::default(),
            tool_name: Some(tool_name),
            tool_input: Some(tool_input),
            tool_response: None,
            tool_result_is_error: false,
            prompt: None,
            message: None,
            stop_hook_active: false,
            subagent_id: None,
            subagent_status: None,
            source: None,
            reason: None,
            trigger: None,
            custom_instructions: None,
            abort_signal,
            env_file: None,
        }
    }

    /// 构造 UserPromptSubmit 上下文。
    /// 调用点: conversation.rs line 829 之后
    #[must_use]
    pub fn for_user_prompt_submit(
        prompt: &'a str,
        session_id: String,
        cwd: PathBuf,
        abort_signal: &'a HookAbortSignal,
    ) -> Self {
        Self {
            event: HookEvent::UserPromptSubmit,
            session_id,
            transcript_path: PathBuf::new(),
            cwd,
            permission_mode: ResolvedPermissionMode::default(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_result_is_error: false,
            prompt: Some(prompt),
            message: None,
            stop_hook_active: false,
            subagent_id: None,
            subagent_status: None,
            source: None,
            reason: None,
            trigger: None,
            custom_instructions: None,
            abort_signal,
            env_file: None,
        }
    }

    /// 构造 SessionStart 上下文。仅在 first_turn 时调用。
    #[must_use]
    pub fn for_session_start(
        source: &'a str,
        session_id: String,
        cwd: PathBuf,
        env_file: Option<&'a Path>,
        abort_signal: &'a HookAbortSignal,
    ) -> Self {
        Self {
            event: HookEvent::SessionStart,
            session_id,
            transcript_path: PathBuf::new(),
            cwd,
            permission_mode: ResolvedPermissionMode::default(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_result_is_error: false,
            prompt: None,
            message: None,
            stop_hook_active: false,
            subagent_id: None,
            subagent_status: None,
            source: Some(source),
            reason: None,
            trigger: None,
            custom_instructions: None,
            abort_signal,
            env_file,
        }
    }

    /// 构造 Stop 上下文。
    /// stop_hook_active = true 表示当前是 Stop hook 触发的递归 run_turn,需短路。
    #[must_use]
    pub fn for_stop(
        stop_hook_active: bool,
        session_id: String,
        cwd: PathBuf,
        abort_signal: &'a HookAbortSignal,
    ) -> Self {
        Self {
            event: HookEvent::Stop,
            session_id,
            transcript_path: PathBuf::new(),
            cwd,
            permission_mode: ResolvedPermissionMode::default(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_result_is_error: false,
            prompt: None,
            message: None,
            stop_hook_active,
            subagent_id: None,
            subagent_status: None,
            source: None,
            reason: None,
            trigger: None,
            custom_instructions: None,
            abort_signal,
            env_file: None,
        }
    }

    /// 构造 PreCompact 上下文。
    #[must_use]
    pub fn for_pre_compact(
        trigger: &'a str,
        session_id: String,
        cwd: PathBuf,
        abort_signal: &'a HookAbortSignal,
    ) -> Self {
        Self {
            event: HookEvent::PreCompact,
            session_id,
            transcript_path: PathBuf::new(),
            cwd,
            permission_mode: ResolvedPermissionMode::default(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_result_is_error: false,
            prompt: None,
            message: None,
            stop_hook_active: false,
            subagent_id: None,
            subagent_status: None,
            source: None,
            reason: None,
            trigger: Some(trigger),
            custom_instructions: None,
            abort_signal,
            env_file: None,
        }
    }
}
```

### 4.2 不可变性保证

- `HookContext` 所有字段为 `&'a` 引用或 owned `String/PathBuf`,handler 无法修改原对象。
- `abort_signal` 是 `&'a HookAbortSignal`,提供 `abort()` 方法但仅能设置 AtomicBool,hook 自身无法影响 runtime 决策。
- handler 返回 `HookRunResult`(owned),runtime 根据 result 决定是否更新 `tool_input` / `tool_response`,hook 不直接修改这些值。
- 序列化(command/webhook handler 用)通过 `serde_json::to_value(ctx)` 生成 owned JSON,与原对象解耦。

### 4.3 序列化格式

`HookContext` 实现自定义 `Serialize`,序列化为与 Claude Code 兼容的 JSON 格式:

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "sess_abc123",
  "transcript_path": "/tmp/claw/sess_abc123/transcript.jsonl",
  "cwd": "/home/user/project",
  "permission_mode": "default",
  "tool_name": "Edit",
  "tool_input": { "file_path": "/home/user/project/src/main.rs", "old_string": "...", "new_string": "..." },
  "tool_response": null,
  "tool_result_is_error": false,
  "prompt": null,
  "message": null,
  "stop_hook_active": false,
  "source": null,
  "reason": null,
  "trigger": null,
  "env_file": null
}
```

实现思路:`impl Serialize for HookContext`,字段名通过 `#[serde(rename = "...")]` 对齐 Claude Code(如 `hook_event_name` 而非 `event`)。

---

## 5. Hook trait + HookRegistry

### 5.1 Hook trait 定义

```rust
// rust/crates/runtime/src/hooks.rs(新增)
//
// Hook trait 是 InlineHookRef 的执行入口。
// 设计为对象安全,支持 Arc<dyn Hook> 动态分发。
// Plugin 场景必需动态分发,因此使用 async_trait 宏(虽然有轻微开销,
// 详见第 12 章风险分析)。

use async_trait::async_trait;

#[async_trait]
pub trait Hook: Send + Sync {
    /// Hook 唯一标识(用于 InlineHookRef.name 注册查找)。
    /// 建议格式: "vendor.plugin_name.hook_name",避免冲突。
    fn id(&self) -> &str;

    /// 主入口:接收 HookContext,返回 HookRunResult。
    /// 实现可借用 ctx.tool_input / ctx.tool_response 等,
    /// 但不可修改原对象(详见 HookContext 不可变性保证)。
    async fn execute(&self, ctx: &HookContext<'_>) -> HookRunResult;

    /// 该 Hook 关心哪些事件。HookRunner 在路由阶段过滤,
    /// 避免不必要的 execute 调用。
    /// 默认空切片表示关心所有事件(不推荐,建议显式列出)。
    fn events(&self) -> &[HookEvent] {
        &[]
    }

    /// 该 Hook 是否支持 matcher(工具类事件)。默认 false。
    /// 实现 true 的 hook 应在 execute 内自行检查 ctx.tool_name。
    fn supports_matcher(&self) -> bool {
        false
    }

    /// 优先级(数字越小越先执行)。默认 100。
    /// 同一事件 + matcher 组内多 hook 时,按 priority 升序执行。
    fn priority(&self) -> u32 {
        100
    }
}
```

### 5.2 HookRegistry 注册表

```rust
// rust/crates/runtime/src/hooks.rs(新增)
//
// HookRegistry 管理 inline hook 的注册与查找。
// 生命周期与 ConversationRuntime 一致(整个 session)。
// 设计为 interior mutability:用 RwLock 而非 Mutex,因为 hook 注册主要在
// 启动期完成,运行期主要是读操作。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub struct HookRegistry {
    hooks: RwLock<HashMap<String, Arc<dyn Hook>>>,
}

impl HookRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            hooks: RwLock::new(HashMap::new()),
        }
    }

    /// 注册一个 hook。若 name 已存在,覆盖旧值并返回旧 Arc。
    /// 注册时机:
    /// 1. runtime 启动期(从配置加载 inline hook)
    /// 2. plugin 加载期(plugin 注册自己的 hook)
    /// 3. 运行期(动态注册,需配合 Reload 机制,P1 实现)
    pub fn register(&self, hook: Arc<dyn Hook>) -> Option<Arc<dyn Hook>> {
        let mut map = self.hooks.write().expect("HookRegistry poisoned");
        map.insert(hook.id().to_string(), hook)
    }

    /// 按 name 查找 hook(InlineHookRef.name 路径)。
    /// 返回 Arc clone,允许 hook 在多并发场景下被同时调用。
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Hook>> {
        let map = self.hooks.read().expect("HookRegistry poisoned");
        map.get(name).cloned()
    }

    /// 列出所有已注册 hook 的 id(用于调试 / /hooks 命令)。
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        let map = self.hooks.read().expect("HookRegistry poisoned");
        map.keys().cloned().collect()
    }

    /// 注销 hook。主要用于 plugin 卸载场景。
    pub fn unregister(&self, name: &str) -> Option<Arc<dyn Hook>> {
        let mut map = self.hooks.write().expect("HookRegistry poisoned");
        map.remove(name)
    }

    /// 清空所有 hook(测试用)。
    pub fn clear(&self) {
        let mut map = self.hooks.write().expect("HookRegistry poisoned");
        map.clear();
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}
```

### 5.3 内置 Inline Hook 示例

```rust
// rust/crates/runtime/src/hooks.rs(新增内置 hook)
//
// 一个简单的示例 inline hook,用于在测试与 demo 中展示用法。
// 实际生产 hook 由 plugin 提供。

pub struct FileWriteAuditHook {
    audit_log: PathBuf,
}

impl FileWriteAuditHook {
    pub fn new(audit_log: PathBuf) -> Self {
        Self { audit_log }
    }
}

#[async_trait]
impl Hook for FileWriteAuditHook {
    fn id(&self) -> &str {
        "claw.builtin.file_write_audit"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PostToolUse]
    }

    fn supports_matcher(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &HookContext<'_>) -> HookRunResult {
        // 仅对 Edit / Write / MultiEdit 工具记录审计
        let tool_name = ctx.tool_name.unwrap_or("");
        if !matches!(tool_name, "Edit" | "Write" | "MultiEdit") {
            return HookRunResult::allow(Vec::new());
        }

        // 追加审计日志(失败不阻断主流程)
        let log_line = format!(
            "{} session={} tool={} input={}\n",
            chrono::Utc::now().to_rfc3339(),
            ctx.session_id,
            tool_name,
            ctx.tool_input.map(|v| v.to_string()).unwrap_or_default(),
        );

        if let Err(e) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.audit_log)
            .and_then(|mut f| f.write_all(log_line.as_bytes()))
        {
            eprintln!("warning: file_write_audit log failed: {e}");
        }

        HookRunResult::allow(Vec::new())
    }
}
```

---

## 6. HookRunner 异步引擎

### 6.1 设计目标

- **统一调度**:同一事件下多个 matcher × 多个 hook 的执行顺序由 `priority` 升序决定。
- **并发控制**:支持 `Sequential`(默认)与 `Parallel` 两种执行模式,后者适用于 Notification 等不可阻断事件。
- **超时处理**:每个 hook 独立 timeout,超时视为 Failed,触发 `failure_policy`。
- **失败策略**:`FailClose`(默认,阻断后续 hook)+ `FailOpen`(继续执行)。
- **短路语义**:可阻断事件 + Deny → 立即返回;不可阻断事件忽略 Deny。

### 6.2 HookRunner 结构

```rust
// rust/crates/runtime/src/hooks.rs(扩展)
//
// HookRunner 是 hook 系统的入口,runtime 在集成点调用 hook_runner.run(event, ctx)。
// 设计要点:
// 1. async fn run,避免阻塞 tokio worker
// 2. 持有 HookConfig(从配置加载)与 HookRegistry(inline hook 查找)
// 3. 持有 reqwest::Client(webhook 复用连接池)
// 4. 持有 LlmRouter(prompt handler 调用 LLM)

use std::sync::Arc;
use std::time::Duration;
use serde::{Serialize, Deserialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct HookRunner {
    /// Hook 配置(从 .claw/hooks.toml 或 settings.json 加载)
    config: HookConfig,
    /// Inline hook 注册表(进程内 hook 通过 name 查找)
    inline_registry: Arc<HookRegistry>,
    /// HTTP 客户端(webhook handler 共用连接池)
    http_client: reqwest::Client,
    /// LLM 路由器(prompt handler 用,P1 实现)
    llm_router: Option<Arc<LlmRouter>>,
}

impl HookRunner {
    #[must_use]
    pub fn new(
        config: HookConfig,
        inline_registry: Arc<HookRegistry>,
        http_client: reqwest::Client,
    ) -> Self {
        Self {
            config,
            inline_registry,
            http_client,
            llm_router: None,
        }
    }

    /// 兼容旧 RuntimeHookConfig 的工厂方法(迁移期用)。
    #[must_use]
    pub fn from_legacy(legacy: crate::config::RuntimeHookConfig) -> Self {
        let config = HookConfig::from_legacy(legacy);
        Self::new(
            config,
            Arc::new(HookRegistry::new()),
            reqwest::Client::new(),
        )
    }
}
```

### 6.3 主调度算法

```rust
impl HookRunner {
    /// 主入口:执行某事件下的所有 hook。
    /// 返回聚合 HookRunResult(包含最严苛的 decision 与所有 messages)。
    pub async fn run(
        &self,
        event: HookEvent,
        ctx: &HookContext<'_>,
    ) -> HookRunResult {
        let matchers = self.config.hooks.get(&event).cloned().unwrap_or_default();
        let mut aggregate = HookRunResult::default();

        for matcher in matchers {
            // matcher 过滤(仅工具类事件生效)
            if event.supports_matcher() && !self.matcher_applies(&matcher.matcher, ctx) {
                continue;
            }

            // 按 priority 升序排序(稳定排序,同 priority 保持配置顺序)
            let mut entries = matcher.hooks.clone();
            entries.sort_by_key(|e| e.priority);

            match matcher.execution {
                HookExecution::Sequential => {
                    // 顺序执行,遇到短路条件立即 break
                    for entry in entries {
                        if !entry.enabled {
                            continue;
                        }
                        let result = self.run_one(&entry, ctx).await;
                        let should_short_circuit = self
                            .merge_and_check_short_circuit(
                                &mut aggregate,
                                result,
                                &event,
                                &entry,
                            );
                        if should_short_circuit {
                            break;
                        }
                    }
                }
                HookExecution::Parallel => {
                    // 并行执行,所有 hook 同时跑,不短路
                    // 适用于 Notification 等不可阻断事件
                    let futures: Vec<_> = entries
                        .iter()
                        .filter(|e| e.enabled)
                        .map(|e| self.run_one(e, ctx))
                        .collect();
                    let results = futures::future::join_all(futures).await;
                    for r in results {
                        self.merge(&mut aggregate, r, &event);
                    }
                }
            }
        }

        aggregate
    }

    /// 检查 matcher 是否匹配当前 tool_name。
    /// matcher 为空或 "*" 表示匹配全部。
    fn matcher_applies(&self, matcher: &str, ctx: &HookContext<'_>) -> bool {
        if matcher.is_empty() || matcher == "*" {
            return true;
        }
        let tool_name = match ctx.tool_name {
            Some(n) => n,
            None => return false,
        };
        // 使用 regex 缓存(P0 用 simple string contains,P1 升级为 regex)
        matcher
            .split('|')
            .any(|pattern| tool_name == pattern.trim())
    }

    async fn run_one(&self, entry: &HookEntry, ctx: &HookContext<'_>) -> HookRunResult {
        let timeout = entry.handler.timeout();
        let fut = match &entry.handler {
            HookHandler::Command(c) => self.run_command(c, ctx),
            HookHandler::Webhook(w) => self.run_webhook(w, ctx),
            HookHandler::Inline(i) => self.run_inline(i, ctx),
            HookHandler::Prompt(p) => self.run_prompt(p, ctx),
        };

        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_elapsed) => HookRunResult::failed(format!(
                "hook {} ({}) timeout after {:?}",
                entry.handler.kind(),
                event_label(ctx.event),
                timeout,
            )),
        }
    }
}
```

### 6.4 失败策略与短路检查

```rust
impl HookRunner {
    /// 合并结果并检查是否应短路。
    /// 返回 true 表示应立即终止后续 hook 执行。
    fn merge_and_check_short_circuit(
        &self,
        aggregate: &mut HookRunResult,
        new: HookRunResult,
        event: &HookEvent,
        entry: &HookEntry,
    ) -> bool {
        // 失败策略检查
        if new.is_failed() {
            match entry.failure_policy {
                FailurePolicy::FailClose => {
                    if event.is_blocking() {
                        aggregate.decision = HookDecision::Deny;
                        aggregate.messages.extend(new.messages);
                        return true; // 短路
                    }
                }
                FailurePolicy::FailOpen => {
                    // 继续执行,但仍记录失败消息
                    aggregate.messages.extend(new.messages);
                    return false;
                }
            }
        }

        // 阻断决策检查(仅在 blocking 事件生效)
        if new.decision == HookDecision::Deny && event.is_blocking() {
            aggregate.decision = HookDecision::Deny;
            aggregate.messages.extend(new.messages);
            aggregate.reason = new.reason.or(aggregate.reason.take());
            return true; // 短路
        }

        // 合并 updated_input / additional_context(后者覆盖前者)
        if let Some(updated) = new.updated_input {
            aggregate.updated_input = Some(updated);
        }
        if let Some(ctx_injected) = new.additional_context {
            aggregate.additional_context = Some(ctx_injected);
        }

        self.merge(aggregate, new, event);
        false
    }

    fn merge(&self, aggregate: &mut HookRunResult, new: HookRunResult, _event: &HookEvent) {
        if new.is_denied() {
            aggregate.denied = true;
        }
        if new.is_failed() {
            aggregate.failed = true;
        }
        if new.is_cancelled() {
            aggregate.cancelled = true;
        }
        aggregate.messages.extend(new.messages);
    }
}
```

### 6.5 Command handler 异步实现

```rust
impl HookRunner {
    /// 异步执行 command hook。
    /// 复用现有 run_command 的 stdin/exit code 契约,
    /// 但改用 tokio::process::Command 让出线程。
    async fn run_command(&self, hook: &CommandHook, ctx: &HookContext<'_>) -> HookRunResult {
        let payload = serde_json::to_string(ctx).unwrap_or_default();

        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(&hook.command);
        command.env("HOOK_EVENT", ctx.event.as_str());
        command.env("HOOK_SESSION_ID", &ctx.session_id);
        command.env("HOOK_CWD", ctx.cwd.to_string_lossy().to_string());
        if let Some(tool_name) = ctx.tool_name {
            command.env("HOOK_TOOL_NAME", tool_name);
        }

        // 写入 stdin
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                return HookRunResult::failed(format!(
                    "command spawn failed: {e}"
                ));
            }
        };

        // 写 stdin(并发任务,避免阻塞)
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes()).await;
        }

        // 等待输出
        let output = match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => {
                return HookRunResult::failed(format!(
                    "command wait failed: {e}"
                ));
            }
        };

        // 复用现有 exit code 契约:0=Allow,2=Deny,其他=Failed
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let parsed = parse_hook_output(ctx.event, "", &hook.command, &stdout, &stderr);

        match output.status.code() {
            Some(0) => HookRunResult::allow(parsed.into_messages()),
            Some(2) => HookRunResult::deny(parsed.into_messages()),
            Some(code) => HookRunResult::failed(format!(
                "command exit {code}: {}",
                parsed.primary_message().unwrap_or("")
            )),
            None => HookRunResult::failed("command terminated by signal".to_string()),
        }
    }
}
```

### 6.6 Webhook handler 异步实现

```rust
impl HookRunner {
    async fn run_webhook(&self, hook: &WebhookHook, ctx: &HookContext<'_>) -> HookRunResult {
        let payload = serde_json::to_value(ctx).unwrap_or_else(|_| json!({}));

        // 模板变量替换(简单实现,P1 升级为 handlebars)
        let url = hook.url
            .replace("{session_id}", &ctx.session_id)
            .replace("{event}", ctx.event.as_str());

        let method = match hook.method.to_uppercase().as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            _ => reqwest::Method::POST,
        };

        let mut req = self
            .http_client
            .request(method, &url)
            .json(&payload);

        // 自定义 headers
        for (k, v) in &hook.headers {
            req = req.header(k, v);
        }

        // HMAC 签名
        if let Some(secret) = &hook.secret {
            let sig = hmac_sha256(secret, &payload.to_string());
            req = req.header("X-Claw-Signature", sig);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
                if status.is_success() {
                    parse_webhook_response(&body, status)
                } else {
                    HookRunResult::failed(format!(
                        "webhook HTTP {}: {}",
                        status,
                        body
                    ))
                }
            }
            Err(e) => HookRunResult::failed(format!("webhook error: {e}")),
        }
    }
}
```

### 6.7 Inline handler 实现

```rust
impl HookRunner {
    async fn run_inline(&self, hook: &InlineHookRef, ctx: &HookContext<'_>) -> HookRunResult {
        match self.inline_registry.get(&hook.name) {
            Some(h) => h.execute(ctx).await,
            None => HookRunResult::failed(format!(
                "inline hook not found: {}",
                hook.name
            )),
        }
    }
}
```

### 6.8 Prompt handler(占位,P1 实现)

```rust
impl HookRunner {
    /// 调用 LLM 评估当前上下文。
    /// prompt 模板支持 $ARGUMENTS / $TOOL_NAME / $SESSION_ID 占位符。
    /// 返回 JSON { decision, reason, continue }。
    async fn run_prompt(&self, hook: &PromptHook, ctx: &HookContext<'_>) -> HookRunResult {
        let router = match &self.llm_router {
            Some(r) => r,
            None => return HookRunResult::failed("llm_router not configured".to_string()),
        };

        // 占位符替换
        let mut prompt = hook.prompt.clone();
        prompt = prompt.replace("$EVENT", ctx.event.as_str());
        prompt = prompt.replace("$SESSION_ID", &ctx.session_id);
        if let Some(tool_name) = ctx.tool_name {
            prompt = prompt.replace("$TOOL_NAME", tool_name);
        }
        if let Some(user_prompt) = ctx.prompt {
            prompt = prompt.replace("$ARGUMENTS", user_prompt);
        }

        // 调用 LLM(P1 实现)
        // let response = router.complete(&prompt, hook.model.as_deref(), hook.temperature, hook.max_tokens).await;
        // let body: Value = serde_json::from_str(&response)?;
        // parse_prompt_response(&body)

        HookRunResult::failed("prompt handler not yet implemented (P1)".to_string())
    }
}
```

---

## 7. run_turn 7 集成点

以下所有行号基于当前 `rust/crates/runtime/src/conversation.rs` 实际状态。每个集成点列出:位置、上下文代码块、注入的 Hook 调用代码、测试用例。

### 7.1 集成点概览

| # | 事件 | 注入位置 | 行号 | 触发条件 |
|---|---|---|---|---|
| 1 | SessionStart | run_turn 入口,loop_detector.reset 之前 | line 834 之前 | `is_first_turn == true` |
| 2 | UserPromptSubmit | run_turn 入口,session.push_user_text 之前 | line 840-842 之前 | 每次 run_turn 调用 |
| 3 | PreToolUse | tool call 循环内,执行前 | line 1175(已存在) | 每次 tool dispatch |
| 4 | PostToolUse / PostToolUseFailure | tool call 循环内,执行后 | line 1280-1293(已存在) | 工具完成时 |
| 5 | PostCustomToolCall | tool call 循环内,MCP 工具分支 | line 1174 内部分流 | `tool_name.starts_with("mcp__")` |
| 6 | Stop | 主循环退出后,TurnSummary 构造之前 | line 1490 之前 | run_turn 结束 |
| 7 | PreCompact | maybe_auto_compact 之前 | line 1488 之前 | 触发压缩时 |
| 8 | SubagentStop | execute_check_subagent 内,子 agent 终态 | line 1879 内 | 子 agent 完成 |

> 注:任务要求 7 个集成点,实际有 8 个(PostToolUse 与 PostToolUseFailure 共享同一集成点位置,故合计 7)。下文按 8 个位置展开。

### 7.2 集成点 1:SessionStart(新增)

**位置**: `conversation.rs::run_turn` line 834 之前(`self.loop_detector.reset()` 之前)。

**当前代码**:

```rust
// rust/crates/runtime/src/conversation.rs line 824-839
pub fn run_turn(
    &mut self,
    user_input: impl Into<String>,
    mut prompter: Option<&mut dyn PermissionPrompter>,
) -> Result<TurnSummary, RuntimeError> {
    let user_input = user_input.into();

    // P2-7 修复:在每个 turn 开始时重置 loop_detector
    self.loop_detector.reset();
    // ...
}
```

**注入的 Hook 调用代码**:

```rust
// 在 self.loop_detector.reset() 之前插入:

// P0 新增:SessionStart hook(仅 first_turn 触发)
// 检查 source 字段决定 hook 是否应执行(startup/resume/clear/compact)
if self.is_first_turn {
    let source = if self.session.messages.is_empty() {
        "startup"
    } else {
        "resume"
    };
    let session_ctx = HookContext::for_session_start(
        source,
        self.session_id.clone(),
        self.cwd.clone(),
        self.env_file.as_deref(),
        &self.hook_abort_signal,
    );
    let session_result = self.hook_runner.run(
        HookEvent::SessionStart,
        &session_ctx,
    ).await;
    if session_result.is_denied() {
        return Err(RuntimeError::new(
            session_result.reason.unwrap_or_else(|| {
                "SessionStart hook denied session".to_string()
            })
        ));
    }
    // 注入 additional_context(如项目说明)
    if let Some(extra) = session_result.additional_context {
        self.pending_session_context = Some(extra);
    }
    self.is_first_turn = false;
}
```

**测试用例**:

```rust
#[tokio::test]
async fn test_session_start_hook_fires_on_first_turn() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::SessionStart, deny_hook("禁止启动"))
        .build();
    let result = runtime.run_turn("hello", None).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("禁止启动"));
}

#[tokio::test]
async fn test_session_start_hook_skipped_after_first_turn() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::SessionStart, allow_hook())
        .build();
    let _ = runtime.run_turn("turn 1", None).await.unwrap();
    let _ = runtime.run_turn("turn 2", None).await.unwrap();
    // 第二个 turn 不应触发 SessionStart
    assert!(!runtime.hook_call_log().contains(&HookEvent::SessionStart.to_string() + "_2"));
}
```

### 7.3 集成点 2:UserPromptSubmit(新增)

**位置**: `conversation.rs::run_turn` line 829 之后,`session.push_user_text` (line 840) 之前。

**当前代码**:

```rust
// rust/crates/runtime/src/conversation.rs line 829-842
let user_input = user_input.into();
// BUG-9:记录 turn 开始时间
self.turn_start.set(Some(Instant::now()));
self.record_turn_started(&user_input);
self.session
    .push_user_text(user_input.clone())
    .map_err(|error| RuntimeError::new(error.to_string()))?;
```

**注入的 Hook 调用代码**:

```rust
// 在 self.record_turn_started(&user_input) 之后、push_user_text 之前插入:

// P0 新增:UserPromptSubmit hook
// 可阻断(prompt 被 deny 则整 turn 终止)、可改写 prompt、可注入 additional_context
let prompt_ctx = HookContext::for_user_prompt_submit(
    &user_input,
    self.session_id.clone(),
    self.cwd.clone(),
    &self.hook_abort_signal,
);
let prompt_result = self.hook_runner.run(
    HookEvent::UserPromptSubmit,
    &prompt_ctx,
).await;
if prompt_result.is_denied() {
    return Err(RuntimeError::new(
        prompt_result.reason.unwrap_or_else(|| {
            "UserPromptSubmit hook denied".to_string()
        })
    ));
}
// 应用 updated_prompt(若 hook 改写了 prompt)
let effective_input = prompt_result.updated_input
    .unwrap_or_else(|| user_input.clone());
// 注入 additional_context(会在 request 构造时拼到 system_prompt)
if let Some(extra) = prompt_result.additional_context {
    self.pending_user_context = Some(extra);
}
```

**测试用例**:

```rust
#[tokio::test]
async fn test_user_prompt_submit_can_deny() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::UserPromptSubmit, deny_hook("敏感词"))
        .build();
    let result = runtime.run_turn("执行 rm -rf", None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_user_prompt_submit_can_inject_context() {
    let mut runtime = build_test_runtime()
        .with_hook(
            HookEvent::UserPromptSubmit,
            inject_context_hook("项目背景: 客户 A 的电商系统"),
        )
        .build();
    let _ = runtime.run_turn("分析架构", None).await.unwrap();
    // 验证 system_prompt 包含注入的上下文
    let last_request = runtime.last_api_request().unwrap();
    assert!(last_request.system_prompt.contains("客户 A 的电商系统"));
}
```

### 7.4 集成点 3:PreToolUse(已有)

**位置**: `conversation.rs` line 1175(已存在,只需把 `run_pre_tool_use_hook` 内部调用切换到 `hook_runner.run`)。

**当前代码**:

```rust
// rust/crates/runtime/src/conversation.rs line 1174-1178
for (tool_use_id, tool_name, input) in pending_tool_uses {
    let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);
    let effective_input = pre_hook_result
        .updated_input()
        .map_or_else(|| input.clone(), ToOwned::to_owned);
    // ...
}
```

**修改后**(把 helper 内部切换到异步 run,wrapper 仍同步):

```rust
// rust/crates/runtime/src/conversation.rs(修改 run_pre_tool_use_hook)
fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
    let input_value: Value = serde_json::from_str(input).unwrap_or(json!({}));
    let ctx = HookContext::for_pre_tool_use(
        tool_name,
        &input_value,
        self.session_id.clone(),
        self.cwd.clone(),
        &self.hook_abort_signal,
    );
    // block_on 同步桥接(因为 run_turn 当前是同步函数,P1 改为 async)
    futures::executor::block_on(self.hook_runner.run(HookEvent::PreToolUse, &ctx))
}
```

**测试用例**(已有,需扩展):

```rust
#[test]
fn test_pre_tool_use_updated_input_is_applied() {
    let mut runtime = build_test_runtime()
        .with_hook(
            HookEvent::PreToolUse,
            update_input_hook(json!({"file_path": "/sanitized"})),
        )
        .build();
    let summary = runtime.run_turn("edit file", None).unwrap();
    // 验证 Edit 工具收到的 input 已被改写
    assert!(runtime.tool_executor.last_input().contains("/sanitized"));
}
```

### 7.5 集成点 4:PostToolUse / PostToolUseFailure(已有)

**位置**: `conversation.rs` line 1280-1293。

**当前代码**:

```rust
// rust/crates/runtime/src/conversation.rs line 1280-1293
let post_hook_result = if is_error {
    self.run_post_tool_use_failure_hook(
        &tool_name,
        &effective_input,
        &output,
    )
} else {
    self.run_post_tool_use_hook(
        &tool_name,
        &effective_input,
        &output,
        false,
    )
};
```

**修改后**:与 PreToolUse 同样,helper 内部切换到 `block_on(self.hook_runner.run(...))`,行为保持一致。同时把 LoopDetector 的逻辑外移到 hook 中(作为内置 inline hook 注册)。

### 7.6 集成点 5:PostCustomToolCall(新增)

**位置**: `conversation.rs` line 1174 循环内,在 PreToolUse 之后、tool dispatch 之前判断 `tool_name.starts_with("mcp__")`。

**注入的 Hook 调用代码**:

```rust
// 在 PreToolUse hook 之后、tool 执行之前插入:

// P0 新增:PostCustomToolCall(MCP 工具专用)
// 仅对 mcp__ 前缀工具生效,允许 hook 改写 updatedOutput
if tool_name.starts_with("mcp__") {
    // 此时还没有 tool_response,PostCustomToolCall 应在 tool 执行后触发
    // 但因 MCP 工具的 output 可能被改写,需要在 result_message 构造之前介入
    // 实际触发点见 line 1278 之后:
    //   if tool_name.starts_with("mcp__") {
    //       let mcp_ctx = HookContext::for_post_custom_tool_call(...);
    //       let mcp_result = block_on(self.hook_runner.run(
    //           HookEvent::PostCustomToolCall, &mcp_ctx));
    //       if let Some(updated) = mcp_result.updated_output {
    //           output = updated;
    //       }
    //   }
}
```

**测试用例**:

```rust
#[test]
fn test_post_custom_tool_call_can_rewrite_output() {
    let mut runtime = build_test_runtime()
        .with_hook(
            HookEvent::PostCustomToolCall,
            update_output_hook("{\"redacted\": true}"),
        )
        .build();
    let _ = runtime.run_turn("call mcp tool", None).await.unwrap();
    // 验证 MCP 工具输出被改写
    assert!(runtime.session.messages.iter().any(|m| {
        m.blocks.iter().any(|b| matches!(b,
            ContentBlock::ToolResult { output, .. } if output.contains("redacted")))
    }));
}
```

### 7.7 集成点 6:Stop(新增)

**位置**: `conversation.rs::run_turn` line 1490 之前(`TurnSummary` 构造之前)。

**当前代码**:

```rust
// rust/crates/runtime/src/conversation.rs line 1488-1498
let auto_compaction = self.maybe_auto_compact();

let summary = TurnSummary {
    assistant_messages,
    tool_results,
    prompt_cache_events,
    iterations,
    usage: self.usage_tracker.cumulative_usage(),
    auto_compaction,
};
self.record_turn_completed(&summary);
```

**注入的 Hook 调用代码**:

```rust
// 在 let auto_compaction = self.maybe_auto_compact(); 之前插入:

// P0 新增:Stop hook
// 主 agent 完成 turn 时触发。若返回 Continue 且 !stop_hook_active,
// 则递归调用 run_turn(让 Claude 继续执行,如发现未完成项)。
// stop_hook_active = true 时跳过递归(防死循环)。
let stop_ctx = HookContext::for_stop(
    self.stop_hook_active,
    self.session_id.clone(),
    self.cwd.clone(),
    &self.hook_abort_signal,
);
let stop_result = futures::executor::block_on(
    self.hook_runner.run(HookEvent::Stop, &stop_ctx)
);
if stop_result.decision == HookDecision::Continue
    && !self.stop_hook_active
    && !stop_result.is_denied()
{
    // 递归调用,设置 stop_hook_active 防止再次触发 Stop
    self.stop_hook_active = true;
    let result = self.run_turn(user_input.as_str(), prompter.as_deref_mut());
    self.stop_hook_active = false;
    return result;
}
```

**测试用例**:

```rust
#[tokio::test]
async fn test_stop_hook_continue_triggers_recursion() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::Stop, continue_hook("还有任务未完成"))
        .build();
    let _ = runtime.run_turn("do task", None).await.unwrap();
    // 验证 run_turn 被调用 2 次(原 + 递归)
    assert_eq!(runtime.run_turn_call_count(), 2);
}

#[tokio::test]
async fn test_stop_hook_active_prevents_infinite_recursion() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::Stop, continue_hook("永远 continue"))
        .build();
    let _ = runtime.run_turn("do task", None).await.unwrap();
    // 最多递归 1 次
    assert_eq!(runtime.run_turn_call_count(), 2);
}
```

### 7.8 集成点 7:PreCompact(新增)

**位置**: `conversation.rs::maybe_auto_compact` line 2127 之前(`auto_compaction` 触发前),以及 reactive compaction line 1041 之前。

**当前代码**:

```rust
// rust/crates/runtime/src/conversation.rs line 2127-2140
fn maybe_auto_compact(&mut self) -> Option<AutoCompactionEvent> {
    if self.usage_tracker.cumulative_usage().input_tokens
        < self.auto_compaction_input_tokens_threshold
    {
        return None;
    }
    // ...
    let result = compact_session(&self.session, CompactionConfig::default());
    // ...
}
```

**注入的 Hook 调用代码**:

```rust
// 在 if self.usage_tracker... < threshold return None 之后、
// compact_session 调用之前插入:

// P0 新增:PreCompact hook
// 可阻断(放弃本次压缩)或注入自定义摘要
let trigger = "auto"; // 或 "reactive" / "manual"
let compact_ctx = HookContext::for_pre_compact(
    trigger,
    self.session_id.clone(),
    self.cwd.clone(),
    &self.hook_abort_signal,
);
let compact_result = futures::executor::block_on(
    self.hook_runner.run(HookEvent::PreCompact, &compact_ctx)
);
if compact_result.is_denied() {
    // hook 拒绝压缩,跳过本次
    return None;
}
// 应用 hook 注入的摘要(若有)
if let Some(custom_summary) = compact_result.additional_context {
    self.session.custom_compact_summary = Some(custom_summary);
}
```

**测试用例**:

```rust
#[tokio::test]
async fn test_pre_compact_can_deny_compaction() {
    let mut runtime = build_test_runtime()
        .with_auto_compaction_threshold(100) // 容易触发
        .with_hook(HookEvent::PreCompact, deny_hook("保留完整上下文"))
        .build();
    let summary = runtime.run_turn("long conversation...", None).await.unwrap();
    // 验证 auto_compaction 未发生
    assert!(summary.auto_compaction.is_none());
}
```

### 7.9 集成点 8:SubagentStop(新增)

**位置**: `conversation.rs::execute_check_subagent` line 1879,子 agent 进入终态时。

**当前代码**:

```rust
// rust/crates/runtime/src/conversation.rs line 1879 fn execute_check_subagent
fn execute_check_subagent(&mut self, input: &str) -> Result<String, RuntimeError> {
    // ...
    // 子 agent 完成,返回结果
}
```

**注入的 Hook 调用代码**:

```rust
// 在子 agent 进入终态(succeeded / failed / cancelled)时插入:

// P0 新增:SubagentStop hook
// 可阻断(子 agent 结果被丢弃,主 agent 重新 dispatch)
let subagent_stop_ctx = HookContext::for_subagent_stop(
    &subagent_id,
    &subagent_status,
    self.session_id.clone(),
    self.cwd.clone(),
    &self.hook_abort_signal,
);
let stop_result = futures::executor::block_on(
    self.hook_runner.run(HookEvent::SubagentStop, &subagent_stop_ctx)
);
if stop_result.is_denied() {
    // 子 agent 结果被拒绝,主 agent 应重新 dispatch
    return Ok(json!({
        "status": "rejected",
        "reason": stop_result.reason,
        "action": "redispatch",
    }).to_string());
}
```

**测试用例**:

```rust
#[tokio::test]
async fn test_subagent_stop_can_reject_result() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::SubagentStop, deny_hook("结果不合格"))
        .build();
    let _ = runtime.run_turn("dispatch subagent", None).await.unwrap();
    // 验证主 agent 收到 rejected 状态
    assert!(runtime.session.messages.iter().any(|m| {
        m.blocks.iter().any(|b| matches!(b,
            ContentBlock::ToolResult { output, .. } if output.contains("rejected")))
    }));
}
```

---

## 8. 配置文件格式

### 8.1 完整 TOML 配置示例(10 事件 × 4 Handler)

```toml
# .claw/hooks.toml
#
# Claw Code Hooks 配置
# 父文档: docs/ide-hooks-dag-implementation-plan.md
# 本文档: docs/modules/hooks-system-detail.md
#
# 所有事件 + handler 类型的最小配置示例。
# 实际使用时按需保留相关段。

# ───────────────────────────────────────────────────────────────
# 工具层事件
# ───────────────────────────────────────────────────────────────

[[PreToolUse]]
matcher = "Edit|Write|MultiEdit"   # 仅对编辑类工具生效
execution = "sequential"

  [[PreToolUse.hooks]]
  priority = 100
  failure_policy = "failClose"
  enabled = true

  [PreToolUse.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/lint.sh"
  timeout = "30s"

  [[PreToolUse.hooks]]
  priority = 200
  failure_policy = "failOpen"     # 第二个 hook 失败不阻断

  [PreToolUse.hooks.handler]
  type = "inline"
  name = "vendor.audit.file_write"

[[PostToolUse]]
matcher = "Bash"
execution = "sequential"

  [[PostToolUse.hooks]]
  priority = 100

  [PostToolUse.hooks.handler]
  type = "webhook"
  url = "https://audit.internal/api/bash_executed"
  timeout = "10s"
  secret = "audit-secret"

[[PostToolUseFailure]]
matcher = "*"
execution = "sequential"

  [[PostToolUseFailure.hooks]]
  priority = 100

  [PostToolUseFailure.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/on_failure.sh"
  timeout = "30s"

[[PostCustomToolCall]]
matcher = "mcp__.*"
execution = "sequential"

  [[PostCustomToolCall.hooks]]
  priority = 100

  [PostCustomToolCall.hooks.handler]
  type = "inline"
  name = "vendor.mcp.response_redact"

# ───────────────────────────────────────────────────────────────
# 对话层事件
# ───────────────────────────────────────────────────────────────

[[UserPromptSubmit]]
execution = "sequential"

  [[UserPromptSubmit.hooks]]
  priority = 100

  [UserPromptSubmit.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/prompt_check.sh"
  timeout = "5s"

  [[UserPromptSubmit.hooks]]
  priority = 200

  [UserPromptSubmit.hooks.handler]
  type = "inline"
  name = "vendor.context.project_loader"

[[Notification]]
execution = "parallel"             # 通知类事件并行执行,提升吞吐

  [[Notification.hooks]]
  priority = 100
  failure_policy = "failOpen"

  [Notification.hooks.handler]
  type = "webhook"
  url = "https://hooks.slack.com/services/T000/B000/XXX"
  timeout = "10s"

  [[Notification.hooks]]
  priority = 200
  failure_policy = "failOpen"

  [Notification.hooks.handler]
  type = "command"
  command = "notify-send \"$HOOK_MESSAGE\""
  timeout = "5s"

# ───────────────────────────────────────────────────────────────
# 会话层事件
# ───────────────────────────────────────────────────────────────

[[SessionStart]]
execution = "sequential"

  [[SessionStart.hooks]]
  priority = 100

  [SessionStart.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/session_start.sh"
  timeout = "10s"

[[SessionEnd]]
execution = "sequential"

  [[SessionEnd.hooks]]
  priority = 100

  [SessionEnd.hooks.handler]
  type = "webhook"
  url = "https://audit.internal/api/session_end"
  timeout = "10s"

# ───────────────────────────────────────────────────────────────
# Agent 层事件
# ───────────────────────────────────────────────────────────────

[[Stop]]
execution = "sequential"

  [[Stop.hooks]]
  priority = 100

  [Stop.hooks.handler]
  type = "prompt"
  prompt = "Evaluate if the task is complete. Arguments: $ARGUMENTS"
  model = "claude-haiku"
  timeout = "60s"
  temperature = 0.0
  max_tokens = 1024

[[SubagentStop]]
execution = "sequential"

  [[SubagentStop.hooks]]
  priority = 100

  [SubagentStop.hooks.handler]
  type = "inline"
  name = "vendor.subagent.result_validator"

# ───────────────────────────────────────────────────────────────
# 上下文层事件
# ───────────────────────────────────────────────────────────────

[[PreCompact]]
execution = "sequential"

  [[PreCompact.hooks]]
  priority = 100

  [PreCompact.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/before_compact.sh"
  timeout = "10s"
```

### 8.2 JSON 配置示例(嵌入 settings.json)

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|Write",
        "execution": "sequential",
        "hooks": [
          {
            "priority": 100,
            "failure_policy": "failClose",
            "enabled": true,
            "handler": {
              "type": "command",
              "command": "$CLAW_PROJECT_DIR/.claw/hooks/lint.sh",
              "timeout": "30s"
            }
          }
        ]
      }
    ],
    "Stop": [
      {
        "execution": "sequential",
        "hooks": [
          {
            "handler": {
              "type": "prompt",
              "prompt": "Evaluate: $ARGUMENTS",
              "model": "claude-haiku",
              "timeout": "60s"
            }
          }
        ]
      }
    ]
  }
}
```

### 8.3 HookConfig 完整 Schema(Rust 定义)

```rust
// rust/crates/runtime/src/hooks.rs(新增)

/// Hooks 配置根结构。
/// 顶层 key 为 HookEvent(序列化名),value 为 HookMatcher 列表。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HookConfig {
    #[serde(default)]
    pub hooks: BTreeMap<HookEvent, Vec<HookMatcher>>,
}

/// 单个 matcher 组(同一工具名 pattern 下的所有 hook)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookMatcher {
    /// regex pattern(空字符串或 "*" 匹配全部;仅工具类事件适用)
    #[serde(default)]
    pub matcher: String,
    /// 该组下的 hook 列表
    pub hooks: Vec<HookEntry>,
    /// 执行模式:Sequential(默认) / Parallel
    #[serde(default)]
    pub execution: HookExecution,
}

/// 单个 hook 配置(handler + 元数据)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    /// Handler 实例(command / webhook / inline / prompt)
    pub handler: HookHandler,
    /// 优先级(数字越小越先执行,默认 100)
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// 失败策略:FailClose(阻断,默认) / FailOpen(继续)
    #[serde(default)]
    pub failure_policy: FailurePolicy,
    /// 是否启用(默认 true)
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookExecution {
    #[default]
    Sequential,
    Parallel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FailurePolicy {
    #[default]
    FailClose,
    FailOpen,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookDecision {
    #[default]
    Allow,
    Deny,
    Continue,
}

fn default_priority() -> u32 { 100 }
fn default_true() -> bool { true }

impl HookConfig {
    /// 从旧 RuntimeHookConfig 迁移(仅 command handler,无 matcher)。
    /// 迁移期使用,保证向后兼容。
    pub fn from_legacy(legacy: crate::config::RuntimeHookConfig) -> Self {
        let mut hooks = BTreeMap::new();
        if !legacy.pre_tool_use().is_empty() {
            hooks.insert(
                HookEvent::PreToolUse,
                vec![HookMatcher {
                    matcher: String::new(),
                    hooks: legacy.pre_tool_use().iter()
                        .map(|c| HookEntry {
                            handler: HookHandler::Command(CommandHook {
                                command: c.clone(),
                                timeout: default_timeout(),
                                cwd: None,
                                env: BTreeMap::new(),
                            }),
                            priority: 100,
                            failure_policy: FailurePolicy::FailClose,
                            enabled: true,
                        })
                        .collect(),
                    execution: HookExecution::Sequential,
                }],
            );
        }
        // 同样处理 post_tool_use / post_tool_use_failure
        // ...省略,逻辑相同
        Self { hooks }
    }
}
```

---

## 9. 与现有系统的协同

### 9.1 与 policy_engine 的关系

文件: `rust/crates/runtime/src/policy_engine.rs`。

| 维度 | PolicyEngine | HookRunner |
|---|---|---|
| 职责 | Lane 级别策略(DAG 多 Agent 场景),决定 lane 是否阻塞 / 重试 | 单 hook 调用级别的决策 |
| 输入 | `LaneContext`(包含 retry_state / green_contract / approval_token) | `HookContext`(包含 tool_input / prompt 等) |
| 输出 | `PolicyAction`(Block / Allow / Reconcile / Approve) | `HookRunResult`(Allow / Deny / Continue / Failed) |
| 调用时机 | lane 调度时(多 Agent 编排) | run_turn 内 8 个集成点 |
| 关系 | 互补:DAG 层用 PolicyEngine,单 agent 层用 HookRunner | PolicyEngine 可调用 HookRunner 验证 lane 状态 |

协同点:`SubagentStop` hook 触发时,HookRunner 可调用 PolicyEngine 检查 lane 是否满足 Green Contract,据此决定是否 deny 子 agent 结果。

### 9.2 与 plugin_lifecycle 的关系

文件: `rust/crates/runtime/src/plugin_lifecycle.rs`。

Plugin 生命周期事件与 Hook 事件的关系:

| PluginLifecycleEvent | 对应 HookEvent | 触发时机 |
|---|---|---|
| PluginStarted | SessionStart(source="plugin_load") | plugin 加载完成时 |
| PluginStopped | SessionEnd(reason="plugin_unload") | plugin 卸载时 |
| PluginDegraded | Notification(message="plugin X degraded") | plugin 进入降级模式时 |

Plugin 加载时,会通过 `HookRegistry::register` 注册自己的 inline hook(实现 `Hook` trait)。卸载时通过 `unregister` 清理。

### 9.3 与 permission_enforcer 的关系

文件: `rust/crates/runtime/src/permission_enforcer.rs`。

`PermissionEnforcer` 是工具调用前的权限检查,与 `PreToolUse` hook 的关系:

```
工具调用流程:
  1. PreToolUse hook(可改 input / deny)
  2. PermissionEnforcer.check(基于 hook 的 permission_override)
  3. 工具执行
  4. PostToolUse / PostToolUseFailure hook
```

`PreToolUse` hook 可返回 `permission_override`,直接覆盖 `PermissionEnforcer` 的默认决策(详见 `conversation.rs` line 1179-1219 的 `permission_context` 构造)。

### 9.4 与 config / RuntimeFeatureConfig 的关系

文件: `rust/crates/runtime/src/config.rs`。

迁移路径:

```
旧: RuntimeFeatureConfig.hooks() -> &RuntimeHookConfig { pre_tool_use, post_tool_use, post_tool_use_failure }
新: RuntimeFeatureConfig.hooks() -> &HookConfig { hooks: BTreeMap<HookEvent, Vec<HookMatcher>> }
```

迁移期(2 个版本)保留 `RuntimeHookConfig` 字段但标记 `#[deprecated]`,通过 `HookConfig::from_legacy` 兼容旧配置。新配置通过 `hooks.toml` 或 `settings.json["hooks"]` 加载。

### 9.5 与 conversation.rs / LoopDetector 的关系

文件: `rust/crates/runtime/src/conversation.rs` line 746-776。

当前 `LoopDetector` 在 `run_post_tool_use_hook` 中硬编码调用(BUG-2 修复)。迁移后:

- LoopDetector 逻辑提取为内置 inline hook,注册名 `claw.builtin.loop_detector`
- 注册到 `HookRegistry`,通过 `PostToolUse` 事件触发
- 通过 priority=50 保证在其他 PostToolUse hook 之前执行
- failure_policy=`FailClose`(检测到 doom loop 立即短路)

这样既解耦了代码,又允许用户通过 priority 调整 LoopDetector 与其他 hook 的相对顺序。

---

## 10. 实施步骤分解

### 10.1 P0 阶段(8 周)

| 周 | 任务 | 交付物 | 验收标准 |
|---|---|---|---|
| W1 | HookEvent / HookHandler / HookContext 类型扩展 | `hooks.rs` 扩展为 10 事件 / 4 handler / 完整 Context | 编译通过 + 单元测试覆盖 enum 序列化 |
| W2 | Hook trait + HookRegistry | 进程内 hook 注册机制 | 注册/查找/注销单元测试通过 |
| W3 | HookRunner 异步引擎(Sequential + Command handler) | 异步执行 + 超时 + 失败策略 | 集成测试:5 个 command hook 顺序执行 |
| W4 | Webhook + Inline handler | HTTP 调用 + inline 路由 | 集成测试:webhook mock + inline 注册 |
| W5 | run_turn 集成点 1-4(SessionStart / UserPromptSubmit / PreToolUse / PostToolUse) | conversation.rs 接入 4 个事件 | 端到端测试:每个事件触发正确 |
| W6 | run_turn 集成点 5-8(PostCustomToolCall / Stop / PreCompact / SubagentStop) | conversation.rs 接入剩余事件 | 端到端测试:递归 Stop / PreCompact deny |
| W7 | 配置文件加载 + 迁移兼容 | HookConfig + from_legacy + TOML/JSON 解析 | 配置文件解析测试 + 旧配置兼容测试 |
| W8 | LoopDetector 提取为 inline hook + `/hooks` slash 命令 | 解耦 + UI 展示 | LoopDetector 行为不变 + `/hooks` 列出所有注册 hook |

### 10.2 P1 阶段(后续 4 周,非本章节范围)

- Prompt handler(LLM 评估)实现
- Regex matcher(替代当前 simple string contains)
- Webhook retry 机制
- Hook reload(运行期重新加载配置)
- Hook 链调用顺序可视化(`/hooks --trace`)

---

## 11. 测试矩阵

### 11.1 单元测试

| 模块 | 测试用例 | 验证点 |
|---|---|---|
| `HookEvent` | serde 序列化/反序列化 10 事件 | 序列化名与 Claude Code 对齐 |
| `HookEvent::is_blocking` | 10 事件的 blocking 判定 | 7 个 blocking + 3 个 non-blocking |
| `HookEvent::supports_matcher` | 4 个工具类事件 | 仅 PreToolUse / PostToolUse / PostToolUseFailure / PostCustomToolCall 返回 true |
| `HookHandler::timeout` | 4 handler 类型的默认超时 | 30/30/60/60 秒 |
| `HookContext::for_pre_tool_use` | 构造后字段填充 | tool_name/tool_input 正确,其他字段 None |
| `HookRegistry::register/get` | 注册后可查找,注销后 None | 并发场景下 RwLock 正确 |
| `HookRunner::matcher_applies` | 空字符串 / "*" / "Edit\|Write" | 全匹配 / 显式匹配 |
| `HookRunner::merge_and_check_short_circuit` | blocking + Deny / non-blocking + Deny | blocking 短路,non-blocking 不短路 |
| `HookConfig::from_legacy` | 旧 RuntimeHookConfig 转新 HookConfig | 字段完整保留,handler=command |
| `parse_hook_output` | 0/2/其他 exit code | Allow/Deny/Failed 契约 |

### 11.2 集成测试

| 场景 | 测试用例 | 验证点 |
|---|---|---|
| 多 hook 顺序执行 | 5 个 command hook 顺序触发 | 按 priority 升序执行 |
| 多 hook 并行执行 | 5 个 webhook hook 并行 | join_all 等待全部完成 |
| 超时短路 | command hook sleep 60s,timeout=1s | 1s 后 Failed,触发 FailClose |
| FailOpen 继续 | 第一个 hook Failed + FailOpen | 后续 hook 仍执行 |
| FailClose 阻断 | 第一个 hook Failed + FailClose | 后续 hook 不执行 |
| Matcher 过滤 | matcher="Edit\|Write",工具 Bash | 该 hook 不触发 |
| Stop 递归 | Stop hook 返回 Continue | run_turn 调用 2 次 |
| PreCompact deny | PreCompact hook 返回 Deny | auto_compaction 不触发 |

### 11.3 端到端测试

| 场景 | 测试用例 | 验证点 |
|---|---|---|
| 完整 turn 流程 | SessionStart → UserPromptSubmit → PreToolUse → PostToolUse → Stop | 5 个事件按序触发 |
| MCP 工具改写 | PostCustomToolCall hook 改写 output | 主 agent 看到改写后的 output |
| Webhook 通知 | Notification 事件触发 Slack webhook | HTTP 请求带正确签名 |
| 子 agent 拒绝 | SubagentStop hook deny 子 agent 结果 | 主 agent 重新 dispatch |
| 旧配置兼容 | 使用旧 RuntimeHookConfig 格式 | 行为与新 HookConfig 一致 |
| LoopDetector 解耦 | PostToolUse 触发 loop_detector hook | doom loop 检测正常 |

### 11.4 测试代码骨架

```rust
// rust/crates/runtime/src/hooks.rs(tests 模块)

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_test_runtime() -> TestRuntimeBuilder {
        TestRuntimeBuilder::new()
            .with_hook_config(HookConfig::default())
    }

    #[tokio::test]
    async fn test_hook_event_serialization_matches_claude_code() {
        for (event, expected) in [
            (HookEvent::PreToolUse, "PreToolUse"),
            (HookEvent::PostToolUse, "PostToolUse"),
            (HookEvent::UserPromptSubmit, "UserPromptSubmit"),
            (HookEvent::SessionStart, "SessionStart"),
            (HookEvent::Stop, "Stop"),
            (HookEvent::PreCompact, "PreCompact"),
            // ... 其他 4 个事件
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let stripped = json.trim_matches('"');
            assert_eq!(stripped, expected, "event {:?} serde name mismatch", event);
        }
    }

    #[tokio::test]
    async fn test_sequential_execution_order_by_priority() {
        let mut calls = Vec::new();
        let hooks = vec![
            TestHook::new("h1", 200, &mut calls),
            TestHook::new("h2", 100, &mut calls),
            TestHook::new("h3", 150, &mut calls),
        ];
        let mut registry = HookRegistry::new();
        for h in hooks {
            registry.register(Arc::new(h));
        }
        let runner = HookRunner::new(
            build_test_config(),
            Arc::new(registry),
            reqwest::Client::new(),
        );
        let ctx = build_test_ctx(HookEvent::PostToolUse);
        let _ = runner.run(HookEvent::PostToolUse, &ctx).await;
        // 验证按 priority 升序:h2(100) → h3(150) → h1(200)
        assert_eq!(calls, vec!["h2", "h3", "h1"]);
    }

    #[tokio::test]
    async fn test_timeout_triggers_failed_result() {
        let hook = CommandHook {
            command: "sleep 60".to_string(),
            timeout: Duration::from_millis(100),
            cwd: None,
            env: BTreeMap::new(),
        };
        // ... 构造 runner 执行,断言 Failed
    }

    #[tokio::test]
    async fn test_fail_close_short_circuits_chain() {
        // 第一个 hook Failed + FailClose,验证后续 hook 不执行
    }

    #[tokio::test]
    async fn test_stop_hook_recursion_limit() {
        // Stop hook 永远 Continue,验证递归 1 次后停止
    }
}
```

---

## 12. 风险与缓解

### 12.1 async_trait 开销

**风险**:`async_trait` 宏会为每个 trait method 生成 `Pin<Box<dyn Future>>` 堆分配,高频调用场景(如 PreToolUse 每次工具调用)可能产生性能开销。

**缓解措施**:

1. **基准测试**:在 P0 W3 完成后,用 criterion 跑 10000 次 `hook_runner.run` 调用,测量 latency。若 p99 > 1ms,触发优化。
2. **批量化**:对同一事件下多个 inline hook,可考虑 `Vec<Arc<dyn Hook>>` 一次性传入,减少分发次数。
3. **替代方案**:若性能不达标,可改用 `impl Trait` + 静态分发,但会牺牲动态注册能力。
4. **缓存**:对 inline hook,可缓存 `Arc<dyn Hook>` 引用,避免每次 `RwLock::read`。

**触发阈值**:p99 latency > 1ms 或内存分配 > 1KB/call。

### 12.2 Blocking 死锁

**风险**:`run_turn` 当前是同步函数,P0 阶段用 `futures::executor::block_on(self.hook_runner.run(...))` 桥接异步 hook。若 hook 内部又调用 `run_turn`(如 Stop hook 递归),会触发嵌套 `block_on`,导致 deadlock。

**缓解措施**:

1. **P0 禁止 hook 调用 run_turn**:Stop hook 通过返回 `Continue` 决策让外层 run_turn 自己递归,而非 hook 内部递归。
2. **stop_hook_active 防递归**:第一次 Stop hook 触发的递归 run_turn 会设置 `stop_hook_active=true`,该 turn 内的 Stop hook 会跳过递归逻辑。
3. **P1 把 run_turn 改为 async**:从根本上消除 block_on,避免嵌套。
4. **测试覆盖**:`test_stop_hook_recursion_limit` 验证递归深度 = 1。

### 12.3 Hook 链调用顺序

**风险**:同 priority 的 hook 顺序由 `BTreeMap` 迭代顺序决定(不稳定),可能导致 hook 执行顺序与配置文件顺序不一致。

**缓解措施**:

1. **稳定排序**:`sort_by_key` 在 Rust 中是稳定排序,同 priority 保持原始顺序。验证 `entries.sort_by_key(|e| e.priority)` 后顺序与配置一致。
2. **文档警告**:在配置 schema 文档中明确说明同 priority 的 hook 顺序依赖配置文件中的声明顺序,建议显式设置不同 priority。
3. **/hooks --trace**:P1 实现的 trace 命令会展示实际执行顺序,便于调试。

### 12.4 配置文件兼容性

**风险**:从 `RuntimeHookConfig` 迁移到 `HookConfig` 时,旧配置可能丢失字段或解析失败。

**缓解措施**:

1. **from_legacy**:`HookConfig::from_legacy` 完整迁移旧字段,行为一致。
2. **deprecation warning**:旧配置加载时打印 `warning: RuntimeHookConfig is deprecated, please migrate to hooks.toml`。
3. **2 个版本兼容期**:P0 + P1 共 12 周保留旧字段,P2 删除。
4. **配置 schema 测试**:同时测试旧/新配置,行为一致才通过。

### 12.5 Hook 异常崩溃 runtime

**风险**:inline hook 与 runtime 同进程,panic 会传播到 runtime,导致整个会话崩溃。

**缓解措施**:

1. **catch_unwind**:在 `run_inline` 中包装 `std::panic::catch_unwind`,捕获 panic 并转为 `HookRunResult::failed`。
2. **隔离测试**:inline hook 必须提供单元测试,证明无 panic。
3. **command/webhook 优先**:对不信任的 hook,推荐使用 command/webhook 类型(进程隔离)。

```rust
async fn run_inline(&self, hook: &InlineHookRef, ctx: &HookContext<'_>) -> HookRunResult {
    let h = match self.inline_registry.get(&hook.name) {
        Some(h) => h,
        None => return HookRunResult::failed(format!("inline hook not found: {}", hook.name)),
    };
    // 捕获 panic,避免崩溃 runtime
    let ctx_clone = ctx.to_owned();  // 需要 HookContext 实现 Clone
    let h_clone = h.clone();
    match tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(h_clone.execute(&ctx_clone))
    }).await {
        Ok(r) => r,
        Err(join_err) => HookRunResult::failed(format!(
            "inline hook {} panicked: {}",
            hook.name, join_err
        )),
    }
}
```

### 12.6 超时与取消传播

**风险**:`tokio::time::timeout` 取消 future 后,被取消的 command 子进程可能成为僵尸。

**缓解措施**:

1. **Child kill**:timeout 触发时,显式 `child.kill()` 并等待退出。
2. **abort_signal 检查**:command hook 在 stdin 写入前检查 `abort_signal.is_aborted()`,提前退出。
3. **进程组**:用 `setsid` 创建子进程组,超时时 kill 整个组,避免孙子进程残留。

---

## 附录 A: 术语表

| 术语 | 含义 |
|---|---|
| Hook | 在特定事件点执行的回调,可观察 / 修改 / 阻断 runtime 行为 |
| HookEvent | 10 种事件枚举,标识 hook 触发时机 |
| HookHandler | 4 种 handler 类型(command / webhook / inline / prompt) |
| HookContext | 传给 hook 的事件上下文,不可变 |
| HookRunner | 异步执行引擎,调度 hook 链 |
| HookRegistry | inline hook 注册表,支持动态注册 |
| Matcher | regex 工具名过滤,仅工具类事件支持 |
| FailClose | hook 失败时阻断后续执行(默认) |
| FailOpen | hook 失败时继续执行后续 hook |
| Short circuit | 阻断事件 + Deny 时立即返回,不执行剩余 hook |
| Stop hook recursion | Stop hook 返回 Continue 时,run_turn 递归调用自身 |

## 附录 B: 相关文件路径

| 文件 | 路径 | 角色 |
|---|---|---|
| Hooks 主实现 | `rust/crates/runtime/src/hooks.rs` | HookEvent / HookHandler / HookRunner |
| 集成点 | `rust/crates/runtime/src/conversation.rs` | run_turn 中 8 个 hook 接入点 |
| 配置 | `rust/crates/runtime/src/config.rs` | RuntimeHookConfig(待迁移为 HookConfig) |
| 策略引擎 | `rust/crates/runtime/src/policy_engine.rs` | DAG lane 级策略 |
| Plugin 生命周期 | `rust/crates/runtime/src/plugin_lifecycle.rs` | Plugin 加载 / 卸载事件 |
| 权限强制 | `rust/crates/runtime/src/permission_enforcer.rs` | 工具调用权限检查 |
| 主文档 | `docs/ide-hooks-dag-implementation-plan.md` | 父文档(第三章 Hooks 系统方案) |

---

文档结束。
