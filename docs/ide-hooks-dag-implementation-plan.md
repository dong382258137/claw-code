# Claw Code 三大模块实现方案:IDE 集成 / Hooks 系统 / DAG 多 Agent 编排

> 文档版本:v1.0
> 制定日期:2026-07-21
> 适用对象:Claw Code v0.2.0(SHA `8af738a`)
> 调研基础:ACP 1.5 / Claude Code Hooks / LangGraph / Anthropic Multi-Agent Research System / MIRIX / CompactionRL / GuardAgent

---

## 目录

1. [总体架构与设计原则](#一总体架构与设计原则)
2. [IDE 集成方案](#二ide-集成方案)
3. [Hooks 系统方案](#三hooks-系统方案)
4. [DAG 多 Agent 编排方案](#四dag-多-agent-编排方案)
5. [三模块协同设计](#五三模块协同设计)
6. [分阶段实施路线图](#六分阶段实施路线图)
7. [风险评估与缓解](#七风险评估与缓解)
8. [参考论文与开源项目](#八参考论文与开源项目)

---

## 模块细化分支索引

> 本主文档作为 v1.0 基线(commit `3cbe180`),三大模块各拆分为独立细化文档进行竖叉分支迭代。
> 主文档保持架构总览与协同设计不变,各分支文档承载具体的 API 设计、代码骨架、测试用例与实施细节。

| 分支 | 文档路径 | 状态 | 当前版本 | 焦点 |
|:-:|---|:-:|:-:|---|
| **IDE 集成** | [docs/modules/ide-integration-detail.md](./modules/ide-integration-detail.md) | 🚧 v0.1 | — | ACP 1.5 升级路径 + ClawAgent 扩展 + LaneEvent 桥接 + VS Code 扩展骨架 |
| **Hooks 系统** | [docs/modules/hooks-system-detail.md](./modules/hooks-system-detail.md) | 🚧 v0.1 | — | 10 事件 × 4 Handler + HookRunner 异步引擎 + run_turn 7 集成点 + 配置示例 |
| **DAG 编排** | [docs/modules/dag-orchestration-detail.md](./modules/dag-orchestration-detail.md) | 🚧 v0.1 | — | petgraph 数据结构 + JoinSet 分层调度 + Plan→DAG 转换 + Checkpointer + YAML 声明式 |

### 分支迭代策略

1. **独立演进**:每个分支文档独立版本号(v0.1 → v1.0),不强制与主文档同步
2. **回写机制**:分支稳定后,关键决策回写主文档对应章节(标注 `[synced from <branch> v<x.x>]`)
3. **冲突优先**:分支与主文档冲突时,以分支为准(分支承载最新设计)
4. **合并节点**:三分支均达 v1.0 后,合并为 v2.0 主文档,启动 P0 实现阶段

---

## 一、总体架构与设计原则

### 1.1 三模块定位

```
┌─────────────────────────────────────────────────────────────┐
│                    Claw Code Runtime                        │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  IDE 集成层(ACP 1.5 stdio server)                   │  │
│  │  - 协议层:agent-client-protocol crate               │  │
│  │  - 入口:claw acp serve / claw-headless              │  │
│  │  - 扩展点:SessionNotification / fs/* / permission   │  │
│  └──────────────────────────────────────────────────────┘  │
│                          ↕                                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  Hooks 系统层(事件驱动中间件)                       │  │
│  │  - 10 事件 × 4 Handler(command/webhook/inline/prompt)│  │
│  │  - 集成点:run_turn 主循环 + tool call 循环           │  │
│  │  - 扩展点:HookEvent enum + HookHandler enum          │  │
│  └──────────────────────────────────────────────────────┘  │
│                          ↕                                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  DAG 多 Agent 编排层(声明式调度)                    │  │
│  │  - 数据结构:petgraph + 自定义 DagGraph              │  │
│  │  - 调度:tokio::JoinSet + CancellationToken            │  │
│  │  - 整合:Plan → DAG 转换 + RecoveryOrchestrator       │  │
│  └──────────────────────────────────────────────────────┘  │
│                          ↕                                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  现有基础(保留不动)                                │  │
│  │  - MultiAgentCoordinator(Fork/Teammate/Worktree)    │  │
│  │  - PlanArtifact + RuleVerifier + RecoveryOrchestrator│  │
│  │  - LaneEvents + NOTEBOOK.md + ToolResultArchive     │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

### 1.2 五大设计原则

| # | 原则 | 说明 |
|---|------|------|
| 1 | **协议优先** | IDE 集成走标准 ACP 1.5,不发明新协议 |
| 2 | **分层不替代** | DAG 是编排层,Fork/Teammate/Worktree 保留为执行后端 |
| 3 | **复用现有接入点** | 所有新代码挂载到 `run_turn` 主循环已知位置 |
| 4 | **缓存保护** | 新功能不污染绝对稳定区(system_prompt + tools_schema) |
| 5 | **渐进式落地** | P0 最小可行 → P1 生产可用 → P2 高级特性 |

### 1.3 关键约束(来自项目记忆)

1. **非 Send 类型隔离**:`ClawAgent` 持有 `Rc<RefCell<...>>`,必须在 `current_thread + LocalSet` 上运行
2. **Hook blocking 语义**:hook 同步阻塞,执行完成后才继续 tool 执行
3. **MultiAgentCoordinator 纯状态机**:`start()` 不执行任务,真正执行在 `conversation.rs::execute_dispatch_subagent`
4. **LaneEvent 容量保护**:全局 sink 容量 512,超容量丢弃最旧一半
5. **ACP 0.10.4 silent drop**:invalid JSON / missing method 静默丢弃(升级到 1.5 可改善)

---

## 二、IDE 集成方案

### 2.1 核心结论

**Claw Code 已具备 IDE 集成基础设施**(Phase A 完成的 `claw acp serve`),需要做的:

1. **升级 ACP 协议版本**:0.10.4 → 1.5(获取 v2 diff/permission 能力)
2. **补齐协议方法**:fs/read_text_file / fs/write_text_file / session/request_permission / session/load / session/resume
3. **激活 LaneEvent 消费者**:把内部事件通过 `SessionNotification` 推送给 IDE
4. **开发 VS Code 扩展**(薄客户端):Zed/JetBrains 零开发,VS Code/Cursor 需自建

### 2.2 ACP 协议升级路径

#### 2.2.1 版本升级影响分析

| 维度 | 0.10.4(当前) | 1.5(目标) | 影响 |
|---|---|---|---|
| 错误处理 | invalid JSON silent drop | 返回 `-32700` parse_error | **改善**(测试需更新) |
| Diff 格式 | v1(简单文本) | v2(带 location + 类型化) | **新增字段** |
| Permission 模型 | 简单 request_permission | v2 typed PermissionOption | **API 变更** |
| Session 配置 | 字符串 | typed boolean config | **破坏性** |
| Content 类型 | 自定义 | 对齐 MCP Content | **类型重命名** |

**升级策略**:
- **P0**:保持 0.10.4 兼容,新增 1.5 特性为 opt-in(`unstable-v2` feature flag)
- **P1**:全面切换到 1.5,废弃 0.10.4 兼容代码
- **测试更新**:A6.4 的 3 个错误路径测试需重写(1.5 会返回 error response)

#### 2.2.2 协议方法补齐清单

| 方法 | 优先级 | 实现位置 | 说明 |
|---|---|---|---|
| `fs/read_text_file` | P0 | `claw-shell/src/agent.rs` 新增 | 读 editor buffer(含未保存内容) |
| `fs/write_text_file` | P0 | 同上 | 写文件走 editor undo 栈 |
| `session/request_permission` | P0 | 同上 | 危险操作审批(替代当前 stub) |
| `session/load` | P1 | 同上 | 从 session_id 恢复 |
| `session/resume` | P1 | 同上 | v1.3.0+ 与 load 统一 |
| `session/fork` | P2 | 同上 | 从某消息分叉 |
| `session/list` | P2 | 同上 | 列出可恢复会话 |
| `session/set_mode` | P2 | 同上 | 切换 plan/act 模式 |
| `session/set_model` | P2 | 同上 | 切换底层模型 |

### 2.3 关键代码骨架

#### 2.3.1 ACP 1.5 升级后的 ClawAgent 扩展

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent)

impl<C: ApiClient> ClawAgent<C> {
    /// P0:读 editor buffer(含未保存内容)
    /// 
    /// ACP 1.5 fs/read_text_file 方法实现。
    /// 委托给 AcpGatewaySender 反向请求 editor。
    pub async fn read_editor_buffer(&self, path: &str) -> Result<String, String> {
        let args = serde_json::json!({ "path": path });
        let result = self.gateway.request(
            acp::ReadTextFileToolArguments::PATH,
            args,
        ).await?;
        result["content"].as_str()
            .map(String::from)
            .ok_or_else(|| "missing content field".into())
    }

    /// P0:写文件走 editor undo 栈
    pub async fn write_editor_buffer(&self, path: &str, content: &str) -> Result<(), String> {
        let args = serde_json::json!({
            "path": path,
            "content": content,
        });
        self.gateway.request("fs/write_text_file", args).await?;
        Ok(())
    }

    /// P0:请求权限(替代当前 stub cancel)
    /// 
    /// ACP 1.5 session/request_permission 反向请求。
    /// 用于危险操作(Bash rm -rf / Write 覆盖)审批。
    pub async fn request_permission(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        options: &[acp::PermissionOption],
    ) -> Result<acp::PermissionOutcome, String> {
        let args = serde_json::json!({
            "toolName": tool_name,
            "toolInput": tool_input,
            "options": options,
        });
        let result = self.gateway.request("session/request_permission", args).await?;
        serde_json::from_value(result).map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl<C: ApiClient> acp::Agent for ClawAgent<C> {
    // 现有方法保留...

    /// P1:加载已存在的 session
    async fn load_session(&mut self, params: acp::LoadSessionRequest) 
        -> Result<acp::LoadSessionResponse, acp::Error> {
        let session_id = params.session_id.clone();
        let session = SessionStore::load(&session_id)
            .map_err(|e| acp::Error::internal(format!("load session failed: {e}")))?;
        // 重建 ConversationRuntime 状态
        self.restore_runtime_state(session)?;
        Ok(acp::LoadSessionResponse {
            session_id,
            mode: session.mode,
            mcp_servers: session.mcp_servers,
            // ...
        })
    }

    /// P2:fork session(从某消息分叉)
    async fn fork_session(&mut self, params: acp::ForkSessionRequest)
        -> Result<acp::ForkSessionResponse, acp::Error> {
        // 实现细节:克隆 session 到 fork_point,后续消息丢弃
        todo!("P2")
    }
}
```

#### 2.3.2 LaneEvent → SessionNotification 桥接

```rust
// rust/crates/runtime/src/lane_events.rs(扩展现有消费者)

/// P0:LaneEvent 消费者,转发到 ACP SessionNotification
/// 
/// 在 run_turn 主循环中定期调用,把内部事件流式推送给 IDE。
pub fn flush_lane_events_to_acp(
    gateway: &AcpGatewaySender<acp::AgentSide>,
    session_id: &acp::SessionId,
) {
    let events = drain_lane_events();
    for event in events {
        if let Some(notification) = lane_event_to_session_update(&event) {
            // fire-and-forget,适合高频低延迟推送
            gateway.session_notification(session_id.clone(), notification);
        }
    }
}

/// LaneEvent → ACP SessionUpdate 转换
fn lane_event_to_session_update(event: &LaneEvent) -> Option<acp::SessionUpdate> {
    match event.event {
        LaneEventName::SubagentHandoff => {
            // 子 agent 启动 → tool_call notification
            Some(acp::SessionUpdate::ToolCall {
                tool_call_id: event.data["subagent_id"].as_str()?.to_string(),
                tool_kind: acp::ToolKind::Execute,
                title: format!("Subagent: {}", event.data["task"].as_str()?),
                status: acp::ToolCallStatus::Pending,
                // ...
            })
        }
        LaneEventName::SubagentResult => {
            // 子 agent 完成 → tool_call 完成
            Some(acp::SessionUpdate::ToolCall {
                tool_call_id: event.data["subagent_id"].as_str()?.to_string(),
                status: if event.failure_class.is_some() {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                },
                // ...
            })
        }
        // 可扩展:DagStepStarted / DagStepCompleted / CommitCreated / PrOpened 等
        _ => None,
    }
}
```

#### 2.3.3 VS Code 扩展骨架(薄客户端)

```typescript
// vscode-claw-extension/src/extension.ts
import { spawn, ChildProcess } from 'child_process';
import { createConnection } from 'vscode-languageserver/node';

let clawProcess: ChildProcess | null = null;

export function activate(context: vscode.ExtensionContext) {
    // 注册 chat participant(@claw)
    const participant = vscode.chat.createChatParticipant('claw', handleChatRequest);
    
    // 注册命令:启动 Claw ACP server
    context.subscriptions.push(
        vscode.commands.registerCommand('claw.startServer', startClawServer)
    );
    
    // 注册命令:停止 Claw ACP server
    context.subscriptions.push(
        vscode.commands.registerCommand('claw.stopServer', stopClawServer)
    );
}

async function startClawServer() {
    if (clawProcess) return;
    
    // spawn claw acp serve 子进程
    clawProcess = spawn('claw', ['acp', 'serve'], {
        cwd: vscode.workspace.rootPath,
        stdio: ['pipe', 'pipe', 'pipe'],
    });
    
    // stderr 走 output channel
    clawProcess.stderr?.on('data', data => {
        outputChannel.append(data.toString());
    });
    
    // stdout 接 JSON-RPC connection
    const connection = createConnection(
        clawProcess.stdout!,
        clawProcess.stdin!
    );
    
    // 初始化握手
    connection.sendRequest('initialize', { protocolVersion: 1 });
    
    // 注册 fs 能力(让 Claw 读 editor buffer)
    connection.onRequest('fs/read_text_file', async (params: any) => {
        const doc = vscode.workspace.textDocuments.find(d => d.uri.path === params.path);
        if (doc) return { content: doc.getText() }; // 返回未保存的 buffer
        const content = await vscode.workspace.fs.readFile(vscode.Uri.file(params.path));
        return { content: Buffer.from(content).toString() };
    });
    
    connection.onRequest('session/request_permission', async (params: any) => {
        const choice = await vscode.window.showWarningMessage(
            `Claw 请求执行: ${params.toolName}`,
            '允许', '拒绝', '始终允许'
        );
        return { outcome: choice === '允许' ? 'allow' : 'deny' };
    });
}
```

### 2.4 IDE 集成验证清单

| IDE | 验证方式 | 预期工作量 |
|---|---|---|
| **Zed** | 改 `settings.json` 加 `agent_servers.claw` | 0 开发(配置即可) |
| **JetBrains** | 写 `~/.jetbrains/acp.json` | 0 开发(配置即可) |
| **Neovim** | CodeCompanion.nvim 配置 `adapter = "acp"` | 0 开发(配置即可) |
| **VS Code** | 开发薄客户端扩展 | 2-3 周 |
| **Cursor** | 复用 VS Code 扩展 | 1 周(适配) |

### 2.5 IDE 集成 P0 交付物

1. `agent-client-protocol` 升级到 1.5(带 `unstable-v2` feature)
2. `claw-shell/src/agent.rs` 补齐 `fs/read_text_file` / `fs/write_text_file` / `session/request_permission`
3. `runtime/src/lane_events.rs` 新增 `flush_lane_events_to_acp` 函数
4. `conversation.rs::run_turn` 在 tool call 循环中调用 `flush_lane_events_to_acp`
5. A6.4 错误路径测试更新(1.5 返回 error response)
6. Zed 接入验证文档

---

## 三、Hooks 系统方案

### 3.1 现状与差距

**现有实现**(`runtime/src/hooks.rs` 1141 行):
- ✅ 3 事件(PreToolUse / PostToolUse / PostToolUseFailure)
- ✅ command handler(子进程)
- ✅ exit code 契约(0=Allow, 2=Deny, 其他=Failed)
- ✅ JSON stdout 解析(decision / permissionDecision / updatedInput)
- ✅ HookAbortSignal(AtomicBool)
- ✅ HookProgressReporter trait

**关键差距**:
- ❌ 缺 7 事件(UserPromptSubmit / SessionStart / SessionEnd / Stop / SubagentStop / PreCompact / Notification)
- ❌ 缺 3 handler(webhook / inline / prompt)
- ❌ 缺 matcher(regex 工具名过滤)
- ❌ 缺 timeout 控制
- ❌ 缺异步执行(当前同步阻塞)
- ❌ 缺 fail-open/fail-close 可配置
- ❌ runtime/hooks.rs 与 plugins/hooks.rs 重复实现

### 3.2 设计目标

| 维度 | 目标 |
|---|---|
| 事件类型 | 10 种(对齐 Claude Code + 保留 PostToolUseFailure) |
| Handler 类型 | 4 种(command / webhook / inline / prompt) |
| 执行模式 | Sequential(默认) + Parallel(可选) |
| 短路语义 | 可阻断事件 exit 2 短路,不可阻断事件全执行 |
| 失败策略 | FailClose(默认) + FailOpen(可配) |
| 超时控制 | 每 hook 独立 timeout(默认 30s) |
| Matcher | regex(仅工具类事件) |

### 3.3 关键代码骨架

#### 3.3.1 统一 HookEvent enum

```rust
// rust/crates/runtime/src/hooks.rs(扩展)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    // ── 工具层(可阻断、可改入参/输出)──
    #[serde(rename = "PreToolUse")]
    PreToolUse,
    #[serde(rename = "PostToolUse")]
    PostToolUse,
    /// Claw Code 特色:保留独立失败事件(优于 Claude Code 并入 PostToolUse)
    #[serde(rename = "PostToolUseFailure")]
    PostToolUseFailure,
    /// MCP 工具专用,可改 updatedOutput
    #[serde(rename = "PostCustomToolCall")]
    PostCustomToolCall,

    // ── 对话层 ──
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit,
    #[serde(rename = "Notification")]
    Notification,

    // ── 会话层 ──
    #[serde(rename = "SessionStart")]
    SessionStart,
    #[serde(rename = "SessionEnd")]
    SessionEnd,

    // ── Agent 层 ──
    #[serde(rename = "Stop")]
    Stop,
    #[serde(rename = "SubagentStop")]
    SubagentStop,

    // ── 上下文层 ──
    #[serde(rename = "PreCompact")]
    PreCompact,
}

impl HookEvent {
    /// 该事件是否支持阻断
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::PostCustomToolCall
                | HookEvent::UserPromptSubmit
                | HookEvent::Stop
                | HookEvent::SubagentStop
                | HookEvent::PreCompact
                | HookEvent::SessionStart
        )
    }

    /// 该事件是否支持 matcher(工具名 regex)
    pub fn supports_matcher(&self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PostCustomToolCall
        )
    }
}
```

#### 3.3.2 统一 HookHandler enum

```rust
// rust/crates/runtime/src/hooks.rs(扩展)

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookHandler {
    /// 子进程命令(已有,保留)— 跨语言、隔离性好
    Command(CommandHook),
    /// HTTP webhook POST — 适合远程审计 / Slack 通知
    Webhook(WebhookHook),
    /// 进程内 Rust trait — 零开销,SDK 嵌入场景
    Inline(InlineHookRef),
    /// LLM-based 评估 — 对应 Claude Code 的 prompt handler
    Prompt(PromptHook),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandHook {
    pub command: String,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookHook {
    pub url: String,
    #[serde(default = "default_http_method")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    /// HMAC 签名密钥(可选)
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHook {
    pub prompt: String,
    /// 使用的模型(默认 Haiku 级快速模型)
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_prompt_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

/// 进程内 Hook 引用(通过注册名查找)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineHookRef {
    /// 在 HookRegistry 中注册的 key
    pub name: String,
}

fn default_timeout() -> Duration { Duration::from_secs(30) }
fn default_prompt_timeout() -> Duration { Duration::from_secs(60) }
fn default_http_method() -> String { "POST".to_string() }

impl HookHandler {
    pub fn timeout(&self) -> Duration {
        match self {
            Self::Command(c) => c.timeout,
            Self::Webhook(w) => w.timeout,
            Self::Inline(_) => Duration::from_secs(60), // inline 默认 60s
            Self::Prompt(p) => p.timeout,
        }
    }
}
```

#### 3.3.3 进程内 Hook trait(对象安全)

```rust
// rust/crates/runtime/src/hooks.rs(新增)

/// 进程内 Hook trait(对象安全,支持动态注册)
/// 
/// 用于 InlineHookRef 通过 name 查找并执行。
/// Plugin 场景必需动态分发,因此使用 async_trait 宏。
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// Hook 唯一标识(用于 InlineHookRef.name 注册)
    fn id(&self) -> &str;

    /// 主入口:接收 HookContext,返回 HookRunResult
    async fn execute(&self, ctx: &HookContext<'_>) -> HookRunResult;

    /// 该 Hook 关心哪些事件(用于路由过滤)
    fn events(&self) -> &[HookEvent];

    /// 该 Hook 是否支持 matcher(默认 false)
    fn supports_matcher(&self) -> bool { false }
}

/// Hook 注册表(进程内 Hook)
pub struct HookRegistry {
    hooks: HashMap<String, Arc<dyn Hook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self { hooks: HashMap::new() }
    }

    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.hooks.insert(hook.id().to_string(), hook);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Hook>> {
        self.hooks.get(name)
    }
}
```

#### 3.3.4 统一配置 Schema

```rust
// rust/crates/runtime/src/hooks.rs(扩展)

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HookConfig {
    #[serde(default)]
    pub hooks: BTreeMap<HookEvent, Vec<HookMatcher>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookMatcher {
    /// regex(空字符串或 "*" 匹配全部;仅工具类事件适用)
    #[serde(default)]
    pub matcher: String,
    pub hooks: Vec<HookEntry>,
    /// 执行模式:Sequential(默认) / Parallel
    #[serde(default)]
    pub execution: HookExecution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    pub handler: HookHandler,
    /// 优先级(数字越小越先执行,默认 100)
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// 失败策略:FailClose(阻断,默认) / FailOpen(继续)
    #[serde(default)]
    pub failure_policy: FailurePolicy,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum HookExecution {
    #[default]
    Sequential,
    Parallel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FailurePolicy {
    #[default]
    FailClose,
    FailOpen,
}

fn default_priority() -> u32 { 100 }
fn default_true() -> bool { true }
```

#### 3.3.5 HookContext(事件上下文)

```rust
// rust/crates/runtime/src/hooks.rs(扩展)

pub struct HookContext<'a> {
    // 公共字段
    pub event: HookEvent,
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub cwd: PathBuf,
    pub permission_mode: ResolvedPermissionMode,

    // 事件特定字段(按 event 取用,Option 处理)
    pub tool_name: Option<&'a str>,
    pub tool_input: Option<&'a Value>,
    pub tool_response: Option<&'a Value>,
    pub tool_result_is_error: bool,
    pub prompt: Option<&'a str>,             // UserPromptSubmit
    pub message: Option<&'a str>,            // Notification
    pub stop_hook_active: bool,              // Stop(防递归)
    pub source: Option<&'a str>,             // SessionStart
    pub reason: Option<&'a str>,             // SessionEnd
    pub trigger: Option<&'a str>,            // PreCompact
    pub custom_instructions: Option<&'a str>,

    // 运行时辅助
    pub abort_signal: &'a HookAbortSignal,
    pub env_file: Option<&'a Path>,          // CLAUDE_ENV_FILE for SessionStart
}

impl<'a> HookContext<'a> {
    /// 从 tool call 构造 PreToolUse context
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
            cwd,
            tool_name: Some(tool_name),
            tool_input: Some(tool_input),
            abort_signal,
            // ... 其他字段 None / 默认
            ..Default::default()
        }
    }
}
```

#### 3.3.6 HookRunner 执行引擎(异步)

```rust
// rust/crates/runtime/src/hooks.rs(扩展)

pub struct HookRunner {
    config: HookConfig,
    inline_registry: HookRegistry,
}

impl HookRunner {
    pub async fn run(
        &self,
        event: HookEvent,
        ctx: &HookContext<'_>,
    ) -> HookRunResult {
        let matchers = self.config.hooks.get(&event).cloned().unwrap_or_default();
        let mut aggregate = HookRunResult::default();

        for matcher in matchers {
            // matcher 过滤(仅工具类事件)
            if event.supports_matcher() && !self.matcher_applies(&matcher.matcher, ctx) {
                continue;
            }

            let mut entries = matcher.hooks.clone();
            entries.sort_by_key(|e| e.priority);

            match matcher.execution {
                HookExecution::Sequential => {
                    for entry in entries {
                        if !entry.enabled { continue; }
                        let r = self.run_one(&entry, ctx).await;
                        let should_short_circuit = self.merge_and_check_short_circuit(
                            &mut aggregate, r, &event, &entry
                        );
                        if should_short_circuit { break; }
                    }
                }
                HookExecution::Parallel => {
                    let futures: Vec<_> = entries.iter()
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
                "hook timeout after {:?}", timeout
            )),
        }
    }

    async fn run_command(&self, hook: &CommandHook, ctx: &HookContext<'_>) -> HookRunResult {
        // 复用现有 run_commands 逻辑(line 338)
        // stdin 传 JSON payload,exit code 0/2/其他
        // 解析 JSON stdout 提取 decision / updatedInput 等
        todo!("复用现有 run_commands 逻辑")
    }

    async fn run_webhook(&self, hook: &WebhookHook, ctx: &HookContext<'_>) -> HookRunResult {
        let payload = serde_json::to_value(ctx).unwrap_or_default();
        let mut req = self.http_client.post(&hook.url).json(&payload);
        if let Some(secret) = &hook.secret {
            let sig = hmac_sha256(secret, &payload.to_string());
            req = req.header("X-Claw-Signature", sig);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let body: Value = resp.json().await.unwrap_or_default();
                parse_webhook_response(&body, status)
            }
            Err(e) => HookRunResult::failed(format!("webhook error: {e}")),
        }
    }

    async fn run_inline(&self, hook: &InlineHookRef, ctx: &HookContext<'_>) -> HookRunResult {
        match self.inline_registry.get(&hook.name) {
            Some(h) => h.execute(ctx).await,
            None => HookRunResult::failed(format!("inline hook not found: {}", hook.name)),
        }
    }

    async fn run_prompt(&self, hook: &PromptHook, ctx: &HookContext<'_>) -> HookRunResult {
        // 调用快速 LLM(Haiku 级)评估
        // prompt 模板:$ARGUMENTS 占位符替换为 ctx 内容
        // 返回 JSON {decision, reason, continue?}
        todo!("P1:调用 LLM 评估")
    }

    fn merge_and_check_short_circuit(
        &self,
        aggregate: &mut HookRunResult,
        new: HookRunResult,
        event: &HookEvent,
        entry: &HookEntry,
    ) -> bool {
        // 失败策略检查
        if new.failed {
            match entry.failure_policy {
                FailurePolicy::FailClose => {
                    if event.is_blocking() {
                        aggregate.decision = HookDecision::Deny;
                        return true; // 短路
                    }
                }
                FailurePolicy::FailOpen => {
                    // 继续执行
                }
            }
        }
        // 阻断决策检查
        if new.decision == HookDecision::Deny && event.is_blocking() {
            aggregate.decision = HookDecision::Deny;
            return true; // 短路
        }
        self.merge(aggregate, new, event);
        false
    }
}
```

#### 3.3.7 run_turn 集成点(精确行号)

```rust
// rust/crates/runtime/src/conversation.rs(扩展现有 run_turn)

pub fn run_turn(&mut self, user_input: &str, ...) -> Result<...> {
    // ── 现有代码(line 824)──
    self.loop_detector.reset(); // line 834

    // ★ P0 新增:UserPromptSubmit hook
    let prompt_ctx = HookContext::for_user_prompt_submit(
        user_input, &self.session_id, self.cwd.clone(), &self.hook_abort_signal
    );
    let prompt_hook_result = self.hook_runner.run(
        HookEvent::UserPromptSubmit, &prompt_ctx
    ).await;
    if prompt_hook_result.decision == HookDecision::Deny {
        return Err(prompt_hook_result.reason);
    }
    // 注入 additional_context
    if let Some(ctx) = prompt_hook_result.additional_context {
        self.inject_context(ctx);
    }

    // ★ P0 新增:SessionStart hook(首次 turn 时)
    if self.is_first_turn {
        let session_ctx = HookContext::for_session_start(
            &self.session_id, self.cwd.clone(), &self.hook_abort_signal
        );
        let _ = self.hook_runner.run(HookEvent::SessionStart, &session_ctx).await;
    }

    // ── 现有代码:语义召回 + PlanArtifact 创建(line 848-895)──
    // ...

    // ── 现有代码:主 ReAct 循环(line 903-1323)──
    loop {
        // ... request 构造 + api_client.stream ...

        // ── 现有代码:tool call 循环(line 1174-1322)──
        for tool_call in tool_calls {
            // ★ 现有:PreToolUse hook(line 1175)
            let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);

            // ★ P0 新增:PostCustomToolCall(对 MCP 工具)
            if tool_name.starts_with("mcp__") {
                // MCP 工具专用 hook 路径
            }

            // ... 执行 tool ...

            // ★ 现有:PostToolUse / PostToolUseFailure hook(line 1281-1293)
            let post_hook_result = if is_error {
                self.run_post_tool_use_failure_hook(...)
            } else {
                self.run_post_tool_use_hook(...)
            };

            // ★ P0 新增:把 tool call 进度推送给 IDE(如果 ACP 模式)
            if let Some(gateway) = &self.acp_gateway {
                flush_lane_events_to_acp(gateway, &self.session_id);
            }
        }
    }

    // ── 现有代码:Review 阶段(line 1325-1399)──
    // ...

    // ★ P0 新增:Stop hook(主循环退出后)
    let stop_ctx = HookContext::for_stop(
        &self.session_id, self.cwd.clone(), &self.hook_abort_signal
    );
    let stop_hook_result = self.hook_runner.run(HookEvent::Stop, &stop_ctx).await;
    if stop_hook_result.decision == HookDecision::Continue && !stop_hook_result.stop_hook_active {
        // 让 Claude 继续执行(可递归,需查 stop_hook_active)
        return self.run_turn(user_input, ...);
    }

    // ★ P0 新增:SubagentStop hook(子 agent 完成时,在 execute_check_subagent 中)
    // 详见 DAG 章节

    Ok(())
}
```

#### 3.3.8 配置文件示例

```json
// .claw/settings.json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          {
            "handler": {
              "type": "command",
              "command": "$CLAW_PROJECT_DIR/.claw/hooks/lint.sh",
              "timeout": "30s"
            },
            "priority": 100,
            "failure_policy": "failClose"
          }
        ],
        "execution": "sequential"
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "handler": {
              "type": "prompt",
              "prompt": "Evaluate if the task is complete: $ARGUMENTS",
              "model": "claude-haiku",
              "timeout": "60s"
            }
          }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          {
            "handler": {
              "type": "webhook",
              "url": "https://hooks.slack.com/services/...",
              "timeout": "10s"
            }
          }
        ],
        "execution": "parallel"
      }
    ]
  }
}
```

### 3.4 Hooks 系统 P0 交付物

1. `runtime/src/hooks.rs` 扩展 `HookEvent` enum 至 10 事件
2. `runtime/src/hooks.rs` 新增 `HookHandler` enum(4 handler 类型)
3. `runtime/src/hooks.rs` 新增 `Hook` trait + `HookRegistry`
4. `runtime/src/hooks.rs` 新增 `HookContext` + `HookConfig` + `HookMatcher`
5. `runtime/src/hooks.rs` 重构 `HookRunner` 为异步
6. `runtime/src/hooks.rs` 统一 `plugins/src/hooks.rs`(消除重复实现)
7. `conversation.rs::run_turn` 在 7 个位置接入新事件
8. `runtime/src/config.rs` 扩展 `RuntimeHookConfig` 支持新 schema
9. `/hooks` slash 命令实现(当前 marked "not yet implemented")
10. 单元测试 + 集成测试

---

## 四、DAG 多 Agent 编排方案

### 4.1 核心结论

**DAG 是编排层,不替代 Fork/Teammate/Worktree**。三模式保留为"执行后端",DAG 层负责:
- 拓扑调度(同层并行 + 跨层 barrier)
- 条件路由(if-else 分支)
- 故障恢复(retry / fallback / replan)
- 检查点持久化(支持 resume)

### 4.2 推荐技术栈

| 用途 | 选型 | 理由 |
|---|---|---|
| DAG 数据结构 | **`petgraph`** | 成熟,`algo::toposort` / `algo::kosaraju_scc` 开箱即用 |
| 异步并行 | **`tokio::task::JoinSet`** | 可单独 abort、可动态追加、可逐个收结果 |
| 取消传播 | **`tokio_util::sync::CancellationToken`** | 协作式取消,可 `child_token()` 形成层级 |
| 序列化 | `serde` + `serde_yaml` | 与现有 `PlanArtifact` 持久化模式一致 |

### 4.3 模块结构

```
rust/crates/runtime/src/dag/
├── mod.rs              # DagGraph + DagScheduler 主入口
├── graph.rs            # petgraph 封装 + SCC 环检测
├── node.rs             # DagNode + NodeStatus 状态机
├── scheduler.rs        # 分层并行调度(JoinSet)
├── checkpoint.rs       # 持久化 + resume
├── condition.rs        # 条件边求值
├── recovery.rs         # retry/fallback/compensation 策略
├── yaml_loader.rs      # YAML → DagGraph
├── mermaid_render.rs   # DagGraph → Mermaid(注入 prompt)
└── tests.rs
```

### 4.4 关键代码骨架

#### 4.4.1 DagNode + NodeStatus

```rust
// rust/crates/runtime/src/dag/node.rs

use crate::multi_agent::CoordinationMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeStatus {
    Pending,
    Ready,          // 依赖已满足,可执行
    Running,
    Succeeded,
    Failed,
    Skipped,        // 上游失败,级联跳过
    Cancelled,
}

impl NodeStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagNode {
    pub id: String,
    pub agent: String,              // agent 名称(对应 MultiAgentCoordinator.spawn 的 name)
    pub mode: CoordinationMode,     // Fork / Teammate / Worktree
    pub task: String,
    pub depends_on: Vec<String>,    // 上游节点 id
    pub condition: Option<String>,  // 条件边表达式(如 "result.status == 'succeeded'")

    // 验证
    pub verify_command: Option<String>,

    // 重试策略
    #[serde(default)]
    pub retry: RetryPolicy,

    // 资源控制
    #[serde(default = "default_node_timeout")]
    pub timeout_secs: u64,
    pub workdir: Option<String>,    // Worktree 模式专用

    // 记忆访问(MIRIX 启发)
    #[serde(default)]
    pub memory_access: MemoryAccess,

    // 运行时状态(不序列化)
    #[serde(skip)]
    pub status: NodeStatus,
    #[serde(skip)]
    pub attempts: u32,
    #[serde(skip)]
    pub result: Option<NodeResult>,
    #[serde(skip)]
    pub started_at_ms: Option<u64>,
    #[serde(skip)]
    pub completed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_backoff")]
    pub backoff: BackoffStrategy,
    pub fallback_agent: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum BackoffStrategy {
    #[default]
    Fixed { base_secs: u64 },
    Exponential { base_secs: u64, max_secs: u64 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryAccess {
    pub read: Vec<MemoryType>,
    pub write: Vec<MemoryType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    Notebook,       // 过程记忆
    Semantic,       // 语义记忆
    Episodic,       // 情节记忆
    Archive,        // 资源记忆(ToolResultArchive)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub status: NodeStatus,
    pub summary: String,
    pub refs: Vec<String>,  // 文件引用(Anthropic filesystem pattern)
    pub tokens_used: u64,
    pub error: Option<String>,
}

fn default_node_timeout() -> u64 { 300 }
fn default_max_attempts() -> u32 { 2 }
fn default_backoff() -> BackoffStrategy {
    BackoffStrategy::Exponential { base_secs: 5, max_secs: 60 }
}
```

#### 4.4.2 DagGraph(petgraph 封装)

```rust
// rust/crates/runtime/src/dag/graph.rs

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::algo::{kosaraju_scc, toposort};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct DagGraph {
    /// petgraph 内部表示(node_id → NodeIndex 映射)
    graph: DiGraph<DagNode, ()>,
    node_map: HashMap<String, NodeIndex>,
    /// DAG 元信息
    pub id: String,
    pub task_summary: String,
    pub max_parallelism: usize,
    pub token_budget: u64,
    pub timeout_secs: u64,
    pub on_failure: DagFailurePolicy,
    pub checkpoint_policy: CheckpointPolicy,
    pub replan_count: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DagFailurePolicy {
    #[default]
    RetryThenEscalate,
    Retry,
    Fallback,
    Abort,
    Escalate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CheckpointPolicy {
    #[default]
    EveryNode,
    OnFailure,
    None,
}

impl DagGraph {
    /// 构造 DAG 并验证(环检测)
    pub fn new(
        id: String,
        task_summary: String,
        nodes: Vec<DagNode>,
        config: DagConfig,
    ) -> Result<Self, DagError> {
        let mut graph = DiGraph::new();
        let mut node_map = HashMap::new();

        // 添加节点
        for mut node in nodes {
            node.status = NodeStatus::Pending;
            let idx = graph.add_node(node.clone());
            node_map.insert(node.id.clone(), idx);
        }

        // 添加边(依赖关系)
        for node in &graph.nodes() {
            for dep_id in &node.depends_on {
                let dep_idx = node_map.get(dep_id)
                    .ok_or(DagError::MissingDependency(dep_id.clone()))?;
                graph.add_edge(*dep_idx, node_map[&node.id], ());
            }
        }

        let dag = Self {
            graph,
            node_map,
            id,
            task_summary,
            max_parallelism: config.max_parallelism,
            token_budget: config.token_budget,
            timeout_secs: config.timeout_secs,
            on_failure: config.on_failure,
            checkpoint_policy: config.checkpoint_policy,
            replan_count: 0,
        };

        // 环检测(SCC > 1 节点即拒绝)
        dag.validate_acyclic()?;

        Ok(dag)
    }

    /// 环检测:用 Kosaraju SCC 算法
    fn validate_acyclic(&self) -> Result<(), DagError> {
        let sccs = kosaraju_scc(&self.graph);
        for scc in sccs {
            if scc.len() > 1 {
                let cycle_nodes: Vec<String> = scc.iter()
                    .filter_map(|idx| self.graph.node_weight(*idx).map(|n| n.id.clone()))
                    .collect();
                return Err(DagError::CycleDetected(cycle_nodes));
            }
        }
        Ok(())
    }

    /// 获取所有就绪节点(依赖已满足 + Pending)
    pub fn ready_nodes(&self) -> Vec<&DagNode> {
        self.graph.node_indices()
            .filter_map(|idx| {
                let node = self.graph.node_weight(idx)?;
                if node.status != NodeStatus::Pending { return None; }
                if self.all_deps_succeeded(&node.id) { Some(node) } else { None }
            })
            .collect()
    }

    /// 检查节点所有依赖是否已成功
    fn all_deps_succeeded(&self, node_id: &str) -> bool {
        let idx = self.node_map[node_id];
        self.graph.neighbors_directed(idx, petgraph::Direction::Incoming)
            .all(|dep_idx| {
                self.graph.node_weight(dep_idx)
                    .map(|n| n.status == NodeStatus::Succeeded)
                    .unwrap_or(false)
            })
    }

    /// 标记节点状态
    pub fn mark_status(&mut self, node_id: &str, status: NodeStatus) {
        if let Some(idx) = self.node_map.get(node_id) {
            if let Some(node) = self.graph.node_weight_mut(*idx) {
                node.status = status;
            }
        }
    }

    /// 拓扑排序(用于线性化展示)
    pub fn topological_order(&self) -> Vec<&DagNode> {
        toposort(&self.graph, None)
            .ok()
            .map(|indices| {
                indices.iter()
                    .filter_map(|idx| self.graph.node_weight(*idx))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 渲染为 Mermaid(注入 prompt 给主 agent 看)
    pub fn render_mermaid(&self) -> String {
        let mut out = String::from("graph LR\n");
        for idx in self.graph.node_indices() {
            if let Some(node) = self.graph.node_weight(idx) {
                out.push_str(&format!("    {}[\"{}\"]\n", node.id, node.task.chars().take(30).collect::<String>()));
            }
        }
        for edge in self.graph.edge_indices() {
            if let Some((src, dst)) = self.graph.edge_endpoints(edge) {
                if let (Some(src_node), Some(dst_node)) = (self.graph.node_weight(src), self.graph.node_weight(dst)) {
                    out.push_str(&format!("    {} --> {}\n", src_node.id, dst_node.id));
                }
            }
        }
        out
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("missing dependency: {0}")]
    MissingDependency(String),
    #[error("cycle detected: {0:?}")]
    CycleDetected(Vec<String>),
    #[error("node not found: {0}")]
    NodeNotFound(String),
    #[error("yaml parse error: {0}")]
    YamlParse(String),
}
```

#### 4.4.3 DagScheduler(分层并行调度)

```rust
// rust/crates/runtime/src/dag/scheduler.rs

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use crate::multi_agent::MultiAgentCoordinator;
use crate::recovery_orchestrator::RecoveryOrchestrator;
use super::graph::{DagGraph, NodeStatus};
use super::node::NodeResult;

pub struct DagScheduler {
    dag: DagGraph,
    coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    recovery: Arc<Mutex<RecoveryOrchestrator>>,
    cancel_token: CancellationToken,
    checkpoint_store: CheckpointStore,
}

impl DagScheduler {
    pub async fn run(&mut self) -> Result<DagRunResult, DagError> {
        let mut total_tokens = 0u64;
        let dag_deadline = tokio::time::Instant::now() + Duration::from_secs(self.dag.timeout_secs);

        loop {
            // 1. 收集就绪节点
            let ready: Vec<_> = self.dag.ready_nodes().into_iter().cloned().collect();
            if ready.is_empty() {
                // 检查是否全部完成
                if self.dag.all_terminal() {
                    return Ok(self.build_result(total_tokens));
                }
                // 死锁检测(无就绪节点但仍有非终态节点)
                return Err(DagError::Deadlock);
            }

            // 2. 同层并行执行(JoinSet)
            let mut joinset = JoinSet::new();
            for node in ready.iter().take(self.dag.max_parallelism) {
                let node_cancel = self.cancel_token.child_token();
                let coordinator = self.coordinator.clone();
                let node = node.clone();
                joinset.spawn(async move {
                    run_node(&node, coordinator, node_cancel).await
                });
            }

            // 3. 逐个收集结果
            while let Some(res) = joinset.join_next().await {
                let node_result = res.map_err(|e| DagError::NodePanic(e.to_string()))??;
                total_tokens += node_result.tokens_used;

                // 4. 检查点持久化
                if self.dag.checkpoint_policy != CheckpointPolicy::None {
                    self.checkpoint_store.save_node(&self.dag.id, &node_result).await?;
                }

                // 5. 条件边求值 + 状态更新
                if node_result.status == NodeStatus::Succeeded {
                    self.dag.mark_status(&node_result.id, NodeStatus::Succeeded);
                    self.evaluate_downstream_conditions(&node_result)?;
                } else {
                    // 失败处理
                    let should_continue = self.handle_node_failure(&node_result).await?;
                    if !should_continue {
                        // 级联跳过下游
                        self.cascade_skip(&node_result.id);
                    }
                }
            }

            // 6. DAG 级超时检查
            if tokio::time::Instant::now() >= dag_deadline {
                self.cancel_token.cancel();
                return Err(DagError::Timeout);
            }
        }
    }

    /// 节点失败处理:retry / fallback / replan
    async fn handle_node_failure(&mut self, result: &NodeResult) -> Result<bool, DagError> {
        let node = self.dag.get_node(&result.id)?;
        if node.attempts < node.retry.max_attempts {
            // 重试
            self.dag.mark_status(&result.id, NodeStatus::Ready);
            self.apply_backoff(&node.retry.backoff, node.attempts).await;
            return Ok(true);  // 继续调度
        }

        // fallback agent
        if let Some(fallback) = &node.retry.fallback_agent {
            // 切换 agent 重试
            self.dag.update_node_agent(&result.id, fallback);
            self.dag.mark_status(&result.id, NodeStatus::Ready);
            return Ok(true);
        }

        // 调用 RecoveryOrchestrator
        let failure_kind = WorkerFailureKind::from_node_failure(result);
        let outcome = self.recovery.lock().await.attempt(failure_kind);
        if outcome.recovered() {
            self.dag.mark_status(&result.id, NodeStatus::Ready);
            return Ok(true);
        }

        // 整 DAG replan
        if self.dag.replan_count < DEFAULT_MAX_REPLANS {
            self.dag.replan_count += 1;
            self.trigger_replan().await?;
            return Ok(true);
        }

        // 关键路径检查
        if self.is_critical_path(&result.id) {
            return Ok(false);  // 关键路径失败,DAG 终止
        }

        Ok(true)  // 非关键路径,继续其他分支
    }
}

/// 执行单个 DAG 节点
async fn run_node(
    node: &DagNode,
    coordinator: Arc<Mutex<MultiAgentCoordinator>>,
    cancel: CancellationToken,
) -> Result<NodeResult, DagError> {
    let start = std::time::Instant::now();

    // 1. 通过 MultiAgentCoordinator spawn 子 agent
    let subagent_id = {
        let mut coord = coordinator.lock().await;
        coord.spawn(&node.agent, &node.task, node.mode)
    };

    // 2. 执行子 agent turn(复用现有 execute_dispatch_subagent 逻辑)
    let result = tokio::select! {
        r = run_subagent_turn_with_cancel(&subagent_id, &node.task, cancel.clone()) => r?,
        _ = cancel.cancelled() => {
            return Ok(NodeResult {
                status: NodeStatus::Cancelled,
                summary: "cancelled".into(),
                refs: vec![],
                tokens_used: 0,
                error: Some("cancelled".into()),
            });
        }
    };

    // 3. 验证(如果有 verify_command)
    if let Some(verify_cmd) = &node.verify_command {
        let exit_code = run_verify_command(verify_cmd).await;
        if exit_code != 0 {
            return Ok(NodeResult {
                status: NodeStatus::Failed,
                summary: format!("verify failed: exit {exit_code}"),
                refs: result.refs,
                tokens_used: result.tokens_used,
                error: Some(format!("verify command failed: {verify_cmd}")),
            });
        }
    }

    // 4. 发布 LaneEvent
    LaneEvent::try_publish(
        LaneEvent::dag_node_completed(&node.id, result.status, &result.summary)
    );

    Ok(NodeResult {
        status: NodeStatus::Succeeded,
        summary: result.summary,
        refs: result.refs,
        tokens_used: result.tokens_used,
        error: None,
    })
}
```

#### 4.4.4 Plan → DAG 转换器

```rust
// rust/crates/runtime/src/dag/yaml_loader.rs

use crate::planner::{PlanArtifact, PlanStep};

impl DagGraph {
    /// 从 PlanArtifact 转换为 DagGraph
    /// 
    /// PlanArtifact 是用户意图层(线性 steps),DagGraph 是执行计划层(可并行 + 条件)。
    /// 默认转换:PlanStep → DagNode,默认线性链。
    /// 若 step.description 含"parallel"/"并行"关键词 → 同层并行。
    pub fn from_plan_artifact(artifact: &PlanArtifact) -> Result<Self, DagError> {
        let mut nodes = Vec::new();
        let mut prev_id: Option<String> = None;

        for step in &artifact.steps {
            let mut node = DagNode::from_plan_step(step);

            // 关键词检测:并行
            let is_parallel = step.description.contains("并行")
                || step.description.contains("parallel");

            if !is_parallel {
                if let Some(prev) = &prev_id {
                    node.depends_on = vec![prev.clone()];
                }
            } else {
                // 并行:继承上一个节点的依赖(同层)
                if let Some(prev) = &prev_id {
                    let prev_node = nodes.iter().find(|n| n.id == *prev).unwrap();
                    node.depends_on = prev_node.depends_on.clone();
                }
            }

            prev_id = Some(node.id.clone());
            nodes.push(node);
        }

        Self::new(
            artifact.id.clone(),
            artifact.task_summary.clone(),
            nodes,
            DagConfig::default(),
        )
    }
}

impl DagNode {
    pub fn from_plan_step(step: &PlanStep) -> Self {
        Self {
            id: step.id.clone(),
            agent: "default".to_string(),
            mode: CoordinationMode::Fork,
            task: step.description.clone(),
            depends_on: vec![],
            condition: None,
            verify_command: step.verify_command.clone(),
            retry: RetryPolicy::default(),
            timeout_secs: 300,
            workdir: None,
            memory_access: MemoryAccess::default(),
            status: NodeStatus::Pending,
            attempts: 0,
            result: None,
            started_at_ms: None,
            completed_at_ms: None,
        }
    }
}
```

#### 4.4.5 YAML DAG 定义格式

```yaml
# .claw/dags/dag-1778000000-ab12.yaml
dag:
  id: dag-1778000000-ab12
  task_summary: "重构 multi_agent 模块为 DAG 编排"
  max_parallelism: 4
  token_budget: 200000
  timeout_secs: 1800
  on_failure: retry_then_escalate
  checkpoint_policy: every_node

  nodes:
    - id: analyze
      agent: "code-analyst"
      mode: Fork
      task: "分析 multi_agent/mod.rs 现状"
      verify_command: "cargo test multi_agent -- --nocapture"
      memory_access:
        read: [notebook]
        write: [notebook]
      timeout_secs: 300
      retry:
        max_attempts: 2
        backoff:
          exponential:
            base_secs: 5
            max_secs: 60

    - id: design
      agent: "architect"
      mode: Fork
      task: "设计 DAG 数据结构"
      depends_on: [analyze]
      condition: "result.status == 'succeeded'"

    - id: impl_v1
      agent: "coder"
      mode: Worktree
      workdir: ".claw/worktrees/impl-v1"
      task: "实现 DagGraph + Scheduler"
      depends_on: [design]
      retry:
        max_attempts: 3
        fallback_agent: "coder-senior"

    - id: impl_v2
      agent: "coder"
      mode: Worktree
      workdir: ".claw/worktrees/impl-v2"
      task: "实现 Checkpointer"
      depends_on: [design]

    - id: test
      agent: "qa"
      mode: Fork
      task: "集成测试"
      depends_on: [impl_v1, impl_v2]
      verify_command: "cargo test --all"
```

#### 4.4.6 Checkpointer

```rust
// rust/crates/runtime/src/dag/checkpoint.rs

use std::path::PathBuf;
use tokio::fs;

pub struct CheckpointStore {
    workspace_root: PathBuf,
}

impl CheckpointStore {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }

    fn dag_dir(&self, dag_id: &str) -> PathBuf {
        self.workspace_root.join(".claw/dags").join(dag_id)
    }

    pub async fn save_dag(&self, dag: &DagGraph) -> Result<(), DagError> {
        let dir = self.dag_dir(&dag.id);
        fs::create_dir_all(&dir).await.ok();
        let path = dir.join("dag.yaml");
        let yaml = serde_yaml::to_string(dag).map_err(|e| DagError::YamlParse(e.to_string()))?;
        fs::write(&path, yaml).await.ok();
        Ok(())
    }

    pub async fn save_node(&self, dag_id: &str, result: &NodeResult) -> Result<(), DagError> {
        let dir = self.dag_dir(dag_id);
        fs::create_dir_all(&dir).await.ok();
        let path = dir.join(format!("nodes/{}.json", result.id));
        fs::create_dir_all(path.parent().unwrap()).await.ok();
        let json = serde_json::to_string_pretty(result).unwrap();
        fs::write(&path, json).await.ok();
        Ok(())
    }

    pub async fn load_dag(&self, dag_id: &str) -> Result<Option<DagGraph>, DagError> {
        let path = self.dag_dir(dag_id).join("dag.yaml");
        if !path.exists() { return Ok(None); }
        let yaml = fs::read_to_string(&path).await.map_err(|e| DagError::YamlParse(e.to_string()))?;
        let dag: DagGraph = serde_yaml::from_str(&yaml).map_err(|e| DagError::YamlParse(e.to_string()))?;
        Ok(Some(dag))
    }

    pub async fn resume(&self, dag_id: &str) -> Result<Option<DagGraph>, DagError> {
        let mut dag = match self.load_dag(dag_id).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        // 加载所有已完成节点的结果
        let nodes_dir = self.dag_dir(dag_id).join("nodes");
        if nodes_dir.exists() {
            for entry in fs::read_dir(&nodes_dir).await.ok() {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json") {
                    let json = fs::read_to_string(&path).await.ok().unwrap_or_default();
                    let result: NodeResult = serde_json::from_str(&json).unwrap();
                    dag.mark_status(&result.id, result.status);
                }
            }
        }
        Ok(Some(dag))
    }
}
```

#### 4.4.7 LaneEvent 扩展

```rust
// rust/crates/runtime/src/lane_events.rs(扩展 LaneEventName)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneEventName {
    // ... 现有 21 个变体 ...
    
    // ★ 新增:DAG 节点事件
    DagNodeStarted,
    DagNodeCompleted,
    DagNodeFailed,
    DagNodeSkipped,
    DagCompleted,
    DagFailed,
}

impl LaneEvent {
    pub fn dag_node_started(dag_id: &str, node_id: &str) -> Self {
        // ...
    }
    pub fn dag_node_completed(dag_id: &str, node_id: &str, status: NodeStatus) -> Self {
        // ...
    }
}
```

#### 4.4.8 新增 dag_run 工具(让 AI 能触发 DAG)

```rust
// rust/crates/rusty-claude-cli/src/plugin_state.rs(扩展)

fn build_runtime_tools() -> Vec<RuntimeToolDefinition> {
    vec![
        // ... 现有工具 ...
        RuntimeToolDefinition {
            name: "dag_run",
            description: "启动一个 DAG 多 agent 编排任务",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_summary": { "type": "string" },
                    "nodes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "agent": { "type": "string" },
                                "task": { "type": "string" },
                                "depends_on": { "type": "array", "items": { "type": "string" } },
                                "mode": { "type": "string", "enum": ["fork", "teammate", "worktree"] },
                                "verify_command": { "type": "string" }
                            },
                            "required": ["id", "agent", "task"]
                        }
                    }
                },
                "required": ["task_summary", "nodes"]
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        RuntimeToolDefinition {
            name: "dag_status",
            description: "查询 DAG 执行状态",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "dag_id": { "type": "string" }
                },
                "required": ["dag_id"]
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}
```

#### 4.4.9 run_turn 集成

```rust
// rust/crates/runtime/src/conversation.rs(扩展 tool call 循环)

// 在 line 1232-1263 的路由分支中添加:
match tool_name.as_str() {
    "dispatch_subagent" => self.execute_dispatch_subagent(input),
    "check_subagent" => self.execute_check_subagent(input),
    "notebook_update" => self.execute_notebook_update(input),
    "recall_full" => self.execute_recall_full(input),
    "session_search" => self.execute_session_search(input),

    // ★ P0 新增:DAG 工具
    "dag_run" => self.execute_dag_run(input).await,
    "dag_status" => self.execute_dag_status(input).await,
    // ...
}

async fn execute_dag_run(&mut self, input: &Value) -> Result<String, String> {
    let task_summary = input["task_summary"].as_str().ok_or("missing task_summary")?;
    let nodes: Vec<DagNode> = serde_json::from_value(input["nodes"].clone())
        .map_err(|e| format!("parse nodes failed: {e}"))?;

    // 构造 DagGraph
    let dag_id = format!("dag-{}-{:x}", chrono::Utc::now().timestamp(), rand::random::<u32>());
    let dag = DagGraph::new(dag_id.clone(), task_summary.to_string(), nodes, DagConfig::default())?;

    // 持久化
    self.checkpoint_store.save_dag(&dag).await?;

    // 启动调度器(异步)
    let mut scheduler = DagScheduler::new(
        dag,
        self.coordinator.clone(),
        self.recovery.clone(),
        self.cancel_token.clone(),
        self.checkpoint_store.clone(),
    );

    // 注入 Mermaid 渲染到 prompt(让主 agent 看到执行计划)
    let mermaid = scheduler.dag.render_mermaid();
    self.inject_context(format!("# DAG Execution Plan\n```mermaid\n{mermaid}\n```"));

    // 启动调度
    let result = scheduler.run().await?;

    // 返回结果摘要给主 agent
    Ok(serde_json::to_string_pretty(&result)?)
}
```

### 4.5 DAG 系统 P0 交付物

1. `runtime/src/dag/` 新模块(9 个文件)
2. `dag/graph.rs`:DagGraph + petgraph 封装 + SCC 环检测
3. `dag/node.rs`:DagNode + NodeStatus + RetryPolicy
4. `dag/scheduler.rs`:分层并行调度(JoinSet + CancellationToken)
5. `dag/checkpoint.rs`:持久化 + resume
6. `dag/yaml_loader.rs`:YAML → DagGraph + PlanArtifact → DagGraph 转换器
7. `dag/mermaid_render.rs`:DagGraph → Mermaid
8. `lane_events.rs` 扩展 6 个 DAG 事件
9. `plugin_state.rs` 新增 `dag_run` / `dag_status` 工具
10. `conversation.rs` 接入 DAG 工具路由
11. `/dag` slash 命令
12. 单元测试 + 集成测试

---

## 五、三模块协同设计

### 5.1 协同场景

```
[用户提交 prompt]
    │
    ▼
[Hooks: UserPromptSubmit]  ← 可阻断 / 注入上下文
    │
    ▼
[Plan: 生成 PlanArtifact]
    │
    ▼
[DAG: plan_to_dag 转换]  ← 注入 Mermaid 到 prompt
    │
    ▼
[DAG 调度:同层并行]
    │
    ├─ [Node A: dispatch_subagent]
    │      │
    │      ├─ [Hooks: PreToolUse]  ← 可阻断 / 改入参
    │      ├─ [执行 subagent turn]
    │      └─ [Hooks: PostToolUse]  ← 可改输出
    │
    ├─ [Node B: dispatch_subagent]  ← 并行
    │      └─ ...
    │
    ▼
[DAG: 节点完成 → Verify]
    │
    ├─ [Succeeded] → 推进下游
    └─ [Failed] → RecoveryOrchestrator → retry/fallback/replan
    │
    ▼
[IDE: SessionNotification 推送]  ← 流式更新给 IDE
    │
    ▼
[Hooks: Stop]  ← 可让 Claude 继续
    │
    ▼
[DAG: all_succeeded? → Completed : trigger_replan]
```

### 5.2 数据流

| 数据流 | 来源 | 目的 | 通道 |
|---|---|---|---|
| 用户 prompt | IDE | Hooks(UserPromptSubmit) | ACP session/prompt |
| Hook 决策 | Hooks | run_turn 主循环 | HookRunResult |
| PlanArtifact | Planner | DAG 转换器 | 内存 |
| DAG 节点结果 | DagScheduler | LaneEvents + CheckpointStore | 内存 + 文件 |
| LaneEvent | 内部 | IDE | SessionNotification |
| 工具调用进度 | run_turn | IDE | SessionNotification(tool_call) |
| 权限请求 | Agent | IDE | session/request_permission |

### 5.3 关键协同点

#### 5.3.1 DAG 节点触发 Hooks

```rust
// dag/scheduler.rs::run_node 内部
async fn run_node(node: &DagNode, ...) -> Result<NodeResult, DagError> {
    // ★ 触发 PreToolUse hook(子 agent dispatch 前)
    let pre_ctx = HookContext::for_pre_tool_use(
        "dispatch_subagent", &serde_json::json!({ "name": node.agent, "task": node.task }),
        session_id.clone(), cwd.clone(), abort_signal
    );
    let pre_hook = hook_runner.run(HookEvent::PreToolUse, &pre_ctx).await;
    if pre_hook.decision == HookDecision::Deny {
        return Ok(NodeResult::failed("blocked by PreToolUse hook"));
    }

    // 执行子 agent ...
    let result = run_subagent_turn(...).await?;

    // ★ 触发 SubagentStop hook
    let stop_ctx = HookContext::for_subagent_stop(
        &result.subagent_id, session_id.clone(), cwd.clone(), abort_signal
    );
    let _ = hook_runner.run(HookEvent::SubagentStop, &stop_ctx).await;

    Ok(result)
}
```

#### 5.3.2 DAG 事件推送到 IDE

```rust
// dag/scheduler.rs::run 内部
while let Some(res) = joinset.join_next().await {
    let node_result = res??;

    // ★ 发布 LaneEvent
    LaneEvent::try_publish(
        LaneEvent::dag_node_completed(&self.dag.id, &node_result)
    );

    // ★ 推送给 IDE(如果 ACP 模式)
    if let Some(gateway) = &self.acp_gateway {
        flush_lane_events_to_acp(gateway, &self.session_id);
    }
}
```

#### 5.3.3 Hooks 触发 DAG 取消

```rust
// hooks.rs::HookRunner::run
// 如果 hook 返回 Deny 且是 DAG 节点场景,可触发 DAG 取消
if hook_result.decision == HookDecision::Deny && ctx.event == HookEvent::PreToolUse {
    if let Some(dag_cancel) = ctx.dag_cancel_token {
        dag_cancel.cancel();
    }
}
```

---

## 六、分阶段实施路线图

### 6.1 Phase 1(P0,4-6 周):最小可行生产级

**目标**:三大模块核心能力落地,可端到端运行。

| 周次 | 模块 | 任务 |
|---|---|---|
| W1 | IDE | ACP 0.10.4 → 1.5 升级(`unstable-v2` opt-in) |
| W1 | Hooks | 统一 `runtime/hooks.rs` 与 `plugins/hooks.rs` |
| W1 | DAG | 新建 `runtime/src/dag/` 模块骨架 |
| W2 | IDE | 实现 `fs/read_text_file` / `fs/write_text_file` / `session/request_permission` |
| W2 | Hooks | 扩展 `HookEvent` 至 10 事件 + `HookHandler` 4 类型 |
| W2 | DAG | `dag/graph.rs` + `dag/node.rs` + SCC 环检测 |
| W3 | IDE | 激活 LaneEvent 消费者 + `flush_lane_events_to_acp` |
| W3 | Hooks | `HookRunner` 异步化 + matcher + timeout |
| W3 | DAG | `dag/scheduler.rs` 分层并行 + `dag/checkpoint.rs` |
| W4 | Hooks | `conversation.rs::run_turn` 7 个位置接入新事件 |
| W4 | DAG | `dag/yaml_loader.rs` + Plan → DAG 转换器 |
| W5 | DAG | `dag_run` / `dag_status` 工具 + LaneEvent 扩展 |
| W5 | 全部 | 集成测试 + 文档 |
| W6 | 全部 | 端到端验证 + bug 修复 |

**P0 交付物**:
- ACP 1.5 stdio server(fs/permission 完整)
- 10 事件 × 4 handler 的 Hooks 系统
- DAG 编排(同层并行 + retry + checkpoint)
- Zed 接入验证
- 单元测试 200+ / 集成测试 20+

### 6.2 Phase 2(P1,3-4 周):生产可用

| 模块 | 任务 |
|---|---|
| IDE | `session/load` / `session/resume` / `session/fork` |
| IDE | VS Code 扩展开发(薄客户端) |
| Hooks | `WebhookHook` + `PromptHook` 实现 |
| Hooks | 并行执行模式 + fail-open/fail-close |
| DAG | 条件边求值 + `fallback_agent` |
| DAG | Mermaid HTML 报告 + 可观测性 |
| 全部 | `/hooks` / `/dag` slash 命令实现 |

### 6.3 Phase 3(P2,长期):高级特性

| 模块 | 任务 |
|---|---|
| IDE | Cursor 扩展适配 |
| IDE | 远程场景支持(ACP HTTP/WebSocket) |
| Hooks | WASM sandbox 评估 |
| Hooks | Hook 性能指标(耗时/失败率) |
| DAG | SAGA 补偿事务 |
| DAG | DAG 级 replan(LLM 重生成子图) |
| DAG | 分布式调度(多机 agent) |

---

## 七、风险评估与缓解

### 7.1 技术风险

| 风险 | 概率 | 影响 | 缓解措施 |
|---|---|---|---|
| ACP 1.5 升级破坏性变更 | 高 | 高 | `unstable-v2` feature opt-in,渐进式迁移 |
| DAG 调度死锁(无就绪节点) | 中 | 高 | 死锁检测 + 超时退出 |
| Hook 同步阻塞影响性能 | 中 | 中 | P0 保留同步,P1 异步化 |
| `Rc<RefCell<...>>` 非 Send 限制 DAG 并行 | 高 | 高 | 子 agent 在独立线程 + channel 通信 |
| petgraph 版本兼容性 | 低 | 低 | 锁定版本 + 兼容性测试 |
| LaneEvent 容量溢出 | 低 | 中 | 容量保护 + 优先级丢弃 |

### 7.2 工程风险

| 风险 | 概率 | 影响 | 缓解措施 |
|---|---|---|---|
| 测试覆盖不足 | 中 | 高 | P0 强制 80% 覆盖率 + 集成测试 |
| 文档滞后 | 中 | 中 | 每个模块完成后立即更新 plan.md |
| 与现有 Plan/Verify/Recover 冲突 | 中 | 高 | DAG 作为编排层,不替代现有逻辑 |
| 配置 schema 破坏性变更 | 中 | 中 | 向后兼容 + 弃用警告 |

### 7.3 已知技术债依赖

以下技术债必须在 P0 阶段解决:

1. **`ClawAgent::cancel` 是 stub**(`agent.rs:305`)
   - DAG 节点取消依赖此功能
   - 解决方案:注入 `CancellationToken` 到 `run_turn` 主循环

2. **`MultiAgentCoordinator::start()` 无执行逻辑**(`multi_agent/mod.rs:138`)
   - DAG 调度器调用此方法但不执行
   - 解决方案:DAG 调度器直接调用 `execute_dispatch_subagent`

3. **LaneEvent 生产消费者未接入**(`lane_events.rs:1035-1042`)
   - IDE 推送依赖此通道
   - 解决方案:P0 激活 `flush_lane_events_to_acp`

4. **`/hooks` 和 `/dag` slash 命令未实现**(`app.rs:1249/1257`)
   - 用户交互入口缺失
   - 解决方案:P0 实现命令分发

---

## 八、参考论文与开源项目

### 8.1 论文

| 论文 | 关键贡献 | 应用模块 |
|---|---|---|
| **MIRIX**(arXiv:2507.07957) | 6 类记忆 + 多 agent 协调 | DAG(memory_access 字段) |
| **Anthropic Multi-Agent Research System**(2025) | Orchestrator-Worker + filesystem pattern | DAG(节点结果持久化) |
| **CompactionRL**(arXiv:2607.05378) | RL 训练压缩策略 | Hooks(PreCompact) |
| **GuardAgent**(ICML 2025,arXiv:2406.09187) | LLM 守卫 agent | Hooks(PromptHook) |
| **MetaGPT**(arXiv:2308.00352) | SOP + Role + Action | DAG(角色分工) |

### 8.2 开源项目

| 项目 | 价值 | 链接 |
|---|---|---|
| **ACP** | IDE 集成协议 | https://github.com/agentclientprotocol/agent-client-protocol |
| **LangGraph** | DAG 编排参照 | https://github.com/langchain-ai/langgraph |
| **petgraph** | DAG 数据结构 | https://github.com/petgraph/petgraph |
| **tower** | Rust 中间件 | https://github.com/tower-rs/tower |
| **Codex CLI** | Rust CLI 参照 | https://github.com/openai/codex |
| **claude-agent-sdk-typescript** | Hooks SDK 参照 | https://github.com/anthropics/claude-agent-sdk-typescript |

### 8.3 工业实践参考

| 实践 | 来源 | 应用 |
|---|---|---|
| ACP 1.5 stdio server | Zed / JetBrains / Hermes | IDE 集成 |
| 10 事件 × 4 handler | Claude Code Hooks | Hooks 系统 |
| StateGraph + checkpointing | LangGraph | DAG 编排 |
| Orchestrator-Worker + filesystem | Anthropic Research | DAG 节点结果持久化 |
| Review gate(Stop hook) | codex-plugin-cc | Hooks + DAG 协同 |

---

## 附录:关键文件路径速查

### 现有文件(扩展)

| 文件 | 用途 | 关键行号 |
|---|---|---|
| `rust/crates/claw-shell/src/agent.rs` | ClawAgent ACP 实现 | L161(prompt) / L305(cancel stub) |
| `rust/crates/runtime/src/hooks.rs` | Hooks 主实现 | L22(HookEvent) / L84(HookRunResult) / L180(HookRunner) / L338(run_commands) |
| `rust/crates/runtime/src/conversation.rs` | run_turn 主循环 | L712(pre_hook) / L1175(PreToolUse 调用) / L1281(PostToolUse 调用) / L1325(Review) |
| `rust/crates/runtime/src/multi_agent/mod.rs` | MultiAgentCoordinator | L78(struct) / L104(spawn) / L138(start stub) |
| `rust/crates/runtime/src/planner/artifact.rs` | PlanArtifact | L50(PlanStep) / L135(PlanArtifact) / L238(trigger_replan) |
| `rust/crates/runtime/src/planner/reviewer.rs` | Review 中间件 | L97(review) |
| `rust/crates/runtime/src/recovery_orchestrator.rs` | RecoveryOrchestrator | L77(attempt) |
| `rust/crates/runtime/src/lane_events.rs` | LaneEvent | L1049(sink) / L1065(try_publish) |
| `rust/crates/runtime/src/config.rs` | 配置加载 | L87(RuntimeHookConfig) / L264(ConfigLoader) |
| `rust/crates/rusty-claude-cli/src/plugin_state.rs` | 工具注册 | L528(注册入口) |
| `rust/crates/commands/src/lib.rs` | Slash 命令 | L1075(SlashCommand enum) |

### 新建文件

| 文件 | 用途 |
|---|---|
| `rust/crates/runtime/src/dag/mod.rs` | DAG 模块入口 |
| `rust/crates/runtime/src/dag/graph.rs` | DagGraph + petgraph |
| `rust/crates/runtime/src/dag/node.rs` | DagNode + NodeStatus |
| `rust/crates/runtime/src/dag/scheduler.rs` | 分层并行调度 |
| `rust/crates/runtime/src/dag/checkpoint.rs` | 持久化 + resume |
| `rust/crates/runtime/src/dag/condition.rs` | 条件边求值 |
| `rust/crates/runtime/src/dag/recovery.rs` | retry/fallback 策略 |
| `rust/crates/runtime/src/dag/yaml_loader.rs` | YAML + Plan → DAG |
| `rust/crates/runtime/src/dag/mermaid_render.rs` | Mermaid 渲染 |
| `vscode-claw-extension/` | VS Code 扩展(薄客户端) |

---

**文档完成**。本方案基于 4 份深度调研(IDE 集成 / Hooks / DAG / 现有代码接入点)综合设计,所有扩展点已精确定位到行号,可直接据此规划实现工作。
