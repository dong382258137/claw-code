# CLAW 多 Agent 编排硬化方案

> **状态**：设计草案(v3 — 含现状核实 / 状态机 / spike / 泛化抽象 / MVP 范围 / 成本门禁 / LlmJudgeGate / checkpoint)
> **日期**：2026-07-22
> **作者**：基于 wizard 闪退根因分析衍生的架构加固设计
> **关联**：本次方案源于一次"能力不足模型（deepseek 旧版）执行诊断任务时凭直觉堆砌防御代码、浪费两轮迭代"的真实事故
>
> **v3 修订要点(基于 2025 最新论文对比)**:
> - **P0 成本门禁**:`UpgradeEntry` 新增 `cost_multiplier` 字段,Subagent 新增 `cost_limit`/`cost_accumulated` 字段,retry loop 升级前调用 `check_cost_limit`。借鉴 Router-R1(NeurIPS 2025)成本奖励 + FrugalGPT 成本感知级联
> - **P0 LlmJudgeGate trait 预留**:诊断/架构任务无法用编译命令验证,引入 LLM-as-judge 门禁。借鉴 Anthropic Multi-Agent Research System 的 rubric 评分 + end-state evaluation。trait 在 MVP 就位,实现留待 v2
> - **P1 checkpoint 预留**:Subagent 新增 `checkpoint_path` 字段 + `save_checkpoint` 方法。借鉴 LangGraph/Temporal durable execution + Anthropic "resume from where the agent was"。MVP 落地 save,restore 留待 v2
> - **P1 spawn_parallel 预留**:借鉴 Anthropic 并行 spawn 3-5 subagents(90% 加速)。MVP 串行退化,v2 接入 tokio
> - **P1 决策持久化(§4.7 新增)**:compaction 前自动提取"决策点"写入 NOTEBOOK.md decisions 段 + FTS5 decision role 加权索引。与 §4.1 的 `diag!` 互补:diag! 解决"运行时信号不可见",decision_log 解决"设计决策信号不可见"。MVP 用启发式关键词检测(零成本),v2 用 LLM 提取
> - P3 预留 `confidence_threshold` 字段,为 v3+ FrugalGPT 式主动升级铺路
>
> **v2 修订要点**:
> - 新增 §0 现状核实章节,修正 v1 中所有事实错误(panic hook 已存在 / 行号偏差 / API 拼写 / ProviderClient 真实构造路径)
> - §4.1 panic hook 改为"提取现有内联 hook"而非"新增"
> - §4.5 retry loop 补充状态转换图,修复 v1 状态不可达漏洞(新增 `reset_for_retry` 方法)
> - §4.2 修正 OnceLock 竞态、移除 `.leak()` 反模式
> - §4.4 抽象 `CommandValidationGate`,支持非 Rust 项目
> - §4.2 `upgrade_map` 配置化,先落地 deepseek-v4-pro / deepseek-v4-flash 双模型路由 MVP
> - MVP 范围见 §10:先深度后广度,首期只覆盖 deepseek 双模型

---

## 0. 现状核实(v2 修订)

> 本章节由 v1 审查发现的事实错误逐条核实而成,所有结论均基于源码直接引用。
> 任何后续章节若与本章冲突,以本章为准。

### 0.1 panic hook 现状(v1 错误)

