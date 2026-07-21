# IDE 集成细化方案:ACP 1.5 升级 + ClawAgent 扩展 + LaneEvent 桥接

> 文档版本:v0.1
> 创建日期:2026-07-21
> 父文档:[ide-hooks-dag-implementation-plan.md](../ide-hooks-dag-implementation-plan.md)
> 焦点:ACP 1.5 升级路径 + ClawAgent 扩展 + LaneEvent 桥接 + VS Code 扩展骨架
> 适用对象:Claw Code v0.2.0(SHA `8af738a`)
> 调研基础:`agent-client-protocol` 0.10.4(已实现)/ 1.3.0(过渡)/ 1.5.0(目标)

---

## 目录

1. [现状审计](#一现状审计)
2. [ACP 1.5 升级路径](#二acp-15-升级路径)
3. [协议方法补齐](#三协议方法补齐)
4. [ClawAgent 扩展](#四clawagent-扩展)
5. [LaneEvent → SessionNotification 桥接](#五laneevent--sessionnotification-桥接)
6. [VS Code 扩展骨架](#六vs-code-扩展骨架)
7. [Zed 集成验证](#七zed-集成验证)
8. [实施步骤分解](#八实施步骤分解)
9. [测试矩阵](#九测试矩阵)
10. [风险与缓解](#十风险与缓解)
11. [参考链接](#十一参考链接)

---

## 一、现状审计

### 1.1 已实现基础设施

Claw Code 在 Phase A(`commit 8af738a`)已完成 ACP 0.10.4 接入层,核心代码组织如下:

| 模块 | 路径 | 职责 |
|------|------|------|
| `claw-acp` crate | [rust/crates/claw-acp/](file:///d:/claw-code-src/rust/crates/claw-acp/) | 协议层:mpsc channel + gateway 转发 + stdio 传输 |
| `claw-shell::agent` | [agent.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs) | `ClawAgent<C>` 实现 `acp::Agent` trait |
| `claw-shell::spawn` | [spawn.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/spawn.rs) | 独立线程 + `LocalSet` 启动模式 |
| `claw-shell::stdio` | [stdio.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/stdio.rs) | `run_stdio_agent` / `run_agent_on_io`(可测试核心) |
| `claw-headless` binary | [headless.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/bin/headless.rs) | 极简 stdio ACP 服务器入口,供 Zed 等 spawn |
| `runtime::lane_events` | [lane_events.rs](file:///d:/claw-code-src/rust/crates/runtime/src/lane_events.rs) | 23 种 `LaneEventName` + 全局 sink(`Mutex<Vec<LaneEvent>>`) |

### 1.2 已实现的 ACP 方法清单(0.10.4)

`ClawAgent<C>` 当前实现 [`acp::Agent`](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs) trait 的方法状态:

| 方法 | 实现状态 | 关键行为 |
|------|---------|---------|
| `initialize` | ✅ 完整 | 返回 `ProtocolVersion::LATEST` + `api_key` auth method |
| `authenticate` | ✅ 完整(伪) | 直接返回 success,无真实认证 |
| `new_session` | ✅ 完整 | 创建 `ConversationRuntime`,从 `Option` 取出 `api_client` / `tool_executor` |
| `load_session` | 🚧 stub | 返回 `method_not_found`(`acp::Error::method_not_found()`) |
| `set_session_mode` | 🚧 stub | 返回 `method_not_found` |
| `prompt` | ✅ 部分 | 调用 `run_turn` 同步阻塞,turn 完成后一次性推送 `AgentMessageChunk`(非真实流式) |
| `cancel` | 🚧 stub | 仅 `tracing::warn!`,返回 `Ok(())`,不中断 `run_turn`(同步 API 无法中断) |
| `ext_method` | 🚧 stub | 返回 `method_not_found` |
| `ext_notification` | ✅ 完整(空) | 返回 `Ok(())`,吞掉所有扩展通知 |

### 1.3 缺失的协议方法(需补齐)

以下方法在 `acp::Agent` trait 中存在,但 `ClawAgent` 未实现真实逻辑:

| 方法 | 优先级 | 缺失原因 | 影响 |
|------|--------|---------|------|
| `fs/read_text_file` | P0 | trait 属于 `acp::Client`(反向请求) | IDE 无法提供 editor buffer 未保存内容 |
| `fs/write_text_file` | P0 | 同上 | 文件写入绕过 editor undo 栈 |
| `session/request_permission` | P0 | 同上 | 危险操作无审批 UI |
| `session/load` | P1 | 当前 stub 返回 `method_not_found` | 无法恢复历史会话 |
| `session/resume` | P1 | v1.3.0+ 与 `load` 统一 | 同上 |
| `session/fork` | P2 | 未实现 | 无法从某消息分叉 |
| `session/list` | P2 | 未实现 | 无法列出可恢复会话 |
| `session/set_mode` | P2 | 当前 stub 返回 `method_not_found` | 无法切换 plan/act |
| `session/set_model` | P2 | 未实现 | 无法切换底层模型 |

### 1.4 silent drop 行为(ACP 0.10.4)

`claw-shell/src/stdio.rs` 的 A6.4 测试套件固化了 0.10.4 的三类错误路径行为:

| 错误场景 | 0.10.4 行为 | 测试名 |
|---------|------------|--------|
| invalid JSON(parse 失败) | 🚧 silent drop(仅 `log::error!`) | `run_agent_on_io_silently_drops_invalid_json` |
| 缺少 `method` 字段 | 🚧 silent drop(当作 response 找不到 id) | `run_agent_on_io_silently_drops_missing_method_field` |
| 未知 `method` 名 | ✅ 返回 `-32601` error response | `run_agent_on_io_returns_error_on_unknown_method` |

**风险**:silent drop 让 IDE 无法感知协议错误,排障困难。ACP 1.3.0+ 会返回 `-32700` parse_error,需在升级时更新测试断言。

### 1.5 LaneEvent 消费者现状

[lane_events.rs](file:///d:/claw-code-src/rust/crates/runtime/src/lane_events.rs) 中的全局 sink 当前状态:

```rust
// rust/crates/runtime/src/lane_events.rs(行 1027-1104)
//
// **架构现状(2026-07-21 阶段 3.5 评估)**:
// - 发布端已接入:4 处生产调用(SubagentHandoff × 1、SubagentResult × 2、ShipPrepared × 1)
// - 消费端:仅测试代码调用 `drain_lane_events`(5 处,均在 `#[cfg(test)]` 内)
// - 生产消费者尚未接入:TUI Sidebar 消费 ToolHistory、TraceAnalyzer 消费 TraceRecord,
//   均不订阅 LaneEvent 流
// - sink 容量上限保护(512 条):防止生产运行中无人 drain 导致内存无限增长
```

**核心问题**:生产路径下 LaneEvent 被发布到全局 sink 但无人消费,等同 fire-and-forget 日志。本分支要接入的 ACP 桥接是第一个真实生产消费者。

---

## 二、ACP 1.5 升级路径

### 2.1 版本对比表

| 维度 | 0.10.4(当前) | 1.3.0(过渡) | 1.5.0(目标) | 影响 |
|------|--------------|-------------|-------------|------|
| 错误处理 | invalid JSON silent drop | 返回 `-32700` parse_error | 同 1.3.0 | ✅ 改善(测试需更新) |
| Diff 格式 | v1(简单文本) | v1 | v2(带 location + 类型化) | 🚧 新增字段 |
| Permission 模型 | 简单 `request_permission` | v2 typed `PermissionOption` | 同 1.3.0 | 🚧 API 变更 |
| Session 配置 | 字符串自由字段 | typed boolean config | 同 1.3.0 | 🚧 破坏性 |
| Content 类型 | 自定义枚举 | 对齐 MCP `Content` | 同 1.3.0 | 🚧 类型重命名 |
| `session/load` | 单独方法 | 与 `session/resume` 统一 | 同 1.3.0 | ✅ 简化 |
| `session/fork` | 不存在 | 新增 | 同 1.3.0 | ✅ 新能力 |
| `session/set_model` | 不存在 | 新增 | 同 1.3.0 | ✅ 新能力 |
| Terminal API | 不存在 | 新增(create/release/wait) | 同 1.3.0 | ✅ 新能力 |
| feature flag | `unstable` | `unstable-v2` | 默认启用 | 🚧 配置变更 |

### 2.2 破坏性变更清单

升级到 1.5.0 需要处理的破坏性变更:

1. **`ContentBlock` 类型重命名**:`acp::ContentBlock::Text` → `acp::Content::Text`,影响 `ClawAgent::prompt` / `extract_user_text` / `notify` 三处
2. **`NewSessionRequest` 字段类型变更**:`mcp_servers: Vec<String>` → `mcp_servers: Vec<McpServerConfig>`,需更新 `new_session` 实现
3. **`SessionConfig` 强类型化**:从 `serde_json::Value` 改为 typed struct,`NewSessionRequest.session_configuration` 字段需要新结构
4. **`PermissionOption` 枚举化**:`request_permission` 的 `options` 字段从 `Vec<serde_json::Value>` 改为 `Vec<PermissionOption>`,需更新反向请求逻辑
5. **`Diff` 类型新增 `location`**:文件 diff 需要带位置信息,影响 `fs/write_text_file` 响应构造
6. **`Terminal` API 新增**:`acp::Client` trait 新增 5 个 terminal 方法(create/release/wait/output/kill),`AcpGatewaySender<acp::AgentSide>` 需实现转发(已存在,见 [gateway.rs:412-441](file:///d:/claw-code-src/rust/crates/claw-acp/src/gateway.rs))

### 2.3 兼容策略

采用**双轨升级**策略,避免一次性破坏所有调用方:

```rust
// rust/crates/claw-acp/Cargo.toml(升级后)
//
// 双 feature 共存:0.10.4 兼容(unstable)+ 1.5.0 特性(unstable-v2)
// 测试时分别用 cargo test --features unstable / --features unstable-v2 验证两套行为
[dependencies]
agent-client-protocol = { version = "1.5", features = ["unstable", "unstable-v2"] }
```

**P0 阶段**:保持 0.10.4 兼容路径(`unstable` feature),新增 1.5 特性为 opt-in(`unstable-v2`),通过运行时 capability 协商决定走哪条路径。

**P1 阶段**:全面切换到 1.5,废弃 0.10.4 兼容代码,移除 `unstable` feature。

**测试更新**:A6.4 的 3 个错误路径测试需重写(1.5 会返回 error response 而非 silent drop),保留旧测试在 `#[cfg(not(feature = "unstable-v2"))]` 下,新增 1.5 行为测试在 `#[cfg(feature = "unstable-v2")]` 下。

---

## 三、协议方法补齐

### 3.1 `initialize`

#### 请求/响应 schema

```rust
/// ACP initialize 请求 — 客户端握手第一步。
/// 
/// 客户端发送自身支持的 protocolVersion,agent 返回双方协商的版本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeRequest {
    /// 客户端期望的协议版本(1 = ACP 1.x 系列)
    pub protocol_version: acp::ProtocolVersion,
    /// 客户端进程信息(可选,用于 agent 端日志)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<acp::ClientInfo>,
    /// 客户端能力声明(fs 读写 / permission / terminal 等)
    #[serde(default)]
    pub client_capabilities: acp::ClientCapabilities,
}

/// ACP initialize 响应 — agent 返回协商结果与自身能力。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResponse {
    /// 最终协商的协议版本(取 client / agent 较小值)
    pub protocol_version: acp::ProtocolVersion,
    /// agent 支持的认证方式列表
    pub auth_methods: Vec<acp::AuthMethod>,
    /// agent 能力声明(支持哪些 session 操作)
    #[serde(default)]
    pub agent_capabilities: acp::AgentCapabilities,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::initialize)
//
// 当前实现已基本可用,需补充 agent_capabilities 字段以声明 1.5 新能力。
// 当 unstable-v2 feature 启用时,告知客户端我们支持 fs/read_text_file 等。

#[async_trait(?Send)]
impl<C> acp::Agent for ClawAgent<C>
where
    C: ApiClient + 'static,
{
    async fn initialize(
        &self,
        _arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        tracing::debug!("claw-agent: initialize");
        let mut resp = acp::InitializeResponse::new(acp::ProtocolVersion::LATEST);
        // 暴露 api_key auth method(本地无真实认证)
        resp.auth_methods = vec![acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
            acp::AuthMethodId::new("api_key"),
            "API Key",
        ))];
        // P0:声明 agent 能力,让 IDE 知道我们支持哪些操作
        // fs_read_text_file / fs_write_text_file / request_permission 走反向请求
        // session/load / session/fork 在 P1 阶段开启
        resp.agent_capabilities = acp::AgentCapabilities {
            fs_read_text_file: true,
            fs_write_text_file: true,
            session_request_permission: true,
            // P1 阶段开启:
            session_load: false,
            session_fork: false,
            // P2 阶段开启:
            session_set_mode: false,
            session_set_model: false,
        };
        Ok(resp)
    }
    // ... 其他方法见后续章节
}
```

#### 错误处理行为

- `protocol_version` 不支持时返回 `-32090` unsupported protocol version
- 当前实现无错误路径(直接接受 LATEST)

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `initialize_returns_latest_protocol_version` | `resp.protocol_version == LATEST` |
| `initialize_exposes_api_key_auth_method` | `auth_methods` 非空,含 `api_key` |
| `initialize_declares_fs_capabilities` | `agent_capabilities.fs_read_text_file == true` |

---

### 3.2 `session/new`

#### 请求/响应 schema

```rust
/// ACP session/new 请求 — 创建新会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionRequest {
    /// 工作目录(必须存在)
    pub cwd: acp::Path,
    /// 初始化 prompt(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<Vec<acp::ContentBlock>>,
    /// MCP 服务器配置(1.5:typed,0.10.4:String)
    #[serde(default)]
    pub mcp_servers: Vec<acp::McpServerConfig>,
    /// 会话配置(1.5:typed SessionConfig)
    #[serde(default)]
    pub session_configuration: acp::SessionConfiguration,
    /// 权限模式(可选覆盖)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<acp::PermissionMode>,
}

/// ACP session/new 响应 — 返回新建会话 ID。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionResponse {
    /// 新会话的唯一标识(由 agent 分配)
    pub session_id: acp::SessionId,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::new_session)
//
// 当前实现已可用,需补充:
// 1. 持久化 session_id 到 SessionStore(供 load_session 使用)
// 2. 处理 initial_prompt(如果有,立即触发一轮 turn)
// 3. 处理 mcp_servers 配置(启动 MCP 子进程)

async fn new_session(
    &self,
    arguments: acp::NewSessionRequest,
) -> Result<acp::NewSessionResponse, acp::Error> {
    tracing::debug!("claw-agent: new_session cwd={:?}", arguments.cwd);
    // 取出 api_client 和 tool_executor(从 Option 中移出)
    let api_client = self.api_client.borrow_mut().take().ok_or_else(|| {
        acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            "api_client already consumed by a previous session",
        )
    })?;
    let tool_executor = self.tool_executor.borrow_mut().take().ok_or_else(|| {
        acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            "tool_executor already consumed by a previous session",
        )
    })?;
    // 构造 Session + Runtime
    let mut session = Session::new().with_workspace_root(arguments.cwd.clone());
    // P1:持久化 session 元数据到 SessionStore
    // session.persist_to_store(&self.session_store)?;
    // P0:处理 mcp_servers 配置(启动子进程)
    for mcp_config in &arguments.mcp_servers {
        tracing::info!("claw-agent: starting mcp server {}", mcp_config.name);
        // session.start_mcp_server(mcp_config.clone()).await?;
    }
    let session_id = acp::SessionId::new(session.session_id.clone());
    let runtime = ConversationRuntime::new(
        session,
        api_client,
        tool_executor,
        self.config.permission_policy.clone(),
        self.config.system_prompt.clone(),
    );
    *self.runtime.borrow_mut() = Some(runtime);
    // P0:如果有 initial_prompt,立即触发一轮 turn(异步任务)
    if let Some(prompt) = arguments.initial_prompt {
        let user_input = Self::extract_user_text(&prompt);
        // 异步触发,不阻塞 new_session 响应
        // self.spawn_initial_turn(session_id.clone(), user_input);
        tracing::debug!("claw-agent: initial_prompt queued: {} chars", user_input.len());
    }
    Ok(acp::NewSessionResponse::new(session_id))
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| `api_client` 已被消费 | `InternalError` | 一个 agent 实例只能 new_session 一次 |
| `cwd` 不存在 | `InvalidParams`(-32602) | 启动前校验 |
| MCP 服务器启动失败 | `InternalError` | 包含失败的服务器名 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `new_session_returns_unique_session_id` | session_id 非空且为字符串 |
| `new_session_consumes_api_client` | 第二次 new_session 返回 InternalError |
| `new_session_with_invalid_cwd_returns_error` | 不存在的 cwd 返回 -32602 |
| `new_session_with_mcp_servers_starts_subprocesses` | mcp_servers 非空时启动子进程 |

---

### 3.3 `session/prompt`

#### 请求/响应 schema

```rust
/// ACP session/prompt 请求 — 发起一轮对话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRequest {
    /// 目标会话 ID(必须已 new_session)
    pub session_id: acp::SessionId,
    /// 用户输入(支持多 ContentBlock)
    pub prompt: Vec<acp::ContentBlock>,
    /// 关联的 agent 思考 ID(用于思维链展示)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_thought_id: Option<acp::AgentThoughtId>,
    /// 是否启用 thinking 模式(1.5 新字段)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<acp::ThinkingMode>,
    /// 上下文模式(1.5:控制历史消息包含范围)
    #[serde(default)]
    pub context_mode: acp::ContextMode,
}

/// ACP session/prompt 响应 — 返回本轮 stop reason。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptResponse {
    /// 停止原因(EndTurn / ToolUse / MaxTokens / Cancelled 等)
    pub stop_reason: acp::StopReason,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::prompt)
//
// P0 改进:在 run_turn 主循环中插入 LaneEvent flush 钩子,
// 让 IDE 实时收到子 agent 启动 / 工具调用进度等通知。

async fn prompt(
    &self,
    arguments: acp::PromptRequest,
) -> Result<acp::PromptResponse, acp::Error> {
    tracing::debug!("claw-agent: prompt session_id={:?}", arguments.session_id.0);
    // 取出 runtime(短暂持有 RefCell 借用)
    let runtime_opt = self.runtime.borrow_mut().take();
    let mut runtime_rc = runtime_opt.ok_or_else(|| {
        acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            "no active session: call new_session first",
        )
    })?;
    let session_id = arguments.session_id.clone();
    let user_input = Self::extract_user_text(&arguments.prompt);
    // P0:在 run_turn 前 flush 一次 LaneEvent,确保 IDE 看到上一轮遗留事件
    Self::flush_lane_events_to_acp(&self.client_gateway, &session_id);
    // run_turn 是同步阻塞 API。
    // current_thread + LocalSet 下直接同步调用:会阻塞 LocalSet 直到 turn 完成,
    // 但不会死锁(没有其他 task 需要并发,且 channel 发送是非阻塞的)。
    let turn_result = runtime_rc.run_turn(user_input, None);
    // P0:run_turn 完成后再次 flush,把本轮 LaneEvent 推送给 IDE
    Self::flush_lane_events_to_acp(&self.client_gateway, &session_id);
    let turn_summary = match turn_result {
        Ok(summary) => summary,
        Err(e) => {
            *self.runtime.borrow_mut() = Some(runtime_rc);
            return Err(acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                e.to_string(),
            ));
        }
    };
    // 推流 assistant 文本(本期简化:turn 完成后一次性推送,非真实流式)
    let text = Self::extract_assistant_text(&runtime_rc);
    if !text.is_empty() {
        self.notify(
            &session_id,
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                acp::ContentBlock::Text(acp::TextContent::new(text)),
            )),
        );
    }
    // 推送 tool call 通知(简化:仅记日志)
    for tool_msg in &turn_summary.tool_results {
        for block in &tool_msg.blocks {
            if let runtime::ContentBlock::ToolResult { tool_name, .. } = block {
                tracing::debug!("claw-agent: tool result for {}", tool_name);
            }
        }
    }
    // runtime 放回
    *self.runtime.borrow_mut() = Some(runtime_rc);
    Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| 无活跃 session | `InternalError` | 未先调用 new_session |
| `run_turn` 内部失败 | `InternalError` | 错误信息透传到 `acp::Error.message` |
| API 调用超时 | `InternalError` | 当前 run_turn 是同步,无法 cancel,需 P1 改造 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `prompt_returns_end_turn_stop_reason` | `stop_reason == EndTurn` |
| `prompt_without_session_returns_error` | 未 new_session 返回 InternalError |
| `prompt_pushes_agent_message_chunk` | 收到至少一条 AgentMessageChunk notification |
| `prompt_flushes_lane_events_before_and_after` | SubagentHandoff 事件被转换为 ToolCall notification |

---

### 3.4 `session/cancel`

#### 请求/响应 schema

```rust
/// ACP session/cancel 通知 — 取消正在进行的 prompt。
/// 
/// 这是 notification(not request),无响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelNotification {
    /// 要取消的会话 ID
    pub session_id: acp::SessionId,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::cancel)
//
// P0:仍是 stub(同步 run_turn 无法中断)
// P1:改造 run_turn 为 async + CancellationToken 后实现真实取消

async fn cancel(&self, _arguments: acp::CancelNotification) -> Result<(), acp::Error> {
    // P0:记录日志,返回 Ok(固化为 stub 契约)
    // P1:触发 CancellationToken,让 run_turn 退出
    tracing::warn!("claw-agent: cancel not yet implemented (sync run_turn)");
    // P1 实现草图:
    // if let Some(token) = self.cancel_token.borrow().as_ref() {
    //     token.cancel();
    //     tracing::info!("claw-agent: cancellation triggered");
    // }
    Ok(())
}

/// P1:在 ClawAgent 中新增 cancel_token 字段
/// 
/// 与 run_turn 改造同步进行。run_turn 接受 CancellationToken 参数,
/// 在 API 调用 / tool 执行循环中检查 token.is_cancelled()。
pub struct ClawAgent<C>
where
    C: ApiClient + 'static,
{
    // ... 现有字段 ...
    /// P1:取消令牌,在 new_session 时创建,prompt 时传入 run_turn
    cancel_token: RefCell<Option<tokio_util::sync::CancellationToken>>,
}
```

#### 错误处理行为

- 当前无错误路径,直接返回 `Ok(())`
- P1 实现后,若 session 不存在应返回 `InternalError`

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `cancel_notification_returns_ok_without_active_prompt` | 现有测试,固化 stub 契约 |
| `cancel_with_invalid_session_returns_error`(P1) | 不存在的 session_id 返回 InternalError |
| `cancel_interrupts_active_prompt`(P1) | prompt 中途 cancel 后 prompt 返回 `StopReason::Cancelled` |

---

### 3.5 `session/update`(反向请求)

`session/update` 是 IDE → agent 的通知,用于 IDE 推送状态变更(如用户切换 tab、激活文件等)。当前 `ext_notification` 路径已能吞掉,但 P1 应提供专用处理。

#### 请求/响应 schema

```rust
/// ACP session/update 通知 — IDE 推送会话状态变更。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUpdateNotification {
    /// 目标会话 ID
    pub session_id: acp::SessionId,
    /// 变更类型(ActiveTabChanged / FileOpened / SelectionChanged 等)
    pub update: acp::SessionUpdateFromClient,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P1 新增方法)
//
// 处理 IDE 推送的状态变更。当前实现仅记日志,
// 后续可用于上下文注入(打开的文件 → 自动加入 system prompt)。

async fn session_update(
    &self,
    arguments: acp::SessionUpdateNotification,
) -> Result<(), acp::Error> {
    tracing::debug!(
        "claw-agent: session_update session_id={:?} update={:?}",
        arguments.session_id.0,
        arguments.update
    );
    // P1:根据 update 类型更新上下文
    match arguments.update {
        acp::SessionUpdateFromClient::ActiveTabChanged { path } => {
            tracing::debug!("claw-agent: active tab changed to {}", path);
            // self.context_assembler.set_active_file(&path);
        }
        acp::SessionUpdateFromClient::SelectionChanged { path, range } => {
            tracing::debug!(
                "claw-agent: selection changed in {} at {:?}",
                path, range
            );
            // self.context_assembler.set_selection(&path, range);
        }
        _ => {
            tracing::debug!("claw-agent: unhandled session update: {:?}", arguments.update);
        }
    }
    Ok(())
}
```

---

### 3.6 `fs/read_text_file`

#### 请求/响应 schema

```rust
/// ACP fs/read_text_file 请求 — agent 反向请求 IDE 读取文件。
/// 
/// 与直接读磁盘的区别:能拿到 editor buffer 中未保存的内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadTextFileRequest {
    /// 要读取的文件路径(相对于 workspace root)
    pub path: acp::Path,
}

/// ACP fs/read_text_file 响应 — 返回文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadTextFileResponse {
    /// 文件文本内容(UTF-8)
    pub content: String,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)
//
// 委托给 client_gateway 反向请求 IDE。
// 走 AcpGatewaySender<acp::AgentSide>::send 路径(已有 forward 实现)。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:读 editor buffer(含未保存内容)
    /// 
    /// ACP 1.5 fs/read_text_file 方法实现。
    /// 委托给 AcpGatewaySender 反向请求 editor。
    /// 
    /// 使用场景:tool 执行 Read 工具时,优先调此方法拿 editor buffer,
    /// 而非直接 fs::read_to_string(后者拿不到未保存内容)。
    pub async fn read_editor_buffer(&self, path: &str) -> Result<String, acp::Error> {
        let request = acp::ReadTextFileRequest {
            path: acp::Path::new(path.to_string()),
        };
        let response = self
            .client_gateway
            .send(request)
            .await
            .map_err(|e| acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("fs/read_text_file failed: {e}"),
            ))?;
        Ok(response.content)
    }
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| IDE 未注册 `fs/read_text_file` handler | `MethodNotFound`(-32601) | capability 协商失败 |
| 文件不存在 | `InvalidParams`(-32602) | IDE 端校验 |
| 路径越权(workspace 外) | `InvalidParams` | 安全保护 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `read_editor_buffer_returns_content` | mock IDE 返回 "hello",断言相等 |
| `read_editor_buffer_propagates_method_not_found` | mock IDE 不注册 handler,断言 -32601 |
| `read_editor_buffer_rejects_out_of_workspace_path` | 路径在 workspace 外时返回 -32602 |

---

### 3.7 `fs/write_text_file`

#### 请求/响应 schema

```rust
/// ACP fs/write_text_file 请求 — agent 反向请求 IDE 写文件。
/// 
/// 走 editor undo 栈,用户可 Ctrl+Z 撤销。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTextFileRequest {
    /// 要写入的文件路径
    pub path: acp::Path,
    /// 文件内容(整体覆盖)
    pub content: String,
    /// 1.5 新增:diff 格式(替代整体覆盖,更高效)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<acp::Diff>,
}

/// ACP fs/write_text_file 响应 — 返回写入结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTextFileResponse {
    /// 是否成功写入(true / false)
    pub written: bool,
    /// 1.5 新增:用户拒绝原因(如选中了 "Deny")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denial_reason: Option<String>,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:写文件走 editor undo 栈
    /// 
    /// 与 fs::write 的区别:走 editor undo 栈,用户可 Ctrl+Z 撤销。
    /// 用于 tool 执行 Write / Edit 工具时。
    pub async fn write_editor_buffer(
        &self,
        path: &str,
        content: &str,
    ) -> Result<bool, acp::Error> {
        let request = acp::WriteTextFileRequest {
            path: acp::Path::new(path.to_string()),
            content: content.to_string(),
            diff: None, // P1:支持 diff 格式
        };
        let response = self
            .client_gateway
            .send(request)
            .await
            .map_err(|e| acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("fs/write_text_file failed: {e}"),
            ))?;
        if !response.written {
            tracing::warn!(
                "claw-agent: write denied by user for path {}: {:?}",
                path,
                response.denial_reason
            );
        }
        Ok(response.written)
    }

    /// P1:写文件 with diff(更高效,只传变更部分)
    pub async fn write_editor_buffer_with_diff(
        &self,
        path: &str,
        diff: acp::Diff,
    ) -> Result<bool, acp::Error> {
        let request = acp::WriteTextFileRequest {
            path: acp::Path::new(path.to_string()),
            content: String::new(), // diff 模式下 content 可空
            diff: Some(diff),
        };
        let response = self.client_gateway.send(request).await.map_err(|e| {
            acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("fs/write_text_file (diff) failed: {e}"),
            )
        })?;
        Ok(response.written)
    }
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| 用户拒绝写入 | `written: false` | 不算错误,agent 应感知并回退 |
| 路径越权 | `InvalidParams` | 同 read |
| editor 冲突(并发写) | `InternalError` | 1.5 新增错误码 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `write_editor_buffer_returns_written_true` | mock IDE 接受,断言 `written: true` |
| `write_editor_buffer_returns_false_when_user_denies` | mock IDE 返回 `written: false` |
| `write_editor_buffer_with_diff_uses_diff_field`(P1) | diff 字段被传递到 IDE |

---

### 3.8 `session/request_permission`

#### 请求/响应 schema

```rust
/// ACP session/request_permission 请求 — agent 反向请求用户审批。
/// 
/// 用于危险操作(Bash rm -rf / Write 覆盖 / 网络请求等)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPermissionRequest {
    /// 目标会话 ID
    pub session_id: acp::SessionId,
    /// 触发审批的 tool 名称(Bash / Write / Read 等)
    pub tool_name: String,
    /// tool 输入参数(用于在 UI 展示)
    pub tool_input: serde_json::Value,
    /// 1.5:typed 权限选项(allow / deny / always_allow 等)
    pub options: Vec<acp::PermissionOption>,
}

/// ACP session/request_permission 响应 — 用户选择。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPermissionResponse {
    /// 用户选择的选项 ID
    pub outcome: acp::PermissionOutcome,
    /// 用户选了 "always" 时的过期时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<acp::Timestamp>,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)
//
// 替代当前 ClawAgent::cancel 的 stub(后者实际上是错误用法,应在 prompt 中调此方法)。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:请求权限(替代当前 stub cancel)
    /// 
    /// ACP 1.5 session/request_permission 反向请求。
    /// 用于危险操作(Bash rm -rf / Write 覆盖)审批。
    /// 
    /// 在 tool 执行前调用,根据 outcome 决定是否继续。
    pub async fn request_permission(
        &self,
        session_id: &acp::SessionId,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<acp::PermissionOutcome, acp::Error> {
        let request = acp::RequestPermissionRequest {
            session_id: session_id.clone(),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            options: vec![
                acp::PermissionOption::Allow,
                acp::PermissionOption::Deny,
                acp::PermissionOption::AlwaysAllow,
            ],
        };
        let response = self
            .client_gateway
            .send(request)
            .await
            .map_err(|e| acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("session/request_permission failed: {e}"),
            ))?;
        Ok(response.outcome)
    }
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| IDE 未注册 handler | `MethodNotFound` | 同 fs/* |
| 用户长时间未响应 | `InternalError` | 超时(默认 60s) |
| IDE 返回未知 outcome | `InternalError` | 协议错误 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `request_permission_returns_allow_when_user_approves` | mock IDE 返回 Allow,断言相等 |
| `request_permission_returns_deny_when_user_rejects` | mock IDE 返回 Deny |
| `request_permission_returns_always_allow_with_expiry` | AlwaysAllow 时 expires_at 非空 |
| `request_permission_times_out_after_60s` | mock IDE 不响应,60s 后返回 InternalError |

---

## 四、ClawAgent 扩展

### 4.1 现有结构分析

当前 `ClawAgent<C>` 定义见 [agent.rs:40-56](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs):

```rust
pub struct ClawAgent<C>
where
    C: ApiClient + 'static,
{
    runtime: RefCell<Option<ConversationRuntime<C, StaticToolExecutor>>>,
    config: ClawAgentConfig,
    api_client: RefCell<Option<C>>,
    tool_executor: RefCell<Option<StaticToolExecutor>>,
    client_gateway: AcpGatewaySender<acp::AgentSide>,
}
```

**关键约束**(来自项目记忆):

1. `runtime` 是 `RefCell<Option<...>>` 而非 `Rc<RefCell<...>>`,因 `new_session` 时整体替换
2. `api_client` / `tool_executor` 用 `Option` 包装,因 `new_session` 时 move 进 runtime
3. `client_gateway` 是 `AcpGatewaySender<acp::AgentSide>`,实现 `Clone`(mpsc sender clone)
4. `ClawAgent` 持有 `Rc<RefCell<...>>` 间接非 Send 类型(通过 `StaticToolExecutor` 内部 `Box<dyn FnMut>`),必须 `current_thread + LocalSet` 上运行

### 4.2 扩展字段设计

```rust
// rust/crates/claw-shell/src/agent.rs(P0 + P1 扩展)
//
// 新增字段:
// - session_store:P1 支持 load_session / session_list
// - cancel_token:P1 支持 cancel 中断 run_turn
// - session_id:跟踪当前活跃 session_id(用于 notification 路由)
// - permission_cache:缓存 AlwaysAllow 决策,避免重复请求

use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

/// P1:会话持久化存储(线程安全,可跨 agent 实例共享)
pub struct SessionStore {
    /// session_id → 序列化状态
    sessions: std::sync::Mutex<HashMap<String, SerializedSession>>,
}

/// P1:序列化的会话状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedSession {
    pub session_id: String,
    pub cwd: String,
    pub mode: acp::SessionMode,
    pub mcp_servers: Vec<acp::McpServerConfig>,
    pub created_at: String,
    pub messages: Vec<runtime::Message>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn save(&self, session: &SerializedSession) -> Result<(), String> {
        let mut map = self.sessions.lock().map_err(|e| e.to_string())?;
        map.insert(session.session_id.clone(), session.clone());
        Ok(())
    }

    pub fn load(&self, session_id: &str) -> Option<SerializedSession> {
        let map = self.sessions.lock().ok()?;
        map.get(session_id).cloned()
    }

    pub fn list(&self) -> Vec<SerializedSession> {
        let map = self.sessions.lock().ok()?;
        map.values().cloned().collect()
    }
}

pub struct ClawAgent<C>
where
    C: ApiClient + 'static,
{
    // 现有字段(保留)
    runtime: RefCell<Option<ConversationRuntime<C, StaticToolExecutor>>>,
    config: ClawAgentConfig,
    api_client: RefCell<Option<C>>,
    tool_executor: RefCell<Option<StaticToolExecutor>>,
    client_gateway: AcpGatewaySender<acp::AgentSide>,

    // P1 新增字段
    /// 当前活跃 session_id(用于 notification 路由与 cancel 定位)
    current_session_id: RefCell<Option<acp::SessionId>>,
    /// 取消令牌:new_session 时创建,prompt 时传入 run_turn
    cancel_token: RefCell<Option<CancellationToken>>,
    /// 会话持久化存储(支持 load_session / session_list)
    session_store: std::rc::Rc<SessionStore>,
    /// 权限缓存:AlwaysAllow 决策缓存,避免重复请求
    permission_cache: RefCell<HashMap<String, acp::PermissionOutcome>>,
}
```

### 4.3 async fn 方法签名

```rust
// rust/crates/claw-shell/src/agent.rs(P0 + P1 新增方法)
//
// 所有新方法都是 ?Send async(因 ClawAgent 持有非 Send 类型),
// 必须在 LocalSet 上调用。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:读 editor buffer(见 §3.6)
    pub async fn read_editor_buffer(&self, path: &str) -> Result<String, acp::Error>;

    /// P0:写文件走 editor undo 栈(见 §3.7)
    pub async fn write_editor_buffer(&self, path: &str, content: &str) -> Result<bool, acp::Error>;

    /// P0:请求权限(见 §3.8)
    pub async fn request_permission(
        &self,
        session_id: &acp::SessionId,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<acp::PermissionOutcome, acp::Error>;

    /// P0:flush LaneEvent 到 ACP SessionNotification
    /// 
    /// 在 run_turn 主循环前后调用,把内部事件流推送给 IDE。
    /// 走 fire-and-forget 路径,不阻塞 turn。
    pub fn flush_lane_events_to_acp(
        gateway: &AcpGatewaySender<acp::AgentSide>,
        session_id: &acp::SessionId,
    ) {
        let events = runtime::drain_lane_events();
        for event in events {
            if let Some(notification) = Self::lane_event_to_session_update(&event) {
                let notif = acp::SessionNotification::new(
                    session_id.clone(),
                    notification,
                );
                // fire-and-forget,适合高频低延迟推送
                gateway.forward_fire_and_forget(notif);
            }
        }
    }

    /// P0:LaneEvent → SessionUpdate 转换(见 §5)
    fn lane_event_to_session_update(event: &runtime::LaneEvent) -> Option<acp::SessionUpdate>;

    /// P1:加载已存在的 session
    pub async fn load_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<SerializedSession, acp::Error>;

    /// P1:从序列化状态恢复 runtime
    pub fn restore_runtime_state(
        &self,
        session: SerializedSession,
    ) -> Result<(), acp::Error>;

    /// P2:fork session(从某消息分叉)
    pub async fn fork_session(
        &self,
        session_id: &acp::SessionId,
        fork_point: &str,
    ) -> Result<acp::SessionId, acp::Error>;

    /// P2:列出可恢复会话
    pub async fn list_sessions(&self) -> Vec<SerializedSession>;

    /// P2:切换 plan/act 模式
    pub async fn set_session_mode(&self, mode: acp::SessionMode) -> Result<(), acp::Error>;

    /// P2:切换底层模型
    pub async fn set_session_model(&self, model: &str) -> Result<(), acp::Error>;
}
```

---

## 五、LaneEvent → SessionNotification 桥接

### 5.1 LaneEvent 完整清单

[runtime/src/lane_events.rs:5-57](file:///d:/claw-code-src/rust/crates/runtime/src/lane_events.rs) 定义了 23 种 `LaneEventName`(注:任务描述称 19 种,实际源码为 23 种):

| # | LaneEventName | wire value | 类别 |
|---|---------------|-----------|------|
| 1 | `Started` | `lane.started` | 生命周期 |
| 2 | `Ready` | `lane.ready` | 生命周期 |
| 3 | `PromptMisdelivery` | `lane.prompt_misdelivery` | 异常 |
| 4 | `Blocked` | `lane.blocked` | 阻塞 |
| 5 | `Red` | `lane.red` | 状态 |
| 6 | `Green` | `lane.green` | 状态 |
| 7 | `CommitCreated` | `lane.commit.created` | Git |
| 8 | `PrOpened` | `lane.pr.opened` | Git |
| 9 | `MergeReady` | `lane.merge.ready` | Git |
| 10 | `Finished` | `lane.finished` | 生命周期(终态) |
| 11 | `Failed` | `lane.failed` | 生命周期(终态) |
| 12 | `Reconciled` | `lane.reconciled` | 生命周期(不确定性) |
| 13 | `Merged` | `lane.merged` | 生命周期(终态) |
| 14 | `Superseded` | `lane.superseded` | 生命周期(终态) |
| 15 | `Closed` | `lane.closed` | 生命周期(终态) |
| 16 | `BranchStaleAgainstMain` | `branch.stale_against_main` | 分支 |
| 17 | `BranchWorkspaceMismatch` | `branch.workspace_mismatch` | 分支 |
| 18 | `ShipPrepared` | `ship.prepared` | 发布 |
| 19 | `ShipCommitsSelected` | `ship.commits_selected` | 发布 |
| 20 | `ShipMerged` | `ship.merged` | 发布 |
| 21 | `ShipPushedMain` | `ship.pushed_main` | 发布 |
| 22 | `SubagentHandoff` | `subagent.handoff` | 子 agent |
| 23 | `SubagentResult` | `subagent.result` | 子 agent |

### 5.2 LaneEvent → SessionUpdate 映射表

| LaneEventName | SessionUpdate 变体 | 关键字段映射 |
|---------------|-------------------|-------------|
| `Started` | `AgentMessageChunk`(text) | `"Lane started"` |
| `Ready` | `AgentMessageChunk`(text) | `"Lane ready"` |
| `PromptMisdelivery` | `AgentMessageChunk`(text) | `"Prompt misdelivery: {detail}"` |
| `Blocked` | `ToolCall`(status=Failed) | tool_call_id=data.blocker_id, title=detail |
| `Red` | `AgentMessageChunk`(text) | `"Lane red: {detail}"` |
| `Green` | `AgentMessageChunk`(text) | `"Lane green: {detail}"` |
| `CommitCreated` | `ToolCall`(status=Completed) | tool_call_id=data.commit, title="Commit: {commit}" |
| `PrOpened` | `ToolCall`(status=Completed) | tool_call_id=data.pr_url, title="PR opened" |
| `MergeReady` | `AgentMessageChunk`(text) | `"Merge ready: {detail}"` |
| `Finished` | `AgentMessageChunk`(text) | `"Lane finished"` |
| `Failed` | `ToolCall`(status=Failed) | tool_call_id=fingerprint, title=detail |
| `Reconciled` | `AgentMessageChunk`(text) | `"Reconciled: {detail}"` |
| `Merged` | `AgentMessageChunk`(text) | `"Merged"` |
| `Superseded` | `AgentMessageChunk`(text) | `"Superseded by {data.superseded_by}"` |
| `Closed` | `AgentMessageChunk`(text) | `"Lane closed"` |
| `BranchStaleAgainstMain` | `AgentMessageChunk`(text) | `"Branch stale: {behind_main} behind"` |
| `BranchWorkspaceMismatch` | `AgentMessageChunk`(text) | `"Workspace mismatch: {detail}"` |
| `ShipPrepared` | `ToolCall`(status=Pending) | tool_call_id=data.commit_range, title="Ship prepared" |
| `ShipCommitsSelected` | `AgentMessageChunk`(text) | `"Selected {count} commits"` |
| `ShipMerged` | `ToolCall`(status=Completed) | tool_call_id=data.commit_range, title="Ship merged" |
| `ShipPushedMain` | `ToolCall`(status=Completed) | tool_call_id=data.commit_range, title="Pushed to main" |
| `SubagentHandoff` | `ToolCall`(status=Pending) | tool_call_id=subagent_id, title="Subagent: {task}" |
| `SubagentResult` | `ToolCall`(status=Completed/Failed) | tool_call_id=subagent_id, title=result |

### 5.3 转换代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)
//
// 把 23 种 LaneEvent 转换为 ACP SessionUpdate。
// 大部分映射到 AgentMessageChunk(text)用于 IDE 展示,
// 关键操作(SubagentHandoff / CommitCreated / Ship*)映射到 ToolCall 更结构化。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// LaneEvent → ACP SessionUpdate 转换
    /// 
    /// 返回 None 表示该事件不需要推送(如内部健康检查)。
    fn lane_event_to_session_update(event: &runtime::LaneEvent) -> Option<acp::SessionUpdate> {
        use runtime::LaneEventName as N;
        match event.event {
            // 子 agent 事件 → ToolCall(结构化展示)
            N::SubagentHandoff => {
                let data = event.data.as_ref()?;
                let subagent_id = data["subagent_id"].as_str()?;
                let task = data["task"].as_str()?;
                Some(acp::SessionUpdate::ToolCall {
                    tool_call_id: acp::ToolCallId::new(subagent_id.to_string()),
                    tool_kind: acp::ToolKind::Execute,
                    title: format!("Subagent: {}", task),
                    status: acp::ToolCallStatus::Pending,
                    raw_input: serde_json::json!({}).into(),
                    raw_output: None.into(),
                    locations: vec![],
                })
            }
            N::SubagentResult => {
                let data = event.data.as_ref()?;
                let subagent_id = data["subagent_id"].as_str()?;
                let status = if event.failure_class.is_some() {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                Some(acp::SessionUpdate::ToolCall {
                    tool_call_id: acp::ToolCallId::new(subagent_id.to_string()),
                    tool_kind: acp::ToolKind::Execute,
                    title: "Subagent completed".to_string(),
                    status,
                    raw_input: serde_json::json!({}).into(),
                    raw_output: data["result"].clone().into(),
                    locations: vec![],
                })
            }
            // Git 事件 → ToolCall
            N::CommitCreated => {
                let data = event.data.as_ref()?;
                let commit = data["commit"].as_str()?;
                Some(acp::SessionUpdate::ToolCall {
                    tool_call_id: acp::ToolCallId::new(commit.to_string()),
                    tool_kind: acp::ToolKind::Execute,
                    title: format!("Commit: {}", &commit[..7.min(commit.len())]),
                    status: acp::ToolCallStatus::Completed,
                    raw_input: serde_json::json!({}).into(),
                    raw_output: event.detail.clone().into(),
                    locations: vec![],
                })
            }
            // 终态事件 → AgentMessageChunk(文本通知)
            N::Finished | N::Failed | N::Merged | N::Superseded | N::Closed => {
                let text = format!(
                    "{}: {}",
                    serde_json::to_value(event.event).ok()?.as_str()?,
                    event.detail.as_deref().unwrap_or("no detail")
                );
                Some(acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                )))
            }
            // 其他事件 → 简单文本
            _ => {
                let text = format!(
                    "{}: {}",
                    serde_json::to_value(event.event).ok()?.as_str()?,
                    event.detail.as_deref().unwrap_or("")
                );
                Some(acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                )))
            }
        }
    }
}
```

### 5.4 flush 时机

`flush_lane_events_to_acp` 应在以下位置调用:

| 调用点 | 时机 | 目的 |
|--------|------|------|
| `prompt` 开始 | `run_turn` 前 | 推送上一轮遗留事件 |
| `prompt` 结束 | `run_turn` 后 | 推送本轮产生的事件 |
| `cancel` 触发 | token.cancel() 后 | 推送中断相关事件 |
| `tool` 执行前后 | tool call 循环内 | 实时推送子 agent handoff(P1,需 run_turn 改造) |
| `new_session` 后 | runtime 创建后 | 推送 session 初始化事件 |
| `load_session` 后 | 状态恢复后 | 推送历史事件重放 |

---

## 六、VS Code 扩展骨架

### 6.1 设计原则

VS Code 扩展采用**薄客户端**模式:

1. 不实现 agent 逻辑,只做 UI 桥接
2. 通过 `child_process.spawn` 启动 `claw-headless` binary
3. 通过 stdin/stdout 传 JSON-RPC,与 ACP 协议完全对齐
4. 复用 `vscode-languageserver/node` 的 `createConnection` 处理 JSON-RPC framing

### 6.2 package.json 骨架

```json
{
  "name": "claw-code",
  "displayName": "Claw Code",
  "description": "ACP-compatible AI coding agent for VS Code",
  "version": "0.1.0",
  "engines": { "vscode": "^1.85.0" },
  "categories": ["Other", "Chat", "Machine Learning"],
  "activationEvents": [
    "onCommand:claw.startServer",
    "onStartupFinished"
  ],
  "main": "./out/extension.js",
  "contributes": {
    "commands": [
      { "command": "claw.startServer", "title": "Claw: Start Server" },
      { "command": "claw.stopServer", "title": "Claw: Stop Server" },
      { "command": "claw.sendPrompt", "title": "Claw: Send Prompt" }
    ],
    "configuration": {
      "title": "Claw Code",
      "properties": {
        "claw.binaryPath": {
          "type": "string",
          "default": "claw-headless",
          "description": "Path to claw-headless binary"
        },
        "claw.model": {
          "type": "string",
          "default": "claude-sonnet-4-5",
          "description": "Model to use"
        },
        "claw.permissionMode": {
          "type": "string",
          "enum": ["read-only", "workspace-write", "danger-full-access"],
          "default": "workspace-write",
          "description": "Permission mode"
        }
      }
    },
    "chatParticipants": [
      {
        "id": "claw",
        "name": "claw",
        "description": "Claw Code AI agent",
        "isSticky": true,
        "commands": [
          { "name": "plan", "description": "Plan mode (read-only)" },
          { "name": "act", "description": "Act mode (default)" }
        ]
      }
    ]
  },
  "scripts": {
    "vscode:prepublish": "npm run compile",
    "compile": "tsc -p ./"
  },
  "dependencies": {
    "vscode-languageserver": "^9.0.1",
    "vscode-languageclient": "^9.0.1"
  }
}
```

### 6.3 extension.ts 骨架

```typescript
// vscode-claw-extension/src/extension.ts
//
// 薄客户端架构:
// - spawn claw-headless 子进程
// - 用 vscode-languageserver 的 createConnection 桥接 stdin/stdout
// - 注册 fs/read_text_file / fs/write_text_file / session/request_permission 反向 handler

import * as vscode from 'vscode';
import { spawn, ChildProcess } from 'child_process';
import { createConnection, TextDocument } from 'vscode-languageserver/node';

let clawProcess: ChildProcess | null = null;
let outputChannel: vscode.OutputChannel;
let connection: any = null;

export function activate(context: vscode.ExtensionContext) {
    outputChannel = vscode.window.createOutputChannel('Claw Code');

    // 注册命令:启动 Claw ACP server
    context.subscriptions.push(
        vscode.commands.registerCommand('claw.startServer', startClawServer)
    );

    // 注册命令:停止 Claw ACP server
    context.subscriptions.push(
        vscode.commands.registerCommand('claw.stopServer', stopClawServer)
    );

    // 注册 chat participant(@claw)
    const participant = vscode.chat.createChatParticipant('claw', handleChatRequest);
    participant.iconPath = new vscode.ThemeIcon('comment-discussion');

    // 监听配置变更
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration(e => {
            if (e.affectsConfiguration('claw')) {
                outputChannel.appendLine('Config changed, restart server to apply');
            }
        })
    );
}

async function startClawServer() {
    if (clawProcess) {
        vscode.window.showWarningMessage('Claw server is already running');
        return;
    }

    const config = vscode.workspace.getConfiguration('claw');
    const binaryPath = config.get<string>('binaryPath', 'claw-headless');
    const model = config.get<string>('model', 'claude-sonnet-4-5');
    const permissionMode = config.get<string>('permissionMode', 'workspace-write');

    // spawn claw-headless 子进程
    clawProcess = spawn(binaryPath, [
        '--model', model,
        '--permission-mode', permissionMode,
    ], {
        cwd: vscode.workspace.rootPath,
        stdio: ['pipe', 'pipe', 'pipe'],
    });

    // stderr 走 output channel
    clawProcess.stderr?.on('data', data => {
        outputChannel.append(`[stderr] ${data.toString()}`);
    });

    // stdout 接 JSON-RPC connection
    connection = createConnection(
        clawProcess.stdout!,
        clawProcess.stdin!
    );
    connection.listen();

    // 初始化握手
    const initResult = await connection.sendRequest('initialize', {
        protocolVersion: 1,
        clientCapabilities: {
            fs_read_text_file: true,
            fs_write_text_file: true,
            session_request_permission: true,
        },
    });
    outputChannel.appendLine(`Initialized: ${JSON.stringify(initResult)}`);

    // 注册 fs/read_text_file 反向请求 handler
    connection.onRequest('fs/read_text_file', async (params: any) => {
        return handleReadTextFile(params);
    });

    // 注册 fs/write_text_file 反向请求 handler
    connection.onRequest('fs/write_text_file', async (params: any) => {
        return handleWriteTextFile(params);
    });

    // 注册 session/request_permission 反向请求 handler
    connection.onRequest('session/request_permission', async (params: any) => {
        return handleRequestPermission(params);
    });

    outputChannel.appendLine('Claw server started');
}

async function handleReadTextFile(params: { path: string }) {
    // 优先从 editor buffer 取(含未保存内容)
    const uri = vscode.Uri.file(params.path);
    const doc = vscode.workspace.textDocuments.find(d => d.uri.fsPath === uri.fsPath);
    if (doc) {
        return { content: doc.getText() };
    }
    // 回退到磁盘读取
    try {
        const content = await vscode.workspace.fs.readFile(uri);
        return { content: Buffer.from(content).toString() };
    } catch (e) {
        throw new Error(`File not found: ${params.path}`);
    }
}

async function handleWriteTextFile(params: { path: string; content: string }) {
    const uri = vscode.Uri.file(params.path);
    // 走 editor API,确保进 undo 栈
    let doc = vscode.workspace.textDocuments.find(d => d.uri.fsPath === uri.fsPath);
    if (!doc) {
        doc = await vscode.workspace.openTextDocument(uri);
    }
    const edit = new vscode.WorkspaceEdit();
    edit.replace(uri, new vscode.Range(0, 0, doc.lineCount, 0), params.content);
    const applied = await vscode.workspace.applyEdit(edit);
    return { written: applied };
}

async function handleRequestPermission(params: {
    toolName: string;
    toolInput: any;
    options: string[];
}) {
    const choice = await vscode.window.showWarningMessage(
        `Claw 请求执行: ${params.toolName}`,
        { modal: true },
        '允许', '拒绝', '始终允许'
    );
    const outcome = choice === '允许' ? 'allow'
        : choice === '始终允许' ? 'always_allow'
        : 'deny';
    return { outcome };
}

async function handleChatRequest(
    request: vscode.ChatRequest,
    context: vscode.ChatContext,
    stream: vscode.ChatResponseStream,
    token: vscode.CancellationToken
) {
    if (!connection) {
        await startClawServer();
    }
    // 发送 prompt 到 ACP server
    // 此处需要先 new_session(若未创建),再 prompt
    // 简化:每次都创建新 session
    stream.markdown('Thinking...\n');
    // TODO: 实现 session 管理 + prompt 流式接收
}

async function stopClawServer() {
    if (connection) {
        connection.dispose();
        connection = null;
    }
    if (clawProcess) {
        clawProcess.kill();
        clawProcess = null;
        outputChannel.appendLine('Claw server stopped');
    }
}

export function deactivate() {
    stopClawServer();
}
```

### 6.4 UI 集成要点

| UI 元素 | 集成方式 | ACP 方法 |
|---------|---------|---------|
| Chat panel | `vscode.chat.createChatParticipant` | `session/prompt` |
| 文件读取 | `vscode.workspace.textDocuments` | `fs/read_text_file` 反向 handler |
| 文件写入 | `vscode.WorkspaceEdit`(走 undo 栈) | `fs/write_text_file` 反向 handler |
| 权限弹窗 | `vscode.window.showWarningMessage`(modal) | `session/request_permission` 反向 handler |
| 输出日志 | `vscode.OutputChannel` | stderr 转发 |
| 状态栏 | `vscode.window.createStatusBarItem` | 显示 server running/stopped |
| 进度条 | `vscode.window.withProgress` | `SessionUpdate::ToolCall` Pending |
| Diff 视图 | `vscode.diff` 命令 | `SessionUpdate::ToolCall` Completed with diff |

---

## 七、Zed 集成验证

### 7.1 agents.json 配置示例

Zed 通过 `~/.config/zed/agents.json`(macOS)/ `%APPDATA%\Zed\agents.json`(Windows)配置 ACP 服务器:

```json
{
  "agent_servers": {
    "claw": {
      "name": "Claw Code",
      "command": {
        "binary": "claw-headless",
        "args": ["--model", "claude-sonnet-4-5", "--permission-mode", "workspace-write"],
        "env": {
          "ANTHROPIC_API_KEY": "${ANTHROPIC_API_KEY}"
        }
      },
      "cwd": "${ZED_WORKTREE_ROOT}",
      "capabilities": {
        "fs_read_text_file": true,
        "fs_write_text_file": true,
        "session_request_permission": true
      }
    }
  }
}
```

### 7.2 启动命令

```bash
# 1. 编译 claw-headless binary
cd d:\claw-code-src\rust
cargo build --release --bin claw-headless

# 2. 复制到 PATH(Windows)
copy target\release\claw-headless.exe C:\Users\38225\.cargo\bin\

# 3. 配置 Zed agents.json(见上)
# 4. 重启 Zed,在 Assistant panel 选择 "Claw Code" agent
# 5. 发送测试 prompt 验证
```

### 7.3 测试步骤

| # | 步骤 | 预期结果 | 验证方式 |
|---|------|---------|---------|
| 1 | Zed 启动,打开 Assistant panel | "Claw Code" 出现在 agent 选择列表 | UI 检查 |
| 2 | 选 "Claw Code",发 "hello" | 收到 assistant 回复 | 看到消息气泡 |
| 3 | 让 Claw 读当前打开的文件 | editor buffer 被读取 | 日志看到 `fs/read_text_file` 调用 |
| 4 | 让 Claw 写文件 | 弹出权限确认对话框 | 选 Allow 后文件被修改 |
| 5 | 让 Claw 执行 Bash 命令 | 弹出权限确认 | 选 Allow 后命令执行,输出推送回 IDE |
| 6 | 关闭 Zed | claw-headless 进程退出 | Task Manager 检查 |
| 7 | 重启 Zed | session 恢复(P1 验证) | 历史消息可见 |

---

## 八、实施步骤分解

### 8.1 P0 阶段(4-6 周)周维度任务

| 周 | 任务 | 交付物 | 验收标准 |
|---|------|--------|---------|
| W1 | ACP 1.5 升级基础 | `claw-acp/Cargo.toml` 升级到 1.5 + 双 feature 编译通过 | `cargo build --features unstable,unstable-v2` 成功 |
| W1 | `ClawAgent` 扩展字段 | 新增 `session_store` / `cancel_token` / `permission_cache` 字段 | 现有测试不回归 |
| W2 | `initialize` 补充 agent_capabilities | 声明 fs/permission 能力 | `initialize_declares_fs_capabilities` 测试通过 |
| W2 | `fs/read_text_file` 实现 | `read_editor_buffer` 方法 + 测试 | 单元测试 3 个通过 |
| W3 | `fs/write_text_file` 实现 | `write_editor_buffer` 方法 + 测试 | 单元测试 3 个通过 |
| W3 | `session/request_permission` 实现 | `request_permission` 方法 + 测试 | 单元测试 4 个通过 |
| W4 | `LaneEvent` 桥接 | `flush_lane_events_to_acp` + `lane_event_to_session_update` | 23 种事件映射测试通过 |
| W4 | `prompt` 集成 flush | 在 `run_turn` 前后调用 flush | 集成测试验证 SubagentHandoff 推送 |
| W5 | Zed 集成验证 | agents.json 配置 + 端到端测试 | 7 步测试通过 |
| W5 | A6.4 测试更新 | 错误路径测试适配 1.5 行为 | 双 feature 下测试均通过 |
| W6 | VS Code 扩展骨架 | package.json + extension.ts 骨架 | 能 spawn server + initialize 握手 |
| W6 | 文档与发布 | 本分支文档 v1.0 + 主文档回写 | 三分支合并准备 |

### 8.2 P1 阶段(后续 4-6 周)

| 任务 | 说明 |
|------|------|
| `run_turn` 改造为 async | 引入 `CancellationToken`,实现真实 cancel |
| `session/load` 实现 | 从 SessionStore 恢复 + 历史消息重放 |
| `session/fork` 实现 | 从某消息分叉新 session |
| `session/set_mode` 实现 | plan/act 模式切换 |
| `session/set_model` 实现 | 运行时模型切换 |
| Diff v2 支持 | `fs/write_text_file` 支持 typed diff |
| VS Code 扩展完整实现 | chat panel + 流式输出 + 多 session |

---

## 九、测试矩阵

### 9.1 单元测试

| 测试文件 | 测试名 | 覆盖方法 | 优先级 |
|---------|--------|---------|--------|
| `agent.rs` | `initialize_returns_latest_protocol_version` | `initialize` | P0 |
| `agent.rs` | `initialize_exposes_api_key_auth_method` | `initialize` | P0 |
| `agent.rs` | `initialize_declares_fs_capabilities` | `initialize` | P0 |
| `agent.rs` | `new_session_returns_unique_session_id` | `new_session` | P0 |
| `agent.rs` | `new_session_consumes_api_client` | `new_session` | P0 |
| `agent.rs` | `prompt_returns_end_turn_stop_reason` | `prompt` | P0 |
| `agent.rs` | `prompt_without_session_returns_error` | `prompt` | P0 |
| `agent.rs` | `prompt_pushes_agent_message_chunk` | `prompt` | P0 |
| `agent.rs` | `prompt_flushes_lane_events_before_and_after` | `prompt` + flush | P0 |
| `agent.rs` | `read_editor_buffer_returns_content` | `fs/read_text_file` | P0 |
| `agent.rs` | `read_editor_buffer_propagates_method_not_found` | `fs/read_text_file` | P0 |
| `agent.rs` | `write_editor_buffer_returns_written_true` | `fs/write_text_file` | P0 |
| `agent.rs` | `write_editor_buffer_returns_false_when_user_denies` | `fs/write_text_file` | P0 |
| `agent.rs` | `request_permission_returns_allow_when_user_approves` | `request_permission` | P0 |
| `agent.rs` | `request_permission_returns_deny_when_user_rejects` | `request_permission` | P0 |
| `agent.rs` | `request_permission_times_out_after_60s` | `request_permission` | P0 |
| `agent.rs` | `lane_event_to_session_update_maps_23_variants` | 桥接 | P0 |
| `agent.rs` | `flush_lane_events_to_acp_drains_sink` | 桥接 | P0 |

### 9.2 集成测试

| 测试文件 | 测试名 | 场景 | 优先级 |
|---------|--------|------|--------|
| `stdio.rs` | `full_handshake_initialize_to_prompt` | initialize → authenticate → new_session → prompt 全流程 | P0 |
| `stdio.rs` | `fs_read_text_file_reverse_request` | agent 反向请求 IDE 读文件 | P0 |
| `stdio.rs` | `fs_write_text_file_reverse_request` | agent 反向请求 IDE 写文件 | P0 |
| `stdio.rs` | `session_request_permission_reverse_request` | agent 反向请求权限审批 | P0 |
| `stdio.rs` | `lane_event_flush_on_subagent_handoff` | SubagentHandoff → ToolCall notification | P0 |
| `stdio.rs` | `cancel_notification_returns_ok_without_active_prompt` | 现有 stub 契约 | P0 |
| `stdio.rs` | `run_agent_on_io_silently_drops_invalid_json` | A6.4(0.10.4 行为) | P0 |
| `stdio.rs` | `run_agent_on_io_returns_error_on_unknown_method` | A6.4(-32601) | P0 |
| `stdio.rs` | `run_agent_on_io_returns_parse_error_on_invalid_json_v2` | A6.4(1.5 行为,unstable-v2) | P0 |

### 9.3 端到端测试

| 测试 | 环境 | 验证点 | 优先级 |
|------|------|--------|--------|
| Zed + claw-headless | Zed dev build | 7 步测试(见 §7.3) | P0 |
| VS Code + extension | VS Code 1.85+ | spawn + initialize + prompt | P1 |
| 多 session 恢复 | Zed 重启 | session/load 后历史可见 | P1 |
| cancel 中断 | 长时间 prompt | cancel 后 StopReason::Cancelled | P1 |

---

## 十、风险与缓解

### 10.1 ACP 1.5 破坏性变更风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|---------|
| `ContentBlock` 重命名导致编译失败 | 高 | 高(3 处调用) | 全局替换 + 编译验证 |
| `SessionConfiguration` 类型变更 | 中 | 中 | 双 feature 下分别实现 |
| `PermissionOption` 枚举化 | 中 | 中 | 适配层转换 `Vec<Value>` ↔ `Vec<PermissionOption>` |
| Terminal API 未实现导致 trait 报错 | 高 | 低 | `AcpGatewaySender` 已实现转发,无需在 `ClawAgent` 实现 |

### 10.2 async_trait Send 约束风险

**问题**:`ClawAgent<C>` 持有 `Rc<RefCell<...>>`(通过 `StaticToolExecutor` 间接),非 `Send`。`acp::Agent` trait 使用 `#[async_trait(?Send)]`,允许非 Send future,但有限制:

- 不能在 `tokio::spawn` 中调用(Send 要求)
- 必须在 `tokio::task::spawn_local` 或 `LocalSet::block_on` 中调用

**现状**:已通过 `spawn_claw_shell`([spawn.rs:69-93](file:///d:/claw-code-src/rust/crates/claw-shell/src/spawn.rs))和 `run_stdio_agent`([stdio.rs:106-129](file:///d:/claw-code-src/rust/crates/claw-shell/src/stdio.rs))正确隔离。

**缓解**:

```rust
// rust/crates/claw-shell/src/agent.rs(约束固化)
//
// 显式标注 ?Send,防止误用 Send 约束的 API。
// 编译时检查:任何尝试 spawn 跨线程的调用会失败。

#[async_trait(?Send)]
impl<C> acp::Agent for ClawAgent<C>
where
    C: ApiClient + 'static,
{
    // 所有方法都是 ?Send async,只能在 LocalSet 上调用
}
```

### 10.3 非 Send 类型隔离

**问题**:`StaticToolExecutor` 内部 `Box<dyn FnMut>` 非 Send,无法跨线程移动。

**现状**:`ClawAgentBuilder<C>` 要求 `C: ApiClient + Send + 'static`(api_client 必须可跨线程移动),`StaticToolExecutor` 在 `build()` 内创建([agent.rs:98-110](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs)),确保不跨线程。

**风险点**:

1. `client_gateway: AcpGatewaySender<acp::AgentSide>` 是 `mpsc::UnboundedSender`,本身 `Send`,可跨线程 clone —— 但其接收方 `AcpGatewayReceiver` 必须在 LocalSet 上运行
2. `SessionStore` 使用 `std::sync::Mutex`(非 `tokio::sync::Mutex`),是 `Send + Sync`,可跨线程共享

**缓解**:

- 在 `spawn_claw_shell` 中严格保持 `LocalSet::block_on` 边界
- `SessionStore` 用 `std::sync::Mutex` 确保可跨线程共享(支持未来多 session 跨 agent 实例)
- 新增字段前检查 `Send` 约束,在 `ClawAgentBuilder::build` 内创建非 Send 类型

### 10.4 LaneEvent sink 容量保护

**问题**:[lane_events.rs:1047](file:///d:/claw-code-src/rust/crates/runtime/src/lane_events.rs) 全局 sink 容量 512,超容量丢弃最旧一半。

**风险**:ACP 桥接启用后,若 IDE 端响应慢,`flush_lane_events_to_acp` 调用间隔变长,sink 可能溢出。

**缓解**:

1. `flush_lane_events_to_acp` 在 `prompt` 开始和结束各调用一次,确保每轮 turn 内 sink 被 drain 两次
2. `forward_fire_and_forget` 不阻塞,即使 IDE 端慢也不影响 agent
3. 在 `tool` 执行循环内增加 flush(P1,需 run_turn 改造)
4. 监控 sink 长度,接近上限时 log warn

### 10.5 协议版本协商失败

**问题**:IDE 不支持 1.5 特性(如 Zed 旧版),但 agent 已启用 `unstable-v2`。

**缓解**:

1. `initialize` 时检查 `client_capabilities`,若 IDE 未声明 fs/permission 能力,agent 不发起反向请求
2. `agent_capabilities` 中声明的能力必须与实际实现匹配(避免 IDE 误以为支持)
3. 保留 0.10.4 兼容路径(`unstable` feature),降级时仍可用

---

## 十一、参考链接

### 11.1 ACP 官方资源

- [Agent Client Protocol Spec](https://agentclientprotocol.com/) — 协议规范主站
- [ACP GitHub: kendru/agent-client-protocol](https://github.com/kendru/agent-client-protocol) — Rust crate 源码
- [agent-client-protocol 0.10.4 docs.rs](https://docs.rs/agent-client-protocol/0.10.4/) — 当前使用版本文档
- [agent-client-protocol 1.5.0 docs.rs](https://docs.rs/agent-client-protocol/1.5.0/) — 目标版本文档(发布后)
- [ACP 1.5 Changelog](https://github.com/kendru/agent-client-protocol/blob/main/CHANGELOG.md) — 版本变更记录

### 11.2 IDE 集成参考

- [Zed agents.json spec](https://zed.dev/docs/agent-servers) — Zed agent 服务器配置
- [Zed Assistant panel](https://zed.dev/docs/assistant-panel) — Zed 助手面板文档
- [VS Code Extension API](https://code.visualstudio.com/api) — VS Code 扩展 API 主入口
- [VS Code Chat Participants API](https://code.visualstudio.com/api/extension-guides/chat-session) — Chat participant 集成指南
- [vscode-languageserver](https://github.com/microsoft/vscode-languageserver-node) — JSON-RPC 桥接库
- [JetBrains ACP plugin](https://github.com/kendru/agent-client-protocol-jetbrains) — JetBrains 集成参考
- [CodeCompanion.nvim](https://github.com/olimorris/codecompanion.nvim) — Neovim ACP 适配器

### 11.3 Claw Code 内部参考

- [主文档:IDE 集成方案](file:///d:/claw-code-src/docs/ide-hooks-dag-implementation-plan.md) — 父文档
- [agent.rs:ClawAgent 实现](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs) — 当前 ACP Agent trait 实现
- [spawn.rs:spawn_claw_shell](file:///d:/claw-code-src/rust/crates/claw-shell/src/spawn.rs) — 独立线程 + LocalSet 启动模式
- [stdio.rs:run_agent_on_io](file:///d:/claw-code-src/rust/crates/claw-shell/src/stdio.rs) — stdio ACP 服务器核心
- [gateway.rs:AcpGatewaySender](file:///d:/claw-code-src/rust/crates/claw-acp/src/gateway.rs) — Gateway 转发层
- [lane_events.rs:LaneEvent](file:///d:/claw-code-src/rust/crates/runtime/src/lane_events.rs) — 23 种内部事件定义
- [headless.rs:claw-headless](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/bin/headless.rs) — stdio 服务器入口 binary
- [claw-acp Cargo.toml](file:///d:/claw-code-src/rust/crates/claw-acp/Cargo.toml) — 0.10.4 版本锁定位置

### 11.4 相关 RFC 与设计文档

- [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) — ACP 1.5 Content 类型对齐的协议
- [JSON-RPC 2.0 Spec](https://www.jsonrpc.org/specification) — ACP 传输层协议
- [tokio LocalSet docs](https://docs.rs/tokio/latest/tokio/task/struct.LocalSet.html) — 非 Send future 执行环境

---

## 附录 A:文档变更日志

| 版本 | 日期 | 变更 |
|------|------|------|
| v0.1 | 2026-07-21 | 初版创建,基于主文档第二章展开 |

---

## 附录 B:术语表

| 术语 | 全称 | 说明 |
|------|------|------|
| ACP | Agent Client Protocol | 编辑器与 AI agent 通信协议 |
| IDE | Integrated Development Environment | 集成开发环境(VS Code / Zed / JetBrains 等) |
| LaneEvent | Lane Event | Claw Code 内部事件总线事件 |
| SessionNotification | ACP Session Notification | agent → IDE 的推送消息 |
| LocalSet | tokio::task::LocalSet | tokio 中运行非 Send future 的执行环境 |
| silent drop | Silent Drop | ACP 0.10.4 中错误消息被静默丢弃的行为 |
| capability | Capability | ACP 协商中声明的能力(fs/permission/terminal) |
| fire-and-forget | Fire and Forget | 发送后不等待响应的推送模式 |
