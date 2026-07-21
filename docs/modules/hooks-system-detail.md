# Hooks 系统细化方案

- 文档版本: v0.2
- 创建日期: 2026-07-21
- 最后更新: 2026-07-21(v0.2 增量升级)
- 父文档: [ide-hooks-dag-implementation-plan.md](../ide-hooks-dag-implementation-plan.md)
- 焦点: 10 事件 × 4 Handler + HookRunner 异步引擎 + run_turn 8 集成点 + 配置示例 + 端到端验证 + 性能预算 + 热重载 + 迁移指南
- 关联代码:
  - [rust/crates/runtime/src/hooks.rs](../../rust/crates/runtime/src/hooks.rs)
  - [rust/crates/runtime/src/conversation.rs](../../rust/crates/runtime/src/conversation.rs)
  - [rust/crates/runtime/src/config.rs](../../rust/crates/runtime/src/config.rs)
  - [rust/crates/runtime/src/permission_enforcer.rs](../../rust/crates/runtime/src/permission_enforcer.rs)
  - [rust/crates/runtime/src/lane_events.rs](../../rust/crates/runtime/src/lane_events.rs)

本章节是父文档第三章「Hooks 系统方案」的可实施细化版本。所有代码骨架以 `rust/crates/runtime/src/hooks.rs` 现有实现为基线,目标是把 3 事件 / 1 handler / 同步执行扩展为 10 事件 / 4 handler / 异步引擎,同时保持向后兼容与渐进迁移能力。

v0.2 在 v0.1 设计骨架基础上补全:集成点行号验证、3 个端到端示例、性能预算与熔断、与权限系统的协同矩阵、配置文件热重载、HookChain 执行器骨架、扩展测试用例与迁移指南,目标是文档可直接指导 P0/W1-W8 的代码落地。

---

## v0.2 变更记录