| v1 声称 | 实际 |
|---|---|
| "panic hook **未注册**(全仓 0 处命中)" | **错误**。[main.rs:13-45](../rust/crates/rusty-claude-cli/src/main.rs#L13-L45) 已注册完整 panic hook,落盘到 `~/.claw/claw-crash.log` |

**v1 后果**:方案 §4.1 要求"main.rs:10 第一行加 `install_panic_hook()`",但 `std::panic::set_hook` 是**替换**不是追加,会覆盖现有 hook。

**v2 修正方向**:不是"新增 panic hook",而是**提取** main.rs:13-45 的内联闭包到 `runtime::diag::install_panic_hook()`,main.rs 改为调用它并删除内联代码。这才是"清理旧版"原则的正确应用。

### 0.2 headless.rs panic hook 缺失(v1 描述自相矛盾)

[headless.rs:22-50](../rust/crates/rusty-claude-cli/src/bin/headless.rs#L22-L50) 确实没有 panic hook。v1 §2.2 声称"全仓 0 处命中"与 §4.1 要求"headless.rs 补 hook"自相矛盾。

**v2 修正**:headless.rs 是真实缺口,需补 `install_panic_hook()` 调用。

### 0.3 ProviderClient 构造路径(v1 虚构 API)

| v1 声称 | 实际 |
|---|---|
| `ProviderClient::from_model(m)` 是"伪 API",项目中不存在 | **错误**。[client.rs:17](../rust/crates/api/src/client.rs#L17) `pub fn from_model(model: &str) -> Result<Self, ApiError>` 真实存在 |

**真实构造路径**([client.rs:17-47](../rust/crates/api/src/client.rs#L17-L47)):

```rust
pub fn from_model(model: &str) -> Result<Self, ApiError> {
    Self::from_model_with_anthropic_auth(model, None)
}

pub fn from_model_with_anthropic_auth(
    model: &str,
    anthropic_auth: Option<AuthSource>,
) -> Result<Self, ApiError> {
    let resolved_model = providers::resolve_model_alias(model);
    match providers::detect_provider_kind(&resolved_model) {
        ProviderKind::Anthropic => Ok(Self::Anthropic(...)),
        ProviderKind::Xai => Ok(Self::Xai(...)),
        ProviderKind::OpenAi => {
            // DashScope 走 dashscope config,其他走 openai config
            let config = match providers::metadata_for_model(&resolved_model) {
                Some(meta) if meta.auth_env == "DASHSCOPE_API_KEY" => OpenAiCompatConfig::dashscope(),
                _ => OpenAiCompatConfig::openai(),
            };
            Ok(Self::OpenAi(OpenAiCompatClient::from_env(config)?))
        }
    }
}
```

**可行性结论**:`run_subagent_turn_with_model` 的 client 构造**完全可行**,直接调用 `ProviderClient::from_model(m)` 即可。spike 报告见 §9。

### 0.4 fallback 链机制(v1 行号 + 名称错误)

| v1 声称 | 实际 |
|---|---|
| `tools/src/lib.rs:5180-5214` `providerFallbacks` | 行号偏差;实际 [tools/lib.rs:5217-5224](../rust/crates/tools/src/lib.rs#L5217-L5224) `load_provider_fallback_config()` |
| 配置 key `providerFallbacks` | 正确,但实际类型是 `ProviderFallbackConfig { primary: Option<String>, fallbacks: Vec<String> }`,定义在 [config.rs:582-602](../rust/crates/runtime/src/config.rs#L582-L602) |

**fallback 链构造路径**:
1. [config.rs:1042-1056](../rust/crates/runtime/src/config.rs#L1042-L1056) `parse_optional_provider_fallbacks` 从 `settings.json` 的 `providerFallbacks` 字段解析
2. [tools/lib.rs:5208-5215](../rust/crates/tools/src/lib.rs#L5208-L5215) `build_provider_entry` 用 `ProviderClient::from_model` 构造每个 entry
3. 仅对 retryable 错误触发,与本次 `upgrade_model`(validation 失败触发)**正交**,不会冲突

### 0.5 deepseek-v4-pro / flash 现状(为 MVP 路由核实)

| 维度 | deepseek-v4-pro | deepseek-v4-flash | 源码依据 |
|---|---|---|---|
| ProviderKind | OpenAi | OpenAi | [mod.rs:257-263](../rust/crates/api/src/providers/mod.rs#L257-L263) `openai/` 前缀路由 |
| auth_env | OPENAI_API_KEY | OPENAI_API_KEY | 同上 |
| base_url | OPENAI_BASE_URL(用户自定义 deepseek endpoint) | 同左 | 同上 |
| context_window | 1M tokens | 1M tokens | [mod.rs:645-648](../rust/crates/api/src/providers/mod.rs#L645-L648) |
| reasoning_content_in_history | 需要 | 需要 | [openai_compat.rs:983](../rust/crates/api/src/providers/openai_compat.rs#L983) `starts_with("deepseek-v4")` |
| v1 `tier_for_model` 推断 | Standard(不含 opus/mini/haiku) | **Standard(错误,应为 Budget)** | v1 §4.2 前缀匹配 `mini/haiku/nano` 才归 Budget,但 `flash` 未覆盖 |

**v1 漏洞**:`tier_for_model` 未识别 `flash` 后缀,导致 deepseek-v4-flash 被误判为 Standard。MVP 必须修正(见 §4.2 v2)。

### 0.6 行号引用偏差汇总(v1 错误)

| v1 引用 | 实际位置 | 偏差 |
|---|---|---|
| `execute_dispatch_subagent` @ conversation.rs:1663 | [conversation.rs:1700](../rust/crates/runtime/src/conversation.rs#L1700) | +37 |
| `run_subagent_turn` @ 1777-1876 | [conversation.rs:1814](../rust/crates/runtime/src/conversation.rs#L1814) | +37 |
| `providerFallbacks` @ tools/lib.rs:5180-5214 | [tools/lib.rs:5217-5224](../rust/crates/tools/src/lib.rs#L5217-L5224) | +37 |
| `MODEL_REGISTRY` @ mod.rs:121 | 实际位置正确 | 0 |
| `metadata_for_model` @ mod.rs:234-289 | [mod.rs:235-289](../rust/crates/api/src/providers/mod.rs#L235-L289) | +1 |
| `provider_capabilities_for_model` @ mod.rs:384-450 | [mod.rs:385-450](../rust/crates/api/src/providers/mod.rs#L385-L450) | +1 |

### 0.7 spawn 签名破坏面(v1 低估)

v1 把 `spawn() -> String` 改为 `spawn() -> Result<String, String>`,会破坏:
- [conversation.rs:1737](../rust/crates/runtime/src/conversation.rs#L1737) 唯一运行时调用方(易改)
- [mod.rs:307-463](../rust/crates/runtime/src/multi_agent/mod.rs#L307-L463) **12 处单元测试**(全部需改)

**v2 修正方向**:保留 `spawn` 原签名 `-> String`(能力校验失败时返回 id 并把 warning 写入 Subagent.notes 字段),新增 `spawn_with_model` 扩展方法返回 `Result`。原 12 处测试零改动。

### 0.8 ProviderDiagnostics 字段数(v1 描述不准)

v1 §2.3 称"ProviderDiagnostics 8 布尔位",实际 [mod.rs:104-119](../rust/crates/api/src/providers/mod.rs#L104-L119) 是 **8 bool + 6 非 bool 字段**(`requested_model`/`resolved_model`/`provider`/`auth_env`/`base_url_env`/`default_base_url`)。

---

## 1. 背景与动机

### 1.1 事故复盘

在修复 `claw.exe` 首次运行 wizard 选中 API key 后闪退的问题时，诊断任务交由 deepseek 旧版 API 执行。该模型表现出两个典型问题：

1. **凭直觉而非信号**：在 Windows 双击闪退场景下，第一动作应该是写文件诊断日志确认错误类型，但模型直接堆砌 `render_error_screen` 防御代码（无效）
2. **误判错误机制**：第二轮加 panic hook + `catch_unwind`，但 `panic-wizard.log` 从未创建，说明从一开始就是普通 `Err` 通过 `?` 传播，panic 假设错误

最终用文件诊断日志定位到根因：wizard 保存的 `_wizard` 字段触发 `ConfigLoader` 严格校验拒绝。修复仅一行：`strip_wizard_settings()`（后已升级为 `ConfigLoader` 对 `_` 前缀 key 宽容）。

### 1.2 核心问题

这类问题**必然会在 CLAW 自身执行任务时复现**，因为 CLAW 是多模型编排平台。根因不在某个模型能力不足，而在**平台层缺乏防治机制**：

- 无模型能力分级 → 诊断任务可能被路由到能力不足的模型
- 无验证门禁 → subagent 改完不验证就宣称完成
- 无迭代上限 → 同一问题无限重试浪费资源
- 无统一诊断基础设施 → 能力不足模型无法获得可靠信号

### 1.3 设计目标

从平台层根治"能力不足模型浪费轮次"，而非依赖单个 agent 自觉。三重防护：

1. **模型能力分级 + 任务路由**：诊断任务拒绝交给能力不足的模型
2. **验证门禁**：修改后必须编译验证，失败回滚
3. **迭代上限 + 模型升级**：失败后自动升级模型重试，达上限中止

---

## 2. 现状分析

### 2.1 MultiAgentCoordinator 现状

| 维度 | 位置 | 现状 |
|---|---|---|
| 定义 | `runtime/src/multi_agent/mod.rs:78` | 簿册式 registry，`Arc<Mutex<HashMap<String, Subagent>>>` |
| `start()` | `mod.rs:138-151` | 仅做状态转换 Created→Running，不分发任务 |
| 任务分发 | `conversation.rs:1663` `execute_dispatch_subagent` | 主 agent 通过 `dispatch_subagent` tool call 触发 |
| 执行模型 | `conversation.rs:1777-1876` `run_subagent_turn` | 同步阻塞、单轮 LLM、无线程/channel/async |
| 状态机 | `mod.rs:40-51` | Created→Running→(Completed\|Failed\|Cancelled) |
| 迭代上限 | **无** | subagent 单轮执行，无 loop 无重试 |
| 模型选择 | **无** | subagent 复用主 agent `api_client`，Subagent 无 model 字段 |
| 结果回流 | `conversation.rs:1730-1748` | 三通道：coordinator 状态 + lane event + `.claw/subagents/{id}.md` 文件 |

**关键缺口**：`run_subagent_turn` 注释明确"子智能体走单轮 LLM 请求"，失败仅返回提示语 "You may retry with a different task description"，重试决策权完全交给主 agent LLM，coordinator 自身不自动重试。

### 2.2 诊断日志现状(v2 修正)

| 维度 | 现状 | 源码依据 |
|---|---|---|
| 统一日志模块 | **不存在**。主 CLI crate 不依赖任何日志 crate | — |
| panic hook(主 binary) | **已注册**。[main.rs:13-45](../rust/crates/rusty-claude-cli/src/main.rs#L13-L45) 内联闭包,落盘 `~/.claw/claw-crash.log` | v1 错误声称"全仓 0 处命中" |
| panic hook(headless) | **未注册**。[headless.rs:22-50](../rust/crates/rusty-claude-cli/src/bin/headless.rs#L22-L50) 无 hook | 真实缺口 |
| 文件诊断日志 | 仅 `paste.rs` 的 `paste_diag_log`,覆盖 paste 子系统 | [paste.rs:40-69](../rust/crates/rusty-claude-cli/src/paste.rs#L40-L69) |
| crash log 落盘 | **已存在**于主 binary,但代码内联不可复用 | main.rs:33-42 |
| `diag!` / `tdiag!` 宏 | **不存在** | — |
| TUI 静默 | `paste.rs:18` 的 `TUI_SILENT` AtomicBool + `set_tui_silent`,已就绪可复用 | [paste.rs:16-23](../rust/crates/rusty-claude-cli/src/paste.rs#L16-L23) |
| tracing | `claw-acp` / `claw-shell` 引入但**未初始化 subscriber**,全部 no-op | — |

**关键缺口(v2 修正)**:不是"panic hook 不存在",而是"main.rs 内联 hook 不可复用 + headless 完全缺失"。`paste_diag_log` 是项目里唯一可参考的"flush-to-disk"实现模板(路径策略、门控、时间戳、append 模式都正确),可推广为通用 `diag!` 宏。

### 2.3 模型能力画像现状

| 维度 | 位置 | 现状 |
|---|---|---|
| ProviderKind | `api/src/providers/mod.rs:32-37` | 3 变体：Anthropic / Xai / OpenAi（DashScope 折叠进 OpenAi）|
| 能力标签 | `mod.rs:60-85` | `ProviderCapabilityReport` 10 维三态标记 + `ProviderDiagnostics` 8 布尔位 |
| 能力矩阵 | `mod.rs:384-450` `provider_capabilities_for_model` | 按 provider kind 硬编码 |
| 模型路由 | `mod.rs:205-289` | 别名解析 + 前缀路由 + env sniffing |
| Fallback 链 | `tools/src/lib.rs:5180-5214` | `providerFallbacks` 配置，仅对 retryable 错误触发 |
| Wizard | `tui/wizard.rs:30-78` | 4 个 KNOWN_PROVIDERS，与运行时 ProviderKind 分离 |

**关键发现**：能力标签系统完整，但**仅用于运行时行为判定**（如是否发送 reasoning_effort），**未用于任务路由**。这是本次方案可复用的关键基础设施。

### 2.4 进程入口

| 入口 | 位置 | 用途 |
|---|---|---|
| 主 binary | `rusty-claude-cli/src/main.rs:10` | claw 命令，可插入 panic hook |
| headless | `rusty-claude-cli/src/bin/headless.rs:22` | ACP stdio 服务器 |
| lib run() | `rusty-claude-cli/src/lib.rs:470` | CLI 子命令分派 |
| TUI | `tui/app.rs:62` `run_tui_repl` | TUI 入口，已有 Drop guard |

---

## 3. 架构设计

### 3.1 模块总览

```
┌─────────────────────────────────────────────────────────┐
│                    主 Agent (LLM)                         │
│        通过 dispatch_subagent tool call 触发               │
└──────────────────────┬──────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────┐
│          execute_dispatch_subagent (conversation.rs)    │
│  ┌─────────────────────────────────────────────────┐    │
│  │ 1. 解析 model / complexity / max_attempts       │    │
│  │ 2. 能力校验 (model_meets_complexity)            │    │
│  │ 3. coordinator.spawn(...)                       │    │
│  └──────────────────────┬──────────────────────────┘    │
│                         │                                │
│  ┌──────────────────────▼──────────────────────────┐    │
│  │     重试循环 (for attempt in 1..=max_attempts)  │    │
│  │  ┌─────────────────────────────────────────┐    │    │
│  │  │ run_subagent_turn_with_model            │    │    │
│  │  │   - 独立 ApiRequest + system prompt      │    │    │
│  │  │   - 诊断 SOP 注入 (Diagnostic 复杂度)   │    │    │
│  │  └─────────────────┬───────────────────────┘    │    │
│  │                    │                             │    │
│  │  ┌─────────────────▼───────────────────────┐    │    │
│  │  │       验证门禁 (ValidationGate)          │    │    │
│  │  │   - CompileValidationGate: cargo build  │    │    │
│  │  │   - (可扩展其他 gate)                    │    │    │
│  │  └─────────────────┬───────────────────────┘    │    │
│  │                    │                             │    │
│  │           通过 ────── 失败(retryable)             │    │
│  │            │            │                        │    │
│  │        complete     upgrade_model + 重试         │    │
│  └──────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────┐
│              runtime::diag (统一诊断)                     │
│  - diag! 宏 (flush-to-disk, TUI 感知)                    │
│  - install_panic_hook() → claw-crash.log                 │
│  - CLAW_DIAG=1 / --diag 开启                             │
└─────────────────────────────────────────────────────────┘
```

### 3.2 六大模块

| # | 模块 | 职责 | 新增/修改 |
|---|---|---|---|
| 1 | `runtime::diag` | 统一诊断日志 + panic hook + crash log | 新建 1 + 改 3 |
| 2 | `api::model_tier` | 模型能力分级 + 任务复杂度匹配 | 新建 1 + 导出 |
| 3 | Subagent 字段扩展 | model/complexity/attempts/validated | 改 2 (mod.rs, conversation.rs) |
| 4 | 验证门禁 trait | ValidationGate + CompileValidationGate | 新建 1 + 改 mod.rs |
| 5 | 重试 + 升级 loop | 迭代上限 + 模型升级链 | 改 conversation.rs |
| 6 | 诊断 SOP 注入 | Diagnostic 复杂度时注入系统提示 | 改 conversation.rs |

---

## 4. 详细设计

### 4.1 模块 1：统一诊断 `runtime::diag`(v2 修正)

**目标(v2 修正)**:**提取** main.rs:13-45 内联 panic hook 到 `runtime::diag`,补齐 headless 缺口,推广 `paste_diag_log` 为全 crate 通用。**不是"新增 panic hook"**(v1 错误)。

**新增文件**：`rust/crates/runtime/src/diag.rs`

```rust
//! 统一诊断日志 — flush-to-disk，TUI 感知，panic 自动落盘。
//! 推广自 paste.rs 的 paste_diag_log 模式。
//! panic hook 提取自 main.rs:13-45 的内联闭包(v2 修正)。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

// v2 修正:OnceLock<bool> 存在竞态 — is_enabled() 先调用会初始化为 false,
// 后续 enable() 的 set(true) 会静默失败。改用 AtomicBool + 显式三态。
// UNINIT=未决定 / ENABLED=开启 / DISABLED=关闭(由首次环境变量读取决定)
use std::sync::OnceLock;

/// 诊断日志开关。
/// v2 修正:用 AtomicBool 替代 OnceLock<bool>,支持运行时强制开启。
static DIAG_ENABLED: AtomicBool = AtomicBool::new(false);
/// 是否已初始化(首次 is_enabled() 调用时从环境变量读取)。
static DIAG_INITIALIZED: OnceLock<()> = OnceLock::new();

/// TUI 静默标志（true 时禁用 stderr 输出，仅写文件）。
static TUI_SILENT: AtomicBool = AtomicBool::new(false);

/// 诊断日志路径：~/.claw/claw-diag.log
pub fn diag_log_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claw").join("claw-diag.log")
}

/// 是否启用诊断日志。
/// v2 修正:首次调用从 CLAW_DIAG 环境变量初始化,后续可被 enable() 覆盖。
pub fn is_enabled() -> bool {
    DIAG_INITIALIZED.get_or_init(|| {
        let env_on = std::env::var("CLAW_DIAG")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        DIAG_ENABLED.store(env_on, Ordering::Relaxed);
    });
    DIAG_ENABLED.load(Ordering::Relaxed)
}

/// 强制开启诊断（由 --diag CLI flag 调用）。
/// v2 修正:用 AtomicBool 的 store 替代 OnceLock::set,避免静默失败。
pub fn enable() {
    DIAG_ENABLED.store(true, Ordering::Relaxed);
    let _ = DIAG_INITIALIZED.set(());
}

/// 设置 TUI 静默模式（由 TUI 入口调用）。
/// v2 新增:同步 paste.rs 的 TUI_SILENT,使两者保持一致。
pub fn set_tui_silent(silent: bool) {
    TUI_SILENT.store(silent, Ordering::Relaxed);
}

/// 写一条诊断日志（flush-to-disk，即使硬崩溃也保留最后状态）。
pub fn log(msg: &str) {
    if !is_enabled() { return; }
    let path = diag_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{ts}] {msg}");
        let _ = f.flush(); // 关键：立即 flush
    }
    // 非 TUI 模式下同时输出到 stderr
    if !TUI_SILENT.load(Ordering::Relaxed) {
        eprintln!("[diag] {msg}");
    }
}

/// 注册 panic hook — 把 panic 信息落盘到 claw-crash.log。
/// v2 修正:本函数**提取**自 main.rs:13-45 的内联闭包,语义保持一致。
/// 必须在 main() 第一行调用。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        let location = info.location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());

        // 落盘 crash log
        let crash_path = diag_log_path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("claw-crash.log");
        if let Some(parent) = crash_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &crash_path,
            format!(
                "PANIC at {location}\nMessage: {msg}\nTimestamp: {}\n\n",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ),
        );

        // 仍输出到 stderr（非 TUI 模式可见）
        eprintln!("thread panicked at {location}: {msg}");
        eprintln!("Crash log written to: {}", crash_path.display());
    }));
}
```

**导出宏 `diag!`**（在 `runtime/src/lib.rs`,需配合 `pub mod diag;`）：

```rust
pub mod diag;  // v2 新增:必须 pub 才能让 $crate::diag::log 路径可达

#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {
        $crate::diag::log(&format!($($arg)*));
    };
}
```

**修改点(v2 修正)**：

- **[main.rs:13-45](../rust/crates/rusty-claude-cli/src/main.rs#L13-L45)**:**删除**内联 panic hook 闭包,改为第一行调用 `runtime::diag::install_panic_hook();`(v1 错误声称"新增",实际是提取替换)
- **[bin/headless.rs:22](../rust/crates/rusty-claude-cli/src/bin/headless.rs#L22)**:`fn main()` 第一行加 `runtime::diag::install_panic_hook();`(真实缺口,补齐)
- **[paste.rs:18](../rust/crates/rusty-claude-cli/src/paste.rs#L18)** `TUI_SILENT`:改为委托调用 `runtime::diag::set_tui_silent()`,paste.rs 保留 `set_tui_silent` 包装函数但内部转发,消除两个独立 AtomicBool 不同步风险
- **[paste.rs:40-69](../rust/crates/rusty-claude-cli/src/paste.rs#L40-L69)** `paste_diag_log`:改为调用 `runtime::diag::log`,保留 `paste_log!` 宏语义不变

**设计要点(v2 修正)**：
- ~~`OnceLock<bool>` 缓存开关检查~~ → 改用 `AtomicBool + OnceLock<()>` 初始化门控,支持 `enable()` 运行时强制开启(修复 v1 竞态)
- `flush()` 立即落盘，即使硬崩溃（segfault）也保留最后状态
- panic hook 落盘 `claw-crash.log` 与常规诊断日志分离，便于 crash 快速定位
- TUI 感知:`TUI_SILENT` 统一到 `runtime::diag`,paste.rs 转发调用,消除双 AtomicBool 不同步风险

---

### 4.2 模块 2：模型能力分级 `api::model_tier`(v2 修正)

**目标**：给 subagent 增加独立 model 选择，按任务复杂度路由到能力匹配的模型。

**v2 修正要点**:
- 移除 `.leak()` 反模式(v1 错误)
- 修正 `tier_for_model` 未识别 `flash` 后缀的漏洞(deepseek-v4-flash 误判)
- `upgrade_model` 改为配置化 `upgrade_map`,首期覆盖 deepseek-v4-pro/flash 双模型 MVP

**新增文件**：`rust/crates/api/src/providers/model_tier.rs`

```rust
//! 模型能力分级 — 用于 subagent 任务路由。
//! 基于 ProviderCapabilityReport 但聚焦"任务可执行性"而非协议细节。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 模型能力层级（粗粒度，用于任务路由）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    /// 旗舰模型（Claude Opus / GPT-4.1 / Grok-3 / deepseek-v4-pro）— 复杂推理、架构设计、诊断
    Flagship,
    /// 标准模型（Claude Sonnet / GPT-4.1-mini / Grok-3-mini）— 常规任务
    Standard,
    /// 轻量模型（Haiku / flash / nano / 本地模型）— 简单编辑、格式化
    Budget,
}

/// 任务复杂度需求 — 由调用方声明，coordinator 据此匹配模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskComplexity {
    /// 简单任务：单文件编辑、已知模式
    Simple,
    /// 诊断任务：根因定位、复杂调试 — 需要强推理能力
    Diagnostic,
    /// 架构决策：多方案评估、trade-off 分析
    Architectural,
}

impl TaskComplexity {
    /// 返回此复杂度所需的最低 ModelTier。
    pub fn min_tier(self) -> ModelTier {
        match self {
            Self::Simple => ModelTier::Budget,
            Self::Diagnostic => ModelTier::Flagship, // 关键：诊断任务必须旗舰
            Self::Architectural => ModelTier::Flagship,
        }
    }
}

/// 按模型名推断能力层级。
/// v2 修正:新增 `flash` 后缀识别(deepseek-v4-flash 归 Budget),
/// v1 仅匹配 mini/haiku/nano 导致 flash 误归 Standard。
pub fn tier_for_model(model: &str) -> ModelTier {
    let lower = model.to_ascii_lowercase();
    // 旗舰：opus / gpt-4.1(非 mini) / grok-3(非 mini/flash) / o3 / o4 / deepseek-v4-pro
    if lower.contains("opus")
        || (lower.starts_with("gpt-4.1") && !lower.contains("mini"))
        || lower == "grok-3"
        || lower.starts_with("o3") || lower.starts_with("o4")
        || lower.ends_with("-pro") // v2: deepseek-v4-pro / qwen-max-pro 等
    {
        ModelTier::Flagship
    }
    // 轻量：haiku / mini / nano / flash(v2 新增)
    else if lower.contains("haiku")
        || lower.contains("mini")
        || lower.contains("nano")
        || lower.contains("flash") // v2: deepseek-v4-flash / grok-flash 等
    {
        ModelTier::Budget
    }
    // 默认标准
    else {
        ModelTier::Standard
    }
}

/// 检查模型是否满足任务复杂度需求。
pub fn model_meets_complexity(model: &str, complexity: TaskComplexity) -> bool {
    tier_for_model(model) >= complexity.min_tier()
}

/// v2 新增:配置化的模型升级路径表。
/// 从 ~/.claw/model-upgrades.json 加载,允许用户自定义升级链。
/// 首期 MVP 内置默认表覆盖 deepseek-v4-pro/flash 双模型路由。
///
/// v3 修订(P0 成本门禁):升级条目从 String 扩展为 UpgradeEntry,
/// 包含成本倍数(cost_multiplier)和置信度阈值(confidence_threshold,留待 v3 主动升级)。
/// 借鉴 Router-R1(NeurIPS 2025)成本奖励 + FrugalGPT 成本感知级联思路。
///
/// 配置文件格式(JSON,v3):
/// {
///   "upgrades": {
///     "deepseek-v4-flash": {
///       "target_model": "deepseek-v4-pro",
///       "cost_multiplier": 10.0,
///       "confidence_threshold": 0.0
///     },
///     "haiku": {
///       "target_model": "sonnet",
///       "cost_multiplier": 5.0,
///       "confidence_threshold": 0.0
///     }
///   }
/// }
///
/// v2 兼容格式(纯字符串,自动转换为 UpgradeEntry,cost_multiplier=1.0):
/// { "upgrades": { "deepseek-v4-flash": "deepseek-v4-pro" } }
#[derive(Debug, Clone)]
pub struct UpgradeEntry {
    /// 升级目标模型
    pub target_model: String,
    /// v3 新增(P0):成本倍数,升级后单次调用成本 = 原成本 × cost_multiplier。
    /// retry loop 用此值估算升级后总成本,超 cost_limit 则拒绝升级。
    /// deepseek-v4-pro 相对 flash 约 10 倍成本(输入)+ 30 倍(输出)。
    pub cost_multiplier: f64,
    /// v3 预留(P3):置信度阈值,低于此值主动升级(不必等 validation 失败)。
    /// MVP 阶段不用,设 0.0 表示禁用主动升级。
    pub confidence_threshold: f64,
}

impl UpgradeEntry {
    /// v2 兼容:从纯字符串构造,成本倍数默认 1.0(不增量)。
    fn from_simple(target: &str) -> Self {
        Self {
            target_model: target.to_string(),
            cost_multiplier: 1.0,
            confidence_threshold: 0.0,
        }
    }
}

/// MVP 默认表(文件不存在时使用)。
/// v3 修订:包含成本倍数,用于 P0 成本门禁计算。
static DEFAULT_UPGRADES: &[(&str, &str, f64)] = &[
    // deepseek 双模型路由 MVP — flash 升级到 pro,成本约 10 倍
    ("deepseek-v4-flash", "deepseek-v4-pro", 10.0),
    ("deepseek-v4-pro", "deepseek-v4-pro", 1.0), // 已是旗舰,返回自身(调用方判 None)
    // Anthropic 链
    ("haiku", "sonnet", 5.0),
    ("sonnet", "opus", 15.0),
    // OpenAI 链
    ("gpt-4.1-mini", "gpt-4.1", 5.0),
    ("gpt-4.1", "o3", 20.0),
    // xAI 链
    ("grok-3-mini", "grok-3", 8.0),
];

/// 加载升级表(配置文件优先,回退到 DEFAULT_UPGRADES)。
/// v3 修订:返回 HashMap<String, UpgradeEntry>,支持成本倍数。
fn upgrade_map() -> HashMap<String, UpgradeEntry> {
    static CACHED: OnceLock<HashMap<String, UpgradeEntry>> = OnceLock::new();
    CACHED.get_or_init(|| {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from);
        if let Some(home) = home {
            let path = home.join(".claw").join("model-upgrades.json");
            if let Ok(content) = std::fs::read_to_string(&path) {
                // v3:尝试解析为 UpgradeEntry 格式(含 cost_multiplier)
                #[derive(serde::Deserialize)]
                struct UpgradeFileV3 {
                    upgrades: HashMap<String, UpgradeEntryV3>,
                }
                #[derive(serde::Deserialize)]
                #[serde(untagged)]
                enum UpgradeEntryV3 {
                    // v3 格式:对象 { target_model, cost_multiplier, confidence_threshold }
                    Full {
                        target_model: String,
                        #[serde(default = "default_cost_multiplier")]
                        cost_multiplier: f64,
                        #[serde(default)]
                        confidence_threshold: f64,
                    },
                    // v2 兼容格式:纯字符串
                    Simple(String),
                }
                fn default_cost_multiplier() -> f64 { 1.0 }
                if let Ok(parsed) = serde_json::from_str::<UpgradeFileV3>(&content) {
                    return parsed.upgrades.into_iter().map(|(k, v)| {
                        let entry = match v {
                            UpgradeEntryV3::Full { target_model, cost_multiplier, confidence_threshold } => UpgradeEntry {
                                target_model,
                                cost_multiplier,
                                confidence_threshold,
                            },
                            UpgradeEntryV3::Simple(target) => UpgradeEntry::from_simple(&target),
                        };
                        (k.to_ascii_lowercase(), entry)
                    }).collect();
                }
            }
        }
        // 回退到默认表
        DEFAULT_UPGRADES.iter()
            .map(|(k, v, cost)| ((*k).to_string(), UpgradeEntry {
                target_model: (*v).to_string(),
                cost_multiplier: *cost,
                confidence_threshold: 0.0,
            }))
            .collect()
    }).clone()
}

/// 模型升级路径 — 失败重试时按能力递增。
/// v2 修正:移除 .leak() 反模式,改为查表 + 显式 String 返回。
/// v3 修订:返回 UpgradeEntry(含成本倍数),供 retry loop 做成本门禁。
/// 返回 None 表示已是最高层级(或当前模型 == 升级目标),无法升级。
pub fn upgrade_model(current: &str, _complexity: TaskComplexity) -> Option<UpgradeEntry> {
    let lower = current.to_ascii_lowercase();
    let map = upgrade_map();
    match map.get(&lower) {
        Some(entry) if entry.target_model != lower => Some(entry.clone()),
        _ => None, // 无升级路径 或 已是顶级
    }
}

/// v3 新增(P0):查询模型成本倍数(相对基准模型)。
/// 用于成本门禁计算。基准为 deepseek-v4-flash(1.0)。
pub fn model_cost_multiplier(model: &str) -> f64 {
    let lower = model.to_ascii_lowercase();
    let map = upgrade_map();
    // 查当前模型的升级条目,取其 cost_multiplier
    map.get(&lower).map(|e| e.cost_multiplier).unwrap_or(1.0)
}
```

**设计要点(v3 修订)**：
- `ModelTier` 用 `PartialOrd` 派生，可直接 `>=` 比较层级
- `tier_for_model` 新增 `flash`/`-pro` 后缀识别,修复 deepseek-v4-flash 误判
- ~~`upgrade_model` 实现简化版升级链~~ → 改为配置化 `upgrade_map`,从 `~/.claw/model-upgrades.json` 加载,回退到 MVP 默认表
- 移除 `.leak()` 反模式(v1 错误),改为 `String::clone()` 显式堆分配
- 与现有 `ProviderCapabilityReport` 正交：后者描述协议能力（streaming/cache），本模块描述任务能力
- **v3 修订(P0 成本门禁)**:`UpgradeEntry` 新增 `cost_multiplier` 字段,借鉴 Router-R1 成本奖励 + FrugalGPT 成本感知级联。retry loop 用此值估算升级后总成本,超 `cost_limit` 则拒绝升级(见 §4.5)
- **v3 预留(P3 主动升级)**:`confidence_threshold` 字段,留待 v3 实现 FrugalGPT 式主动升级(不必等 validation 失败,置信度低就升级)。MVP 阶段设 0.0 禁用
- **v3 兼容性**:`UpgradeEntryV3` 使用 `#[serde(untagged)]` 支持 v2 纯字符串格式自动转换,旧配置文件无需修改
- **MVP 边界**:`DEFAULT_UPGRADES` 首期只覆盖 deepseek-v4-flash → deepseek-v4-pro 单跳升级(cost_multiplier=10.0),验证流程后再扩展(见 §10)

---

### 4.3 模块 3：Subagent 字段扩展(v3 修订)

**修改文件**：`rust/crates/runtime/src/multi_agent/mod.rs`

**v2 修正要点**:
- 保留 `spawn` 原签名 `-> String`(避免破坏 12 处测试)
- 新增 `spawn_with_model` 扩展方法返回 `Result`
- 新增 `reset_for_retry` 方法(修复 v1 retry loop 状态不可达漏洞)
- 新增 `notes` 字段记录能力校验 warning(而非直接拒绝 spawn)

**v3 修订要点**:
- 新增 `checkpoint_path` 字段(P1,借鉴 LangGraph/Anthropic durable execution)
- 新增 `cost_limit` 字段(P0,借鉴 Router-R1 成本奖励 + FrugalGPT 成本感知)
- 新增 `cost_accumulated` 字段(P0,累计已消耗成本)
- 新增 `save_checkpoint` 预留方法(P1,MVP 不实现 restore)

```rust
// Subagent 结构体增加字段（mod.rs:55-74）
pub struct Subagent {
    pub id: String,
    pub name: String,
    pub mode: CoordinationMode,
    pub task: String,
    pub status: SubagentStatus,
    pub workdir: Option<PathBuf>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub result: Option<String>,
    // ── 新增字段 ──
    /// subagent 使用的模型（None 时复用主 agent client）。
    pub model: Option<String>,
    /// 任务复杂度（用于能力校验和升级路由）。
    pub complexity: TaskComplexity,
    /// 最大尝试次数（默认 1，诊断任务建议 3）。
    pub max_attempts: u32,
    /// 当前尝试次数。
    pub attempts: u32,
    /// 验证门禁结果（None=未验证，Some(true)=通过，Some(false)=失败）。
    pub validated: Option<bool>,
    /// v2 新增:能力校验 warning 或其他诊断信息(非致命)。
    pub notes: Vec<String>,
    /// v3 新增(P1 checkpoint):checkpoint 文件路径,每轮 turn 后保存对话历史+中间状态。
    /// 借鉴 LangGraph/Temporal/Microsoft Agent Framework 的 durable execution 模式,
    /// 以及 Anthropic "resume from where the agent was" 工程实践。
    /// MVP 阶段 save_checkpoint 先落地,restore_from_checkpoint 留待 v2。
    pub checkpoint_path: Option<PathBuf>,
    /// v3 新增(P0 成本门禁):本 subagent 的成本上限(美元)。
    /// retry loop 升级模型前检查 cost_accumulated + 预估成本,超限则拒绝升级。
    /// 借鉴 Router-R1 成本奖励 + FrugalGPT 成本感知级联。
    pub cost_limit: f64,
    /// v3 新增(P0 成本门禁):累计已消耗成本(美元)。
    /// 每轮 turn 后根据 token 用量 × 模型单价累加。
    pub cost_accumulated: f64,
}
```

**v2 修正:`spawn` 保持原签名,新增 `spawn_with_model`**（`mod.rs:104-109`）：

```rust
impl MultiAgentCoordinator {
    /// 原有 spawn — 保持签名不变,内部委托 spawn_with_model。
    /// v2 修正:不改为 Result 返回,避免破坏 12 处现有测试。
    pub fn spawn(
        &self,
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
    ) -> String {
        // 原行为:无 model/complexity,默认 Simple + max_attempts=1
        self.spawn_with_model(name, task, mode, None, TaskComplexity::Simple, 1)
            .unwrap_or_else(|_| "spawn-failed".to_string())
    }

    /// v2 新增:带模型选择的 spawn。
    /// 能力校验失败时返回 Err(调用方决定是否降级)。
    pub fn spawn_with_model(
        &self,
        name: impl Into<String>,
        task: impl Into<String>,
        mode: CoordinationMode,
        model: Option<String>,
        complexity: TaskComplexity,
        max_attempts: u32,
    ) -> Result<String, String> {
        // 能力校验:如果指定了 model,检查是否满足 complexity
        let mut notes = Vec::new();
        if let Some(ref m) = &model {
            if !model_meets_complexity(m, complexity) {
                // v2 修正:不直接拒绝,记录 warning 并允许 spawn
                // (能力不足模型仍可尝试,coordinator 会在 retry 时升级)
                notes.push(format!(
                    "warning: model '{}' does not meet complexity {:?} (requires {:?} or above)",
                    m, complexity, complexity.min_tier()
                ));
            }
        }
        // ... 原有 id 生成 + workdir 逻辑 ...
        // 构造 Subagent 时填充新字段
        Ok(id)
    }

    /// v2 新增:重置 subagent 状态以供重试。
    /// 修复 v1 retry loop 状态不可达漏洞:
    /// v1 中 run_subagent_turn 成功后调用 complete(),status 变 Completed,
    /// 但 validate 失败后重试时 complete() 会拒绝("cannot complete from Completed")。
    /// 本方法把 Completed → Running,清空 result,attempts 保留累加。
    pub fn reset_for_retry(&self, subagent_id: &str) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        // 允许从 Completed/Failed 重置(但 Running 状态说明上轮未结束,拒绝)
        if agent.status == SubagentStatus::Running {
            return Err(format!(
                "subagent {subagent_id} still running, cannot reset_for_retry"
            ));
        }
        agent.status = SubagentStatus::Running;
        agent.completed_at = None;
        agent.result = None;
        agent.validated = None;
        // attempts 保留累加(由 increment_attempts 维护)
        Ok(())
    }

    /// v2 新增:递增尝试次数。
    pub fn increment_attempts(&self, subagent_id: &str) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        agent.attempts += 1;
        Ok(())
    }

    /// v3 新增(P0 成本门禁):记录一轮 turn 的成本消耗。
    /// 借鉴 Router-R1 成本奖励 `R_cost ∝ -m(P_LLM) · T_out` 思路,
    /// 每轮 turn 后根据 token 用量 × 模型单价累加到 cost_accumulated。
    ///
    /// 参数:
    /// - `subagent_id`: subagent ID
    /// - `tokens_in`: 输入 token 数
    /// - `tokens_out`: 输出 token 数
    /// - `model`: 使用的模型(用于查询单价)
    pub fn record_cost(
        &self,
        subagent_id: &str,
        tokens_in: u32,
        tokens_out: u32,
        model: &str,
    ) -> Result<(), String> {
        let mut agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents
            .get_mut(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        // 查询模型单价(美元/1M tokens),MVP 硬编码 deepseek 双模型
        let (price_in, price_out) = model_pricing(model);
        let cost = (tokens_in as f64 / 1_000_000.0) * price_in
                 + (tokens_out as f64 / 1_000_000.0) * price_out;
        agent.cost_accumulated += cost;
        Ok(())
    }

    /// v3 新增(P0 成本门禁):检查升级后预估成本是否超限。
    /// 在 retry loop 调用 upgrade_model 前,先调用此方法。
    /// 借鉴 FrugalGPT 成本感知级联 + Router-R1 成本奖励。
    ///
    /// 返回 Ok(()) 表示可以升级,Err 表示超限应中止。
    pub fn check_cost_limit(
        &self,
        subagent_id: &str,
        upgraded_model: &str,
        estimated_tokens: u32,
    ) -> Result<(), String> {
        let agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents.get(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        let (price_in, price_out) = model_pricing(upgraded_model);
        let estimated_cost = (estimated_tokens as f64 / 1_000_000.0) * (price_in + price_out);
        if agent.cost_accumulated + estimated_cost > agent.cost_limit {
            return Err(format!(
                "cost limit exceeded: accumulated ${:.4} + estimated ${:.4} > limit ${:.4}",
                agent.cost_accumulated, estimated_cost, agent.cost_limit
            ));
        }
        Ok(())
    }

    /// v3 新增(P1 checkpoint):保存 subagent 状态到 checkpoint 文件。
    /// 借鉴 LangGraph/Temporal durable execution + Anthropic "resume from where the agent was"。
    /// MVP 阶段仅保存元状态,对话历史由 conversation.rs 注入(留待 v2)。
    /// restore_from_checkpoint 留待 v2 实现。
    pub fn save_checkpoint(&self, subagent_id: &str) -> Result<(), String> {
        let agents = self.subagents.lock().expect("subagents lock poisoned");
        let agent = agents.get(subagent_id)
            .ok_or_else(|| format!("subagent not found: {subagent_id}"))?;
        if let Some(path) = &agent.checkpoint_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let state = serde_json::json!({
                "id": agent.id,
                "task": agent.task,
                "model": agent.model,
                "attempts": agent.attempts,
                "max_attempts": agent.max_attempts,
                "validated": agent.validated,
                "notes": agent.notes,
                "cost_accumulated": agent.cost_accumulated,
                "cost_limit": agent.cost_limit,
                "status": format!("{:?}", agent.status),
                "saved_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
            });
            let bytes = serde_json::to_vec_pretty(&state)
                .map_err(|e| format!("serialize checkpoint failed: {e}"))?;
            std::fs::write(path, bytes)
                .map_err(|e| format!("write checkpoint failed: {e}"))?;
        }
        Ok(())
    }

    /// v3 新增(P0):查询 subagent 的成本上限(用于错误消息展示)。
    pub fn get_cost_limit(&self, subagent_id: &str) -> Option<f64> {
        let agents = self.subagents.lock().expect("subagents lock poisoned");
        agents.get(subagent_id).map(|a| a.cost_limit)
    }

    /// v3 新增(P1):并行 spawn 多个 subagent(预留接口)。
    /// 借鉴 Anthropic Multi-Agent Research System:lead agent 并行 spawn 3-5 subagents,
    /// 复杂查询研究时间减少 90%。
    /// MVP 阶段退化为串行调用 spawn_with_model,v2 接入 tokio 实现真并行。
    pub fn spawn_parallel(&self, tasks: Vec<SpawnRequest>) -> Vec<Result<String, String>> {
        // MVP:串行执行,v2 改为 tokio::join_all 并行
        tasks.into_iter()
            .map(|t| self.spawn_with_model(
                t.name, t.task, t.mode, t.model, t.complexity, t.max_attempts,
            ))
            .collect()
    }
}

/// v3 新增(P1):spawn_parallel 的请求参数。
pub struct SpawnRequest {
    pub name: String,
    pub task: String,
    pub mode: CoordinationMode,
    pub model: Option<String>,
    pub complexity: TaskComplexity,
    pub max_attempts: u32,
}

/// v3 新增(P0 成本门禁):模型单价表(美元/1M tokens)。
/// MVP 硬编码 deepseek 双模型,后续可从配置加载。
/// 价格参考 deepseek 官网 2025-07 定价。
fn model_pricing(model: &str) -> (f64, f64) {
    let lower = model.to_ascii_lowercase();
    match lower.as_str() {
        "deepseek-v4-flash" => (0.14, 0.28),   // 输入 $0.14/M,输出 $0.28/M
        "deepseek-v4-pro"   => (1.40, 2.80),   // 输入 $1.40/M,输出 $2.80/M(约 10 倍 flash)
        // 其他模型回退到 deepseek-v4-flash 价格(保守估算)
        _ => (0.14, 0.28),
    }
}
```

---

### 4.4 模块 4：验证门禁(v2 修正)

**v2 修正要点**:
- 抽象 `CommandValidationGate` 支持任意命令(cargo/npm/python/pytest 等),非 Rust 项目可用
- 引入 `ValidationContext` 传递 workspace_root / changed_files / model,避免每个 gate 重复造轮子
- 用 `git diff --name-only` 检测实际修改的文件,而非 v1 的 result_text 关键字匹配(避免误判)

**新增文件**：`rust/crates/runtime/src/multi_agent/validation.rs`

```rust
//! subagent 完成后的验证门禁。
//! 实现此 trait 并注册到 coordinator，可在 subagent 完成后自动验证。

use std::path::{Path, PathBuf};

/// v2 新增:验证上下文 — 传递给每个 gate,避免 trait 方法参数膨胀。
/// v1 trait 方法签名只有 (id, task, result_path),gate 实现需自带 workspace_root,
/// 现在改为通过 context 统一注入。
pub struct ValidationContext<'a> {
    pub subagent_id: &'a str,
    pub task: &'a str,
    pub result_path: &'a Path,        // .claw/subagents/{id}.md
    pub workspace_root: &'a Path,
    pub changed_files: &'a [PathBuf], // git diff --name-only 的结果
    pub model: &'a str,
}

/// subagent 完成后的验证门禁。
pub trait ValidationGate: Send + Sync {
    /// 验证 subagent 结果。返回 Err 表示验证失败。
    fn validate(&self, ctx: &ValidationContext) -> Result<(), ValidationError>;

    /// v2 新增:gate 名称,用于诊断日志。
    fn name(&self) -> &'static str { "unnamed" }
}

#[derive(Debug)]
pub struct ValidationError {
    pub message: String,
    pub retryable: bool,    // true=可重试，false=直接失败
}

/// v2 新增:通用命令验证门禁。
/// 支持任意 shell 命令(cargo build / npm run build / python -m pytest 等),
/// 通过 `file_filter` 正则决定是否触发(避免非代码修改触发编译)。
///
/// 示例:
/// - Rust:  CommandValidationGate::new("cargo-build", ["cargo","build","--release"], r"\.rs$")
/// - Node:  CommandValidationGate::new("npm-build", ["npm","run","build"], r"\.(ts|tsx|js|jsx)$")
/// - Python:CommandValidationGate::new("pytest", ["python","-m","pytest"], r"\.py$")
pub struct CommandValidationGate {
    gate_name: String,
    command: Vec<String>,
    workspace_root: PathBuf,
    file_filter: regex::Regex,  // 匹配 changed_files 中任一文件才触发
}

impl CommandValidationGate {
    pub fn new(
        name: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
        workspace_root: impl Into<PathBuf>,
        file_filter_pattern: &str,
    ) -> Self {
        Self {
            gate_name: name.into(),
            command: command.into_iter().map(|s| s.into()).collect(),
            workspace_root: workspace_root.into(),
            file_filter: regex::Regex::new(file_filter_pattern)
                .expect("invalid file_filter pattern"),
        }
    }
}

impl ValidationGate for CommandValidationGate {
    fn validate(&self, ctx: &ValidationContext) -> Result<(), ValidationError> {
        // v2 修正:用 changed_files + 正则判断是否触发,而非 v1 的 result_text 关键字匹配
        let triggered = ctx.changed_files.iter().any(|f| {
            self.file_filter.is_match(&f.to_string_lossy())
        });
        if !triggered {
            return Ok(()); // 无相关文件修改,跳过
        }
        let output = std::process::Command::new(&self.command[0])
            .args(&self.command[1..])
            .current_dir(&self.workspace_root)
            .output();
        match output {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(ValidationError {
                message: format!(
                    "{} failed:\nstdout: {}\nstderr: {}",
                    self.gate_name,
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr),
                ),
                retryable: true,
            }),
            Err(e) => Err(ValidationError {
                message: format!("failed to run {}: {e}", self.gate_name),
                retryable: false, // 环境错误不可重试
            }),
        }
    }

    fn name(&self) -> &'static str {
        // Box<str> 避免 'static 约束问题;此处简化,实际可用 Box<dyn Display>
        "command"
    }
}

/// v2 保留:Rust 专用门禁(基于 CommandValidationGate 的便捷构造)。
pub fn rust_compile_gate(workspace_root: PathBuf) -> CommandValidationGate {
    CommandValidationGate::new(
        "cargo-build",
        ["cargo", "build", "--release"],
        workspace_root,
        r"\.rs$",
    )
}

/// v2 新增:从 git diff 检测 subagent 修改的文件。
/// 在 coordinator.validate() 调用前执行,结果填入 ValidationContext。
pub fn detect_changed_files(workspace_root: &Path) -> Vec<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(workspace_root)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(PathBuf::from)
                .collect()
        }
        _ => Vec::new(), // 非 git 仓库或 git 不可用,返回空(门禁可能跳过)
    }
}
```

**Coordinator 集成**（`multi_agent/mod.rs`）：

```rust
pub struct MultiAgentCoordinator {
    subagents: Arc<Mutex<HashMap<String, Subagent>>>,
    id_counter: Arc<Mutex<u64>>,
    // 新增：验证门禁链
    validation_gates: Arc<Mutex<Vec<Box<dyn ValidationGate>>>>,
    // v2 新增:workspace_root,用于 detect_changed_files
    workspace_root: Option<Arc<PathBuf>>,
}

impl MultiAgentCoordinator {
    /// 注册验证门禁。
    pub fn add_validation_gate(&self, gate: Box<dyn ValidationGate>) {
        self.validation_gates.lock().expect("gates lock")
            .push(gate);
    }

    /// v2 新增:设置 workspace_root(从 ConversationRuntime 注入)。
    pub fn set_workspace_root(&self, root: PathBuf) {
        if let Some(slot) = &self.workspace_root {
            let mut guard = slot.lock().expect("workspace_root lock");
            *guard = root;
        }
    }

    /// 验证 subagent 结果 — 调用所有注册的 gate。
    /// v2 修正:构造 ValidationContext,注入 workspace_root / changed_files / model。
    pub fn validate(&self, subagent_id: &str) -> Result<(), ValidationError> {
        let agents = self.subagents.lock().expect("lock");
        let agent = agents.get(subagent_id)
            .ok_or_else(|| ValidationError { message: "not found".into(), retryable: false })?;
        if agent.status != SubagentStatus::Completed {
            return Err(ValidationError { message: "not completed".into(), retryable: false });
        }
        let task = agent.task.clone();
        let result = agent.result.clone().unwrap_or_default();
        let model = agent.model.clone().unwrap_or_default();
        drop(agents); // 释放锁

        let result_path = std::path::PathBuf::from(&result);
        let workspace_root = self.workspace_root
            .as_ref()
            .and_then(|w| w.lock().ok().map(|g| g.clone()))
            .unwrap_or_else(|| PathBuf::from("."));
        let changed_files = detect_changed_files(&workspace_root);

        let ctx = ValidationContext {
            subagent_id,
            task: &task,
            result_path: &result_path,
            workspace_root: &workspace_root,
            changed_files: &changed_files,
            model: &model,
        };

        let gates = self.validation_gates.lock().expect("gates lock");
        for gate in gates.iter() {
            gate.validate(&ctx)?;
        }
        Ok(())
    }
}
```

**设计要点(v2 修正)**：
- ~~trait 方法签名 (id, task, result_path)~~ → 改为 `ValidationContext` 统一注入,避免每个 gate 自带 workspace_root
- ~~CompileValidationGate 硬编码 cargo~~ → 抽象为 `CommandValidationGate`,支持任意命令 + 正则文件过滤
- ~~result_text 关键字匹配检测代码修改~~ → 改为 `git diff --name-only` 检测实际修改的文件,避免误判
- `retryable` 字段区分"可重试的编译错误"vs"不可重试的环境错误"
- 门禁链按注册顺序执行，首个失败即返回
- **MVP 边界**:首期只注册 `rust_compile_gate`,验证流程稳定后再扩展 npm/pytest(见 §10)

#### 4.4.1 LlmJudgeGate(v3 新增 P0 trait 预留)

> **背景**:`CommandValidationGate` 只能验证代码编译类任务,但 `TaskComplexity::Diagnostic`(诊断任务)和 `Architectural`(架构决策)**无法用编译命令验证**。诊断任务的正确性需要判断根因定位是否准确、修复方案是否真正解决问题。
>
> **借鉴**:Anthropic Multi-Agent Research System 使用 LLM-as-judge 按 rubric 打分 0.0-1.0,单次 LLM 调用 + 单一 prompt 与人类判断最一致。Anthropic 强调 **end-state evaluation**:不评判中间步骤,只评判最终状态是否正确。
>
> **MVP 边界**:trait 设计在 MVP 就位,实现留待 v2。诊断任务 MVP 阶段用人工验收 + `rust_compile_gate` 双重确认。

```rust
/// v3 新增(P0 trait 预留, P2 实现):LLM-as-judge 验证门禁。
/// 用于诊断/架构类任务,确定性命令无法验证的场景。
/// 借鉴 Anthropic Multi-Agent Research System 的 LLM-as-judge 模式:
/// - 单次 LLM 调用 + 单一 prompt 输出 0.0-1.0 分数
/// - rubric 包含:准确性/完整性/根因定位/方案可行性
/// - end-state evaluation:只评判最终状态,不评判中间步骤
pub struct LlmJudgeGate {
    /// 评判模型(建议用旗舰,如 deepseek-v4-pro,保证判断质量)
    judge_model: String,
    /// 评分标准(rubric),注入到 judge prompt
    rubric: String,
    /// 通过阈值(0.0-1.0),低于此分则 validation 失败
    pass_threshold: f64,
    /// workspace_root(用于读取 changed_files 内容供 judge 参考)
    workspace_root: PathBuf,
}

impl LlmJudgeGate {
    /// v3 新增:诊断任务默认 rubric(借鉴 Anthropic rubric 设计)。
    pub fn diagnostic_default(judge_model: &str, workspace_root: PathBuf) -> Self {
        Self {
            judge_model: judge_model.to_string(),
            rubric: r#"请按以下 rubric 评分(0.0-1.0),只输出分数:
1. 根因定位准确性 (0.3):是否正确定位问题根本原因?
2. 修复方案可行性 (0.3):方案是否真正解决问题,非治标不治本?
3. 完整性 (0.2):是否覆盖所有相关场景和边界条件?
4. 副作用评估 (0.2):是否评估引入新问题的风险?
总分 = 各项加权求和。"#.to_string(),
            pass_threshold: 0.7, // 默认 0.7 通过
            workspace_root,
        }
    }
}

impl ValidationGate for LlmJudgeGate {
    fn validate(&self, ctx: &ValidationContext) -> Result<(), ValidationError> {
        let result_text = std::fs::read_to_string(ctx.result_path).unwrap_or_default();

        // 构造 judge prompt(借鉴 Anthropic end-state evaluation)
        let prompt = format!(
            "任务: {}\n\
             使用的模型: {}\n\
             修改的文件: {:?}\n\
             \n\
             subagent 最终结果:\n\
             {}\n\
             \n\
             评分标准:\n\
             {}",
            ctx.task, ctx.model, ctx.changed_files, result_text, self.rubric
        );

        // 构造 judge client(复用真实存在的 ProviderClient::from_model)
        let client = ProviderClient::from_model(&self.judge_model)
            .map_err(|e| ValidationError {
                message: format!("judge client construction failed: {e}"),
                retryable: false, // 客户端构造失败不可重试
            })?;

        // 调用 judge model,解析分数
        let score = call_judge_model(&client, &prompt).map_err(|e| ValidationError {
            message: format!("judge model call failed: {e}"),
            retryable: true, // LLM 调用失败可重试
        })?;

        if score >= self.pass_threshold {
            Ok(())
        } else {
            Err(ValidationError {
                message: format!(
                    "LLM judge score {score:.2} below threshold {}",
                    self.pass_threshold
                ),
                retryable: true, // 评分不达标可重试(升级模型后可能改善)
            })
        }
    }

    fn name(&self) -> &'static str { "llm-judge" }
}

/// v3 预留(P2):调用 judge model 并解析分数。
/// MVP 阶段不实现,留待 v2。
fn call_judge_model(client: &ProviderClient, prompt: &str) -> Result<f64, String> {
    // TODO(v2):构造 ApiRequest,调用 client.stream,解析响应中的分数
    // 借鉴 Anthropic:单次 LLM 调用 + 单一 prompt 输出 0.0-1.0
    Err("LlmJudgeGate not implemented in MVP, see §10.5".to_string())
}
```

**设计要点(v3 新增)**：
- **P0 trait 预留**:`LlmJudgeGate` 实现 `ValidationGate` trait,与 `CommandValidationGate` 共用 `ValidationContext`,MVP 阶段不注册但设计已就位
- **借鉴 Anthropic rubric**:诊断任务 rubric 包含根因定位/方案可行性/完整性/副作用评估四维,加权求和
- **end-state evaluation**:只评判 `result_path` 最终结果,不评判中间步骤(Anthropic 强调 agent 可能走不同路径到达同一目标)
- **judge_model 选择**:建议用旗舰模型(deepseek-v4-pro)做 judge,避免"弱模型评判强模型"问题
- **pass_threshold 默认 0.7**:平衡严格性与可用性,可配置
- **P2 实现时机**:MVP 验收后,v2 阶段实现 `call_judge_model`,届时诊断任务可自动验证

---

### 4.5 模块 5：重试 + 模型升级 Loop(v2 修正)

**v2 修正要点**:
- 补充状态转换图,修复 v1 状态不可达漏洞(新增 `reset_for_retry` 调用)
- `upgrade_model` 返回 None 时立即 `fail()` 退出,而非 v1 的 `continue` 浪费 attempt
- `run_subagent_turn_with_model` 调用真实存在的 `ProviderClient::from_model`(v1 误以为不存在)

#### 4.5.1 状态转换图(v2 新增)

```
                        ┌─────────────────────────────────────────────────┐
                        │            SubagentStatus 状态机                 │
                        │       (v2 修正:补全 reset_for_retry 边)          │
                        └─────────────────────────────────────────────────┘

        spawn_with_model           start()              run_subagent_turn
   ┌────────────────────┐    ┌─────────────────┐    ┌─────────────────────┐
   │                    ▼    │                 ▼    │                     ▼
 ┌─┴────┐            ┌──────────┐         ┌──────────┐         ┌──────────────┐
 │Created│──────────►│ Running  │────────►│ Running  │────────►│  Completed   │
 └──────┘            └────┬─────┘  (turn  └────┬─────┘ (turn   └──────┬───────┘
                          │       ok)          │       err)          │
                          │                    │                      │
                          │ cancel()           │                      │ validate()
                          │                    ▼                      ▼
                          ▼              ┌──────────┐         ┌──────────────┐
                     ┌──────────┐        │  Failed  │         │   Ok(())     │──► 终态(成功)
                     │Cancelled │        └────┬─────┘         │ 门禁通过     │
                     └──────────┘             │               └──────────────┘
                          ▲                   │
                          │                   │ retryable && attempt < max
                          │                   ▼
                          │            ┌──────────────────────┐
                          │            │ reset_for_retry()    │  ◄── v2 新增
                          │            │ Completed/Failed     │      修复 v1 状态不可达
                          │            │   → Running          │
                          │            │ (清空 result,attempts │
                          │            │  保留累加)            │
                          │            └──────────┬───────────┘
                          │                       │
                          │                       │ upgrade_model()
                          │                       ▼
                          │            ┌──────────────────────┐
                          │            │ current_model 升级   │
                          │            │ (None 时 → fail())   │  ◄── v2 修正
                          │            │  立即终止,不浪费     │      v1 None 时 continue 浪费 attempt
                          │            │  剩余 attempt        │
                          │            └──────────┬───────────┘
                          │                       │
                          │                       │ 下一轮 run_subagent_turn
                          │                       ▼
                          │             回到 Running 状态
                          │
                          └─── 终态(用户取消 / 达到 max_attempts / 不可重试错误)
```

**关键状态转换(v2 修正)**:
- `Running --turn ok--> Running`(turn 完成但未 validate)→ `complete()` → `Completed` → `validate()`
- `Completed --validate retryable fail--> reset_for_retry() --> Running`(v2 新增,修复 v1 漏洞)
- `Running --turn err--> Failed --retryable--> reset_for_retry() --> Running`
- `Running --turn err--> Failed --not retryable--> 终态`
- `upgrade_model() == None --> fail() --> 终态`(v2 修正,不再 continue)

#### 4.5.2 retry loop 代码(v2 修正)

**修改**：`conversation.rs` 的 `execute_dispatch_subagent` 增加 retry loop：

```rust
// conversation.rs execute_dispatch_subagent 内
// v2 修正:用 spawn_with_model 替代原 spawn,获取 max_attempts
let subagent_id = coordinator.spawn_with_model(
    name, task, mode, model.clone(), complexity, max_attempts,
).map_err(|e| format!("spawn failed: {e}"))?;
coordinator.start(&subagent_id).map_err(|e| format!("start failed: {e}"))?;

let mut current_model = model.clone()
    .or_else(|| Some(self.api_client.model.clone()));

for attempt in 1..=max_attempts {
    $crate::diag!("subagent {} attempt {}/{} with model {:?}",
        subagent_id, attempt, max_attempts, current_model);

    coordinator.increment_attempts(&subagent_id)?;

    let subagent_result = self.run_subagent_turn_with_model(
        &subagent_id, name, task, current_model.as_deref()
    );
    // v3 新增(P0):run_subagent_turn_with_model 内部应调用 coordinator.record_cost()
    // 记录本轮 token 消耗,供成本门禁累计。此处由内部实现负责。

    // v3 新增(P1 checkpoint):每轮 turn 后保存 checkpoint
    // 借鉴 LangGraph durable execution + Anthropic "resume from where the agent was"
    let _ = coordinator.save_checkpoint(&subagent_id);

    match subagent_result {
        Ok(result_ref) => {
            // turn 成功 → complete() → validate()
            coordinator.complete(&subagent_id, &result_ref)
                .map_err(|e| format!("complete failed: {e}"))?;
            match coordinator.validate(&subagent_id) {
                Ok(()) => {
                    // 验证通过 → 终态(成功)
                    return Ok(ToolResult::text(format!(
                        "Subagent completed (attempt {attempt}). Result: {result_ref}"
                    )));
                }
                Err(ve) if ve.retryable && attempt < max_attempts => {
                    $crate::diag!("validation failed (attempt {attempt}): {}", ve.message);
                    // v2 修正:reset_for_retry 把 Completed → Running
                    // v1 漏洞:不调用 reset,下一轮 complete() 会拒绝
                    coordinator.reset_for_retry(&subagent_id)?;
                    // v2 修正:升级模型;None 时立即 fail 退出
                    if let Some(ref m) = current_model {
                        match upgrade_model(m, complexity) {
                            Some(entry) => {
                                // v3 新增(P0 成本门禁):升级前检查成本上限
                                // 借鉴 Router-R1 成本奖励 + FrugalGPT 成本感知级联
                                // 预估下一轮 token 用量(MVP 硬编码 100K,后续可从历史推断)
                                let estimated_tokens: u32 = 100_000;
                                match coordinator.check_cost_limit(
                                    &subagent_id, &entry.target_model, estimated_tokens
                                ) {
                                    Ok(()) => {
                                        $crate::diag!(
                                            "upgraded model: {} → {} (cost_multiplier={:.1})",
                                            m, entry.target_model, entry.cost_multiplier
                                        );
                                        current_model = Some(entry.target_model);
                                    }
                                    Err(cost_err) => {
                                        // v3:成本超限,拒绝升级,立即失败
                                        $crate::diag!("cost limit exceeded, aborting upgrade: {}", cost_err);
                                        let _ = coordinator.fail(&subagent_id,
                                            &format!("cost limit exceeded during upgrade: {cost_err}; validation error: {}", ve.message));
                                        return Ok(ToolResult::text(format!(
                                            "Subagent failed: cost limit ${:.4} exceeded. Validation error: {}",
                                            coordinator.get_cost_limit(&subagent_id).unwrap_or(0.0),
                                            ve.message
                                        )));
                                    }
                                }
                            }
                            None => {
                                // v2 修正:v1 此处 continue 浪费 attempt,改为立即失败
                                $crate::diag!("model {} already at flagship, cannot upgrade", m);
                                let _ = coordinator.fail(&subagent_id,
                                    &format!("validation failed and model cannot upgrade: {}", ve.message));
                                return Ok(ToolResult::text(format!(
                                    "Subagent failed: model at flagship but validation still fails: {}",
                                    ve.message
                                )));
                            }
                        }
                    }
                    continue;
                }
                Err(ve) => {
                    let _ = coordinator.fail(&subagent_id, &ve.message);
                    return Ok(ToolResult::text(format!(
                        "Subagent failed validation after {attempt} attempts: {}", ve.message
                    )));
                }
            }
        }
        Err(e) if attempt < max_attempts => {
            $crate::diag!("attempt {attempt} failed: {e}, retrying with upgraded model");
            // v2 修正:turn 失败时 coordinator 已被 fail(),需 reset_for_retry
            coordinator.reset_for_retry(&subagent_id)?;
            if let Some(ref m) = current_model {
                match upgrade_model(m, complexity) {
                    Some(entry) => {
                        // v3 新增(P0 成本门禁):turn 失败升级同样检查成本
                        let estimated_tokens: u32 = 100_000;
                        match coordinator.check_cost_limit(
                            &subagent_id, &entry.target_model, estimated_tokens
                        ) {
                            Ok(()) => {
                                $crate::diag!(
                                    "upgraded model after turn failure: {} → {} (cost×{:.1})",
                                    m, entry.target_model, entry.cost_multiplier
                                );
                                current_model = Some(entry.target_model);
                            }
                            Err(cost_err) => {
                                $crate::diag!("cost limit exceeded, aborting upgrade: {}", cost_err);
                                let _ = coordinator.fail(&subagent_id,
                                    &format!("cost limit exceeded: {cost_err}; turn error: {e}"));
                                return Ok(ToolResult::text(format!(
                                    "Subagent failed: cost limit exceeded. Turn error: {e}"
                                )));
                            }
                        }
                    }
                    None => {
                        // v2 修正:无升级路径立即失败
                        let _ = coordinator.fail(&subagent_id, &e);
                        return Ok(ToolResult::text(format!(
                            "Subagent failed after {attempt} attempts (no model upgrade): {e}"
                        )));
                    }
                }
            }
            continue;
        }
        Err(e) => {
            let _ = coordinator.fail(&subagent_id, &e);
            return Ok(ToolResult::text(format!(
                "Subagent failed after {max_attempts} attempts: {e}"
            )));
        }
    }
}
```

#### 4.5.3 `run_subagent_turn_with_model` 新增方法(v2 修正)

**v2 修正**:`ProviderClient::from_model` 真实存在([client.rs:17](../rust/crates/api/src/client.rs#L17)),v1 误以为不存在。spike 报告见 §9。

```rust
/// 带模型选择的 subagent turn 执行。
/// model 为 None 时复用主 agent client；Some 时构造独立 client。
/// v2 修正:使用真实存在的 ProviderClient::from_model(见 §0.3 + §9 spike)。
fn run_subagent_turn_with_model(
    &mut self,
    subagent_id: &str,
    name: &str,
    task: &str,
    model: Option<&str>,
) -> Result<String, String> {
    let workspace_root = self.workspace_root.as_ref().ok_or_else(|| {
        "workspace_root not configured — subagent requires filesystem access".to_string()
    })?;

    // v2 修正:用真实的 ProviderClient::from_model 构造独立 client
    // (v1 误以为此 API 不存在,实际 [client.rs:17] 已定义)
    let client = if let Some(m) = model {
        $crate::diag!("constructing dedicated client for model: {m}");
        ProviderClient::from_model(m)
            .map_err(|e| format!("failed to construct client for {m}: {e}"))?
    } else {
        // model 为 None:复用主 agent client(原 run_subagent_turn 行为)
        return self.run_subagent_turn(subagent_id, name, task);
    };

    // 构造子智能体 system_prompt(同 run_subagent_turn,可复用辅助函数)
    let subagent_system_prompt = build_subagent_system_prompt(name, subagent_id, task);

    // 构造 user message
    let user_message = ConversationMessage {
        role: MessageRole::User,
        blocks: vec![ContentBlock::Text {
            text: format!("请执行以下任务:\n\n{task}"),
        }],
        usage: None,
    };
    let request = ApiRequest {
        system_prompt: subagent_system_prompt,
        messages: vec![user_message],
    };

    // 用独立 client 调用 LLM(而非 self.api_client)
    let events = client.stream(request)
        .map_err(|e| format!("subagent LLM request failed (model {m}): {e}"))?;

    // 后续解析 + 写文件逻辑与 run_subagent_turn 相同
    let (assistant_message, _usage, _cache_events) = build_assistant_message(events)
        .map_err(|e| format!("subagent response parsing failed: {e}"))?;

    // ... 提取 text + 写 .claw/subagents/{id}.md(复用现有逻辑)...
    Ok(result_path.to_string_lossy().into_owned())
}
```

---

### 4.6 模块 6：诊断 SOP 注入

**修改文件**：`rust/crates/runtime/src/conversation.rs` 的 `run_subagent_turn`

给 subagent 的 system prompt 追加诊断 SOP（仅当 `complexity == Diagnostic`）：

```rust
// conversation.rs run_subagent_turn 内的 system_prompt 构建
let system_prompt = if complexity == TaskComplexity::Diagnostic {
    format!("{base_prompt}\n\n\
    ## 诊断任务执行规范\n\
    1. 遇到崩溃/闪退类问题，第一动作是写文件诊断日志（CLAW_DIAG=1 或调用 diag! 宏），\
       而非凭直觉堆砌防御代码\n\
    2. 先用可靠信号确认错误类型（panic vs Err vs 配置错误），再决定修复方向\n\
    3. 修改后必须运行 `cargo build` 验证编译通过\n\
    4. 声称修复后必须提供复现验证证据（重新运行原场景确认不崩溃）\n\
    5. 禁止在未验证根因的情况下堆砌 catch_unwind / panic hook 等防御性代码")
} else {
    base_prompt
};
```

**设计要点**：
- SOP 仅注入 Diagnostic 复杂度，避免污染简单任务
- 规则固化到系统提示，能力不足模型也无法绕过
- 与验证门禁形成"提示层 + 强制层"双重防护

---

### 4.7 模块 7：决策持久化 `runtime::decision_log`(v3 新增)

> **与 §4.1 的关系**:
> - §4.1 的 `diag!` 宏解决"**运行时信号不可见**"(panic/error/状态变更)
> - §4.7 解决"**设计决策信号不可见**"(为什么选 A 不选 B、权衡了什么、放弃了什么)
> - 两者正交:`diag!` 记录"发生了什么",`decision_log` 记录"决定了什么、为什么"
>
> **背景**:在一次长对话中,讨论了"`spawn()` 签名为何保持 `-> String` 而非 `-> Result<String, String>`",理由是"避免破坏 12 处现有测试"。但 context compaction 触发后,`summarize_messages`(compact.rs:546-631)的 Key timeline 只记录"discussed spawn signature",决策理由丢失。后续 turn 无法回溯"为什么这样设计",导致可能推翻已有决策重新讨论。
>
> **现状核实**(基于源码):
> - `summarize_messages` 生成结构化 XML 摘要(Scope/Tools/Pending work/Key timeline),但 **Key timeline 是事实摘要,非决策摘要**
> - `compress_summary_text`(summary_compression.rs)压缩到 1200 chars/24 lines,进一步丢失决策细节
> - NOTEBOOK.md 已存在(notebook.rs),有 5 段(plan/subagents/attempted/preferences/key_files),**但无 decisions 段**
> - NOTEBOOK 由 LLM 主动维护(notebook_update tool),**但无自动捕获机制**,LLM 不会自觉记录"为什么做这个决策"
> - FTS5 索引是全工作区跨 session 的(history_search.rs),compaction 不清理索引,**但决策推理淹没在噪声中**,BM25 排序不优先决策内容

#### 4.7.1 决策点数据模型

**新增文件**：`rust/crates/runtime/src/decision_log.rs`

```rust
//! 决策持久化 — 在 context compaction 触发前自动提取"决策点"并持久化。
//! 与 diag! 互补:diag! 记录"发生了什么",decision_log 记录"决定了什么、为什么"。
//!
//! 现状核实:
//! - summarize_messages(compact.rs:546-631)的 Key timeline 是事实摘要,非决策摘要
//! - NOTEBOOK.md(notebook.rs)有 5 段但无 decisions 段
//! - FTS5(history_search.rs)全历史可搜,但决策推理淹没在噪声中

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// 决策点 — 一个关键设计决策的完整记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionPoint {
    /// 决策 ID(递增序号 + 时间戳哈希)
    pub id: String,
    /// 决策上下文(什么场景下做的决策)
    pub context: String,
    /// 决策内容(做了什么决定)
    pub decision: String,
    /// 决策理由(为什么这样做)
    pub rationale: String,
    /// 被否决的替代方案(为什么没选其他选项)
    pub alternatives: Vec<String>,
    /// 时间戳(ms)
    pub timestamp_ms: u64,
    /// 来源 session_id
    pub session_id: String,
}

/// 决策检测策略。
#[derive(Debug, Clone)]
pub enum DetectionStrategy {
    /// MVP:启发式关键词检测。零 LLM 调用,零成本。
    /// 检测包含决策信号的消息:decided/chose/trade-off/权衡/否决/放弃/之所以/因为
    Heuristic,
    /// v2:LLM 提取。用轻量模型(flash)从待压缩消息中提取决策结构。
    /// 成本低(flash),但需要 LLM 调用。
    LlmExtract { model: String },
}

/// 启发式决策检测关键词(中英文)。
/// MVP 策略:零成本,零 LLM 调用。
const DECISION_KEYWORDS: &[&str] = &[
    // 英文决策信号
    "decided", "chose", "chosen", "trade-off", "tradeoff", "alternative",
    "rejected", "ruled out", "instead of", "rather than", "over ",
    // 中文决策信号
    "决定", "选择", "权衡", "否决", "放弃", "之所以", "因为", "而非",
    "而不是", "替代方案", "备选",
];

/// 检测单条消息是否包含决策信号。
/// MVP:关键词匹配。v2:LLM 提取。
pub fn detect_decision_signal(message_text: &str) -> bool {
    let lower = message_text.to_ascii_lowercase();
    DECISION_KEYWORDS.iter().any(|kw| {
        lower.contains(&kw.to_ascii_lowercase())
    })
}

/// 从待压缩的消息列表中提取决策点。
/// 在 compact_session 执行前调用,确保决策不随原始消息消失。
///
/// MVP 策略(Heuristic):
/// 1. 遍历待压缩消息,检测决策关键词
/// 2. 命中关键词的消息,提取该消息 + 前一条消息作为上下文
/// 3. 用模板构造 DecisionPoint(context/decision/rationale 由 LLM 后续填充或人工确认)
///
/// v2 策略(LlmExtract):
/// 1. 用 flash 模型从待压缩消息中提取结构化决策
/// 2. 自动填充 context/decision/rationale/alternatives
pub fn extract_decisions_before_compaction(
    messages: &[&str],
    strategy: &DetectionStrategy,
    session_id: &str,
) -> Vec<DecisionPoint> {
    match strategy {
        DetectionStrategy::Heuristic => {
            let mut decisions = Vec::new();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for (i, msg) in messages.iter().enumerate() {
                if detect_decision_signal(msg) {
                    // 提取上下文:前一条消息(如果有)
                    let context = if i > 0 { messages[i - 1] } else { "" };
                    // MVP:整条消息作为 decision + rationale 的候选
                    // v2 会用 LLM 精确提取
                    let id = format!("d{}-{}", now_ms, i);
                    decisions.push(DecisionPoint {
                        id,
                        context: truncate(context, 200),
                        decision: truncate(msg, 300),
                        rationale: truncate(msg, 500),
                        alternatives: Vec::new(), // MVP 不提取,留待 v2 LLM 填充
                        timestamp_ms: now_ms,
                        session_id: session_id.to_string(),
                    });
                }
            }
            decisions
        }
        DetectionStrategy::LlmExtract { model } => {
            // v2 实现:调用 flash 模型提取结构化决策
            // TODO(v2):构造 prompt,调用 ProviderClient::from_model(model)
            Vec::new()
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max]) }
}
```

#### 4.7.2 NOTEBOOK.md 新增 decisions 段

**修改文件**：`rust/crates/runtime/src/notebook.rs`

```rust
// notebook.rs 修改:SECTION_TAGS 新增 "decisions"
// v3 修订:从 5 段扩展到 6 段
pub const SECTION_TAGS: &[&str] = &[
    "plan",
    "subagents",
    "attempted",
    "preferences",
    "key_files",
    "decisions",  // v3 新增:决策持久化段
];
```

**决策点写入 NOTEBOOK 的格式**(在 `decision_log.rs` 中):

```rust
/// 将决策点渲染为 NOTEBOOK.md `<decisions>` 段的 XML 格式。
/// 与现有 NOTEBOOK 段格式一致(notebook.rs:259-280 的 render_for_prompt)。
pub fn render_decision_for_notebook(decisions: &[DecisionPoint]) -> String {
    if decisions.is_empty() { return String::new(); }
    let mut lines = vec!["<decisions>".to_string()];
    for d in decisions.iter().take(20) { // NOTEBOOK 16K 上限,最多 20 条决策
        lines.push(format!(
            "- [{}] {} — {}",
            &d.id[..8.min(d.id.len())],
            d.decision,
            d.rationale,
        ));
        if !d.alternatives.is_empty() {
            lines.push(format!("  alternatives: {}", d.alternatives.join("; ")));
        }
    }
    lines.push("</decisions>".to_string());
    lines.join("\n")
}

/// v3 新增:将决策点追加到 NOTEBOOK.md 的 decisions 段。
/// 复用 Notebook::save 的原子写机制(notebook.rs:133-158),遵守 16K 上限。
pub fn persist_decisions_to_notebook(
    workspace_root: &std::path::Path,
    decisions: &[DecisionPoint],
) -> Result<(), String> {
    let mut notebook = crate::notebook::Notebook::load(workspace_root)
        .map_err(|e| format!("load notebook failed: {e}"))?;
    let rendered = render_decision_for_notebook(decisions);
    // 追加到 decisions 段(不覆盖,累积记录)
    let existing = notebook.get_section("decisions").unwrap_or_default();
    let combined = format!("{existing}\n{rendered}");
    // 检查 16K 上限(notebook.rs:74 NOTEBOOK_MAX_CHARS)
    if combined.len() > 14_000 {
        // 超限时保留最近 10 条决策,丢弃旧决策
        let lines: Vec<&str> = combined.lines().collect();
        let trimmed = lines.iter().rev().take(100).collect::<Vec<_>>()
            .into_iter().rev().collect::<Vec<_>>().join("\n");
        notebook.set_section("decisions", &trimmed);
    } else {
        notebook.set_section("decisions", &combined);
    }
    notebook.save(workspace_root)
        .map_err(|e| format!("save notebook failed: {e}"))?;
    Ok(())
}
```

#### 4.7.3 compaction 协同:压缩前提取决策

**修改文件**：`rust/crates/runtime/src/conversation.rs` 的 auto_compaction 路径

> **关键插入点**:在 [conversation.rs:2201-2227](../rust/crates/runtime/src/conversation.rs#L2201-L2227) 的 auto_compaction 调用 `compact_session` **之前**,插入决策提取步骤。

```rust
// conversation.rs auto_compaction 路径(约 2201 行附近)
// v3 新增:在 compact_session 之前提取决策点

if usage_tracker.cumulative_usage().input_tokens >= threshold {
    // v3 新增 §4.7:compaction 前提取决策点
    // 避免决策推理随原始消息消失
    let messages_to_compact: Vec<&str> = self.session.messages
        .iter()
        .rev()
        .skip(self.compaction_config.preserve_recent_messages)
        .map(|m| extract_text_from_message(m))
        .collect();
    let strategy = DetectionStrategy::Heuristic; // MVP:零成本启发式
    let decisions = decision_log::extract_decisions_before_compaction(
        &messages_to_compact,
        &strategy,
        &self.session.session_id,
    );
    if !decisions.is_empty() {
        $crate::diag!("extracted {} decision points before compaction", decisions.len());
        // 持久化到 NOTEBOOK.md decisions 段
        if let Some(ws) = &self.workspace_root {
            if let Err(e) = decision_log::persist_decisions_to_notebook(ws, &decisions) {
                $crate::diag!("failed to persist decisions to notebook: {e}");
            }
        }
        // v3 新增:同步写入 FTS5 索引(role="decision"),提升可检索性
        if let Some(history_index) = &self.session.history_index {
            for d in &decisions {
                let content = format!(
                    "[DECISION {}] context: {}\ndecision: {}\nrationale: {}\nalternatives: {}",
                    d.id, d.context, d.decision, d.rationale,
                    d.alternatives.join("; "),
                );
                let _ = history_index.index_message(
                    &content,
                    &d.session_id,
                    "decision",  // v3 新增 role 类型
                    0,           // message_index=0 表示决策点
                    d.timestamp_ms,
                );
            }
        }
    }

    // 原有 compact_session 调用(不变)
    // ...
}
```

#### 4.7.4 FTS5 搜索优化:决策高权重排序

**修改文件**：`rust/crates/runtime/src/history_search.rs` 的 search 方法

```rust
// history_search.rs search 方法修改
// v3 新增:决策点(role="decision")在 BM25 rank 基础上加权,提升可检索性

impl HistoryIndex {
    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<SearchHit>, SearchError> {
        // 原有 FTS5 BM25 查询(不变)
        let mut hits = self.bm25_search(query, top_k * 2)?; // 多取一倍,用于加权后截断

        // v3 新增:role="decision" 的命中加权(rank 降低 = 排名提前)
        // BM25 rank 越低越相关,决策点 rank × 0.5(提前排名)
        for hit in hits.iter_mut() {
            if hit.role == "decision" {
                hit.rank *= 0.5; // 决策点加权:rank 减半 = 相关性翻倍
            }
        }

        // 重新排序并截断到 top_k
        hits.sort_by(|a, b| a.rank.partial_cmp(&b.rank).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k);
        Ok(hits)
    }
}
```

#### 4.7.5 与现有机制的协同关系

```
┌─────────────────────────────────────────────────────────────────────┐
│                    信息持久化四层架构(现状)                         │
│              tool_result_archive.rs:20-30 注释明确说明               │
├─────────────────────────────────────────────────────────────────────┤
│ Layer 1: Main Context(LLM 推理窗口)                                │
│   ├─ microcompact 摘要(每 turn 末尾,保留最近 4 条 tool result)     │
│   └─ compact_session 摘要(100K token 阈值,生成 XML summary)       │
├─────────────────────────────────────────────────────────────────────┤
│ Layer 2: NOTEBOOK.md(LLM 主动维护的关键信息)                       │
│   ├─ plan / subagents / attempted / preferences / key_files(5 段)  │
│   └─ decisions(v3 新增 §4.7) ← 自动提取 + LLM 追加                 │
├─────────────────────────────────────────────────────────────────────┤
│ Layer 3: ToolResultArchive(被动归档的原始 tool output)             │
│   └─ .claw/tool_results_archive.jsonl(512KB 上限,recall_full 取回) │
├─────────────────────────────────────────────────────────────────────┤
│ Layer 4: FTS5 history.db(跨 session 全历史搜索)                   │
│   ├─ user / assistant / system / tool 消息(BM25 rank)             │
│   └─ decision(v3 新增 §4.7,加权排序) ← compaction 前自动索引      │
└─────────────────────────────────────────────────────────────────────┘
```

**协同点**:

| 触发时机 | 现有机制 | v3 新增(§4.7) |
|---|---|---|
| 每 turn 末尾 | microcompact 摘要 tool result | — |
| 100K token 阈值 | compact_session 生成 XML summary | **compact_session 前提取决策 → NOTEBOOK decisions 段 + FTS5 decision role** |
| compaction 后 | `notebook_refresh_pending=true`,提醒 LLM 刷新 plan/subagents | LLM 也可调用 `notebook_update` 追加 decisions 段 |
| panic/crash | `diag!` 落盘 claw-crash.log(§4.1) | — |
| LLM 搜索 | `session_search` 查 FTS5 全历史 | **decision role 加权,决策点优先排序** |

#### 4.7.6 设计要点

- **与 §4.1 正交**:`diag!` 记录运行时信号(panic/error),`decision_log` 记录设计决策(为什么这样做),两者互补不重叠
- **MVP 零成本策略**:启发式关键词检测,无 LLM 调用,零额外成本。v2 才用 flash 模型精确提取
- **compaction 前提取**:关键设计——在 `compact_session` 执行**之前**提取决策,确保决策不随原始消息消失
- **NOTEBOOK decisions 段**:复用现有 `Notebook::save` 原子写 + 16K 上限,与 plan/subagents 段并列
- **FTS5 decision role**:新增 role 类型,搜索时加权排序,解决"决策推理淹没在噪声中"问题
- **不破坏现有不变量**:NOTEBOOK 不在 message history 中(notebook.rs:30-37),compaction 不影响 NOTEBOOK;FTS5 compaction 不清理索引(已验证)
- **DAG 协同预留**:dag-orchestration-detail.md:2048-2068 的 `append_to_notebook` 设计应改为调用 `persist_decisions_to_notebook`,对齐 notebook.rs 实际实现(当前设计文档绕过了原子写和 16K 限制)
- **MVP 边界**:启发式检测 + NOTEBOOK decisions 段 + FTS5 decision role。v2 实现 LLM 提取 + 决策过期/合并

---

## 5. 实施路径

### 5.1 分步实施

按依赖顺序分 4 步，每步可独立编译验证：

| 步骤 | 模块 | 依赖 | 新建文件 | 修改文件 | 验证命令 |
|---|---|---|---|---|---|
| 1 | 诊断模块 `runtime::diag` + panic hook | 无 | `diag.rs` | `lib.rs`, `main.rs`, `headless.rs`, `paste.rs` | `cargo build -p runtime -p rusty-claude-cli --release` |
| 2 | 模型分级 `api::model_tier` | 无 | `model_tier.rs` | `providers/mod.rs`, `api/lib.rs` | `cargo build -p api --release` |
| 3 | Subagent 字段扩展 + spawn 签名 | 步骤 2 | 无 | `multi_agent/mod.rs`, `conversation.rs` | `cargo build -p runtime --release` |
| 4 | 验证门禁 + 重试 loop + SOP 注入 | 步骤 1+2+3 | `validation.rs` | `conversation.rs`, `mod.rs` | `cargo build -p runtime -p rusty-claude-cli --release` |

### 5.2 验证策略

每步完成后执行：

```bash
# 1. 编译验证
cargo build -p runtime -p api -p rusty-claude-cli --release

# 2. 单元测试（如可用）
cargo test -p runtime --release config_validate  # 验证 _ 前缀 key 宽容
cargo test -p api --release model_tier            # 验证能力分级

# 3. 运行时验证
$env:CLAW_DIAG=1
./target/release/claw-plus.exe
# 检查 ~/.claw/claw-diag.log 是否生成

# 4. panic hook 验证
# 触发一个 panic（测试代码），确认 ~/.claw/claw-crash.log 生成
```

### 5.3 回滚策略

- 每步独立提交，便于 git bisect 定位
- 步骤 1 的 panic hook 可通过 `CLAW_DIAG=0` 完全禁用
- 步骤 3 的 `spawn_simple` 保持向后兼容，不破坏现有调用
- 步骤 4 的验证门禁可通过不注册 gate 来跳过

---

## 6. 风险与缓解(v2 修正)

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| ~~`upgrade_model` 升级路径不完整~~ → v2 改为配置化 | 中 | subagent 卡在能力不足模型 | `upgrade_map` 从 `~/.claw/model-upgrades.json` 加载,可热更新;升级到旗舰仍失败时返回 None,loop 立即 fail 退出(v1 是 continue 浪费 attempt) |
| ~~验证门禁误判(非 Rust 项目)~~ → v2 已抽象 | 低 | 错误拒绝合法修改 | `CommandValidationGate` 用 `git diff --name-only` + 正则过滤,非 Rust 项目注册对应命令(npm/pytest)即可;MVP 首期只注册 `rust_compile_gate` |
| panic hook 影响性能 | 低 | 启动延迟 | hook 仅在 panic 时执行，正常路径零开销 |
| `diag!` 宏污染生产输出 | 低 | 日志文件膨胀 | `CLAW_DIAG=1` 默认关闭，仅诊断时开启 |
| 模型分级规则过时 | 中 | 新模型被误判 | `tier_for_model` 基于前缀匹配,新增模型需更新规则;v2 已覆盖 `flash`/`-pro` 后缀;可考虑从 `MODEL_REGISTRY` 动态推断 |
| **v2 新增:retry loop 状态机不闭合**(v1 漏洞) | 高 | 重试时 `complete()` 拒绝,subagent 永远卡在 Completed | v2 新增 `reset_for_retry()` 方法,validate 失败后先 reset 再重试(见 §4.5.1 状态图) |
| **v2 新增:OnceLock 竞态**(v1 漏洞) | 中 | `--diag` flag 静默失效 | v2 改用 `AtomicBool + OnceLock<()>` 初始化门控,`enable()` 用 `store(true)` 而非 `set`(见 §4.1) |
| **v2 新增:TUI_SILENT 双 AtomicBool 不同步** | 低 | TUI 模式下 stderr 污染 alternate screen | v2 把 paste.rs 的 `TUI_SILENT` 改为转发 `runtime::diag::set_tui_silent()`,单一真源 |
| **v2 新增:deepseek-v4-flash 误判为 Standard**(v1 漏洞) | 高 | MVP 路由失效,flash 被当作 Standard 接受诊断任务 | v2 `tier_for_model` 新增 `flash` 后缀识别(见 §4.2) |
| **v2 新增:spawn 签名破坏 12 处测试**(v1 低估) | 中 | 实施成本上升 | v2 保留 `spawn` 原签名,新增 `spawn_with_model` 扩展方法(见 §4.3) |
| **v3 新增:成本失控**(v2 缺失) | 高 | deepseek-v4-pro 相对 flash 成本约 10 倍,retry 升级导致账单失控 | v3 提前到 MVP:`cost_limit`/`cost_accumulated` 字段 + `check_cost_limit` 升级前检查(见 §4.3/§4.5)。借鉴 Router-R1 成本奖励 + FrugalGPT 成本感知 |
| **v3 新增:诊断任务无法自动验证**(v2 缺失) | 中 | `rust_compile_gate` 只能验证编译,诊断/架构任务正确性无法判断 | v3 引入 `LlmJudgeGate` trait 预留(P0),实现留待 v2。借鉴 Anthropic LLM-as-judge rubric |
| **v3 新增:崩溃后无法恢复**(v2 缺失) | 中 | subagent 长时间运行后崩溃,从头重跑浪费成本 | v3 新增 `checkpoint_path` 字段 + `save_checkpoint`(P1),`restore_from_checkpoint` 留待 v2。借鉴 LangGraph durable execution |
| **v3 新增:预估 token 用量不准** | 中 | `check_cost_limit` 用硬编码 100K 估算,实际可能偏差 | MVP 阶段保守估算(偏高),v2 从历史 turn 的实际 token 用量推断 |

---

## 7. 未来扩展

### 7.1 异步执行

当前 subagent 同步阻塞。未来可接入 tokio：

```rust
// mod.rs join_all 注释已指出方向
pub async fn join_all_async(&self) -> JoinStats {
    // 用 tokio::join! 并发等待所有 Running 状态的 subagent
}
```

### 7.2 能力画像动态化

`tier_for_model` 当前硬编码前缀。可扩展为从 `MODEL_REGISTRY` 元数据推断：

```rust
pub fn tier_for_model(model: &str) -> ModelTier {
    if let Some(meta) = metadata_for_model(model) {
        // 基于 meta.context_window / meta.pricing 推断
    }
    // fallback 到前缀匹配
}
```

### 7.3 验证门禁扩展

```rust
/// 测试运行门禁
pub struct TestValidationGate { ... }

/// Lint 检查门禁
pub struct LintValidationGate { ... }

/// 复现验证门禁 — 重新运行原崩溃场景
pub struct ReproValidationGate {
    pub repro_command: String,
}
```

### 7.4 成本控制

模型升级会增加成本。可增加成本上限：

```rust
pub struct CostGuard {
    pub max_usd_per_subagent: f64,
    pub current_spend: Arc<Mutex<f64>>,
}
```

---

## 8. 关键文件索引(v2 修正行号)

| 关注点 | 文件 | 行号 | 备注 |
|---|---|---|---|
| MultiAgentCoordinator 定义 | [mod.rs](../rust/crates/runtime/src/multi_agent/mod.rs) | 78 | 准确 |
| Coordinator.start() | [mod.rs:138-151](../rust/crates/runtime/src/multi_agent/mod.rs#L138-L151) | 138-151 | 准确 |
| Coordinator.spawn() 原签名 | [mod.rs:104-135](../rust/crates/runtime/src/multi_agent/mod.rs#L104-L135) | 104-135 | v2 保持不变 |
| Coordinator.complete() | [mod.rs:154-169](../rust/crates/runtime/src/multi_agent/mod.rs#L154-L169) | 154-169 | v2 retry loop 关键约束 |
| 任务分发 execute_dispatch_subagent | [conversation.rs:1700](../rust/crates/runtime/src/conversation.rs#L1700) | 1700 | v1 错称 1663 |
| subagent turn 执行 run_subagent_turn | [conversation.rs:1814](../rust/crates/runtime/src/conversation.rs#L1814) | 1814 | v1 错称 1777 |
| 现有 panic hook(主 binary) | [main.rs:13-45](../rust/crates/rusty-claude-cli/src/main.rs#L13-L45) | 13-45 | v2 提取目标 |
| 现有 paste_diag_log 模板 | [paste.rs:40-69](../rust/crates/rusty-claude-cli/src/paste.rs#L40-L69) | 40-69 | 准确 |
| TUI_SILENT 静默机制 | [paste.rs:16-23](../rust/crates/rusty-claude-cli/src/paste.rs#L16-L23) | 16-23 | 准确 |
| ProviderClient::from_model | [client.rs:17-47](../rust/crates/api/src/client.rs#L17-L47) | 17-47 | v2 新增,v1 误以为不存在 |
| ProviderKind 枚举 | [mod.rs:32-37](../rust/crates/api/src/providers/mod.rs#L32-L37) | 32-37 | 准确 |
| ProviderDiagnostics(8 bool + 6 非 bool) | [mod.rs:104-119](../rust/crates/api/src/providers/mod.rs#L104-L119) | 104-119 | v1 错称"8 布尔位" |
| 能力矩阵 provider_capabilities_for_model | [mod.rs:385-450](../rust/crates/api/src/providers/mod.rs#L385-L450) | 385-450 | v1 错称 384 |
| 模型路由 metadata_for_model | [mod.rs:235-289](../rust/crates/api/src/providers/mod.rs#L235-L289) | 235-289 | v1 错称 234 |
| detect_provider_kind | [mod.rs:341-362](../rust/crates/api/src/providers/mod.rs#L341-L362) | 341-362 | v2 新增,deepseek 路由关键 |
| deepseek-v4 token limit | [mod.rs:645-648](../rust/crates/api/src/providers/mod.rs#L645-L648) | 645-648 | v2 新增,MVP 路由关键 |
| Fallback 链构造 build_provider_entry | [tools/lib.rs:5208-5215](../rust/crates/tools/src/lib.rs#L5208-L5215) | 5208-5215 | v1 错称 5180-5214 |
| Fallback 配置加载 load_provider_fallback_config | [tools/lib.rs:5217-5224](../rust/crates/tools/src/lib.rs#L5217-L5224) | 5217-5224 | v2 新增 |
| ProviderFallbackConfig 定义 | [config.rs:582-602](../rust/crates/runtime/src/config.rs#L582-L602) | 582-602 | v2 新增 |
| parse_optional_provider_fallbacks | [config.rs:1042-1056](../rust/crates/runtime/src/config.rs#L1042-L1056) | 1042-1056 | v2 新增 |
| 主进程入口 | [main.rs:10](../rust/crates/rusty-claude-cli/src/main.rs#L10) | 10 | 准确 |
| headless 入口(缺 panic hook) | [headless.rs:22](../rust/crates/rusty-claude-cli/src/bin/headless.rs#L22) | 22 | v2 补 hook |
| compaction 摘要生成 summarize_messages | [compact.rs:546-631](../rust/crates/runtime/src/compact.rs#L546-L631) | 546-631 | v3 §4.7 决策提取前置点 |
| 摘要体积压缩 compress_summary_text | [summary_compression.rs](../rust/crates/runtime/src/summary_compression.rs) | 全文件 | v3 §4.7 决策丢失根因 |
| auto_compaction 触发点 | [conversation.rs:2201-2227](../rust/crates/runtime/src/conversation.rs#L2201-L2227) | 2201-2227 | v3 §4.7.3 决策提取插入点 |
| NOTEBOOK.md 数据模型 | [notebook.rs](../rust/crates/runtime/src/notebook.rs) | 73-94 | v3 §4.7.2 新增 decisions 段 |
| NOTEBOOK 注入 system_prompt | [conversation.rs:954-975](../rust/crates/runtime/src/conversation.rs#L954-L975) | 954-975 | v3 §4.7 复用 |
| notebook_refresh_pending 机制 | [conversation.rs:383-397](../rust/crates/runtime/src/conversation.rs#L383-L397) | 383-397 | v3 §4.7 复用 |
| FTS5 索引构建 index_message | [session.rs:704-726](../rust/crates/runtime/src/session.rs#L704-L726) | 704-726 | v3 §4.7.3 新增 decision role |
| FTS5 搜索 history_search | [history_search.rs](../rust/crates/runtime/src/history_search.rs) | 47-54 | v3 §4.7.4 decision 加权 |
| tool_result_archive 三层架构注释 | [tool_result_archive.rs:20-30](../rust/crates/runtime/src/tool_result_archive.rs#L20-L30) | 20-30 | v3 §4.7.5 四层架构引用 |
| DAG ↔ NOTEBOOK 协同(设计文档) | [dag-orchestration-detail.md:2038-2069](../docs/modules/dag-orchestration-detail.md) | 2038-2069 | v3 §4.7.6 对齐建议 |

---

## 9. ProviderClient 构造路径 Spike 报告(v2 新增)

> **目标**:验证 `run_subagent_turn_with_model` 中 `ProviderClient::from_model(m)` 的可行性,确认 deepseek-v4-pro/flash 双模型 MVP 路由的客户端构造路径完整可用。

### 9.1 Spike 结论

**可行**。`ProviderClient::from_model` 是真实存在的公开 API,deepseek-v4-pro/flash 通过 `ProviderKind::OpenAi` 路径正确构造,无需额外适配。

### 9.2 调用链路追踪

```
run_subagent_turn_with_model(model: "deepseek-v4-pro")
    │
    ▼
ProviderClient::from_model("deepseek-v4-pro")           [client.rs:17]
    │
    ▼
Self::from_model_with_anthropic_auth(model, None)        [client.rs:21]
    │
    ├─ providers::resolve_model_alias("deepseek-v4-pro")
    │  → "deepseek-v4-pro"(无别名,原样返回)              [mod.rs:206-232]
    │
    ├─ providers::detect_provider_kind("deepseek-v4-pro")
    │  ├─ metadata_for_model("deepseek-v4-pro")           [mod.rs:235-289]
    │  │  ├─ 不匹配 "claude" / "grok" / "openai/" / "gpt-" / "qwen" / "kimi"
    │  │  └─ 返回 None
    │  ├─ OPENAI_BASE_URL 是否设置?                       [mod.rs:350]
    │  │  └─ 若用户配 deepseek endpoint:是 → 返回 OpenAi
    │  └─ (否则走 auth sniffing,通常也落到 OpenAi)
    │
    ▼
ProviderKind::OpenAi 分支                                [client.rs:34-46]
    │
    ├─ metadata_for_model("deepseek-v4-pro") 再次查询
    │  └─ 返回 None → 走 _ 分支 → OpenAiCompatConfig::openai()
    │
    ▼
OpenAiCompatClient::from_env(OpenAiCompatConfig::openai())
    │
    ├─ 读取 OPENAI_API_KEY                                [openai_compat.rs]
    ├─ 读取 OPENAI_BASE_URL(用户应指向 deepseek endpoint)
    └─ 构造 OpenAiCompatClient { api_key, base_url, config }
    │
    ▼
ProviderClient::OpenAi(openai_client)  ✅ 构造成功
```

### 9.3 deepseek-v4-flash 路径(对称验证)

`deepseek-v4-flash` 与 `deepseek-v4-pro` 走**完全相同**的调用链路,因为:
- `resolve_model_alias` 对两者都返回原样(无别名注册)
- `detect_provider_kind` 对两者都返回 `ProviderKind::OpenAi`
- `OpenAiCompatClient::from_env` 读取相同的 `OPENAI_API_KEY` / `OPENAI_BASE_URL`

**唯一差异**:模型名字符串本身,在 LLM 请求时作为 `request.model` 字段发送,由 deepseek 服务端区分 pro/flash 能力。

### 9.4 用户配置要求

MVP 用户需在环境变量中配置:

```bash
# Windows PowerShell
$env:OPENAI_API_KEY = "sk-deepseek-xxx"
$env:OPENAI_BASE_URL = "https://api.deepseek.com/v1"
```

或在 `.claw/settings.json` 中配置 `providerFallbacks`:

```json
{
  "providerFallbacks": {
    "primary": "deepseek-v4-flash",
    "fallbacks": ["deepseek-v4-pro"]
  }
}
```

### 9.5 Spike 发现的潜在问题

1. **base_url 共享**:pro/flash 共享同一 `OPENAI_BASE_URL`,若用户同时用 OpenAI 和 deepseek,需用 `openai/deepseek-v4-pro` 前缀路由(走 `metadata_for_model` 的 `openai/` 分支)。MVP 首期假设用户只用 deepseek,不处理多 provider 共存。

2. **reasoning_content 序列化**:`deepseek-v4-*` 需要 `reasoning_content_in_history`([openai_compat.rs:983](../rust/crates/api/src/providers/openai_compat.rs#L983) `starts_with("deepseek-v4")`),`ProviderClient::from_model` 构造的 client 会自动处理,无需 subagent 额外配置。

3. **fallback 链与 upgrade_map 协调**:现有 `providerFallbacks`(retryable 错误触发)与 `upgrade_map`(validation 失败触发)正交,不冲突。但 MVP 首期应**禁用** `providerFallbacks`,只用 `upgrade_map`,避免双层重试混淆。验证流程稳定后再考虑组合。

### 9.6 Spike 验证命令

```bash
# 编译验证
cargo build -p api --release

# 单元测试:验证 from_model 对 deepseek 双模型成功构造
cargo test -p api --release provider_client_integration -- --nocapture

# 手动 spike(需设置 OPENAI_API_KEY / OPENAI_BASE_URL 指向 deepseek)
$env:CLAW_DIAG=1
./target/release/claw-plus.exe --model deepseek-v4-flash --diag
# 检查 ~/.claw/claw-diag.log 中 "constructing dedicated client" 日志
```

---

## 10. MVP 范围:deepseek-v4-pro / flash 双模型路由(v3 修订)

> **理念**:先深度后广度。首期只覆盖 deepseek 双模型,验证端到端流程(能力分级 → 路由 → retry → 升级 → 验证 → 成本门禁),再扩展到其他 provider。
>
> **v3 修订**:成本门禁从 v2 提前到 MVP 必须(P0),因为 deepseek-v4-pro 相对 flash 成本约 10 倍,无门禁会导致 retry 失控。借鉴 Router-R1 成本奖励 + FrugalGPT 成本感知级联。

### 10.1 MVP 范围定义(v3 修订)

| 维度 | MVP 包含 | MVP 排除(留待 v2/v3 扩展) |
|---|---|---|
| **模型** | deepseek-v4-pro / deepseek-v4-flash | Claude/Grok/GPT 系列 |
| **ModelTier** | Flagship(pro) / Budget(flash) | Standard(无对应 deepseek 模型) |
| **TaskComplexity** | Simple / Diagnostic | Architectural(首期不验证) |
| **upgrade_map** | `{"deepseek-v4-flash": "deepseek-v4-pro", cost_multiplier: 10.0}` 单跳 | haiku→sonnet→opus 多跳链 |
| **ValidationGate** | `rust_compile_gate` 单一注册 | npm/pytest/lint 等 |
| **LlmJudgeGate** | **trait 预留**(P0),实现留待 v2 | `call_judge_model` 实现 |
| **诊断 SOP** | Diagnostic 复杂度注入 | Architectural 复杂度注入 |
| **retry** | max_attempts=2(flash 失败 → 升级 pro 一次) | 可配置 max_attempts |
| **成本门禁(P0)** | `cost_limit` + `record_cost` + `check_cost_limit` | 从历史 token 推断预估用量 |
| **checkpoint(P1)** | `save_checkpoint` 落地(元状态),`checkpoint_path` 字段 | `restore_from_checkpoint` 实现 |
| **spawn_parallel(P1)** | 接口预留(串行退化) | tokio 真并行 |
| **cost guard** | ~~不实现~~ → v3 提前到 MVP(P0) | 从历史 token 推断预估用量 |

### 10.2 MVP 用户故事(v3 新增场景 5)

**场景 1:简单任务路由到 flash**

```
主 agent: dispatch_subagent({name: "fmt", task: "格式化 mod.rs", complexity: "simple", model: "deepseek-v4-flash"})
→ coordinator.spawn_with_model(model: flash, complexity: Simple, max_attempts: 1)
→ model_meets_complexity("flash", Simple) = true ✅
→ run_subagent_turn_with_model(flash) → 成功 → validate(cargo build) → 通过 → 完成
```

**场景 2:诊断任务路由到 pro(直接)**

```
主 agent: dispatch_subagent({name: "diag", task: "定位 wizard 闪退", complexity: "diagnostic", model: "deepseek-v4-pro"})
→ coordinator.spawn_with_model(model: pro, complexity: Diagnostic, max_attempts: 2)
→ model_meets_complexity("pro", Diagnostic) = true ✅
→ run_subagent_turn_with_model(pro) → 成功 → validate → 通过 → 完成
```

**场景 3:诊断任务误派 flash,自动升级 pro**

```
主 agent: dispatch_subagent({name: "diag", task: "定位崩溃", complexity: "diagnostic", model: "deepseek-v4-flash"})
→ coordinator.spawn_with_model(model: flash, complexity: Diagnostic, max_attempts: 2)
→ model_meets_complexity("flash", Diagnostic) = false ⚠️ (记录 warning 到 notes)
→ attempt 1: run_subagent_turn_with_model(flash) → 成功但 validate 失败(编译错误)
→ record_cost(flash tokens) → save_checkpoint()
→ reset_for_retry()
→ upgrade_model("flash", Diagnostic) = Some(UpgradeEntry{target: "pro", cost_multiplier: 10.0}) ✅
→ check_cost_limit("pro", estimated_tokens=100K) → Ok(()) (未超限)
→ attempt 2: run_subagent_turn_with_model(pro) → 成功 → validate → 通过 → 完成
```

**场景 4:诊断任务误派 flash,升级后仍失败**

```
主 agent: dispatch_subagent({name: "diag", task: "定位崩溃", complexity: "diagnostic", model: "deepseek-v4-flash"})
→ attempt 1: flash → validate 失败 → reset → upgrade to pro → check_cost_limit Ok
→ attempt 2: pro → validate 失败
→ attempt == max_attempts(2) → fail("validation failed after 2 attempts")
```

**场景 5(v3 新增 P0):成本超限,拒绝升级**

```
主 agent: dispatch_subagent({
  name: "diag", task: "大规模重构", complexity: "diagnostic",
  model: "deepseek-v4-flash", cost_limit: 0.50  // 50 美分上限
})
→ attempt 1: flash → 消耗 80K tokens → record_cost($0.022) → validate 失败
→ reset_for_retry()
→ upgrade_model("flash") = Some(UpgradeEntry{target: "pro", cost_multiplier: 10.0})
→ check_cost_limit("pro", estimated_tokens=100K):
    accumulated $0.022 + estimated $0.42 (pro 100K) = $0.442 < $0.50 → Ok ✅
→ attempt 2: pro → 消耗 120K tokens → record_cost($0.336) → validate 失败
→ attempt == max_attempts(2) → 但此时 accumulated $0.358 < limit $0.50,正常 fail

// 对比:若 cost_limit=0.30
→ attempt 1: flash → $0.022 → validate 失败 → reset
→ upgrade to pro → check_cost_limit:
    accumulated $0.022 + estimated $0.42 = $0.442 > $0.30 → Err ❌
→ fail("cost limit $0.30 exceeded: accumulated $0.022 + estimated $0.42")
→ 不浪费 pro 调用费用,直接中止
```

### 10.3 MVP 实施清单(v3 修订)

| 步骤 | 任务 | 验证 | 优先级 |
|---|---|---|---|
| 1 | 实现 `runtime::diag`(提取 panic hook + 推广 paste_diag_log) | `cargo build -p runtime`;手动触发 panic 检查 `~/.claw/claw-crash.log` | P0 |
| 2 | 实现 `api::model_tier`(`tier_for_model` 含 flash/pro + `upgrade_map` 配置化 + `UpgradeEntry.cost_multiplier`) | `cargo test -p api model_tier`;验证 flash→Budget, pro→Flagship, cost_multiplier=10.0 | P0 |
| 3 | 扩展 Subagent 字段(`checkpoint_path`/`cost_limit`/`cost_accumulated`) + `spawn_with_model` + `reset_for_retry` + `record_cost` + `check_cost_limit` + `save_checkpoint` | `cargo test -p runtime multi_agent`;原 12 处测试零改动 | P0 |
| 4 | 实现 `validation.rs`(`CommandValidationGate` + `rust_compile_gate` + `LlmJudgeGate` trait 预留) | `cargo build -p runtime`;手动注册 gate 验证;`LlmJudgeGate` 编译通过但不注册 | P0 |
| 5 | 改造 `execute_dispatch_subagent` retry loop(调用 `run_subagent_turn_with_model`,集成成本门禁 + checkpoint 保存) | `cargo build -p rusty-claude-cli`;手动 dispatch 验证成本门禁触发 | P0 |
| 6 | 诊断 SOP 注入(Diagnostic 复杂度) | 手动派发诊断任务,检查 system_prompt 含 SOP | P1 |
| 7 | `spawn_parallel` 接口预留(串行退化) | `cargo build -p runtime`;接口编译通过 | P1 |
| 8 | **决策持久化 §4.7**:`decision_log.rs` + NOTEBOOK decisions 段 + FTS5 decision role + compaction 前提取 | `cargo build -p runtime`;触发 compaction 后检查 NOTEBOOK.md 含 `<decisions>` 段;`session_search` 能命中 decision role | P1 |
| 9 | **端到端 MVP 验证**:场景 1-5 全部跑通(含成本超限场景 5) | 见 §10.2 用户故事 | P0 |

### 10.4 MVP 验收标准(v3 修订)

**模型路由(P0)**:
- [ ] `tier_for_model("deepseek-v4-flash") == ModelTier::Budget`
- [ ] `tier_for_model("deepseek-v4-pro") == ModelTier::Flagship`
- [ ] `upgrade_model("deepseek-v4-flash", Diagnostic) == Some(UpgradeEntry{target: "deepseek-v4-pro", cost_multiplier: 10.0})`
- [ ] `upgrade_model("deepseek-v4-pro", Diagnostic) == None`(已顶级)
- [ ] `model_meets_complexity("deepseek-v4-flash", Diagnostic) == false`
- [ ] `model_meets_complexity("deepseek-v4-pro", Diagnostic) == true`

**成本门禁(P0 v3 新增)**:
- [ ] `model_pricing("deepseek-v4-flash") == (0.14, 0.28)`
- [ ] `model_pricing("deepseek-v4-pro") == (1.40, 2.80)`
- [ ] `record_cost` 正确累加 `cost_accumulated`(flash 80K tokens → ~$0.022)
- [ ] `check_cost_limit` 在 accumulated + estimated > limit 时返回 Err
- [ ] `check_cost_limit` 在 accumulated + estimated ≤ limit 时返回 Ok
- [ ] 场景 5:cost_limit=0.30 时,flash 失败后升级 pro 被成本门禁拒绝,不浪费 pro 调用

**端到端流程(P0)**:
- [ ] 简单任务路由到 flash,一次成功
- [ ] 诊断任务路由到 pro,一次成功
- [ ] 诊断任务误派 flash,validate 失败后自动升级 pro 重试成功
- [ ] 升级后仍失败,正确返回 fail 而非无限重试
- [ ] 成本超限时,正确返回 fail 而非强行升级(场景 5)

**checkpoint(P1)**:
- [ ] `save_checkpoint` 每轮 turn 后写入 `checkpoint_path` 指定文件
- [ ] checkpoint 文件包含 id/task/model/attempts/cost_accumulated 字段
- [ ] checkpoint 保存失败不影响主流程(`let _ =` 容错)

**诊断层(P0)**:
- [ ] `~/.claw/claw-diag.log` 记录完整 retry 链路(attempt 1/2, model 升级, cost 检查)
- [ ] `~/.claw/claw-crash.log` 在 panic 时生成(headless 入口也覆盖)

**LlmJudgeGate trait 预留(P0)**:
- [ ] `LlmJudgeGate` 实现 `ValidationGate` trait,编译通过
- [ ] `LlmJudgeGate::diagnostic_default` 构造的 rubric 含根因定位/方案可行性/完整性/副作用评估四维
- [ ] MVP 阶段不注册 `LlmJudgeGate`(诊断任务用人工验收 + rust_compile_gate)

**决策持久化 §4.7(P1)**:
- [ ] `detect_decision_signal` 能检测中英文决策关键词(decided/权衡/否决/而非 等)
- [ ] `extract_decisions_before_compaction(Heuristic)` 从含决策关键词的消息中提取 `DecisionPoint`
- [ ] NOTEBOOK.md `SECTION_TAGS` 包含 `"decisions"`(6 段)
- [ ] `persist_decisions_to_notebook` 写入 NOTEBOOK.md 的 `<decisions>` 段,遵守 16K 上限
- [ ] compaction 触发后,NOTEBOOK.md 出现 `<decisions>` 段(用含决策关键词的长对话触发 compaction 验证)
- [ ] 决策点同步写入 FTS5 索引,role=`"decision"`
- [ ] `session_search` 搜索决策关键词时,decision role 命中排名优先于普通消息(rank × 0.5 加权)
- [ ] DAG 协同:dag-orchestration-detail.md 的 `append_to_notebook` 建议改为调用 `persist_decisions_to_notebook`(文档对齐)

### 10.5 MVP 后的扩展路径(v3 修订)

MVP 验收通过后,按以下顺序扩展(每步独立可回滚):

**v2 阶段(深度完善)**:
1. **`LlmJudgeGate` 实现**(P2):实现 `call_judge_model`,诊断任务自动验证,借鉴 Anthropic rubric
2. **`restore_from_checkpoint` 实现**(P2):崩溃后从 checkpoint 恢复,借鉴 LangGraph durable execution
3. **spawn_parallel 真并行**(P2):接入 tokio,subagent 并发,借鉴 Anthropic 90% 加速
4. **多 ValidationGate**:注册 npm/pytest/lint gate,验证多语言项目
5. **Architectural 复杂度**:加入架构决策 SOP
6. **决策持久化 LLM 提取**(P2):`DetectionStrategy::LlmExtract` 用 flash 模型精确提取 context/decision/rationale/alternatives,替代 MVP 的启发式截断

**v3 阶段(广度扩展)**:
6. **Anthropic 链**:加入 haiku→sonnet→opus 升级链,验证多 provider 共存
7. **OpenAI 链**:加入 gpt-4.1-mini→gpt-4.1→o3 升级链
8. **xAI 链**:加入 grok-3-mini→grok-3 升级链

**v3+ 探索(前沿研究)**:
9. **FrugalGPT 式主动升级**(P3):不必等 validation 失败,置信度低就主动升级,利用 `confidence_threshold` 字段
10. **Router-R1 式 RL 路由**(P3):用 RL 训练路由器,泛化到新模型,借鉴 Router-R1 成本奖励 + 模型描述符
11. **RouteLLM 式偏好数据路由**(P3):用人类偏好数据训练路由器,从查询本身推断难度

---

## 12. v3 Phase 3 实施完成记录(2026-07-28)

### 12.1 已完成工作项

| # | 工作项 | 文件 | 测试 |
|:-:|------|------|------|
| 1 | **多 provider 升级链**(Anthropic/OpenAI/xAI) | `runtime/src/multi_agent/upgrade.rs` + `api/src/upgrade.rs` | runtime 14 + api 9 |
| 2 | **spawn_parallel_via_dag 真并行** | `runtime/src/conversation.rs` | 4 集成测试 |
| 3 | **DAG 部分失败容错(FailFast::Off)** | `runtime/src/multi_agent/dag/types.rs` + `scheduler.rs` | 含在 spawn_parallel 测试 |
| 4 | **异步接口变体(避免 block_on)** | `runtime/src/conversation.rs` | 4 async + 1 sync 测试 |
| 5 | **CLI tool 接入(spawn_parallel_subagents)** | `runtime/src/conversation.rs` + `rusty-claude-cli/src/plugin_state.rs` | 7 测试 |
| 6 | **DetectionStrategy::LlmExtract 端到端** | `runtime/src/conversation.rs` + `rusty-claude-cli/src/app.rs` | 2 测试 |

### 12.2 关键设计决策

#### FailFast 枚举(`types.rs`)
- `FailFast::On`(默认):任一 node 失败(耗尽 retry)后立即取消整个 DAG,返回 `Err(DagError::NodeFailed)`。向后兼容。
- `FailFast::Off`:标记失败节点,跳过其下游依赖,继续执行独立分支。DAG 正常结束(返回 `Ok`),结果仅含成功节点。
- 依赖失败传播:FailFast::Off 时,若 node 的任一 `depends_on` 在 `failed` 或 `skipped` 集合中,该 node 标记为 `Skipped` 并加入 `completed`(防止 `ready_nodes` 反复列出)。

#### spawn_parallel_via_dag 三变体(`conversation.rs`)
1. `spawn_parallel_via_dag(tasks)` — 同步,默认 FailFast::On(向后兼容)
2. `spawn_parallel_via_dag_with_fail_fast(tasks, fail_fast)` — 同步,可配置 FailFast
3. `spawn_parallel_via_dag_async(tasks, fail_fast)` — 异步,供 async 调用方使用(避免 block_on)

共享逻辑提取到三个私有辅助:`prepare_dag_for_spawn_parallel`、`build_spawn_parallel_graph`、`map_dag_run_result`。

#### spawn_parallel_subagents 工具
- 注册为 `RuntimeToolDefinition`(与 `dispatch_subagent` 同级)
- 执行由 `ConversationRuntime::run_turn` 拦截,路由到 `execute_spawn_parallel_subagents`
- JSON 输入:`tasks` 数组(每项含 name/task/model 必填,mode/complexity 可选)+ `fail_fast`(可选,默认 on)
- 输出:可读多行字符串,标明每个任务的成功/失败

#### DetectionStrategy 端到端接入
- `ConversationRuntime` 新增 `detection_strategy: DetectionStrategy` 字段(默认 `Heuristic`)
- 新增 `with_detection_strategy(strategy)` builder + `detection_strategy()` getter
- `maybe_auto_compact` 从 `self.detection_strategy.clone()` 读取策略(替代硬编码 `Heuristic`)
- `app.rs::build_runtime` 在 `DecisionExtractorClient` 注册成功后调用 `with_detection_strategy(LlmExtract { model })`
- 3 路降级保证:client 未注册 / LLM 调用失败 / JSON 解析失败 → 自动回退 Heuristic

### 12.3 测试验证

- runtime crate:**1398 passed**, 0 failed, 2 ignored
- rusty-claude-cli crate:**373 passed**, 0 failed
- api crate:**183 passed**, 0 failed
- 新增测试:**20 个**(spawn_parallel 系列 9 + execute_spawn_parallel_subagents 7 + detection_strategy 2 + dag FailFast 2)

### 12.4 后续工作

- ~~spawn_parallel_via_dag_async 生产集成~~:**已完成**(v3 Phase 4,见 §13)。
- ~~DAG partial failure tolerance 进一步增强~~:**已完成**(v3 Phase 4,见 §13)。
- ~~DetectionStrategy 运行时切换~~:**已完成**(v3 Phase 4,见 §13)。

---

## 13. v3 Phase 4 实施完成记录(2026-07-28)

### 13.1 已完成工作项

| # | 工作项 | 文件 | 测试 |
|:-:|------|------|------|
| 1 | **DetectionStrategy 运行时切换** | `runtime/src/decision_log.rs` + `runtime/src/conversation.rs` + `commands/src/lib.rs` + `rusty-claude-cli/src/app.rs` + `rusty-claude-cli/src/format.rs` + `rusty-claude-cli/src/session_mgr.rs` + `runtime/src/lib.rs` | format 11 + commands 3 |
| 2 | **FailFast::Off 增强** | `runtime/src/multi_agent/dag/types.rs` + `scheduler.rs` + `mod.rs` + `status.rs` | 7 回归测试 |
| 3 | **async 变体生产集成** | `runtime/src/conversation.rs` | 5 新测试(3 async + 2 共享逻辑) |

### 13.2 关键设计决策

#### DetectionStrategy 运行时切换(`/detection-strategy` 命令)

- **方案 A(直接 setter)**:`ConversationRuntime::set_detection_strategy(&mut self, strategy)` 原地切换,不重建 runtime(因 `detection_strategy` 是简单字段)。
- **命令格式**:`/detection-strategy [heuristic|llm[:<model>]]`
  - 无参数:打印当前策略报告(含 ●/○ 标记)
  - `heuristic`:切换为启发式(零成本)
  - `llm`:切换为 LLM 提取(默认 `deepseek-v4-flash`)
  - `llm:<model>`:切换为 LLM 提取并指定模型
- **降级行为**:切换到 `LlmExtract` 但未注册 client 时,自动 3 路降级为 Heuristic,不阻塞 compaction。
- **DetectionStrategy 新增 `PartialEq`**:支持"已是该策略则不切换"短路。
- **SlashCommand 新增 `DetectionStrategy` 变体**:4 处注册点(spec / enum / canonical_name / parse) + 2 处 match 补全(session_mgr / is_runtime_state_change)。

#### FailFast::Off 增强(retry_failed / recover_skipped)

- **新增 `DagRunResult` 结构体**:含 `successes: Vec<NodeResult>` + `failures: Vec<(DagNodeId, String)>` + `skipped: Vec<DagNodeId>`,提供 `is_all_success()` / `into_successes()` 等方法。
- **新增 `DagStatus::CompletedWithFailures`**:FailFast::Off 下若有 failed/skipped,终态为 `CompletedWithFailures`(区别于 `Completed` 全成功)。
- **新增 `ProgressEvent::NodeSkipped`**:独立事件(不再复用 `NodeFailed`),含 `node_id` + `reason`。
- **新增 `DagScheduler::run_with_details()`**:返回 `DagRunResult` 而非 `Vec<NodeResult>`(向后兼容:`run()` 仍返回 `Vec<NodeResult>`)。
- **新增 `DagScheduler::retry_failed(&self, node_ids)`**:构造子 DAG(清除 depends_on,调用方负责确保依赖已满足),用同一 executor + FailFast 策略重新执行。
- **新增 `DagScheduler::recover_skipped(&self, node_ids)`**:与 `retry_failed` 共享子图构造逻辑,语义区别在调用方约定(针对 skipped 节点)。
- **`failed` 集合升级为 `HashMap<DagNodeId, String>`**:存储最后一次错误信息,供 `DagRunResult.failures` 使用。
- **`run_inner` 返回值改为 `DagRunResult`**:`run()` / `run_with_progress()` 通过 `into_successes()` 提取保持向后兼容。

#### async 变体生产集成

- **新增 `execute_spawn_parallel_subagents_async`**:async 镜像,内部调用 `spawn_parallel_via_dag_async`(直接 `.await`,无 `block_on`)。
- **共享逻辑提取**:`parse_spawn_parallel_input`(解析 JSON)+ `format_spawn_parallel_results`(格式化输出),同步/async 变体复用。
- **当前状态**:接口就绪,但 `run_turn` 仍是同步函数,async 变体暂未被生产路径调用。待 `run_turn_async` 改造完成后可直接接入。
- **适用场景**:claw-shell ACP 路径(已在 tokio runtime 中),未来 `run_turn_async` 改造后可消除"嵌套 runtime + block_on"开销。

### 13.3 测试验证

- runtime crate:**1410 passed**, 0 failed, 2 ignored(新增 12 个:7 dag + 5 async)
- rusty-claude-cli crate:**383 passed**, 0 failed(新增 11 个 format 测试)
- commands crate:**46 passed**, 0 failed(新增 3 个解析测试 + 更新命令计数)
- api crate:**183 passed**, 0 failed
- **新增测试总计:26 个**

### 13.4 后续工作

- `run_turn_async` 改造:目前 `run_turn` 是同步函数,TUI/REPL/JSON 路径均无 tokio runtime。改造为 async 后,`execute_spawn_parallel_subagents_async` 可直接接入,消除 `spawn_parallel_via_dag_with_fail_fast` 的临时 runtime 构造开销。
- FailFast::Off 进一步增强:目前 `retry_failed` / `recover_skipped` 构造全新子 DAG,未保留原 scheduler 的 DagStore 桥接。未来可支持"在原 DagRun 上追加 retry 记录"。
- `/detection-strategy` 增强:目前切换后立即生效,未来可加 `--dry-run` 预览或 `--verify` 校验 client 是否已注册。

---

## 14. 总结

本方案从平台层根治"能力不足模型浪费轮次"问题，**五重防护**(v3 从四重升级):

1. **路由层**：模型能力分级 + 任务复杂度匹配，诊断任务拒绝交给能力不足模型
2. **执行层**：验证门禁强制编译通过 + LlmJudgeGate 预留(v3),迭代上限防止无限重试，模型升级链自动提升能力
3. **成本层(v3 新增)**:`cost_limit`/`cost_accumulated` + `check_cost_limit` 升级前门禁,防止模型升级导致账单失控。借鉴 Router-R1 成本奖励 + FrugalGPT 成本感知
4. **诊断层**：统一 `diag!` 宏 + panic hook + crash log + checkpoint(v3 预留),让所有模型都能获得**运行时信号**
5. **决策层(v3 §4.7 新增)**:compaction 前自动提取决策点 → NOTEBOOK decisions 段 + FTS5 decision role 加权索引,让**设计决策信号**在 context compaction 后仍可回溯。与诊断层互补:diag! 记录"发生了什么",decision_log 记录"决定了什么、为什么"

核心设计原则：

- **源头控制**：在 coordinator 分发前校验能力，而非在 subagent 失败后补救
- **完成信号优先**：验证门禁提供可靠完成信号，不依赖 agent 自我宣称
- **最小侵入**：`spawn` 保持原签名向后兼容,新增 `spawn_with_model` 扩展(v2 修正),`CLAW_DIAG` 默认关闭
- **清理旧版**：`paste_diag_log` 统一到 `runtime::diag`,main.rs 内联 panic hook 提取到 `runtime::diag`(v2 修正)
- **先深度后广度**:MVP 首期只覆盖 deepseek-v4-pro/flash 双模型路由,验证端到端流程后再扩展(v2 新增)
- **成本感知(v3 新增)**:所有模型升级决策都经过成本门禁,借鉴 Router-R1 把成本作为一等公民的设计
- **渐进式预留(v3 新增)**:LlmJudgeGate/checkpoint/spawn_parallel 在 MVP 阶段 trait/字段/接口就位,实现分阶段落地,避免过度工程化的同时为演进铺路
- **决策不随 compaction 消失(v3 §4.7 新增)**:compaction 前提取决策点,写入 NOTEBOOK + FTS5,确保"为什么这样设计"在 context 压缩后仍可回溯。借鉴项目现有四层信息持久化架构(Main Context / NOTEBOOK / ToolResultArchive / FTS5),在 compaction 路径插入决策提取步骤,不新建存储层

方案分 9 步实施(v3 从 8 步扩展),每步独立可验证,按 P0/P1 优先级逐步落地。MVP 范围见 §10,先做深度(单 provider 双模型端到端 + 成本门禁 + 决策持久化),再做广度(多 provider 多升级链 + LlmJudgeGate + checkpoint 恢复 + 并行执行 + LLM 决策提取)。