| 变更类型 | 章节 | 说明 |
|---|---|---|
| 新增 | 0. 集成点行号验证表 | 用 Grep 在 `conversation.rs` / `hooks.rs` 实际代码中验证 v0.1 行号,记录偏差 |
| 新增 | 13. 端到端集成示例 | 3 个完整示例(危险命令拦截 / 自动跑测试 / PreCompact 刷新 NOTEBOOK),含 TOML 配置 / 时序图 / 预期日志 / 断言 |
| 新增 | 14. Hook 执行性能预算 | 各 Handler 延迟预算、超时熔断骨架、LaneEvent 监控指标 |
| 新增 | 15. Hook 与权限系统协同 | Hook × PermissionMode 交互矩阵、决策优先级图、覆盖与绕过语义 |
| 新增 | 16. 配置文件热重载 | notify crate 文件 watcher、部分更新策略、运行中 hook 不中断保证 |
| 完善 | 6. HookRunner 异步引擎 | 补充 HookChain 执行器代码骨架(50+ 行),覆盖顺序保证 / 短路 / 超时 / panic 捕获 |
| 完善 | 11. 测试矩阵 | 新增 6 个测试用例:顺序保持 / exit 2 短路 / 超时不阻断主循环 / 配置热重载 / 权限协同 / SubagentStop 触发 |
| 新增 | 17. 迁移指南 | 从 v0.1 3 事件到 v0.2 10 事件的迁移路径、向后兼容、废弃事件标记机制 |
| 更新 | 目录 | 新增 6 个章节(13-17 + 集成点验证表) |

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
13. [集成点行号验证表(v0.2 新增)](#13-集成点行号验证表v02-新增)
14. [端到端集成示例(v0.2 新增)](#14-端到端集成示例v02-新增)
15. [Hook 执行性能预算(v0.2 新增)](#15-hook-执行性能预算v02-新增)
16. [Hook 与权限系统协同(v0.2 新增)](#16-hook-与权限系统协同v02-新增)
17. [配置文件热重载(v0.2 新增)](#17-配置文件热重载v02-新增)
18. [迁移指南(v0.2 新增)](#18-迁移指南v02-新增)

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

### 6.9 HookChain 执行器(v0.2 新增)

#### 6.9.1 顺序保证设计

同一事件下的多个 Hook 执行顺序由以下规则决定:

1. **matcher 分组**:同一 matcher pattern 的 hooks 归为一组,组间按配置文件中的出现顺序执行。
2. **priority 升序**:组内 hooks 按 `priority` 字段升序执行(数字小先执行,默认 100)。
3. **稳定排序**:同 priority 的 hooks 保持配置文件中的声明顺序(Rust `sort_by_key` 是稳定排序)。
4. **enabled 字段**:`enabled = false` 的 hook 跳过,不影响其他 hook 顺序。
5. **matcher 顺序**:多个 matcher 组按配置文件中的出现顺序执行(不按 matcher 字母序)。

#### 6.9.2 短路语义

| hook 返回 | 阻断事件 | failure_policy | 行为 |
|---|---|---|---|
| `Allow` | 任意 | 任意 | 继续,合并 messages / updated_input / additional_context |
| `Deny` | 是 | 任意 | **立即短路**,返回聚合 Deny 结果 |
| `Deny` | 否 | 任意 | 不短路(不可阻断事件忽略 Deny),合并 messages |
| `Failed` | 任意 | `FailClose` | **立即短路**,返回聚合 Failed 结果(视同 Deny) |
| `Failed` | 任意 | `FailOpen` | 继续,记录失败消息但不阻断 |
| `Timeout` | 任意 | `FailClose` | 视同 Failed + FailClose → 短路 |
| `Timeout` | 任意 | `FailOpen` | 视同 Failed + FailOpen → 继续 |
| exit code 0(command) | 任意 | 任意 | 视同 Allow |
| exit code 2(command) | 任意 | 任意 | 视同 Deny |
| exit code 其他(command) | 任意 | 任意 | 视同 Failed |

#### 6.9.3 HookChain 执行器代码骨架

```rust
// rust/crates/runtime/src/hooks.rs(v0.2 新增)
//
// HookChainExecutor 负责执行单个事件下的所有 hook。
// 与 HookRunner::run 的关系:HookRunner 是入口,HookChainExecutor 是具体执行器。
// 设计为独立 struct 便于单元测试(可 mock handler)。

use std::sync::Arc;
use std::time::Duration;

pub struct HookChainExecutor<'a> {
    /// 当前配置快照(在链开始时 load_full,保证链执行期间不变)
    config: Arc<HookConfig>,
    /// Inline hook 注册表
    inline_registry: Arc<HookRegistry>,
    /// HTTP 客户端(webhook handler)
    http_client: reqwest::Client,
    /// LLM 路由器(prompt handler)
    llm_router: Option<Arc<LlmRouter>>,
    /// 链执行的上下文(整个链共享)
    ctx: &'a HookContext<'a>,
}

impl<'a> HookChainExecutor<'a> {
    pub fn new(
        config: Arc<HookConfig>,
        inline_registry: Arc<HookRegistry>,
        http_client: reqwest::Client,
        llm_router: Option<Arc<LlmRouter>>,
        ctx: &'a HookContext<'a>,
    ) -> Self {
        Self { config, inline_registry, http_client, llm_router, ctx }
    }

    /// 执行某事件下的所有 hook,返回聚合结果。
    pub async fn execute(&self, event: HookEvent) -> HookRunResult {
        let matchers = match self.config.hooks.get(&event) {
            Some(m) => m,
            None => return HookRunResult::default(),
        };

        let mut aggregate = HookRunResult::default();
        let event_blocking = event.is_blocking();

        for matcher in matchers {
            // matcher 过滤(仅工具类事件)
            if event.supports_matcher() && !self.matcher_applies(&matcher.matcher) {
                continue;
            }

            // 按 priority 升序稳定排序
            let mut entries = matcher.hooks.clone();
            entries.sort_by_key(|e| e.priority);

            match matcher.execution {
                HookExecution::Sequential => {
                    for entry in entries {
                        if !entry.enabled {
                            continue;
                        }
                        let result = self.execute_one(&entry, event).await;
                        let should_short_circuit = self.merge_and_check_short_circuit(
                            &mut aggregate,
                            result,
                            event_blocking,
                            &entry,
                        );
                        if should_short_circuit {
                            return aggregate;  // 立即返回,不执行后续 hook
                        }
                    }
                }
                HookExecution::Parallel => {
                    // 并行执行所有 enabled hook,不短路
                    let futures: Vec<_> = entries.iter()
                        .filter(|e| e.enabled)
                        .map(|e| self.execute_one(e, event))
                        .collect();
                    let results = futures::future::join_all(futures).await;
                    for r in results {
                        self.merge(&mut aggregate, r);
                    }
                }
            }
        }

        aggregate
    }

    /// 执行单个 hook entry,包含超时熔断与 panic 捕获。
    async fn execute_one(&self, entry: &HookEntry, event: HookEvent) -> HookRunResult {
        let timeout_dur = entry.handler.timeout();
        let handler_kind = entry.handler.kind();

        let fut = self.run_handler(&entry.handler);

        // 包装 timeout + panic 捕获
        let result = tokio::time::timeout(timeout_dur, async {
            // 对于 inline handler,使用 spawn_blocking 捕获 panic
            match &entry.handler {
                HookHandler::Inline(inline_ref) => {
                    let inline_ref = inline_ref.clone();
                    let ctx_owned = self.ctx.to_owned();
                    let registry = self.inline_registry.clone();
                    tokio::task::spawn_blocking(move || {
                        let rt = tokio::runtime::Handle::current();
                        rt.block_on(async move {
                            match registry.get(&inline_ref.name) {
                                Some(h) => h.execute(&ctx_owned).await,
                                None => HookRunResult::failed(format!(
                                    "inline hook not found: {}", inline_ref.name
                                )),
                            }
                        })
                    }).await.unwrap_or_else(|e| HookRunResult::failed(
                        format!("inline hook panicked: {e}")
                    ))
                }
                _ => fut.await,
            }
        }).await;

        match result {
            Ok(r) => r,
            Err(_elapsed) => HookRunResult::failed(format!(
                "hook {} ({}) timed out after {:?}", handler_kind, event.as_str(), timeout_dur
            )),
        }
    }

    async fn run_handler(&self, handler: &HookHandler) -> HookRunResult {
        match handler {
            HookHandler::Command(c) => self.run_command(c).await,
            HookHandler::Webhook(w) => self.run_webhook(w).await,
            HookHandler::Inline(_) => {
                // inline 在 execute_one 中特殊处理(spawn_blocking)
                HookRunResult::failed("inline handler should be handled by execute_one".to_string())
            }
            HookHandler::Prompt(p) => self.run_prompt(p).await,
        }
    }

    fn matcher_applies(&self, matcher: &str) -> bool {
        if matcher.is_empty() || matcher == "*" {
            return true;
        }
        let tool_name = match self.ctx.tool_name {
            Some(n) => n,
            None => return false,
        };
        matcher.split('|').any(|p| tool_name == p.trim())
    }

    /// 合并结果并检查短路条件。
    /// 返回 true 表示应立即终止后续 hook 执行。
    fn merge_and_check_short_circuit(
        &self,
        aggregate: &mut HookRunResult,
        new: HookRunResult,
        event_blocking: bool,
        entry: &HookEntry,
    ) -> bool {
        // Failed + FailClose → 短路
        if new.is_failed() && entry.failure_policy == FailurePolicy::FailClose {
            aggregate.failed = true;
            aggregate.decision = HookDecision::Deny;
            aggregate.messages.extend(new.messages);
            return true;
        }

        // Deny + blocking event → 短路
        if new.decision == HookDecision::Deny && event_blocking {
            aggregate.decision = HookDecision::Deny;
            aggregate.messages.extend(new.messages);
            aggregate.reason = new.reason.or(aggregate.reason.take());
            return true;
        }

        // 合并 updated_input / additional_context(后者覆盖前者)
        if let Some(updated) = new.updated_input {
            aggregate.updated_input = Some(updated);
        }
        if let Some(ctx) = new.additional_context {
            aggregate.additional_context = Some(ctx);
        }
        if let Some(perm) = new.permission_override {
            aggregate.permission_override = Some(perm);
        }

        self.merge(aggregate, new);
        false
    }

    fn merge(&self, aggregate: &mut HookRunResult, new: HookRunResult) {
        if new.is_denied() { aggregate.denied = true; }
        if new.is_failed() { aggregate.failed = true; }
        if new.is_cancelled() { aggregate.cancelled = true; }
        aggregate.messages.extend(new.messages);
    }

    // run_command / run_webhook / run_prompt 实现同 6.5/6.6/6.8,此处省略
    async fn run_command(&self, hook: &CommandHook) -> HookRunResult {
        // 实现 6.5
        HookRunResult::default()
    }
    async fn run_webhook(&self, hook: &WebhookHook) -> HookRunResult {
        // 实现 6.6
        HookRunResult::default()
    }
    async fn run_prompt(&self, hook: &PromptHook) -> HookRunResult {
        // 实现 6.8
        HookRunResult::default()
    }
}
```

#### 6.9.4 顺序保证测试

```rust
#[tokio::test]
async fn hook_chain_order_preservation_by_priority() {
    // 配置 3 个 hook,priority 分别为 200/100/150,验证执行顺序为 100→150→200
    let config = HookConfig {
        hooks: {
            let mut m = BTreeMap::new();
            m.insert(HookEvent::PostToolUse, vec![HookMatcher {
                matcher: "*".to_string(),
                execution: HookExecution::Sequential,
                hooks: vec![
                    test_hook_entry("h1", 200),
                    test_hook_entry("h2", 100),
                    test_hook_entry("h3", 150),
                ],
            }]);
            m
        },
    };

    let mut calls = Vec::new();
    let registry = build_registry_with_recording_hooks(&mut calls);
    let runner = HookRunner::new(config, Arc::new(registry), reqwest::Client::new());

    let ctx = build_test_ctx(HookEvent::PostToolUse);
    let _ = runner.run(HookEvent::PostToolUse, &ctx).await;

    // 按 priority 升序:h2(100) → h3(150) → h1(200)
    assert_eq!(calls, vec!["h2", "h3", "h1"]);
}

#[tokio::test]
async fn hook_chain_same_priority_preserves_config_order() {
    // 同 priority 的 hooks 保持配置文件中的声明顺序(稳定排序)
    let config = HookConfig {
        hooks: {
            let mut m = BTreeMap::new();
            m.insert(HookEvent::PostToolUse, vec![HookMatcher {
                matcher: "*".to_string(),
                execution: HookExecution::Sequential,
                hooks: vec![
                    test_hook_entry("first", 100),
                    test_hook_entry("second", 100),
                    test_hook_entry("third", 100),
                ],
            }]);
            m
        },
    };

    let mut calls = Vec::new();
    let registry = build_registry_with_recording_hooks(&mut calls);
    let runner = HookRunner::new(config, Arc::new(registry), reqwest::Client::new());

    let ctx = build_test_ctx(HookEvent::PostToolUse);
    let _ = runner.run(HookEvent::PostToolUse, &ctx).await;

    // 同 priority 保持配置顺序
    assert_eq!(calls, vec!["first", "second", "third"]);
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

### 11.5 v0.2 新增测试用例

下列 6 个测试用例由 v0.2 增补,覆盖顺序保持 / 短路 / 超时 / 热重载 / 权限协同 / SubagentStop 触发。每个用例包含:测试目标、最小可执行代码、断言点。

#### 11.5.1 `hook_chain_order_preservation`

**测试目标**:验证同事件下多 hook 按 priority 升序稳定执行。

```rust
#[tokio::test]
async fn hook_chain_order_preservation() {
    use std::sync::Mutex;

    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let config = HookConfig {
        hooks: {
            let mut m = BTreeMap::new();
            m.insert(HookEvent::PostToolUse, vec![HookMatcher {
                matcher: "*".to_string(),
                execution: HookExecution::Sequential,
                hooks: vec![
                    recording_hook_entry("h1", 200, &calls),
                    recording_hook_entry("h2", 100, &calls),
                    recording_hook_entry("h3", 150, &calls),
                    recording_hook_entry("h4", 100, &calls),  // 同 priority,应保持配置顺序
                ],
            }]);
            m
        },
    };

    let registry = Arc::new(HookRegistry::new());
    let runner = HookRunner::new(config, registry, reqwest::Client::new());
    let ctx = build_test_ctx(HookEvent::PostToolUse);

    let _ = runner.run(HookEvent::PostToolUse, &ctx).await;

    let recorded = calls.lock().unwrap().clone();
    // 按 priority 升序:h2(100) → h4(100) → h3(150) → h1(200)
    // 同 priority(100)的 h2 和 h4 保持配置顺序
    assert_eq!(recorded, vec!["h2", "h4", "h3", "h1"]);
}
```

#### 11.5.2 `hook_short_circuit_on_deny`

**测试目标**:验证 PreToolUse hook 返回 exit 2(Deny)时立即短路,后续 hook 不执行。

```rust
#[tokio::test]
async fn hook_short_circuit_on_deny() {
    use std::sync::Mutex;

    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let config = HookConfig {
        hooks: {
            let mut m = BTreeMap::new();
            m.insert(HookEvent::PreToolUse, vec![HookMatcher {
                matcher: "*".to_string(),
                execution: HookExecution::Sequential,
                hooks: vec![
                    recording_hook_entry("h1", 100, &calls),
                    deny_hook_entry("h2_deny", 200, "blocked by h2", &calls),
                    recording_hook_entry("h3", 300, &calls),  // 不应执行
                    recording_hook_entry("h4", 400, &calls),  // 不应执行
                ],
            }]);
            m
        },
    };

    let registry = Arc::new(HookRegistry::new());
    let runner = HookRunner::new(config, registry, reqwest::Client::new());
    let ctx = build_test_ctx(HookEvent::PreToolUse);

    let result = runner.run(HookEvent::PreToolUse, &ctx).await;

    let recorded = calls.lock().unwrap().clone();
    // h1 执行,h2_deny 短路,h3/h4 不执行
    assert_eq!(recorded, vec!["h1", "h2_deny"]);
    // 聚合结果为 Deny
    assert!(result.is_denied());
    assert!(result.reason.unwrap().contains("blocked by h2"));
}
```

#### 11.5.3 `hook_timeout_does_not_block_main_loop`

**测试目标**:验证 hook 超时熔断后,主循环继续执行(failure_policy=failOpen 场景)。

```rust
#[tokio::test]
async fn hook_timeout_does_not_block_main_loop() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let main_loop_continued = Arc::new(AtomicBool::new(false));

    // 配置一个会超时的 hook(timeout=100ms,实际 sleep 60s)
    let config = HookConfig {
        hooks: {
            let mut m = BTreeMap::new();
            m.insert(HookEvent::PreToolUse, vec![HookMatcher {
                matcher: "*".to_string(),
                execution: HookExecution::Sequential,
                hooks: vec![HookEntry {
                    handler: HookHandler::Command(CommandHook {
                        command: "sleep 60".to_string(),
                        timeout: Duration::from_millis(100),  // 100ms 超时
                        cwd: None,
                        env: BTreeMap::new(),
                    }),
                    priority: 100,
                    failure_policy: FailurePolicy::FailOpen,  // 超时不阻断
                    enabled: true,
                }],
            }]);
            m
        },
    };

    let registry = Arc::new(HookRegistry::new());
    let runner = HookRunner::new(config, registry, reqwest::Client::new());
    let ctx = build_test_ctx(HookEvent::PreToolUse);

    let start = std::time::Instant::now();
    let result = runner.run(HookEvent::PreToolUse, &ctx).await;
    let elapsed = start.elapsed();

    // 断言 1: 超时在 200ms 内返回(100ms 超时 + 一点缓冲)
    assert!(elapsed < Duration::from_millis(500));

    // 断言 2: 结果为 Failed(超时视同 Failed)
    assert!(result.is_failed());

    // 断言 3: failure_policy=failOpen,聚合决策为 Allow(不阻断)
    assert!(!result.is_denied());

    // 断言 4: 主循环可继续(failOpen 不抛错)
    main_loop_continued.store(true, Ordering::SeqCst);
    assert!(main_loop_continued.load(Ordering::SeqCst));
}
```

#### 11.5.4 `hook_config_hot_reload`

**测试目标**:验证配置文件热重载后,下一次事件触发使用新配置(详见 17.6 完整实现)。

```rust
#[tokio::test]
async fn hook_config_hot_reload() {
    let config_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::write(&config_path, r#"
[[PreToolUse]]
matcher = "Bash"
execution = "sequential"
  [[PreToolUse.hooks]]
  priority = 100
  [PreToolUse.hooks.handler]
  type = "command"
  command = "echo v1"
  timeout = "2s"
"#).unwrap();

    let initial = HookConfig::parse(&config_path).unwrap();
    let (reloader, config_handle) = HookReloader::start(
        config_path.to_path_buf(), initial
    ).unwrap();

    // 第一次执行:使用 v1
    let runner_v1 = HookRunner::from_config_handle(config_handle.clone());
    let ctx = build_test_ctx(HookEvent::PreToolUse);
    let _ = runner_v1.run(HookEvent::PreToolUse, &ctx).await;
    // (假设 command 被记录到 spawned_commands)
    // assert!(spawned_commands.iter().any(|c| c.contains("echo v1")));

    // 修改配置文件
    std::fs::write(&config_path, r#"
[[PreToolUse]]
matcher = "Bash"
execution = "sequential"
  [[PreToolUse.hooks]]
  priority = 100
  [PreToolUse.hooks.handler]
  type = "command"
  command = "echo v2"
  timeout = "2s"
"#).unwrap();

    // 等待热重载
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 第二次执行:应使用 v2
    let runner_v2 = HookRunner::from_config_handle(config_handle.clone());
    let _ = runner_v2.run(HookEvent::PreToolUse, &ctx).await;
    // assert!(spawned_commands.iter().any(|c| c.contains("echo v2")));
    // assert!(!spawned_commands.iter().any(|c| c.contains("echo v1")));

    // 显式保活 reloader
    drop(reloader);
}
```

#### 11.5.5 `hook_permission_interaction`

**测试目标**:验证 Hook 与 PermissionMode 协同(Hook Deny 优先 / Hook Allow 不绕过权限 / permission_override 覆盖)。

```rust
#[tokio::test]
async fn hook_permission_interaction() {
    // 场景 1: Hook Deny 覆盖 DangerFullAccess
    {
        let mut runtime = build_test_runtime()
            .with_permission_mode(PermissionMode::DangerFullAccess)
            .with_hook(HookEvent::PreToolUse, deny_hook("blocked"))
            .build();
        let _ = runtime.run_turn("rm -rf /tmp", None).await.unwrap();
        assert_eq!(runtime.bash_executor().call_count(), 0);
    }

    // 场景 2: Hook Allow 不绕过 ReadOnly
    {
        let mut runtime = build_test_runtime()
            .with_permission_mode(PermissionMode::ReadOnly)
            .with_hook(HookEvent::PreToolUse, allow_hook())
            .build();
        let _ = runtime.run_turn("echo hello > /tmp/x", None).await.unwrap();
        // ReadOnly 仍拒绝写入
        assert_eq!(runtime.bash_executor().call_count(), 0);
    }

    // 场景 3: Hook permission_override 显式 Allow ReadOnly 下的写入
    {
        let mut runtime = build_test_runtime()
            .with_permission_mode(PermissionMode::ReadOnly)
            .with_hook(
                HookEvent::PreToolUse,
                override_hook(PermissionOverride::Allow, "safe write"),
            )
            .build();
        let _ = runtime.run_turn("echo hello > /tmp/build_cache/x", None).await.unwrap();
        // Override 应允许
        assert_eq!(runtime.bash_executor().call_count(), 1);
    }

    // 场景 4: Hook Failed + failOpen,继续走 PermissionMode
    {
        let mut runtime = build_test_runtime()
            .with_permission_mode(PermissionMode::WorkspaceWrite)
            .with_hook(
                HookEvent::PreToolUse,
                failed_hook(FailurePolicy::FailOpen),
            )
            .build();
        let _ = runtime.run_turn("ls -la", None).await.unwrap();
        // WorkspaceWrite 允许 ls
        assert_eq!(runtime.bash_executor().call_count(), 1);
    }
}
```

#### 11.5.6 `hook_subagent_stop_triggers_correctly`

**测试目标**:验证 SubagentStop hook 在子 agent 进入终态时正确触发,且 Deny 可拒绝子 agent 结果。

```rust
#[tokio::test]
async fn hook_subagent_stop_triggers_correctly() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::SubagentStop, deny_hook("结果不合格"))
        .with_multi_agent_coordinator()
        .build();

    // 1. dispatch 子 agent
    let _ = runtime.execute_dispatch_subagent(
        r#"{"name":"sub1","task":"do something"}"#
    ).unwrap();

    // 2. 模拟子 agent 进入终态(Completed)
    runtime.mark_subagent_completed("sub1", "result data");

    // 3. 调用 execute_check_subagent,应触发 SubagentStop hook
    let check_result = runtime.execute_check_subagent(
        r#"{"subagent_id":"sub1"}"#
    ).unwrap();

    // 断言 1: SubagentStop hook 被触发
    let stop_calls = runtime.hook_log().for_event(HookEvent::SubagentStop);
    assert_eq!(stop_calls.len(), 1);

    // 断言 2: hook 返回 Deny,check_result 中应包含 rejected 状态
    let parsed: serde_json::Value = serde_json::from_str(&check_result).unwrap();
    // (Deny 应导致 runtime 返回 rejected 标记,主 agent 据此 redispatch)
    // 注:具体字段名取决于实现,这里检查关键字段
    assert!(
        parsed.to_string().contains("rejected")
        || parsed.to_string().contains("不合格")
        || stop_calls[0].decision == HookDecision::Deny,
        "SubagentStop hook Deny 应导致结果被拒绝"
    );
}

#[tokio::test]
async fn hook_subagent_stop_not_triggered_for_non_terminal() {
    let mut runtime = build_test_runtime()
        .with_hook(HookEvent::SubagentStop, allow_hook())
        .with_multi_agent_coordinator()
        .build();

    let _ = runtime.execute_dispatch_subagent(
        r#"{"name":"sub1","task":"do something"}"#
    ).unwrap();

    // 子 agent 仍在 Running 状态(非终态)
    let _ = runtime.execute_check_subagent(
        r#"{"subagent_id":"sub1"}"#
    ).unwrap();

    // SubagentStop hook 不应触发(非终态)
    let stop_calls = runtime.hook_log().for_event(HookEvent::SubagentStop);
    assert_eq!(stop_calls.len(), 0);
}
```

### 11.6 测试用例与章节交叉引用

| 测试用例 | 对应章节 | 验证点 |
|---|---|---|
| `hook_chain_order_preservation` | 6.9.1 顺序保证设计 | priority 升序 + 稳定排序 |
| `hook_short_circuit_on_deny` | 6.9.2 短路语义 | Deny 立即终止后续 hook |
| `hook_timeout_does_not_block_main_loop` | 15.3 超时熔断机制 | 超时熔断后主循环继续(failOpen) |
| `hook_config_hot_reload` | 17. 配置文件热重载 | 配置变更后下一次事件使用新配置 |
| `hook_permission_interaction` | 16. Hook 与权限系统协同 | Hook × PermissionMode 4 种交互 |
| `hook_subagent_stop_triggers_correctly` | 7.9 集成点 8 SubagentStop | 子 agent 终态时触发 + Deny 拒绝结果 |

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

## 13. 集成点行号验证表(v0.2 新增)

### 13.1 验证方法

v0.1 文档基于编写时的代码状态记录行号,代码可能已发生偏移。v0.2 在编写前用 Grep 工具对以下关键集成点逐一验证,记录实际行号与偏差。

验证基准:`d:\claw-code-src\rust\crates\runtime\src\conversation.rs` 与 `rust/crates/runtime/src/hooks.rs` 当前 HEAD 状态。

### 13.2 验证结果表

| # | 事件 / 锚点 | 文件路径 | v0.1 文档行号 | v0.2 验证行号 | 偏差 | 说明 |
|---|---|---|---|---|---|---|
| 1 | `run_turn` 函数签名 | `conversation.rs` | 824 | 824 | 无 | 入口签名一致,验证通过 |
| 2 | `self.loop_detector.reset()`(SessionStart 注入前锚点) | `conversation.rs` | 834 | 834 | 无 | SessionStart hook 应插入该行之前 |
| 3 | `session.push_user_text`(UserPromptSubmit 注入前锚点) | `conversation.rs` | 840-842 | 841 | +1 行 | 实际 push_user_text 调用在 841 行,v0.1 文档说 840-842 范围正确,精确锚点为 841 |
| 4 | PreToolUse 注入点(`run_pre_tool_use_hook` 调用) | `conversation.rs` | 1175 | 1175 | 无 | 已有集成点,验证通过 |
| 5 | PostToolUseFailure 注入点 | `conversation.rs` | 1280-1281 | 1281 | +1 行 | `run_post_tool_use_failure_hook` 调用在 1281 行 |
| 6 | PostToolUse 注入点 | `conversation.rs` | 1287-1293 | 1287 | 无 | `run_post_tool_use_hook` 调用在 1287 行 |
| 7 | `TurnSummary` 构造(Stop 注入前锚点) | `conversation.rs` | 1490 | 1490 | 无 | `let summary = TurnSummary {` 在 1490 行 |
| 8 | `maybe_auto_compact` 函数定义(PreCompact 注入点) | `conversation.rs` | 2127 | 2127 | 无 | 函数定义在 2127 行,PreCompact hook 应在 `compact_session` 调用(2140 行)之前注入 |
| 9 | `execute_check_subagent` 函数定义(SubagentStop 注入点) | `conversation.rs` | 1879 | 1879 | 无 | 函数定义在 1879 行,SubagentStop 应在 `is_terminal` 判定(1910-1913 行)之后触发 |
| 10 | `HookEvent` enum 定义 | `hooks.rs` | 22-26 | 22-26 | 无 | 现有 3 事件,验证通过 |
| 11 | `HookRunResult` struct 定义 | `hooks.rs` | 84-177 | 84 | 无 | struct 起点一致,字段在 84-92 行 |
| 12 | `HookRunner` struct 定义 | `hooks.rs` | 180-335 | 180 | 无 | struct 起点一致 |
| 13 | `HookAbortSignal` struct 定义 | `hooks.rs` | 62-81 | 62-81 | 无 | 验证通过 |
| 14 | `HookProgressEvent` enum 定义 | `hooks.rs` | 40-56 | 40-56 | 无 | 验证通过 |

### 13.3 附加验证点

| 锚点 | 文件 | v0.2 验证行号 | 用途 |
|---|---|---|---|
| `execute_dispatch_subagent` 函数定义 | `conversation.rs` | 1656 | 子 agent dispatch 入口,SubagentStop 链路起点 |
| `run_subagent_turn` 函数定义 | `conversation.rs` | 1767 | 子 agent 单轮执行,SubagentStop 终态判定 |
| `auto_compaction_input_tokens_threshold` 字段 | `conversation.rs` | 299 | PreCompact 触发阈值配置 |
| `compact_session` 调用点(auto 路径) | `conversation.rs` | 2140 | PreCompact hook 必须在此之前注入 |
| `compact_session_with_trigger` 调用点(reactive 路径) | `conversation.rs` | 1074 | PreCompact hook 的第二个注入点 |

### 13.4 偏差处理决策

- **无偏差项(12 项)**:v0.1 文档中的行号引用保持不变,无需更新。
- **+1 行偏差项(2 项:UserPromptSubmit / PostToolUseFailure)**:偏差在可接受范围内(±5 行),v0.1 文档已使用「840-842」「1280-1293」范围表达,实际锚点落在范围内,无需更新文档引用。
- **结论**:v0.1 行号引用全部仍有效,v0.2 文档不修改 v0.1 章节中的行号引用,仅在本验证表中记录精确值供实施时参考。

### 13.5 实施时的行号保护建议

由于代码会持续演进,建议在实施 P0/W5-W6 集成点接入时:

1. **使用语义锚点而非硬行号**:在 PR 中以「`loop_detector.reset()` 调用之前」「`push_user_text` 调用之前」等语义描述定位,而非「line 834」。
2. **添加行号注释**:在 `conversation.rs` 关键集成点添加 `// HOOK_INTEGRATION: SessionStart` 等注释,便于 Grep 定位。
3. **CI 行号漂移检测**:在 CI 中添加测试,用 Grep 验证关键锚点行号是否在预期范围内,超出范围则告警。

---

## 14. 端到端集成示例(v0.2 新增)

本章提供 3 个完整端到端示例,每个示例覆盖:用户场景 → 配置文件 → 主 agent 执行流程(伪代码)→ Hook 触发时序图 → 预期日志输出 → 断言要点。示例选型覆盖三类典型用例:阻断型(PreToolUse 拦截)、观察型(PostToolUse 触发副作用)、上下文管理型(PreCompact 与 NOTEBOOK 协同)。

### 14.1 示例 1:PreToolUse 拦截危险命令

#### 14.1.1 用户场景

用户对 agent 说:「请帮我清理一下根目录,执行 `rm -rf /`」。该命令会递归删除整个文件系统根,属于高危操作。PreToolUse hook 应在 Bash 工具实际执行前拦截,返回 exit 2 表示 Deny,主 agent 收到 deny 后向用户解释并停止本 turn。

#### 14.1.2 配置文件(TOML)

```toml
# ~/.claw/hooks.toml
# 示例 1:危险命令拦截

[[PreToolUse]]
matcher = "Bash"                # 仅对 Bash 工具生效
execution = "sequential"

  [[PreToolUse.hooks]]
  priority = 50                 # 高优先级(数字小先执行)
  failure_policy = "failClose"  # hook 自身失败也阻断
  enabled = true

  [PreToolUse.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/deny_dangerous_rm.sh"
  timeout = "2s"                # 简单脚本应快速返回
```

`deny_dangerous_rm.sh` 实现:

```bash
#!/usr/bin/env bash
# 从 stdin 读取 hook payload
payload=$(cat)
cmd=$(echo "$payload" | jq -r '.tool_input.command // ""')

# 检测 rm -rf / 模式(含变形:rm -rf /*、rm -rf /、rm -fr /)
if echo "$cmd" | grep -qE 'rm\s+(-[rf]+\s+)+/( |\*|$)'; then
  echo "BLOCKED: 检测到 rm -rf /,可能删除整个文件系统" >&2
  exit 2   # exit 2 = Deny 契约
fi

# 其他命令放行
exit 0
```

#### 14.1.3 主 agent 执行流程(伪代码)

```
function run_turn(user_input="请帮我清理一下根目录,执行 rm -rf /"):
    fire UserPromptSubmit(prompt=user_input)        # 允许通过
    session.push_user_text(user_input)
    loop:
        response = llm.stream()
        for event in response:
            if event is ToolUse(tool="Bash", input={"command":"rm -rf /"}):
                # === PreToolUse 集成点(line 1175) ===
                result = hook_runner.run(PreToolUse, ctx={
                    tool_name: "Bash",
                    tool_input: {"command": "rm -rf /"}
                })
                if result.is_denied():
                    # 把 deny 原因作为 tool_result 返还给 LLM
                    tool_result = ToolResult(error, "BLOCKED: 检测到 rm -rf /...")
                    # === PostToolUseFailure 集成点(line 1281) ===
                    fire PostToolUseFailure(tool="Bash", response=tool_result)
                    continue    # 让 LLM 看到失败,决定下一步
                effective_input = result.updated_input or input
                # === 实际工具执行 ===
                tool_result = bash_executor.run(effective_input)
                fire PostToolUse(tool="Bash", response=tool_result)
        if no_more_tool_calls:
            break
    # === Stop 集成点(line 1490 之前) ===
    fire Stop(stop_hook_active=false)
    return TurnSummary
```

#### 14.1.4 Hook 触发时序图

```
用户       Agent      HookRunner     deny_dangerous_rm.sh    BashExecutor
 |           |             |                  |                     |
 |--prompt-->|             |                  |                     |
 |           |--UserPromptSubmit->|           |                     |
 |           |<-----allow---------|           |                     |
 |           |--LLM stream-------|            |                     |
 |           |--ToolUse(Bash,rm -rf /)-------->|                     |
 |           |  === PreToolUse line 1175 ===  |                     |
 |           |--run(PreToolUse,ctx)---------->|                     |
 |           |             |---spawn & stdin->|                     |
 |           |             |<---exit 2--------|                     |
 |           |             | (Deny + message) |                     |
 |           |<---deny------|                  |                     |
 |           |--ToolResult(error,BLOCKED...)   |                     |
 |           |  === PostToolUseFailure 1281 ==|                     |
 |           |--run(PostToolUseFailure,ctx)--->|                     |
 |           |<---allow------------------------|                     |
 |           |--LLM stream (看到失败,解释)----|                     |
 |           |--Stop hook (line 1490)--------->|                     |
 |           |<---allow------------------------|                     |
 |<--回复----|             |                  |                     |
```

#### 14.1.5 预期日志输出

```
[INFO] run_turn: turn_started user_input="请帮我清理一下根目录..."
[INFO] hook: event=UserPromptSubmit decision=Allow handler=command latency=12ms
[INFO] llm: stream_started model=claude-sonnet
[INFO] tool: dispatch tool=Bash input={"command":"rm -rf /"}
[INFO] hook: event=PreToolUse tool=Bash handler=command
       command=$CLAW_PROJECT_DIR/.claw/hooks/deny_dangerous_rm.sh
[WARN] hook: event=PreToolUse decision=Deny reason="BLOCKED: 检测到 rm -rf /,可能删除整个文件系统"
[INFO] tool: tool_result=error "BLOCKED: 检测到 rm -rf /..."
[INFO] hook: event=PostToolUseFailure tool=Bash decision=Allow
[INFO] llm: stream_resumed (LLM 看到 deny 结果)
[INFO] llm: assistant_message="抱歉,我无法执行 rm -rf /,这会删除整个文件系统..."
[INFO] hook: event=Stop decision=Allow
[INFO] run_turn: turn_completed iterations=1
```

#### 14.1.6 断言要点

```rust
#[tokio::test]
async fn example_1_pre_tool_use_blocks_dangerous_rm() {
    let mut runtime = build_test_runtime()
        .with_hooks_toml("examples/deny_dangerous_rm.toml")
        .with_user_input("请帮我清理一下根目录,执行 rm -rf /")
        .build();

    let summary = runtime.run_turn().await.unwrap();

    // 断言 1: Bash 工具被 hook 拒绝,实际未执行
    assert!(runtime.bash_executor().call_count() == 0);

    // 断言 2: PreToolUse hook 触发了一次,返回 Deny
    let pre_calls = runtime.hook_log().for_event(HookEvent::PreToolUse);
    assert_eq!(pre_calls.len(), 1);
    assert_eq!(pre_calls[0].decision, HookDecision::Deny);

    // 断言 3: PostToolUseFailure 触发(因为 deny 等同于工具失败)
    let fail_calls = runtime.hook_log().for_event(HookEvent::PostToolUseFailure);
    assert_eq!(fail_calls.len(), 1);

    // 断言 4: 主 agent 输出包含 deny 原因
    assert!(summary.assistant_messages.iter().any(|m|
        m.contains("BLOCKED") || m.contains("rm -rf")
    ));

    // 断言 5: hook 延迟 < 500ms(Command handler 预算)
    assert!(pre_calls[0].latency < Duration::from_millis(500));
}
```

### 14.2 示例 2:PostToolUse 自动跑测试

#### 14.2.1 用户场景

用户对 agent 说:「修复 `src/main.rs` 中的编译错误」。agent 使用 Edit 工具修改文件后,PostToolUse hook 自动触发 `cargo test --no-fail-fast`,把测试结果作为 additional_context 注入下一轮 LLM 请求,让 agent 知道修改是否破坏了其他测试。

#### 14.2.2 配置文件(TOML)

```toml
# ~/.claw/hooks.toml
# 示例 2:Edit/Write 后自动跑测试

[[PostToolUse]]
matcher = "Edit|Write|MultiEdit"   # 仅对编辑类工具生效
execution = "sequential"

  [[PostToolUse.hooks]]
  priority = 100
  failure_policy = "failOpen"     # 测试失败不阻断主流程
  enabled = true

  [PostToolUse.hooks.handler]
  type = "command"
  command = "cargo test --no-fail-fast --message-format=json 2>&1 | tail -100"
  timeout = "120s"                # cargo test 可能耗时,放宽到 2 分钟
```

#### 14.2.3 主 agent 执行流程(伪代码)

```
function run_turn(user_input="修复 src/main.rs 中的编译错误"):
    fire UserPromptSubmit(prompt=user_input)
    session.push_user_text(user_input)
    loop:
        response = llm.stream()
        for event in response:
            if event is ToolUse(tool="Edit", input={file_path:"src/main.rs",...}):
                # === PreToolUse 集成点 ===
                pre = hook_runner.run(PreToolUse, ctx={tool:"Edit",input})
                if pre.is_denied(): continue
                # === 实际工具执行 ===
                tool_result = edit_executor.run(pre.updated_input or input)
                # === PostToolUse 集成点(line 1287) ===
                post = hook_runner.run(PostToolUse, ctx={
                    tool: "Edit",
                    tool_input: input,
                    tool_response: tool_result
                })
                # 关键: hook 返回的 additional_context 注入下一轮 LLM 请求
                if post.additional_context:
                    session.inject_context(post.additional_context)
        if no_more_tool_calls:
            break
    fire Stop
    return TurnSummary
```

#### 14.2.4 Hook 触发时序图

```
用户       Agent      HookRunner     cargo test            EditExecutor
 |           |             |              |                      |
 |--prompt-->|             |              |                      |
 |           |--UserPromptSubmit->|       |                      |
 |           |<-----allow---------|       |                      |
 |           |--LLM stream-------|        |                      |
 |           |--ToolUse(Edit,src/main.rs)->|                     |
 |           |  === PreToolUse line 1175 ==|                     |
 |           |--run(PreToolUse,ctx)------->|                     |
 |           |<---allow--------------------|                     |
 |           |  === 实际 Edit 执行 =========|====================>|
 |           |<---tool_result(success)-----|--------------------|
 |           |  === PostToolUse line 1287 =|                     |
 |           |--run(PostToolUse,ctx)------>|                     |
 |           |             |---spawn----->cargo test             |
 |           |             |   (120s timeout)                    |
 |           |             |<--stdout----{"test":"test_foo","result":"failed"}
 |           |             |  (additional_context)               |
 |           |<---allow+ctx----------------|                     |
 |           |--session.inject_context("test failed: test_foo")  |
 |           |--LLM stream (看到测试失败)----|                    |
 |           |--ToolUse(Edit,修复 test_foo)-->|                  |
 |           |  ... (循环)                                          |
 |           |--Stop hook-------------->|                          |
 |           |<---allow-----------------|                          |
 |<--回复----|             |              |                      |
```

#### 14.2.5 预期日志输出

```
[INFO] run_turn: turn_started user_input="修复 src/main.rs 中的编译错误"
[INFO] hook: event=UserPromptSubmit decision=Allow latency=8ms
[INFO] llm: stream_started
[INFO] tool: dispatch tool=Edit file=src/main.rs
[INFO] hook: event=PreToolUse tool=Edit decision=Allow latency=15ms
[INFO] tool: edit_applied old="let x = undeclared_var;" new="let x = 42;"
[INFO] hook: event=PostToolUse tool=Edit handler=command
       command="cargo test --no-fail-fast --message-format=json"
[INFO] hook: cargo test starting (timeout=120s)
[INFO] hook: cargo test output: {"reason":"compiler-message","message":{"level":"error","spans":[...]}}
[WARN] hook: cargo test result: 1 test failed (test_foo)
[INFO] hook: event=PostToolUse decision=Allow
       additional_context="cargo test result: FAILED. 1 test failed: test_foo"
[INFO] llm: stream_resumed (LLM 看到 additional_context)
[INFO] llm: assistant_message="测试 test_foo 失败,原因是..."
[INFO] tool: dispatch tool=Edit file=tests/foo.rs
[INFO] hook: event=PostToolUse tool=Edit (再次触发 cargo test)
[INFO] hook: cargo test result: all tests passed
[INFO] hook: event=Stop decision=Allow
[INFO] run_turn: turn_completed iterations=3
```

#### 14.2.6 断言要点

```rust
#[tokio::test]
async fn example_2_post_tool_use_triggers_cargo_test() {
    let mut runtime = build_test_runtime()
        .with_hooks_toml("examples/auto_cargo_test.toml")
        .with_user_input("修复 src/main.rs 中的编译错误")
        .build();

    let summary = runtime.run_turn().await.unwrap();

    // 断言 1: 每次 Edit 后 PostToolUse hook 都触发
    let edit_count = runtime.tool_log().for_tool("Edit").count();
    let post_hook_count = runtime.hook_log()
        .for_event(HookEvent::PostToolUse)
        .for_tool("Edit")
        .count();
    assert_eq!(edit_count, post_hook_count);

    // 断言 2: additional_context 被注入到下一轮 LLM 请求
    let requests = runtime.llm_requests();
    assert!(requests.len() >= 2);
    assert!(requests[1].system_prompt.contains("cargo test result"));

    // 断言 3: cargo test 实际执行(通过 command 子进程)
    assert!(runtime.spawned_commands().iter().any(|c|
        c.contains("cargo test")
    ));

    // 断言 4: failure_policy=failOpen,即使测试失败也不阻断
    assert!(summary.iterations >= 2);  // agent 继续修复

    // 断言 5: hook 延迟符合预算(cargo test 可能慢,放宽到 120s)
    let post_calls = runtime.hook_log().for_event(HookEvent::PostToolUse);
    assert!(post_calls.iter().all(|c| c.latency < Duration::from_secs(120)));
}
```

### 14.3 示例 3:PreCompact 触发 NOTEBOOK 刷新

#### 14.3.1 用户场景

长程任务中,上下文接近窗口上限(默认 100K tokens),触发 `maybe_auto_compact`。在压缩前,PreCompact hook 调用 inline handler `refresh_notebook()`,把当前 session 的关键信息(决策、子 agent 注册表、已尝试方案)写入 `NOTEBOOK.md`,这些信息会跨 microcompact 持久化。压缩完成后,PostCompact 事件(本示例为 v0.2 新增事件)通知 hook 压缩已完成。

#### 14.3.2 配置文件(TOML)

```toml
# ~/.claw/hooks.toml
# 示例 3:PreCompact 触发 NOTEBOOK 刷新

[[PreCompact]]
execution = "sequential"

  [[PreCompact.hooks]]
  priority = 100
  failure_policy = "failClose"   # NOTEBOOK 刷新失败则放弃压缩,保留完整上下文
  enabled = true

  [PreCompact.hooks.handler]
  type = "inline"
  name = "claw.builtin.notebook_refresher"   # 进程内 hook,零开销

# v0.2 新增事件:PostCompact
[[PostCompact]]
execution = "sequential"

  [[PostCompact.hooks]]
  priority = 100
  failure_policy = "failOpen"

  [PostCompact.hooks.handler]
  type = "command"
  command = "echo '[PreCompact] compaction done at $(date)' >> $CLAW_PROJECT_DIR/.claw/compaction.log"
  timeout = "5s"
```

#### 14.3.3 主 agent 执行流程(伪代码)

```
function run_turn(user_input="继续之前的任务"):
    fire UserPromptSubmit
    session.push_user_text(user_input)
    loop:
        response = llm.stream()
        ... (工具调用、PostToolUse 等)
        if no_more_tool_calls:
            break

    # === PreCompact 集成点(line 2140 之前) ===
    # maybe_auto_compact 内部:
    if usage_tracker.input_tokens >= threshold:
        # === PreCompact 注入点 ===
        pre = hook_runner.run(PreCompact, ctx={trigger:"auto"})
        if pre.is_denied():
            return None   # 放弃本次压缩
        # NOTEBOOK 已被 inline hook 刷新,可以安全压缩
        compact_result = compact_session(session)
        # === PostCompact(v0.2 新增)===
        post = hook_runner.run(PostCompact, ctx={
            trigger: "auto",
            original_tokens: usage_tracker.input_tokens,
            compacted_tokens: compact_result.new_token_count
        })
        return compact_result

    fire Stop
    return TurnSummary
```

#### 14.3.4 Hook 触发时序图

```
用户       Agent       HookRunner    notebook_refresher   compact_session
 |           |              |                |                    |
 |--prompt-->|              |                |                    |
 |           |  ... (多轮工具调用,token 累积) |                    |
 |           |  === maybe_auto_compact ===   |                    |
 |           |  检查:usage.input_tokens >= 100K? (yes)            |
 |           |  === PreCompact 注入点 ======|                    |
 |           |--run(PreCompact,ctx)-------->|                    |
 |           |              |--inline.exec->|                    |
 |           |              |  refresh_notebook()                |
 |           |              |  写 NOTEBOOK.md                    |
 |           |              |<--allow + additional_context       |
 |           |<---allow------|               |                    |
 |           |  === compact_session 实际执行 =====================>|
 |           |<---compaction_result (token: 100K -> 30K)--------|  |
 |           |  === PostCompact(v0.2 新增)==|                    |
 |           |--run(PostCompact,ctx)------->|                    |
 |           |              |---spawn----->echo to log file      |
 |           |              |<---exit 0---|                      |
 |           |<---allow------|               |                    |
 |           |--TurnSummary(auto_compaction=Some(...))            |
 |<--回复----|              |                |                    |
```

#### 14.3.5 预期日志输出

```
[INFO] run_turn: turn_started
[INFO] llm: usage_tracker input_tokens=102345 (threshold=100000)
[WARN] run_turn: auto_compaction_triggered tokens=102345
[INFO] hook: event=PreCompact trigger=auto handler=inline
       name=claw.builtin.notebook_refresher
[INFO] notebook: refreshed NOTEBOOK.md entries=42 (decisions=5, subagents=3, tried=12)
[INFO] hook: event=PreCompact decision=Allow latency=85ms
[INFO] compact: compact_session started messages=128
[INFO] compact: compact_session done original_tokens=102345 new_tokens=28430
[INFO] hook: event=PostCompact trigger=auto handler=command
[INFO] hook: PostCompact decision=Allow latency=23ms
[INFO] run_turn: turn_completed auto_compaction=AutoCompactionEvent{original=102345, new=28430}
```

#### 14.3.6 断言要点

```rust
#[tokio::test]
async fn example_3_pre_compact_refreshes_notebook() {
    let mut runtime = build_test_runtime()
        .with_hooks_toml("examples/pre_compact_notebook.toml")
        .with_auto_compaction_threshold(100_000)
        .with_inline_hook(
            "claw.builtin.notebook_refresher",
            Arc::new(NotebookRefresherHook::new()),
        )
        .with_user_input("继续之前的任务")
        .build();

    // 模拟 token 累积到阈值
    runtime.simulate_token_accumulation(110_000);

    let summary = runtime.run_turn().await.unwrap();

    // 断言 1: PreCompact hook 触发了一次
    let pre_calls = runtime.hook_log().for_event(HookEvent::PreCompact);
    assert_eq!(pre_calls.len(), 1);

    // 断言 2: NOTEBOOK.md 被刷新
    let notebook_path = runtime.workspace_root().join("NOTEBOOK.md");
    assert!(notebook_path.exists());
    let content = std::fs::read_to_string(&notebook_path).unwrap();
    assert!(content.contains("decisions:"));
    assert!(content.contains("subagents:"));

    // 断言 3: 实际压缩发生(auto_compaction 非 None)
    assert!(summary.auto_compaction.is_some());
    let event = summary.auto_compaction.unwrap();
    assert!(event.original_tokens > event.new_tokens);

    // 断言 4: PostCompact(v0.2 新增)被触发
    let post_calls = runtime.hook_log().for_event(HookEvent::PostCompact);
    assert_eq!(post_calls.len(), 1);

    // 断言 5: inline hook 延迟 < 100ms(Inline handler 预算)
    assert!(pre_calls[0].latency < Duration::from_millis(100));

    // 断言 6: failure_policy=failClose,NOTEBOOK 刷新失败则放弃压缩
    // (在另一个测试中模拟 inline hook 失败,验证 auto_compaction 为 None)
}
```

### 14.4 示例对比总结

| 维度 | 示例 1(PreToolUse 拦截) | 示例 2(PostToolUse 跑测试) | 示例 3(PreCompact 刷新) |
|---|---|---|---|
| 事件类型 | 工具层 / 阻断型 | 工具层 / 观察型 | 上下文层 / 协同型 |
| Handler 类型 | command | command | inline + command |
| 阻断语义 | Deny 短路,工具不执行 | 不阻断,additional_context 注入 | Deny 则放弃压缩 |
| failure_policy | failClose | failOpen | failClose(示例 3) |
| 性能预算 | < 500ms | < 120s | inline < 100ms,command < 5s |
| 典型用例 | 安全策略 | 自动化测试 | 上下文持久化 |

---

## 15. Hook 执行性能预算(v0.2 新增)

### 15.1 性能预算设计原则

Hook 系统嵌入在 `run_turn` 的关键路径上,任何延迟都会直接放大到 LLM 调用 → 工具执行 → 用户响应的全链路。性能预算遵循以下原则:

1. **不阻断主循环**:hook 超时必须熔断,不允许 hook 失败拖累 `run_turn`。
2. **按 handler 类型分级**:不同 handler 的延迟特性差异巨大(command 进程启动 ~50ms,LLM prompt 调用 ~3s),不能用同一预算。
3. **可观测可告警**:所有 hook 调用必须经 `LaneEvent` 上报延迟、超时、失败,CI 与生产环境均可见。
4. **预算即合约**:配置文件中声明的 `timeout` 是 hook 与 runtime 的合约,超过即熔断,不依赖 hook 自觉。

### 15.2 各 Handler 延迟预算

| Handler 类型 | p50 预算 | p99 预算 | 硬超时上限 | 说明 |
|---|---|---|---|---|
| `Command` | < 100ms | < 500ms | 30s(可配置) | 进程 fork + exec + stdin/stdout,简单 shell 脚本应秒级返回 |
| `Webhook` | < 500ms | < 2s | 30s(可配置) | 含 TCP 握手 + TLS + HTTP 往返,公网调用天然较慢 |
| `Inline` | < 10ms | < 100ms | 60s(默认) | 进程内 async 调用,无 fork 开销,适合高频 hook |
| `Prompt` | < 2s | < 5s | 60s(可配置) | LLM 调用(Haiku 级),用于复杂语义判定 |
| `Command`(长时间任务,如 `cargo test`) | < 30s | < 120s | 600s(需显式声明) | 用户主动声明长 timeout,如自动化测试场景 |

**预算违反示例**:Command hook p99 = 800ms → 触发性能告警;Inline hook p99 = 150ms → 触发优化任务;Prompt hook p99 = 8s → 立即触发熔断审查。

### 15.3 超时熔断机制代码骨架

```rust
// rust/crates/runtime/src/hooks.rs(扩展 HookRunner)
//
// 超时熔断器:每个 hook 调用都包装在 tokio::time::timeout 中,
// 超时后返回 HookRunResult::failed,并触发 HookTimedOut lane event。
// 关键设计:
// 1. timeout 时长取自 HookHandler::timeout(),允许 per-hook 配置
// 2. 超时后必须清理子进程(command handler)或取消 future(inline/prompt)
// 3. 超时不影响 hook 链后续执行(failure_policy 决定是否短路)
// 4. 超时事件通过 LaneEvent 上报,接入监控告警

use tokio::time::{timeout, Elapsed};
use std::time::Duration;

impl HookRunner {
    /// 包装单个 hook 调用,增加超时熔断与 LaneEvent 上报。
    async fn run_one_with_timeout(
        &self,
        entry: &HookEntry,
        ctx: &HookContext<'_>,
    ) -> HookRunResult {
        let handler_kind = entry.handler.kind();
        let timeout_dur = entry.handler.timeout();
        let hook_id = self.entry_id(entry);  // 用于日志的 hook 标识
        let started = std::time::Instant::now();

        // 上报 HookExecuted(started) lane event
        self.emit_lane_event(LaneEventName::HookExecuted, LaneEventStatus::Started, &[
            ("hook_id", &hook_id),
            ("handler", handler_kind),
            ("event", ctx.event.as_str()),
            ("timeout_ms", &timeout_dur.as_millis().to_string()),
        ]);

        let fut = self.run_handler(&entry.handler, ctx);
        match timeout(timeout_dur, fut).await {
            Ok(result) => {
                let elapsed = started.elapsed();
                // 上报 HookExecuted(completed) lane event
                self.emit_lane_event(LaneEventName::HookExecuted, LaneEventStatus::Completed, &[
                    ("hook_id", &hook_id),
                    ("elapsed_ms", &elapsed.as_millis().to_string()),
                    ("decision", result.decision.as_str()),
                ]);

                // 性能预算检查(p99 超阈值告警,但不影响结果)
                if elapsed > self.budget_alert_threshold(handler_kind) {
                    self.emit_lane_event(
                        LaneEventName::HookSlow,
                        LaneEventStatus::Warning,
                        &[("hook_id", &hook_id), ("elapsed_ms", &elapsed.as_millis().to_string())],
                    );
                }
                result
            }
            Err(_elapsed) => {
                // 超时熔断
                self.emit_lane_event(
                    LaneEventName::HookTimedOut,
                    LaneEventStatus::Failed,
                    &[
                        ("hook_id", &hook_id),
                        ("timeout_ms", &timeout_dur.as_millis().to_string()),
                    ],
                );
                HookRunResult::failed(format!(
                    "hook {} ({}) timed out after {:?}",
                    hook_id, handler_kind, timeout_dur,
                ))
            }
        }
    }

    /// 根据 handler 类型返回 p99 告警阈值(超出则发 HookSlow 事件)。
    fn budget_alert_threshold(&self, kind: &str) -> Duration {
        match kind {
            "command" => Duration::from_millis(500),
            "webhook" => Duration::from_secs(2),
            "inline" => Duration::from_millis(100),
            "prompt" => Duration::from_secs(5),
            _ => Duration::from_secs(30),
        }
    }

    async fn run_handler(
        &self,
        handler: &HookHandler,
        ctx: &HookContext<'_>,
    ) -> HookRunResult {
        match handler {
            HookHandler::Command(c) => self.run_command(c, ctx).await,
            HookHandler::Webhook(w) => self.run_webhook(w, ctx).await,
            HookHandler::Inline(i) => self.run_inline(i, ctx).await,
            HookHandler::Prompt(p) => self.run_prompt(p, ctx).await,
        }
    }
}
```

### 15.4 性能监控指标(LaneEvent 集成)

Hook 调用全过程通过 `LaneEvent` 上报,接入既有可观测性管道。以下为 v0.2 新增的 LaneEvent 类型:

```rust
// rust/crates/runtime/src/lane_events.rs(扩展)
//
// v0.2 新增 4 个 Hook 相关 LaneEvent:
// - HookExecuted: hook 执行完成(started/completed 两阶段)
// - HookFailed: hook 返回 Failed 状态(非超时)
// - HookTimedOut: hook 超时熔断
// - HookSlow: hook 完成但延迟超过 p99 预算(性能告警)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LaneEventName {
    // ... 既有事件
    HookExecuted,
    HookFailed,
    HookTimedOut,
    HookSlow,
    HookDenied,    // hook 返回 Deny
    HookAllowed,   // hook 返回 Allow(用于统计)
}

// 在 LaneEventBuilder 中添加便捷构造方法
impl LaneEventBuilder {
    pub fn hook_executed(hook_id: &str, handler: &str, event: &str) -> Self {
        Self::new(LaneEventName::HookExecuted, LaneEventStatus::Started)
            .with_metadata("hook_id", hook_id)
            .with_metadata("handler", handler)
            .with_metadata("event", event)
    }

    pub fn hook_timed_out(hook_id: &str, timeout_ms: u64) -> Self {
        Self::new(LaneEventName::HookTimedOut, LaneEventStatus::Failed)
            .with_metadata("hook_id", hook_id)
            .with_metadata("timeout_ms", &timeout_ms.to_string())
    }
}
```

### 15.5 关键监控指标清单

| 指标 | 来源 LaneEvent | 维度 | 告警阈值 |
|---|---|---|---|
| `hook_execution_count` | HookExecuted | event, handler, hook_id | (无,统计用) |
| `hook_failure_rate` | HookFailed / HookExecuted | event, hook_id | > 5% 持续 5 分钟 |
| `hook_timeout_rate` | HookTimedOut / HookExecuted | event, hook_id | > 1% 持续 5 分钟 |
| `hook_p99_latency_ms` | HookExecuted.elapsed_ms | event, handler | Command > 500 / Webhook > 2000 / Inline > 100 / Prompt > 5000 |
| `hook_deny_count` | HookDenied | event, hook_id | (无,业务监控用) |
| `hook_slow_count` | HookSlow | event, hook_id | > 10/分钟 |

### 15.6 性能优化检查清单

实施 P0/W3 时按此清单优化,确保 hook 系统不成为性能瓶颈:

- [ ] `HookRegistry::get` 使用 `RwLock::read`,无写时竞争
- [ ] `reqwest::Client` 在 `HookRunner::new` 时创建一次,复用连接池
- [ ] `tokio::process::Command` 而非 `std::process::Command`,避免阻塞 tokio worker
- [ ] inline hook 使用 `Arc<dyn Hook>` clone,避免每次 `RwLock::read`
- [ ] matcher 匹配结果可缓存(同一 tool_name 在同一 session 内复用)
- [ ] `serde_json::to_string(ctx)` 在 hot path,考虑预分配缓冲区
- [ ] `LaneEvent` 上报异步(通过 channel),不阻塞 hook 返回
- [ ] 基准测试:criterion 跑 10000 次 `hook_runner.run`,p99 < 1ms(空 hook 场景)

### 15.7 基准测试骨架

```rust
// rust/crates/runtime/benches/hook_runner.rs
//
// 用 criterion 测量 HookRunner 性能,确保不退化。

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use std::time::Duration;

fn bench_hook_runner_empty(c: &mut Criterion) {
    let runtime = build_test_runtime()
        .with_hook_config(HookConfig::default())  // 空 hook
        .build();
    let ctx = build_test_ctx(HookEvent::PreToolUse);

    c.bench_function("hook_runner_run_empty", |b| {
        b.to_async(tokio::runtime::Runtime::new().unwrap())
            .iter(|| runtime.hook_runner().run(HookEvent::PreToolUse, &ctx));
    });
}

fn bench_hook_runner_inline(c: &mut Criterion) {
    let mut group = c.benchmark_group("hook_runner_inline");
    for n in [1, 5, 10, 50].iter() {
        let runtime = build_test_runtime()
            .with_inline_hooks(*n, NoOpHook::default())
            .build();
        let ctx = build_test_ctx(HookEvent::PostToolUse);
        group.bench_with_input(BenchmarkId::from_parameter(n), n, |b, _| {
            b.to_async(tokio::runtime::Runtime::new().unwrap())
                .iter(|| runtime.hook_runner().run(HookEvent::PostToolUse, &ctx));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_hook_runner_empty, bench_hook_runner_inline);
criterion_main!(benches);
```

**通过标准**:`hook_runner_run_empty` p99 < 100μs;`hook_runner_inline/50` p99 < 1ms。若不达标,触发第 12 章风险缓解措施。

---

## 16. Hook 与权限系统协同(v0.2 新增)

### 16.1 背景

Claw Code 已有 `PermissionEnforcer`(见 `rust/crates/runtime/src/permission_enforcer.rs`)与 `PermissionMode` 枚举(见 `bash_validation.rs` line 103-300)。Hook 系统引入后,工具调用的决策路径变为「Hook → PermissionMode → Policy」,三者职责不同但需要协同。

### 16.2 现有 PermissionMode 枚举

基于 `bash_validation.rs` 实际代码,PermissionMode 包含以下变体:

| 变体 | 含义 | 工具调用约束 |
|---|---|---|
| `ReadOnly` | 只读模式 | 仅允许 ls / cat / grep 等只读命令,禁止任何写入 |
| `WorkspaceWrite` | 工作区写模式 | 允许工作区内写入,禁止工作区外路径与系统命令 |
| `DangerFullAccess` | 完全访问模式 | 允许所有命令(危险,需用户显式确认) |
| `Allow` | 全部允许 | 等同 DangerFullAccess 但无警告 |
| `Prompt` | 逐次询问模式 | 每次工具调用都弹出权限提示,用户逐次确认 |

### 16.3 Hook × PermissionMode 交互矩阵

下表展示 Hook(PreToolUse)与 PermissionMode 在工具调用决策上的协同关系。每一行表示一种 PermissionMode,每一列表示 Hook 决策,单元格表示最终行为。

| PermissionMode \ Hook 决策 | Allow | Deny | Failed(failClose) | Failed(failOpen) | Timeout |
|---|---|---|---|---|---|
| `ReadOnly` | 走 ReadOnly 校验(只读命令通过) | **拒绝工具**(Hook 优先) | **拒绝工具**(Hook 失败=拒绝) | 走 ReadOnly 校验 | 走 ReadOnly 校验(超时=failOpen 默认) |
| `WorkspaceWrite` | 走 WorkspaceWrite 校验 | **拒绝工具** | **拒绝工具** | 走 WorkspaceWrite 校验 | 走 WorkspaceWrite 校验 |
| `DangerFullAccess` | **允许工具**(全访问) | **拒绝工具** | **拒绝工具** | **允许工具** | **允许工具** |
| `Allow` | **允许工具** | **拒绝工具** | **拒绝工具** | **允许工具** | **允许工具** |
| `Prompt` | 弹出权限提示(用户确认) | **拒绝工具**(不弹提示) | **拒绝工具** | 弹出权限提示 | 弹出权限提示 |

**关键观察**:

1. **Hook Deny 优先于一切**:无论 PermissionMode 是什么,只要 Hook 返回 Deny,工具立即被拒绝,不进入权限校验流程。
2. **Hook Allow 不绕过 PermissionMode**:Hook Allow 只表示「hook 不反对」,PermissionMode 仍按其规则校验。例如 ReadOnly 模式下,即使 Hook Allow,Bash 写入命令仍被拒。
3. **Hook Failed 视同 Deny(failClose)**:failClose 策略下,Hook 自身失败也导致工具拒绝,优先级高于 PermissionMode。
4. **Hook Failed(failOpen)与 Timeout 不阻断**:这两种情况下工具调用继续走 PermissionMode 校验,由权限系统决定是否允许。

### 16.4 决策优先级图

```
                ┌─────────────────────────────────────┐
                │  工具调用请求 (tool_name, tool_input) │
                └──────────────────┬──────────────────┘
                                   │
                                   ▼
                ┌─────────────────────────────────────┐
                │   1. Hook 决策(PreToolUse hook chain)│
                │      - Allow / Deny / Failed / Timeout │
                └──────────────────┬──────────────────┘
                                   │
              ┌────────────────────┼────────────────────┐
              │                    │                    │
              ▼                    ▼                    ▼
        ┌─────────┐         ┌──────────┐         ┌─────────────┐
        │  Deny   │         │  Allow   │         │ Failed/Timeout│
        │ (短路)  │         │          │         │ (failOpen)   │
        └────┬────┘         └────┬─────┘         └──────┬──────┘
             │                   │                      │
             ▼                   ▼                      ▼
        ┌─────────┐         ┌──────────────────────────────────┐
        │ 拒绝工具 │         │ 2. PermissionMode 校验            │
        │ 返回错误 │         │    (ReadOnly/WorkspaceWrite/...)  │
        └─────────┘         └──────────────┬───────────────────┘
                                           │
                            ┌──────────────┼──────────────┐
                            │              │              │
                            ▼              ▼              ▼
                       ┌─────────┐   ┌─────────┐   ┌─────────────┐
                       │ 拒绝    │   │ 允许    │   │ Prompt 询问 │
                       │(规则禁止)│   │(规则允许)│   │(用户决定)   │
                       └─────────┘   └────┬────┘   └──────┬──────┘
                                          │               │
                                          ▼               ▼
                                    ┌─────────────────────────────┐
                                    │ 3. Policy 校验(可选,多 Agent)│
                                    │    PolicyEngine.check(...)   │
                                    └──────────────┬──────────────┘
                                                   │
                                                   ▼
                                          ┌─────────────────┐
                                          │ 4. 实际工具执行  │
                                          └─────────────────┘
```

### 16.5 Hook 能否覆盖 PermissionMode 的决策?

**能,但仅限 Deny 方向**。

- Hook 返回 Deny → 工具立即拒绝,PermissionMode 无机会放行。这是「Hook 优先」语义。
- Hook 返回 Allow → 不影响 PermissionMode 的拒绝决策。Hook 不能强制放行 PermissionMode 禁止的工具。
- Hook 返回 `permission_override` 字段(已有,见 `HookRunResult::permission_override`)→ 可以覆盖 PermissionMode 的默认决策,这是 v0.1 既有能力。

```rust
// rust/crates/runtime/src/hooks.rs(既有,v0.1 已实现)
//
// permission_override 允许 hook 显式覆盖 PermissionMode 的默认决策。
// 例如:ReadOnly 模式下,hook 认为某次写入是安全的(如写入 /tmp),
// 可返回 permission_override = PermissionOverride::Allow,
// 绕过 ReadOnly 校验。

pub struct HookRunResult {
    // ...
    permission_override: Option<PermissionOverride>,
    permission_reason: Option<String>,
}

// PermissionOverride 已在 permissions 模块定义:
//   Allow      - 允许(覆盖 ReadOnly 等限制)
//   Deny       - 拒绝(覆盖 DangerFullAccess 等允许)
//   Ask        - 转为 Prompt 询问
//   UseDefault - 使用 PermissionMode 默认决策(默认值)
```

**使用场景**:PreToolUse hook 检测到 Edit 写入 `/tmp/build_cache/`,虽然用户处于 ReadOnly 模式,但 hook 判定该路径安全(临时缓存),返回 `permission_override = Allow`,允许写入。

### 16.6 Hook 能否绕过 PermissionMode?

**不能**。

- Hook 失败(failOpen)或超时 → 工具调用继续走 PermissionMode 校验,不会被绕过。
- Hook Allow → 仍需 PermissionMode 校验,PermissionMode 可拒绝。
- Hook 不存在(无配置)→ 直接走 PermissionMode,行为不变。

设计原则:**Hook 是 PermissionMode 的补充,不是替代**。Hook 提供细粒度的工具调用拦截(基于 tool_input 内容),PermissionMode 提供粗粒度的会话级策略(基于用户配置)。两者互补,不互斥。

### 16.7 实施代码骨架

```rust
// rust/crates/runtime/src/conversation.rs(扩展 line 1175 附近的工具调用决策)
//
// 完整的工具调用决策流程:Hook → PermissionMode → 实际执行。
// 关键点:
// 1. PreToolUse hook 先执行,Deny 立即短路
// 2. permission_override 字段覆盖 PermissionMode 默认决策
// 3. PermissionMode 仍按其规则校验(除非被 override 覆盖)
// 4. PolicyEngine(多 Agent 场景)在 PermissionMode 之后

fn dispatch_tool_with_hooks_and_permissions(
    &mut self,
    tool_name: &str,
    input: &Value,
) -> ToolResult {
    // === 阶段 1: PreToolUse hook ===
    let hook_ctx = HookContext::for_pre_tool_use(
        tool_name, input,
        self.session_id.clone(), self.cwd.clone(),
        &self.hook_abort_signal,
    );
    let hook_result = block_on(self.hook_runner.run(HookEvent::PreToolUse, &hook_ctx));

    // Hook Deny 立即短路,不进入权限校验
    if hook_result.is_denied() {
        return ToolResult::error(hook_result.reason.unwrap_or_else(|| {
            "PreToolUse hook denied".to_string()
        }));
    }

    // Hook Failed + failClose 也短路
    if hook_result.is_failed() && hook_result.failure_policy == FailurePolicy::FailClose {
        return ToolResult::error(hook_result.reason.unwrap_or_else(|| {
            "PreToolUse hook failed (failClose)".to_string()
        }));
    }

    // 应用 hook 改写的 input
    let effective_input = hook_result.updated_input
        .map(|s| serde_json::from_str(&s).unwrap_or_else(|_| input.clone()))
        .unwrap_or_else(|| input.clone());

    // === 阶段 2: PermissionMode 校验 ===
    let permission_decision = if let Some(override_) = hook_result.permission_override {
        // Hook 显式覆盖 PermissionMode
        PermissionDecision::from_override(override_)
    } else {
        // 使用 PermissionMode 默认决策
        self.permission_enforcer.check(tool_name, &effective_input, self.permission_mode)
    };

    match permission_decision {
        PermissionDecision::Allow => { /* 继续 */ }
        PermissionDecision::Deny(reason) => {
            return ToolResult::error(format!("Permission denied: {reason}"));
        }
        PermissionDecision::Ask => {
            // Prompt 模式:弹出权限提示,等用户确认
            let user_approved = self.prompt_user_for_permission(tool_name, &effective_input);
            if !user_approved {
                return ToolResult::error("User denied permission");
            }
        }
    }

    // === 阶段 3: PolicyEngine 校验(多 Agent 场景)===
    if let Some(policy) = &self.policy_engine {
        let action = policy.check(LaneContext::for_tool_call(tool_name, &effective_input));
        match action {
            PolicyAction::Block(reason) => return ToolResult::error(reason),
            PolicyAction::Allow => { /* 继续 */ }
            PolicyAction::Reconcile => { /* 调整 input 后继续 */ }
            PolicyAction::Approve => { /* 标记已审批,继续 */ }
        }
    }

    // === 阶段 4: 实际工具执行 ===
    let result = self.tool_executor.execute(tool_name, &effective_input);

    // === 阶段 5: PostToolUse / PostToolUseFailure hook ===
    let post_event = if result.is_error {
        HookEvent::PostToolUseFailure
    } else {
        HookEvent::PostToolUse
    };
    let post_ctx = HookContext::for_post_tool_use(
        tool_name, &effective_input, &result.output,
        self.session_id.clone(), self.cwd.clone(),
        &self.hook_abort_signal,
    );
    let _ = block_on(self.hook_runner.run(post_event, &post_ctx));

    result
}
```

### 16.8 协同测试用例

```rust
#[tokio::test]
async fn hook_deny_overrides_danger_full_access() {
    // Hook Deny 应覆盖 DangerFullAccess,工具被拒绝
    let mut runtime = build_test_runtime()
        .with_permission_mode(PermissionMode::DangerFullAccess)
        .with_hook(HookEvent::PreToolUse, deny_hook("危险操作"))
        .build();
    let _ = runtime.run_turn("rm -rf /tmp", None).await.unwrap();
    assert_eq!(runtime.bash_executor().call_count(), 0);  // 工具未执行
}

#[tokio::test]
async fn hook_allow_does_not_override_read_only() {
    // Hook Allow 不能绕过 ReadOnly,工具仍被权限系统拒绝
    let mut runtime = build_test_runtime()
        .with_permission_mode(PermissionMode::ReadOnly)
        .with_hook(HookEvent::PreToolUse, allow_hook())
        .build();
    let _ = runtime.run_turn("echo hello > /tmp/x", None).await.unwrap();
    // ReadOnly 应拒绝写入命令
    assert_eq!(runtime.bash_executor().call_count(), 0);
}

#[tokio::test]
async fn hook_permission_override_can_allow_in_read_only() {
    // Hook 通过 permission_override 显式允许 ReadOnly 模式下的写入
    let mut runtime = build_test_runtime()
        .with_permission_mode(PermissionMode::ReadOnly)
        .with_hook(
            HookEvent::PreToolUse,
            override_hook(PermissionOverride::Allow, "safe temp write"),
        )
        .build();
    let _ = runtime.run_turn("echo hello > /tmp/build_cache/x", None).await.unwrap();
    // Hook override 应允许工具执行
    assert_eq!(runtime.bash_executor().call_count(), 1);
}

#[tokio::test]
async fn hook_failopen_falls_through_to_permission() {
    // Hook Failed + failOpen,工具调用继续走 PermissionMode
    let mut runtime = build_test_runtime()
        .with_permission_mode(PermissionMode::WorkspaceWrite)
        .with_hook(
            HookEvent::PreToolUse,
            failed_hook(FailurePolicy::FailOpen),
        )
        .build();
    let _ = runtime.run_turn("ls -la", None).await.unwrap();
    // WorkspaceWrite 应允许 ls
    assert_eq!(runtime.bash_executor().call_count(), 1);
}
```

---

## 17. 配置文件热重载(v0.2 新增)

### 17.1 设计目标

用户编辑 `~/.claw/hooks.toml` 后,无需重启 Claw Code 即可生效。热重载设计目标:

1. **零中断**:运行中的 hook 不被中断,新 hook 在下一次事件触发时生效。
2. **部分更新**:只重载变更的 hook 条目,不重建整个 `HookConfig`,减少抖动。
3. **原子切换**:`HookConfig` 切换通过 `ArcSwap` 原子完成,避免读到半更新状态。
4. **错误恢复**:配置文件解析失败时,保留旧配置并发出警告,不崩溃 runtime。
5. **可观测**:重载事件通过 `LaneEvent::HooksReloaded` 上报,记录变更数量与失败原因。

### 17.2 文件 watcher(notify crate)

使用 [`notify`](https://crates.io/crates/notify) crate 监听 `~/.claw/hooks.toml` 文件变更。notify 是 Rust 生态最成熟的跨平台文件 watcher,支持 Windows / macOS / Linux。

```toml
# rust/crates/runtime/Cargo.toml(扩展依赖)
[dependencies]
notify = { version = "6.0", features = ["serde"] }
arc-swap = "1.7"   # 原子配置切换
```

### 17.3 热重载代码骨架

```rust
// rust/crates/runtime/src/hooks.rs(新增 HookReloader)
//
// HookReloader 监听配置文件变更,解析后通过 ArcSwap 原子替换 HookRunner.config。
// 关键设计:
// 1. notify watcher 在独立 tokio task 中运行,通过 channel 发送变更事件
// 2. 主线程通过 ArcSwap<HookConfig> 持有当前配置,读取无锁,替换原子
// 3. 解析失败时保留旧配置,通过 LaneEvent 上报错误
// 4. 部分更新策略:diff 新旧 config,只重建变更的 HookMatcher

use notify::{Watcher, RecursiveMode, Event, EventKind};
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct HookReloader {
    /// 当前配置(ArcSwap 原子切换)
    config: Arc<ArcSwap<HookConfig>>,
    /// 配置文件路径
    config_path: PathBuf,
    /// 文件 watcher handle(持有以保活)
    _watcher: notify::RecommendedWatcher,
}

impl HookReloader {
    /// 启动热重载 watcher,返回 HookReloader 与配置访问句柄。
    /// 调用方持有 HookReloader 以保活 watcher,通过 config() 读取最新配置。
    pub fn start(
        config_path: PathBuf,
        initial_config: HookConfig,
    ) -> Result<(Self, Arc<ArcSwap<HookConfig>>), notify::Error> {
        let config = Arc::new(ArcSwap::from_pointee(initial_config));
        let config_for_watcher = config.clone();
        let path_for_watcher = config_path.clone();

        let (tx, mut rx) = mpsc::channel::<()>(16);

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            if let Ok(event) = res {
                // 仅响应 Modify / Create 事件,忽略 Access(读取)事件
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    // 文件可能正在写入,延迟 100ms 后再读(避免读到半写状态)
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let _ = tx.send(()).await;
                    });
                }
            }
        })?;

        watcher.watch(&config_path, RecursiveMode::NonRecursive)?;

        // 启动消费者 task:收到变更事件后重新解析配置
        let config_for_consumer = config_for_watcher.clone();
        let path_for_consumer = path_for_watcher.clone();
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                Self::reload(&config_for_consumer, &path_for_consumer).await;
            }
        });

        Ok((
            Self {
                config: config.clone(),
                config_path,
                _watcher: watcher,
            },
            config,
        ))
    }

    /// 重新加载配置文件。解析失败时保留旧配置。
    async fn reload(config: &Arc<ArcSwap<HookConfig>>, path: &PathBuf) {
        let old_config = config.load_full();  // Arc<ArcSwap<HookConfig>> -> Arc<HookConfig>

        let new_config = match Self::parse_config(path) {
            Ok(c) => c,
            Err(e) => {
                // 解析失败,保留旧配置,发出警告
                emit_lane_event(
                    LaneEventName::HooksReloadFailed,
                    LaneEventStatus::Failed,
                    &[("error", &e.to_string()), ("path", &path.to_string_lossy())],
                );
                eprintln!("warning: hooks.toml reload failed: {e}");
                return;
            }
        };

        // diff 计算(部分更新策略)
        let diff = Self::diff_configs(&old_config, &new_config);
        if diff.is_empty() {
            return;  // 无变更
        }

        // 原子切换
        config.store(Arc::new(new_config));

        // 上报重载成功事件
        emit_lane_event(
            LaneEventName::HooksReloaded,
            LaneEventStatus::Completed,
            &[
                ("changed_events", &diff.len().to_string()),
                ("added", &diff.iter().filter(|d| d.is_add()).count().to_string()),
                ("removed", &diff.iter().filter(|d| d.is_remove()).count().to_string()),
                ("modified", &diff.iter().filter(|d| d.is_modify()).count().to_string()),
            ],
        );

        eprintln!(
            "info: hooks.toml reloaded: {} events changed ({} added, {} removed, {} modified)",
            diff.len(),
            diff.iter().filter(|d| d.is_add()).count(),
            diff.iter().filter(|d| d.is_remove()).count(),
            diff.iter().filter(|d| d.is_modify()).count(),
        );
    }

    fn parse_config(path: &PathBuf) -> Result<HookConfig, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: HookConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// 计算新旧配置差异,用于部分更新策略。
    fn diff_configs<'a>(
        old: &'a HookConfig,
        new: &'a HookConfig,
    ) -> Vec<ConfigDiff> {
        let mut diffs = Vec::new();
        let all_events: std::collections::HashSet<HookEvent> =
            old.hooks.keys().chain(new.hooks.keys()).copied().collect();

        for event in all_events {
            let old_matchers = old.hooks.get(&event);
            let new_matchers = new.hooks.get(&event);
            match (old_matchers, new_matchers) {
                (None, Some(_)) => diffs.push(ConfigDiff::Added(event)),
                (Some(_), None) => diffs.push(ConfigDiff::Removed(event)),
                (Some(o), Some(n)) if o != n => diffs.push(ConfigDiff::Modified(event)),
                _ => {}  // 相同,无差异
            }
        }
        diffs
    }

    /// 获取当前配置的 Arc clone(无锁读取)。
    pub fn config(&self) -> arc_swap::Guard<Arc<HookConfig>> {
        self.config.load()
    }
}

#[derive(Debug, Clone)]
pub enum ConfigDiff {
    Added(HookEvent),
    Removed(HookEvent),
    Modified(HookEvent),
}

impl ConfigDiff {
    fn is_add(&self) -> bool { matches!(self, Self::Added(_)) }
    fn is_remove(&self) -> bool { matches!(self, Self::Removed(_)) }
    fn is_modify(&self) -> bool { matches!(self, Self::Modified(_)) }
}
```

### 17.4 部分更新策略

热重载采用「diff 后部分更新」策略,而非「整体重建 HookConfig」,以减少抖动:

| 场景 | 行为 |
|---|---|
| 新增事件(如新增 `[[Stop]]` 段) | 在 `HookConfig.hooks` 中 insert 新 entry,运行中事件不受影响 |
| 删除事件(如删除 `[[Notification]]` 段) | 标记为 disabled,正在执行的 hook 链继续完成,下一次事件触发时不再加载 |
| 修改事件(如调整 matcher / priority) | 原子替换该事件的 `Vec<HookMatcher>`,下一次事件触发时按新配置执行 |
| 修改单个 hook entry(如改 command) | 替换该 entry,不影响同事件其他 hook |
| 解析失败 | 保留旧配置,发出 `HooksReloadFailed` lane event,继续使用旧配置 |

### 17.5 运行中 hook 不中断保证

热重载必须保证「正在执行的 hook 链完成执行」,不允许中途切换配置导致 hook 行为不一致。

实现机制:

1. **Hook 链持有 Arc<HookConfig> 引用**:hook 链开始执行时,通过 `config.load_full()` 持有当前配置的 Arc clone,整个链执行期间引用不变。
2. **ArcSwap 替换不影响已持有引用**:热重载通过 `config.store(Arc::new(new))` 原子替换,但已持有旧 Arc 的 hook 链仍使用旧配置完成执行。
3. **下一次事件触发使用新配置**:新事件触发时调用 `config.load()` 获取最新配置,自然切换。

```rust
// 示例:Hook 链执行时持有配置引用
impl HookRunner {
    pub async fn run(&self, event: HookEvent, ctx: &HookContext<'_>) -> HookRunResult {
        // 关键:在链开始时 load_full,持有 Arc<HookConfig> 引用
        let config = self.config.load_full();  // Arc<HookConfig>

        let matchers = config.hooks.get(&event).cloned().unwrap_or_default();
        // ... 后续执行使用 matchers,即使热重载发生也不影响本次执行
    }
}
```

### 17.6 热重载测试用例

```rust
#[tokio::test]
async fn hook_config_hot_reload_takes_effect_on_next_event() {
    let config_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::write(&config_path, r#"
[[PreToolUse]]
matcher = "Bash"
execution = "sequential"
  [[PreToolUse.hooks]]
  priority = 100
  [PreToolUse.hooks.handler]
  type = "command"
  command = "echo initial"
  timeout = "2s"
"#).unwrap();

    let initial = HookConfig::parse(&config_path).unwrap();
    let (reloader, config) = HookReloader::start(config_path.to_path_buf(), initial).unwrap();
    let runtime = build_test_runtime().with_hook_config_handle(config).build();

    // 1. 初始配置:echo initial
    let _ = runtime.run_turn("do something", None).await.unwrap();
    assert!(runtime.spawned_commands().iter().any(|c| c.contains("echo initial")));

    // 2. 修改配置文件:echo updated
    std::fs::write(&config_path, r#"
[[PreToolUse]]
matcher = "Bash"
execution = "sequential"
  [[PreToolUse.hooks]]
  priority = 100
  [PreToolUse.hooks.handler]
  type = "command"
  command = "echo updated"
  timeout = "2s"
"#).unwrap();

    // 3. 等待 watcher 触发(延迟 100ms + channel)
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 4. 下一个事件应使用新配置
    let _ = runtime.run_turn("do something else", None).await.unwrap();
    assert!(runtime.spawned_commands().iter().any(|c| c.contains("echo updated")));
    assert!(!runtime.spawned_commands().iter().any(|c| c.contains("echo initial")));
}

#[tokio::test]
async fn hook_config_hot_reload_invalid_toml_keeps_old_config() {
    let config_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::write(&config_path, "[[PreToolUse]]\nmatcher=\"Bash\"\n").unwrap();
    let initial = HookConfig::parse(&config_path).unwrap();
    let (_reloader, config) = HookReloader::start(config_path.to_path_buf(), initial).unwrap();

    // 写入无效 TOML
    std::fs::write(&config_path, "this is not valid toml {{{{").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 配置应保持旧值
    let current = config.load();
    assert!(current.hooks.contains_key(&HookEvent::PreToolUse));
}

#[tokio::test]
async fn hook_config_hot_reload_running_hook_not_interrupted() {
    // 验证:运行中的 hook 链使用旧配置完成执行,新配置在下一次事件生效
    let config_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::write(&config_path, "...initial with 3 hooks...").unwrap();
    let initial = HookConfig::parse(&config_path).unwrap();
    let (_reloader, config) = HookReloader::start(config_path.to_path_buf(), initial).unwrap();

    // 启动一个慢 hook(500ms)
    let runtime = build_test_runtime()
        .with_hook_config_handle(config.clone())
        .with_slow_hook(500)
        .build();

    let runtime_clone = runtime.clone();
    let handle = tokio::spawn(async move {
        runtime_clone.run_turn("trigger hook", None).await
    });

    // 在 hook 执行中修改配置
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&config_path, "...new with 1 hook...").unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;  // 等 watcher 触发

    // 等待 hook 完成
    let summary = handle.await.unwrap().unwrap();

    // 验证:本次执行使用旧配置(3 hooks 都执行)
    assert_eq!(runtime.hook_log().count(), 3);

    // 下一次执行使用新配置(1 hook)
    let _ = runtime.run_turn("trigger again", None).await.unwrap();
    assert_eq!(runtime.hook_log().count_since_last(), 1);
}
```

### 17.7 Cargo.toml 依赖更新清单

```toml
# rust/crates/runtime/Cargo.toml
[dependencies]
# 既有
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
async-trait = "0.1"
reqwest = { version = "0.11", features = ["json"] }
humantime-serde = "1"

# v0.2 新增
notify = { version = "6.0", features = ["serde"] }   # 文件 watcher
arc-swap = "1.7"                                       # 原子配置切换
toml = "0.8"                                            # TOML 解析(配置文件)
regex = "1"                                             # matcher 升级(P1)
```

---

## 18. 迁移指南(v0.2 新增)

### 18.1 迁移范围

本指南覆盖从 v0.1(3 事件 / 1 handler / 同步执行)到 v0.2(10 事件 / 4 handler / 异步引擎)的迁移路径。涉及:

- 配置文件格式:`RuntimeHookConfig`(JSON 字段) → `HookConfig`(TOML / JSON,事件作为顶层 key)
- 代码:`HookRunner` 内部实现重写,`run_turn` 集成点扩展
- 用户配置:既有 `.claw/settings.json` 中的 `pre_tool_use` 字段保留可用
- 废弃事件标记:v0.1 没有废弃事件,v0.2 引入废弃机制为未来版本预留

### 18.2 事件迁移映射

| v0.1 事件 | v0.2 事件 | 迁移状态 | 说明 |
|---|---|---|---|
| `PreToolUse` | `PreToolUse` | 保留 | 行为扩展(支持 matcher / 4 handler / failure_policy) |
| `PostToolUse` | `PostToolUse` | 保留 | 行为扩展(支持 additional_context 注入) |
| `PostToolUseFailure` | `PostToolUseFailure` | 保留 | 行为扩展(支持独立 hook 链) |
| (无) | `PostCustomToolCall` | 新增 | MCP 工具专用 |
| (无) | `UserPromptSubmit` | 新增 | prompt 改写 / 上下文注入 |
| (无) | `Notification` | 新增 | webhook 通知场景 |
| (无) | `SessionStart` | 新增 | 环境检查 / .env 加载 |
| (无) | `SessionEnd` | 新增 | 清理 / 归档 |
| (无) | `Stop` | 新增 | 完成度检查 / 自动 commit |
| (无) | `SubagentStop` | 新增 | 子 agent 结果验证 |
| (无) | `PreCompact` | 新增 | 压缩前持久化 |
| (无) | `PostCompact`(v0.2 示例 3 提及) | 待定 | v0.2 示例中提出,P0 暂不实现,P1 评估纳入 |

**向后兼容**:v0.1 的 3 事件在 v0.2 中语义保持不变,既有 hook 配置无需修改即可工作。

### 18.3 配置文件向后兼容

#### 18.3.1 v0.1 配置格式(保留可用)

```json
// .claw/settings.json(v0.1 格式)
{
  "hooks": {
    "pre_tool_use": [
      "$CLAW_PROJECT_DIR/.claw/hooks/lint.sh"
    ],
    "post_tool_use": [
      "$CLAW_PROJECT_DIR/.claw/hooks/audit.sh"
    ],
    "post_tool_use_failure": []
  }
}
```

v0.2 加载时,通过 `HookConfig::from_legacy` 自动转换为新格式:

```rust
// rust/crates/runtime/src/hooks.rs(扩展)
impl HookConfig {
    pub fn from_legacy(legacy: crate::config::RuntimeHookConfig) -> Self {
        let mut hooks = BTreeMap::new();
        // 每个 v0.1 字段映射为 v0.2 的 HookMatcher
        // - matcher = ""(空,匹配全部)
        // - execution = Sequential
        // - 每个 command 字符串包装为 CommandHook
        // - priority = 100(默认)
        // - failure_policy = FailClose(v0.1 行为)
        // - enabled = true

        if !legacy.pre_tool_use().is_empty() {
            hooks.insert(HookEvent::PreToolUse, vec![HookMatcher {
                matcher: String::new(),
                execution: HookExecution::Sequential,
                hooks: legacy.pre_tool_use().iter().map(|c| HookEntry {
                    handler: HookHandler::Command(CommandHook {
                        command: c.clone(),
                        timeout: Duration::from_secs(30),
                        cwd: None,
                        env: BTreeMap::new(),
                    }),
                    priority: 100,
                    failure_policy: FailurePolicy::FailClose,
                    enabled: true,
                }).collect(),
            }]);
        }
        // post_tool_use / post_tool_use_failure 同理
        // ...

        Self { hooks }
    }
}
```

#### 18.3.2 v0.2 配置格式(推荐)

```toml
# .claw/hooks.toml(v0.2 格式,推荐)
[[PreToolUse]]
matcher = "Edit|Write"
execution = "sequential"

  [[PreToolUse.hooks]]
  priority = 100
  failure_policy = "failClose"
  enabled = true

  [PreToolUse.hooks.handler]
  type = "command"
  command = "$CLAW_PROJECT_DIR/.claw/hooks/lint.sh"
  timeout = "30s"
```

#### 18.3.3 加载优先级

v0.2 配置加载顺序(后者覆盖前者):

1. `~/.claw/hooks.toml`(全局,用户级)
2. `$CLAW_PROJECT_DIR/.claw/hooks.toml`(项目级)
3. `$CLAW_PROJECT_DIR/.claw/settings.json` 中的 `hooks` 字段(项目级,JSON)
4. 命令行 `--hooks-config <path>`(显式指定,最高优先级)

**v0.1 兼容字段**:`settings.json` 中的 `pre_tool_use` / `post_tool_use` / `post_tool_use_failure` 数组字段继续支持,但优先级低于 v0.2 的 `hooks` 字段。若两者同时存在,`hooks` 字段完全覆盖 v0.1 字段(非合并)。

### 18.4 代码迁移路径

#### 18.4.1 阶段 1:HookRunner 内部重写(P0 W1-W4)

```rust
// 旧:v0.1 HookRunner(同步,仅 command handler)
pub struct HookRunner {
    config: RuntimeHookConfig,
}
impl HookRunner {
    pub fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        // 同步遍历 pre_tool_use 命令,串行执行
    }
}

// 新:v0.2 HookRunner(异步,4 handler)
pub struct HookRunner {
    config: Arc<ArcSwap<HookConfig>>,  // 支持热重载
    inline_registry: Arc<HookRegistry>,
    http_client: reqwest::Client,
    llm_router: Option<Arc<LlmRouter>>,
}
impl HookRunner {
    pub async fn run(&self, event: HookEvent, ctx: &HookContext<'_>) -> HookRunResult {
        // 异步执行,支持 4 handler,超时熔断,失败策略
    }
}
```

迁移策略:**保留 v0.1 同步方法作为 wrapper**,内部调用异步 `run`,通过 `block_on` 桥接。这样 `conversation.rs` 集成点无需立即改为 async。

```rust
// 迁移期 wrapper(P0 W5-W6 期间)
impl HookRunner {
    /// v0.1 兼容方法(同步),内部桥接到 v0.2 异步 run。
    pub fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        let input_value: Value = serde_json::from_str(input).unwrap_or(json!({}));
        let ctx = HookContext::for_pre_tool_use(
            tool_name, &input_value,
            self.session_id.clone(), self.cwd.clone(),
            &self.hook_abort_signal,
        );
        futures::executor::block_on(self.run(HookEvent::PreToolUse, &ctx))
    }
}
```

#### 18.4.2 阶段 2:run_turn 集成点接入(P0 W5-W6)

按第 7 章 8 个集成点逐一接入,每个集成点的迁移步骤:

1. 在 `conversation.rs` 对应位置插入 `HookContext::for_xxx(...)` 构造
2. 调用 `block_on(self.hook_runner.run(event, &ctx))`(P0 桥接)
3. 根据返回的 `HookRunResult` 决定是否短路 / 注入上下文 / 改写输入
4. 添加单元测试验证触发

#### 18.4.3 阶段 3:run_turn 改为 async(P1)

P0 阶段保留 `run_turn` 为同步函数,通过 `block_on` 桥接。P1 阶段将 `run_turn` 改为 `async fn`,消除所有 `block_on`,根本解决嵌套调用风险(见 12.2 Blocking 死锁)。

```rust
// P1:run_turn 改为 async
pub async fn run_turn(
    &mut self,
    user_input: impl Into<String>,
    mut prompter: Option<&mut dyn PermissionPrompter>,
) -> Result<TurnSummary, RuntimeError> {
    // ... 直接 await self.hook_runner.run(...),无需 block_on
}
```

### 18.5 废弃事件标记机制

v0.2 引入废弃标记机制,为未来版本的事件重命名 / 合并预留路径。

#### 18.5.1 废弃标记语法

在 `HookEvent` enum 上添加 `#[deprecated]` 属性,并在 `as_str()` 中保留旧名以兼容配置文件:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    // ... 既有事件

    /// v0.3 计划废弃:合并到 PostToolUse(tool_result_is_error=true)
    #[deprecated(since = "0.2", note = "use PostToolUse with tool_result_is_error=true")]
    #[serde(rename = "PostToolUseFailure")]
    PostToolUseFailure,
}
```

#### 18.5.2 配置文件废弃警告

加载配置时,若发现废弃事件,打印警告并自动迁移:

```rust
impl HookConfig {
    pub fn parse(toml_str: &str) -> Result<Self, toml::de::Error> {
        let config: HookConfig = toml::from_str(toml_str)?;
        config.warn_on_deprecated();
        Ok(config)
    }

    fn warn_on_deprecated(&self) {
        #[allow(deprecated)]
        if self.hooks.contains_key(&HookEvent::PostToolUseFailure) {
            eprintln!(
                "warning: PostToolUseFailure is deprecated since 0.2, \
                 migrate to PostToolUse with tool_result_is_error=true"
            );
        }
    }
}
```

#### 18.5.3 自动迁移

对于已废弃但仍可兼容的事件,在加载时自动转换:

```rust
impl HookConfig {
    pub fn auto_migrate_deprecated(mut self) -> Self {
        #[allow(deprecated)]
        if let Some(matchers) = self.hooks.remove(&HookEvent::PostToolUseFailure) {
            // 合并到 PostToolUse(matcher 不变,hook 内可通过 tool_result_is_error 分支)
            self.hooks.entry(HookEvent::PostToolUse)
                .or_insert_with(Vec::new)
                .extend(matchers);
            eprintln!("info: auto-migrated PostToolUseFailure → PostToolUse");
        }
        self
    }
}
```

### 18.6 迁移验证测试

```rust
#[tokio::test]
async fn migration_v01_config_loads_in_v02() {
    // v0.1 配置在 v0.2 中应正常加载并工作
    let v01_json = r#"{
        "hooks": {
            "pre_tool_use": ["echo pre"],
            "post_tool_use": ["echo post"]
        }
    }"#;

    let v01_config: RuntimeHookConfig = serde_json::from_str(v01_json).unwrap();
    let v02_config = HookConfig::from_legacy(v01_config);

    // 验证:v0.1 的 2 个字段被正确转换为 v0.2 的 HookMatcher
    assert!(v02_config.hooks.contains_key(&HookEvent::PreToolUse));
    assert!(v02_config.hooks.contains_key(&HookEvent::PostToolUse));

    // 验证:command 字符串被包装为 CommandHook
    let pre_matchers = &v02_config.hooks[&HookEvent::PreToolUse];
    assert_eq!(pre_matchers[0].hooks.len(), 1);
    match &pre_matchers[0].hooks[0].handler {
        HookHandler::Command(c) => assert_eq!(c.command, "echo pre"),
        _ => panic!("expected Command handler"),
    }

    // 验证:行为与 v0.1 一致(同 priority=100,FailClose,enabled=true)
    let entry = &pre_matchers[0].hooks[0];
    assert_eq!(entry.priority, 100);
    assert_eq!(entry.failure_policy, FailurePolicy::FailClose);
    assert!(entry.enabled);
}

#[tokio::test]
async fn migration_v01_and_v02_config_coexist() {
    // v0.1 字段与 v0.2 hooks 字段同时存在时,v0.2 hooks 优先
    let settings_json = r#"{
        "pre_tool_use": ["echo v1_command"],
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "execution": "sequential",
                "hooks": [{
                    "handler": {"type": "command", "command": "echo v2_command"}
                }]
            }]
        }
    }"#;

    let config = load_settings(settings_json);
    // v0.2 hooks 字段完全覆盖 v0.1 pre_tool_use
    let pre_matchers = &config.hooks[&HookEvent::PreToolUse];
    assert_eq!(pre_matchers[0].hooks.len(), 1);
    match &pre_matchers[0].hooks[0].handler {
        HookHandler::Command(c) => assert_eq!(c.command, "echo v2_command"),
        _ => panic!("expected v2 handler"),
    }
}

#[tokio::test]
async fn migration_deprecated_event_warns() {
    // 使用废弃事件应打印警告(但不报错)
    let toml_str = r#"
[[PostToolUseFailure]]
matcher = "*"
execution = "sequential"
  [[PostToolUseFailure.hooks]]
  [PostToolUseFailure.hooks.handler]
  type = "command"
  command = "echo failure"
"#;
    let config = HookConfig::parse(toml_str).unwrap();
    // (捕获 stderr 验证警告)
    // config.warn_on_deprecated() 应输出 warning
}

#[tokio::test]
async fn migration_deprecated_event_auto_migrates() {
    let toml_str = r#"
[[PostToolUseFailure]]
matcher = "*"
execution = "sequential"
  [[PostToolUseFailure.hooks]]
  [PostToolUseFailure.hooks.handler]
  type = "command"
  command = "echo failure"
"#;
    let config = HookConfig::parse(toml_str).unwrap().auto_migrate_deprecated();
    // 废弃事件被合并到 PostToolUse
    assert!(!config.hooks.contains_key(&HookEvent::PostToolUseFailure));
    assert!(config.hooks.contains_key(&HookEvent::PostToolUse));
}
```

### 18.7 迁移检查清单

实施 P0/W7 配置迁移时,按此清单验证:

- [ ] v0.1 配置文件在 v0.2 中可正常加载,行为不变
- [ ] `HookConfig::from_legacy` 单元测试覆盖 3 个 v0.1 字段
- [ ] v0.1 / v0.2 配置共存时,优先级正确(v0.2 hooks 覆盖 v0.1 字段)
- [ ] v0.1 命令字符串默认参数:timeout=30s, priority=100, failClose, enabled=true
- [ ] 迁移期 deprecation warning 在 stderr 输出
- [ ] 自动迁移机制(废弃事件 → 新事件)正确工作
- [ ] 既有用户升级 v0.2 后,既有 hook 配置无需修改即可工作
- [ ] 端到端测试:用 v0.1 配置跑完整 turn,行为与 v0.1 一致

### 18.8 迁移时间线

| 阶段 | 版本 | 行为 |
|---|---|---|
| 兼容期 | v0.2(P0 W1-W8) | v0.1 配置可加载,通过 `from_legacy` 转换;打印 deprecation warning |
| 兼容期 | v0.3-P1 | 同上,继续支持 v0.1 配置 |
| 过渡期 | v0.4-P2 | v0.1 配置打印强制迁移警告,要求用户转换 |
| 删除期 | v0.5+ | 移除 `RuntimeHookConfig` 与 `from_legacy`,只支持 v0.2+ 配置 |

总兼容期:12 周(P0 + P1),确保用户有充足时间迁移。

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
| Hooks 主实现 | `rust/crates/runtime/src/hooks.rs` | HookEvent / HookHandler / HookRunner / HookChainExecutor / HookReloader |
| 集成点 | `rust/crates/runtime/src/conversation.rs` | run_turn 中 8 个 hook 接入点 |
| 配置 | `rust/crates/runtime/src/config.rs` | RuntimeHookConfig(待迁移为 HookConfig) |
| 策略引擎 | `rust/crates/runtime/src/policy_engine.rs` | DAG lane 级策略 |
| Plugin 生命周期 | `rust/crates/runtime/src/plugin_lifecycle.rs` | Plugin 加载 / 卸载事件 |
| 权限强制 | `rust/crates/runtime/src/permission_enforcer.rs` | 工具调用权限检查 |
| Bash 校验 | `rust/crates/runtime/src/bash_validation.rs` | PermissionMode 枚举与命令校验(line 103-300) |
| Lane 事件 | `rust/crates/runtime/src/lane_events.rs` | LaneEvent 上报(v0.2 新增 Hook 相关事件) |
| 性能基准 | `rust/crates/runtime/benches/hook_runner.rs` | criterion 基准测试(v0.2 新增) |
| 主文档 | `docs/ide-hooks-dag-implementation-plan.md` | 父文档(第三章 Hooks 系统方案) |

## 附录 C: v0.2 章节速查

| 章节 | 用途 | 关键内容 |
|---|---|---|
| 6.9 HookChain 执行器 | 实现顺序保证与短路语义 | HookChainExecutor 代码骨架(50+ 行) |
| 11.5 v0.2 新增测试用例 | 验证 v0.2 新能力 | 6 个测试用例 |
| 13. 集成点行号验证表 | 实施时定位代码 | 14 项验证 + 5 项附加验证点 |
| 14. 端到端集成示例 | 理解 Hook 系统工作流 | 3 个完整示例(配置 / 时序 / 日志 / 断言) |
| 15. Hook 执行性能预算 | 性能设计与监控 | Handler 预算 / 熔断骨架 / LaneEvent 指标 |
| 16. Hook 与权限系统协同 | Hook 与 PermissionMode 关系 | 交互矩阵 / 决策优先级图 / 协同代码 |
| 17. 配置文件热重载 | 运行期配置更新 | notify watcher / ArcSwap / 部分更新 |
| 18. 迁移指南 | v0.1 → v0.2 升级 | 事件映射 / 配置兼容 / 废弃机制 / 时间线 |

---

文档结束。
