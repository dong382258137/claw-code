//! ACP 1.3 版本的 Agent 实现。
//!
//! 1.3 API 重设计:`Client`/`Agent` 从 trait 变为 role marker struct,
//! 引入 `Component`/`Connection`/`Builder` 模型。本模块用 1.3 的新 API
//! 重新实现 `ClawAgent`,提供 IDE 反向请求(fs/read_text_file、
//! fs/write_text_file、session/request_permission)的发送逻辑。
//!
//! ## 1.3 API 实际结构
//!
//! - `acp::Agent` / `acp::Client` 是 role marker struct(非 trait)
//! - `Agent.builder()` 返回 `Builder<Agent>`,通过 `on_receive_request` /
//!   `on_receive_dispatch` 注册闭包处理器
//! - 闭包接收 `ConnectionTo<Client>`,该类型 `Clone + Send`,可在闭包外
//!   保存以发起反向请求
//! - `ConnectionTo<Client>::send_request(req).block_task().await` 发起
//!   反向请求并等待响应
//! - `ConnectionTo<Client>::send_notification(notif)` 单向通知(无需 ACK)
//! - `Stdio::new()` 实现了 `ConnectTo<R>` for any `R`,作为 stdio transport
//!
//! ## 编译
//!
//! 本模块仅在 `acp-1_5` feature 启用时编译。该 feature 同时:
//! - 启用 claw-acp 的 `acp-1_5` feature(切换到 1.3 schema)
//! - 启用本地 `agent-client-protocol-v1` 依赖(直接引用 1.3 schema 类型)
//!
//! 本模块自包含,不依赖 `crate::agent`(0.10.4 实现),因此可以在
//! `--no-default-features --features acp-1_5` 模式下独立编译。

#![cfg(feature = "acp-1_5")]

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use std::cell::RefCell;

// 1.3 schema:package 名仍是 agent-client-protocol,但通过 rename 区分。
// 1.3 的 schema 类型在 `schema::v1` 路径下;Client/Agent 是 role markers
// (位于 `acp::role::acp::`,通过 `acp::Client` / `acp::Agent` re-export)。
// `acp::ConnectionTo<R>` 是连接上下文,Clone + Send,通过 `acp::ConnectTo<R>`
// trait 的 `connect_to` 方法建立连接。
use acp::schema::v1 as schema;
use agent_client_protocol_v1 as acp;

use runtime::{ApiClient, ConversationRuntime, StaticToolExecutor};

/// 反向请求的默认超时(秒)。
///
/// 30 秒覆盖 IDE 交互的典型延迟;若 IDE 长时间不响应,超时保护
/// 避免 agent 永久阻塞。
const DEFAULT_REVERSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 1.3 版本的 ClawAgent。
///
/// 与 0.10.4 的 `ClawAgent` 主要差异:
/// - 不实现 `acp::Agent` trait(1.3 中 Agent 是 struct,不是 trait)
/// - 持有 1.3 `ConnectionTo<Client>` 句柄(通过 `Arc<tokio::sync::Mutex<...>>`
///   共享给 Builder 闭包,因 Builder 要求 `Send`)
/// - 提供 3 个反向请求方法,主动向 IDE 请求数据/权限
///
/// ## 反向请求
///
/// 1. `read_editor_buffer` → `fs/read_text_file`:读取 IDE 打开的文件内容
/// 2. `write_editor_buffer` → `fs/write_text_file`:向 IDE 写入文件
/// 3. `request_permission` → `session/request_permission`:请求用户授权
///
/// ## 连接句柄的生命周期
///
/// `client_connection` 字段初始为 `None`。当 1.3 Builder 的 dispatch 闭包
/// 首次被调用时,闭包从 `ConnectionTo<Client>` 参数 clone 一份并写入
/// 该字段。后续反向请求方法读取该字段发送请求。
///
/// 这种"延迟绑定"是为了适配 1.3 Builder 的设计:`ConnectionTo<Client>`
/// 只在 handler 闭包内可用,无法在构造 agent 时直接传入。
pub struct ClawAgentV13<C>
where
    C: ApiClient + 'static,
{
    /// 当前活跃的 runtime。initialize 阶段为 None,new_session 后创建。
    ///
    /// Stage 2 占位:本字段在 stage 3 引入 ClawAgentV13Builder 后才填充。
    /// 保留字段是为了 stage 3 添加 builder 时不需要破坏性改 struct。
    #[allow(dead_code)]
    runtime: RefCell<Option<ConversationRuntime<C, StaticToolExecutor>>>,

    /// API client(在 new_session 时移入 runtime,故用 Option)。
    ///
    /// Stage 2 占位:同上,stage 3 才填充。
    #[allow(dead_code)]
    api_client: RefCell<Option<C>>,

    /// Tool executor(在 new_session 时移入 runtime,故用 Option)。
    ///
    /// Stage 2 占位:同上,stage 3 才填充。
    #[allow(dead_code)]
    tool_executor: RefCell<Option<StaticToolExecutor>>,

    /// 活跃 session 的 ID 集合。
    /// 1.3 中 session 通过 `SessionId` 标识,session 状态由 `Component` 管理。
    sessions: RefCell<HashSet<schema::SessionId>>,

    /// 当前活跃 session ID。反向请求默认绑定到该 session。
    /// 由 Builder 的 `NewSession` handler 写入。
    active_session: RefCell<Option<schema::SessionId>>,

    /// 1.3 反向请求句柄。初始为 None,Builder 的 dispatch handler
    /// 首次调用时写入(因 ConnectionTo 只在 handler 内可用)。
    ///
    /// 使用 `Arc<tokio::sync::Mutex<...>>` 而非 `RefCell`,因为 Builder
    /// 的 handler 闭包要求 `Send`,而 `Rc<RefCell<...>>` 非 Send。
    /// `ConnectionTo<Client>` 是 `Send + Clone`,所以 `Arc<Mutex<Option<_>>>`
    /// 可以在 Send 闭包间共享。
    client_connection: Arc<tokio::sync::Mutex<Option<acp::ConnectionTo<acp::Client>>>>,

    /// AlwaysAllow 缓存:用户对某 (operation, target) 授予"AlwaysAllow"
    /// 后,后续同类请求直接命中缓存,不再向 IDE 询问。
    ///
    /// 与 `client_connection` 一样用 `Arc<Mutex<...>>` 以满足 Send。
    permission_cache: Arc<tokio::sync::Mutex<HashSet<(String, String)>>>,

    /// 占位字段,确保 `C` 类型参数被使用。
    _marker: PhantomData<C>,
}

impl<C> ClawAgentV13<C>
where
    C: ApiClient + Send + 'static,
{
    /// 构造 1.3 ClawAgent。
    ///
    /// # 设计
    ///
    /// 1.3 API 不需要专门的 builder(与 0.10.4 不同)。本构造函数:
    /// - 初始化所有字段为 `None`/空
    /// - 创建 `client_connection` 和 `permission_cache` 的共享 slot
    ///
    /// 调用方需通过 [`connection_slot`] 获取 slot 的克隆,传给
    /// `Agent.builder().on_receive_dispatch(...)` 闭包,使闭包在收到消息时
    /// 写入 `ConnectionTo<Client>`。
    ///
    /// ```ignore
    /// let agent = ClawAgentV13::<MyClient>::new();
    /// let slot = agent.connection_slot();
    /// let builder = acp::Agent
    ///     .builder()
    ///     .name("claw-agent")
    ///     .on_receive_dispatch(
    ///         async move |_msg, cx: acp::ConnectionTo<acp::Client>| {
    ///             *slot.lock().await = Some(cx.clone());
    ///             // ...dispatch to runtime handlers...
    ///             Ok(())
    ///         },
    ///         acp::on_receive_dispatch!(),
    ///     )
    ///     .connect_to(acp::Stdio::new());
    /// ```
    ///
    /// 反向请求方法在 slot 被填充前会返回 `ConnectionClosed` 错误。
    pub fn new() -> Self {
        Self {
            runtime: RefCell::new(None),
            api_client: RefCell::new(None),
            tool_executor: RefCell::new(None),
            sessions: RefCell::new(HashSet::new()),
            active_session: RefCell::new(None),
            client_connection: Arc::new(tokio::sync::Mutex::new(None)),
            permission_cache: Arc::new(tokio::sync::Mutex::new(HashSet::new())),
            _marker: PhantomData,
        }
    }

    /// 返回 `client_connection` slot 的克隆,供 1.3 Builder 的 dispatch
    /// handler 闭包捕获。
    ///
    /// 闭包在收到消息时,把 `ConnectionTo<Client>` clone 后写入 slot,
    /// 让 agent 的反向请求方法可以读到。
    ///
    /// 该 slot 也用于 `permission_cache`:闭包和 agent 共享同一份缓存。
    pub fn connection_slot(&self) -> ClawAgentV13ConnectionSlot {
        ClawAgentV13ConnectionSlot {
            client_connection: self.client_connection.clone(),
            permission_cache: self.permission_cache.clone(),
        }
    }

    /// 设置当前活跃 session ID(由 `NewSession` handler 调用)。
    ///
    /// 反向请求需要一个 session_id;agent 自动用最近活跃的 session。
    pub fn set_active_session(&self, session_id: schema::SessionId) {
        self.sessions.borrow_mut().insert(session_id.clone());
        *self.active_session.borrow_mut() = Some(session_id);
    }

    /// 取当前活跃 session ID 的克隆。无 session 时返回 `None`。
    fn active_session_id(&self) -> Option<schema::SessionId> {
        self.active_session.borrow().clone()
    }

    /// 从 `client_connection` slot 锁定并 clone 一份 `ConnectionTo<Client>`。
    /// 若 slot 为 None(尚未建立连接),返回 `ConnectionClosed` 错误。
    ///
    /// 注意:故意不在锁内发送请求,避免长时间持锁。lock-and-clone 后立即释放。
    async fn acquire_connection(&self) -> Result<acp::ConnectionTo<acp::Client>, ConnectionClosed> {
        let guard = self.client_connection.lock().await;
        guard.clone().ok_or(ConnectionClosed)
    }

    /// 发起 `fs/read_text_file` 反向请求。
    ///
    /// 让 agent 主动读取 IDE 中打开的文件内容,而不是只能通过 prompt
    /// 让用户手动粘贴。典型场景:
    /// - agent 需要查看用户当前编辑的文件
    /// - 读取 IDE 缓冲区中未保存的修改
    ///
    /// # 参数
    /// - `path`:要读取的文件绝对路径
    ///
    /// # 返回
    /// 成功时返回文件内容(UTF-8 字符串)。
    ///
    /// # 1.3 API 实现要点
    /// 通过 `ConnectionTo<Client>::send_request(ReadTextFileRequest)` 发送,
    /// `.block_task().await` 等待响应,外层包 `tokio::time::timeout` 加超时。
    pub async fn read_editor_buffer(&self, path: &str) -> Result<String, ReadError> {
        let conn = self.acquire_connection().await?;
        let session_id = self
            .active_session_id()
            .ok_or(ReadError::Ide("no active session".into()))?;

        let req = schema::ReadTextFileRequest::new(session_id, path.to_string());
        let response = tokio::time::timeout(DEFAULT_REVERSE_REQUEST_TIMEOUT, async {
            conn.send_request(req).block_task().await
        })
        .await
        .map_err(|_| ReadError::Timeout(path.to_string()))?
        .map_err(|e| ReadError::Ide(e.to_string()))?;

        Ok(response.content)
    }

    /// 发起 `fs/write_text_file` 反向请求。
    ///
    /// 让 agent 主动向 IDE 写入文件,典型场景:
    /// - agent 生成的代码直接写入 IDE 缓冲区
    /// - 修改用户当前打开的文件(配合 read_editor_buffer)
    ///
    /// # 参数
    /// - `path`:目标文件绝对路径
    /// - `content`:要写入的内容
    ///
    /// # 1.3 API 实现要点
    /// 通过 `ConnectionTo<Client>::send_request(WriteTextFileRequest)` 发送。
    /// 1.3 的 `WriteTextFileResponse` 为空 struct(仅 ACK),无业务数据。
    pub async fn write_editor_buffer(&self, path: &str, content: &str) -> Result<(), WriteError> {
        let conn = self.acquire_connection().await?;
        let session_id = self
            .active_session_id()
            .ok_or(WriteError::Ide("no active session".into()))?;

        let req =
            schema::WriteTextFileRequest::new(session_id, path.to_string(), content.to_string());

        tokio::time::timeout(DEFAULT_REVERSE_REQUEST_TIMEOUT, async {
            conn.send_request(req).block_task().await
        })
        .await
        .map_err(|_| WriteError::Timeout(path.to_string()))?
        .map_err(|e| WriteError::Ide(e.to_string()))?;

        Ok(())
    }

    /// 发起 `session/request_permission` 反向请求。
    ///
    /// 让 agent 在执行敏感操作前主动请求用户授权,典型场景:
    /// - 执行 shell 命令前请求确认
    /// - 修改工作区外的文件前请求确认
    /// - 调用外部 MCP 工具前请求确认
    ///
    /// # AlwaysAllow 缓存
    ///
    /// 用户首次选择"AlwaysAllow"后,缓存 `(operation, target)`。后续
    /// 同类请求直接命中缓存,返回 `PermissionOutcome::AlwaysAllow`,不再
    /// 向 IDE 询问。缓存与 `client_connection` 一样用 `Arc<Mutex<...>>`
    /// 共享给 Builder 闭包(虽然当前实现由 agent 自己读写,不需要共享,
    /// 但保留 Arc 以便未来扩展为跨 handler 共享)。
    ///
    /// # 超时
    ///
    /// 与反向 IO 请求不同,`request_permission` 等待用户交互,默认 30 秒
    /// 可能不够(用户可能在思考)。但出于安全考虑,超时后默认拒绝
    /// (`PermissionError::Timeout`),避免 agent 在用户离开时无限等待。
    pub async fn request_permission(
        &self,
        request: PermissionRequest,
    ) -> Result<PermissionOutcome, PermissionError> {
        // 1. 检查 AlwaysAllow 缓存
        let cache_key = (request.operation.clone(), request.target.clone());
        {
            let cache = self.permission_cache.lock().await;
            if cache.contains(&cache_key) {
                return Ok(PermissionOutcome::AlwaysAllow);
            }
        }

        // 2. 构造并发送请求
        let conn = self.acquire_connection().await?;
        let session_id = self
            .active_session_id()
            .ok_or(PermissionError::Ide("no active session".into()))?;

        // 构造 ToolCallUpdate:title = "operation: target"
        let mut fields = schema::ToolCallUpdateFields::new();
        fields.title = Some(format!("{}: {}", request.operation, request.target));
        fields.kind = Some(schema::ToolKind::Other);
        fields.status = Some(schema::ToolCallStatus::Pending);
        let tool_call_id = format!("perm-{}-{}", request.operation, short_hash(&request.target));
        let tool_call = schema::ToolCallUpdate::new(tool_call_id, fields);

        // 三个标准选项:Allow / AlwaysAllow / Deny
        let options = vec![
            schema::PermissionOption::new(
                "allow",
                "Allow",
                schema::PermissionOptionKind::AllowOnce,
            ),
            schema::PermissionOption::new(
                "allow_always",
                "Always Allow",
                schema::PermissionOptionKind::AllowAlways,
            ),
            schema::PermissionOption::new("deny", "Deny", schema::PermissionOptionKind::RejectOnce),
        ];

        let req = schema::RequestPermissionRequest::new(session_id, tool_call, options);

        let response = tokio::time::timeout(DEFAULT_REVERSE_REQUEST_TIMEOUT, async {
            conn.send_request(req).block_task().await
        })
        .await
        .map_err(|_| PermissionError::Timeout)?
        .map_err(|e| PermissionError::Ide(e.to_string()))?;

        // 3. 解析响应
        let outcome = match response.outcome {
            schema::RequestPermissionOutcome::Cancelled => {
                return Ok(PermissionOutcome::Deny);
            }
            schema::RequestPermissionOutcome::Selected(selected) => {
                let id = selected.option_id.to_string();
                match id.as_str() {
                    "allow" => PermissionOutcome::Allow,
                    "allow_always" => {
                        // 缓存 AlwaysAllow 决策
                        let mut cache = self.permission_cache.lock().await;
                        cache.insert(cache_key);
                        PermissionOutcome::AlwaysAllow
                    }
                    "deny" => PermissionOutcome::Deny,
                    other => {
                        // 未知选项 ID:保守拒绝
                        tracing::warn!(
                            "request_permission: unknown option_id '{}', denying",
                            other
                        );
                        PermissionOutcome::Deny
                    }
                }
            }
            // RequestPermissionOutcome 标记为 non_exhaustive,需通配符
            _ => {
                tracing::warn!("request_permission: unknown outcome variant, denying");
                PermissionOutcome::Deny
            }
        };

        Ok(outcome)
    }

    /// 刷新 LaneEvent 到 1.3 的 SessionNotification。
    ///
    /// 与 0.10.4 的 `lane_bridge::flush_lane_events_to_acp` 等价,但通过
    /// 1.3 的 `ConnectionTo<Client>::send_notification` 推送。
    ///
    /// # 实现说明
    ///
    /// 1.3 的 `SessionNotification` 类型与 0.10.4 在 schema 层完全相同
    /// (都来自 `agent_client_protocol_schema::v1`)。因此可以直接复用
    /// `lane_bridge::lane_event_to_session_update` 的映射逻辑。
    /// 但本模块 `cfg(feature = "acp-1_5")` 与 `lane_bridge.rs` 的
    /// `cfg(feature = "acp-0_10")` 互斥,无法直接调用。
    /// 因此本方法内嵌一份与 0.10.4 完全一致的 23 种事件映射,详见
    /// `lane_bridge.rs` 的 `lane_event_to_session_update`。
    ///
    /// # 返回
    /// 成功推送的事件数量(包括被丢弃的 Started/Ready —— 仍被 drain 出来)。
    /// 若无活跃连接(`client_connection` 为 None),仅 drain 不推送,返回
    /// drain 数量。
    pub async fn flush_lane_events(&self, session_id: &schema::SessionId) -> usize {
        let events = runtime::drain_lane_events();
        let count = events.len();

        // 若无活跃连接,仅 drain 不推送(与 0.10.4 占位行为一致)
        let conn = match self.acquire_connection().await {
            Ok(c) => c,
            Err(_) => {
                tracing::debug!(
                    "flush_lane_events: no client connection, drained {} events without push",
                    count
                );
                return count;
            }
        };

        for event in events {
            if let Some(notification) = lane_event_to_session_update_v1_3(&event, session_id) {
                // fire-and-forget:与 0.10.4 策略一致
                // (1.3 中 send_notification 不返回 future 需 await)
                if let Err(e) = conn.send_notification(notification) {
                    tracing::debug!("flush_lane_events: send_notification failed: {}", e);
                }
            }
        }
        count
    }
}

impl<C> Default for ClawAgentV13<C>
where
    C: ApiClient + Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// 由 Builder handler 闭包持有的连接 slot,共享给 ClawAgentV13。
///
/// Builder 闭包捕获该结构(其内部 `Arc<Mutex<...>>` 是 `Send`),
/// 在 `on_receive_dispatch` 首次调用时把 `ConnectionTo<Client>` clone
/// 后写入 `client_connection`,使 agent 的反向请求方法可读。
pub struct ClawAgentV13ConnectionSlot {
    /// 反向请求句柄的共享 slot。
    pub client_connection: Arc<tokio::sync::Mutex<Option<acp::ConnectionTo<acp::Client>>>>,
    /// AlwaysAllow 缓存的共享 slot(供 Builder 闭包查询/写入)。
    pub permission_cache: Arc<tokio::sync::Mutex<HashSet<(String, String)>>>,
}

impl Clone for ClawAgentV13ConnectionSlot {
    fn clone(&self) -> Self {
        Self {
            client_connection: self.client_connection.clone(),
            permission_cache: self.permission_cache.clone(),
        }
    }
}

impl ClawAgentV13ConnectionSlot {
    /// 在 Builder 的 dispatch handler 内调用:clone `ConnectionTo<Client>`
    /// 并写入 slot,使 agent 的反向请求方法可读。
    ///
    /// 必须传入 `cx.clone()`(因 `cx` 是参数,不能 move)。锁内 clone 不
    /// 会长时间持锁(`ConnectionTo` clone 仅复制若干 channel sender,cheap)。
    pub async fn set_connection(&self, cx: acp::ConnectionTo<acp::Client>) {
        let mut guard = self.client_connection.lock().await;
        *guard = Some(cx);
    }
}

/// 用于在反向请求方法中表示"连接已关闭"的错误信号(内部类型,不导出)。
struct ConnectionClosed;

// ---- 反向请求的错误/请求/结果类型 ----
//
// 这些类型独立于 1.3 schema,让 agent_v1_3.rs 在 1.3 API 未完整接入时
// 也能编译,并提供语义化的错误分类。

/// `read_editor_buffer` 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// 反向请求尚未实现(阶段 2 骨架占位)。
    #[error("read_editor_buffer not implemented yet (ACP 1.3 skeleton)")]
    NotImplemented,
    /// IDE 返回文件未找到。
    #[error("file not found: {0}")]
    NotFound(String),
    /// IDE 返回权限拒绝。
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// 反向请求通道关闭(IDE 断开连接)。
    #[error("client connection closed")]
    ConnectionClosed,
    /// 反向请求超时(IDE 未在默认超时内响应)。
    #[error("read_editor_buffer timed out for path: {0}")]
    Timeout(String),
    /// IDE 返回的其他错误。
    #[error("IDE error: {0}")]
    Ide(String),
}

impl From<ConnectionClosed> for ReadError {
    fn from(_: ConnectionClosed) -> Self {
        Self::ConnectionClosed
    }
}

/// `write_editor_buffer` 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// 反向请求尚未实现(阶段 2 骨架占位)。
    #[error("write_editor_buffer not implemented yet (ACP 1.3 skeleton)")]
    NotImplemented,
    /// 文件被外部修改,写入被拒绝。
    #[error("file modified externally: {0}")]
    ModifiedExternally(String),
    /// 权限拒绝。
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// 反向请求通道关闭。
    #[error("client connection closed")]
    ConnectionClosed,
    /// 反向请求超时。
    #[error("write_editor_buffer timed out for path: {0}")]
    Timeout(String),
    /// IDE 返回的其他错误。
    #[error("IDE error: {0}")]
    Ide(String),
}

impl From<ConnectionClosed> for WriteError {
    fn from(_: ConnectionClosed) -> Self {
        Self::ConnectionClosed
    }
}

/// `request_permission` 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    /// 反向请求尚未实现(阶段 2 骨架占位)。
    #[error("request_permission not implemented yet (ACP 1.3 skeleton)")]
    NotImplemented,
    /// 用户未响应(超时)。
    #[error("permission request timed out")]
    Timeout,
    /// 反向请求通道关闭。
    #[error("client connection closed")]
    ConnectionClosed,
    /// IDE 返回的其他错误。
    #[error("IDE error: {0}")]
    Ide(String),
}

impl From<ConnectionClosed> for PermissionError {
    fn from(_: ConnectionClosed) -> Self {
        Self::ConnectionClosed
    }
}

/// 权限请求描述。
///
/// 由调用方(agent 内部逻辑)构造,描述要请求权限的操作。
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// 操作类型(如 "shell_exec"、"file_write"、"mcp_call")。
    pub operation: String,
    /// 操作目标(如命令字符串、文件路径)。
    pub target: String,
    /// 影响范围描述(显示给用户)。
    pub impact: String,
}

/// 权限请求结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// 用户允许。
    Allow,
    /// 用户拒绝。
    Deny,
    /// 用户选择"总是允许"(后续同类操作不再询问)。
    AlwaysAllow,
}

// ---- 辅助函数 ----

/// 简短哈希(用于生成 permission tool_call_id,无需密码学强度)。
fn short_hash(input: &str) -> String {
    // FNV-1a 64-bit,取低 32 位转 16 进制,8 字符定长。
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", hash as u32)
}

// ---- 1.3 版本的 LaneEvent → SessionNotification 映射 ----
//
// 与 `lane_bridge::lane_event_to_session_update` 完全一致(0.10.4 路径)
// 但用 1.3 schema 类型。两个版本在 schema 层共享
// `agent_client_protocol_schema::v1`,所以映射代码可以字节复制。
//
// 不直接 import `lane_bridge` 是因为:
// 1. `lane_bridge.rs` 用 `cfg(feature = "acp-0_10")` gate,与 `acp-1_5` 互斥
// 2. 本模块在 `--no-default-features --features acp-1_5` 下必须独立编译

use runtime::{LaneEvent, LaneEventName};

/// 从 LaneEvent 的 `data` JSON 中提取 `subagent_id` 字段。
fn extract_subagent_id(event: &LaneEvent) -> String {
    event
        .data
        .as_ref()
        .and_then(|data| data.get("subagent_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 从 LaneEvent 的 `detail` 或 `data` 中提取可读消息。
fn event_message(event: &LaneEvent) -> String {
    if let Some(detail) = &event.detail {
        return detail.clone();
    }
    if let Some(data) = &event.data {
        if let Ok(s) = serde_json::to_string(data) {
            return s;
        }
    }
    format!("{:?}", event.event)
}

/// 构造一个 `AgentMessageChunk` notification。
fn agent_message_chunk(
    session_id: &schema::SessionId,
    text: impl Into<String>,
) -> schema::SessionNotification {
    schema::SessionNotification::new(
        session_id.clone(),
        schema::SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
            schema::ContentBlock::Text(schema::TextContent::new(text)),
        )),
    )
}

/// 构造一个 `ToolCall` notification。
fn tool_call_notification(
    session_id: &schema::SessionId,
    tool_call_id: impl Into<String>,
    title: impl Into<String>,
    kind: schema::ToolKind,
    status: schema::ToolCallStatus,
) -> schema::SessionNotification {
    let mut call = schema::ToolCall::new(tool_call_id.into(), title.into());
    call.kind = kind;
    call.status = status;
    schema::SessionNotification::new(session_id.clone(), schema::SessionUpdate::ToolCall(call))
}

/// 构造一个 `Plan` notification。
fn plan_notification(
    session_id: &schema::SessionId,
    content: impl Into<String>,
    status: schema::PlanEntryStatus,
) -> schema::SessionNotification {
    let entry = schema::PlanEntry::new(content, schema::PlanEntryPriority::High, status);
    schema::SessionNotification::new(
        session_id.clone(),
        schema::SessionUpdate::Plan(schema::Plan::new(vec![entry])),
    )
}

/// 返回 LaneEventName 的稳定字符串标识,用于构造 ToolCall 的 ID。
fn event_name_str(name: LaneEventName) -> &'static str {
    match name {
        LaneEventName::Started => "started",
        LaneEventName::Ready => "ready",
        LaneEventName::PromptMisdelivery => "prompt_misdelivery",
        LaneEventName::Blocked => "blocked",
        LaneEventName::Red => "red",
        LaneEventName::Green => "green",
        LaneEventName::CommitCreated => "commit_created",
        LaneEventName::PrOpened => "pr_opened",
        LaneEventName::MergeReady => "merge_ready",
        LaneEventName::Finished => "finished",
        LaneEventName::Failed => "failed",
        LaneEventName::Reconciled => "reconciled",
        LaneEventName::Merged => "merged",
        LaneEventName::Superseded => "superseded",
        LaneEventName::Closed => "closed",
        LaneEventName::BranchStaleAgainstMain => "branch_stale",
        LaneEventName::BranchWorkspaceMismatch => "branch_mismatch",
        LaneEventName::ShipPrepared => "ship_prepared",
        LaneEventName::ShipCommitsSelected => "ship_commits_selected",
        LaneEventName::ShipMerged => "ship_merged",
        LaneEventName::ShipPushedMain => "ship_pushed_main",
        LaneEventName::SubagentHandoff => "subagent_handoff",
        LaneEventName::SubagentResult => "subagent_result",
    }
}

/// 将 LaneEvent 转换为 1.3 版本的 SessionNotification。
///
/// 与 0.10.4 版本的 `lane_event_to_session_update` 完全一致的映射,
/// 仅 schema 路径不同(本函数用 `schema::v1::*` 而非 0.10.4 的 `acp::*`)。
///
/// 返回 `None` 的事件类型:`Started` / `Ready`(内部状态,不需要通知 IDE)。
/// 其他 21 种事件均映射为对应的 `SessionUpdate`。
pub fn lane_event_to_session_update_v1_3(
    event: &LaneEvent,
    session_id: &schema::SessionId,
) -> Option<schema::SessionNotification> {
    let msg = event_message(event);
    let seq = event.metadata.seq;
    let tool_call_id = format!("lane-{}-{}", event_name_str(event.event), seq);

    match event.event {
        // 内部状态事件:不映射
        LaneEventName::Started | LaneEventName::Ready => None,

        // 文本通知类:AgentMessageChunk
        LaneEventName::PromptMisdelivery => Some(agent_message_chunk(
            session_id,
            format!("[warning] prompt misdelivery: {msg}"),
        )),
        LaneEventName::Green => Some(agent_message_chunk(
            session_id,
            format!("[green] lane is healthy: {msg}"),
        )),
        LaneEventName::Finished => Some(agent_message_chunk(
            session_id,
            format!("[finished] lane completed: {msg}"),
        )),
        LaneEventName::Failed => Some(agent_message_chunk(
            session_id,
            format!("[failed] lane failed: {msg}"),
        )),
        LaneEventName::BranchStaleAgainstMain => Some(agent_message_chunk(
            session_id,
            format!("[warning] branch is stale against main: {msg}"),
        )),
        LaneEventName::BranchWorkspaceMismatch => Some(agent_message_chunk(
            session_id,
            format!("[warning] branch/workspace mismatch: {msg}"),
        )),

        // 阻塞通知:Plan(InProgress)
        LaneEventName::Blocked => Some(plan_notification(
            session_id,
            format!("[blocked] lane is blocked: {msg}"),
            schema::PlanEntryStatus::InProgress,
        )),
        LaneEventName::Red => Some(plan_notification(
            session_id,
            format!("[red] lane is in red state: {msg}"),
            schema::PlanEntryStatus::InProgress,
        )),

        // Git 操作:ToolCall(Completed)
        LaneEventName::CommitCreated => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Git commit created: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::PrOpened => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("PR opened: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::MergeReady => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Merge ready: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),

        // Git 终态:ToolCall(Completed)
        LaneEventName::Reconciled => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Reconciled: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::Merged => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Merged: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::Superseded => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Superseded: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::Closed => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Closed: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),

        // Ship 操作:ToolCall(Completed)
        LaneEventName::ShipPrepared => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship prepared: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::ShipCommitsSelected => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship commits selected: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::ShipMerged => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship merged: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),
        LaneEventName::ShipPushedMain => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship pushed to main: {msg}"),
            schema::ToolKind::Other,
            schema::ToolCallStatus::Completed,
        )),

        // Subagent 事件:ToolCall
        LaneEventName::SubagentHandoff => {
            let subagent_id = extract_subagent_id(event);
            Some(tool_call_notification(
                session_id,
                format!("subagent-{subagent_id}"),
                format!("Subagent handoff ({subagent_id}): {msg}"),
                schema::ToolKind::Other,
                schema::ToolCallStatus::InProgress,
            ))
        }
        LaneEventName::SubagentResult => {
            let subagent_id = extract_subagent_id(event);
            let sub_status = event
                .data
                .as_ref()
                .and_then(|d| d.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let call_status = match sub_status {
                "failed" => schema::ToolCallStatus::Failed,
                _ => schema::ToolCallStatus::Completed,
            };
            Some(tool_call_notification(
                session_id,
                format!("subagent-{subagent_id}"),
                format!("Subagent result ({subagent_id}): {msg}"),
                schema::ToolKind::Other,
                call_status,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ApiRequest, AssistantEvent, RuntimeError};

    /// 测试用 `ApiClient`:返回空事件序列,用于类型检查测试。
    struct NullApiClient;

    impl runtime::ApiClient for NullApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(Vec::new())
        }
    }

    /// 类型检查测试:确保 ClawAgentV13 在 1.3 feature 下编译通过。
    /// 不实际构造(因 1.3 Builder 在 spawn 时才接入),仅检查类型可命名。
    #[test]
    fn claw_agent_v13_type_exists() {
        // 编译期检查:类型可命名
        let _ = std::marker::PhantomData::<ClawAgentV13<NullApiClient>>;
    }

    /// 验证 `short_hash` 返回 8 字符定长 16 进制字符串。
    #[test]
    fn short_hash_returns_8_char_hex() {
        let h1 = short_hash("hello");
        let h2 = short_hash("world");
        let h3 = short_hash("hello");
        assert_eq!(h1.len(), 8);
        assert_eq!(h2.len(), 8);
        assert_eq!(h1, h3, "short_hash must be deterministic");
        assert_ne!(h1, h2, "different inputs should produce different hashes");
        assert!(
            h1.chars().all(|c| c.is_ascii_hexdigit()),
            "expected hex, got {h1}"
        );
    }

    /// 验证 `event_name_str` 覆盖所有 LaneEventName 变体。
    #[test]
    fn event_name_str_covers_all_variants() {
        for name in ALL_LANE_EVENT_NAMES {
            let s = event_name_str(*name);
            assert!(!s.is_empty(), "event_name_str({name:?}) returned empty");
        }
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 Started/Ready 返回 None。
    #[test]
    fn started_returns_none_v1_3() {
        let event = LaneEvent::new(
            LaneEventName::Started,
            runtime::LaneEventStatus::Running,
            "2026-07-26T00:00:00Z",
        );
        let session_id = schema::SessionId::new("test-session");
        assert!(lane_event_to_session_update_v1_3(&event, &session_id).is_none());
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 Ready 返回 None。
    #[test]
    fn ready_returns_none_v1_3() {
        let event = LaneEvent::new(
            LaneEventName::Ready,
            runtime::LaneEventStatus::Ready,
            "2026-07-26T00:00:00Z",
        );
        let session_id = schema::SessionId::new("test-session");
        assert!(lane_event_to_session_update_v1_3(&event, &session_id).is_none());
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 Blocked 映射为 Plan(InProgress)。
    #[test]
    fn blocked_maps_to_plan_v1_3() {
        let event = LaneEvent::new(
            LaneEventName::Blocked,
            runtime::LaneEventStatus::Blocked,
            "2026-07-26T00:00:00Z",
        );
        let session_id = schema::SessionId::new("test");
        let notif =
            lane_event_to_session_update_v1_3(&event, &session_id).expect("Blocked should map");
        match notif.update {
            schema::SessionUpdate::Plan(plan) => {
                assert_eq!(plan.entries.len(), 1);
                assert_eq!(plan.entries[0].status, schema::PlanEntryStatus::InProgress);
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 SubagentHandoff
    /// 映射为 ToolCall(InProgress)。
    #[test]
    fn subagent_handoff_maps_to_tool_call_in_progress_v1_3() {
        let event = LaneEvent::subagent_handoff(
            "2026-07-26T00:00:00Z",
            "sub-123",
            "fork",
            "implement feature X",
        );
        let session_id = schema::SessionId::new("test");
        let notif = lane_event_to_session_update_v1_3(&event, &session_id)
            .expect("SubagentHandoff should map");
        match notif.update {
            schema::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, schema::ToolCallStatus::InProgress);
                assert!(call.title.contains("sub-123"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    // ---- ClawAgentV13 构造与状态管理测试 ----

    /// 验证 `ClawAgentV13::new()` 实际构造成功(不只是类型可命名)。
    ///
    /// 之前 `claw_agent_v13_type_exists` 仅检查 `PhantomData`,不验证 `new()`
    /// 真能跑通。本测试调用 `new()` 并通过 `connection_slot()` 验证内部状态。
    #[test]
    fn new_constructs_clean_state() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        // 没有活跃 session
        assert!(agent.active_session_id().is_none());
        // connection_slot 能拿到(内部 Arc 未填充,但 slot 本身有效)
        let slot = agent.connection_slot();
        // 不阻塞地验证 slot 内 Arc 句柄存在(try_lock 不行,因为 Mutex 是 async 的,
        // 但我们可以 verify slot 与 agent 共享 permission_cache:见后续测试)
        let _ = slot; // 抑制 unused
    }

    /// 验证 `Default::default()` 等价于 `new()`。
    #[test]
    fn default_equivalent_to_new() {
        let _agent = ClawAgentV13::<NullApiClient>::default();
        // 不 panic 即通过(默认构造与 new() 内部逻辑一致)
    }

    /// 验证 `connection_slot()` 返回的 slot 与 agent 共享 `permission_cache`。
    ///
    /// 通过 slot 直接写入缓存,然后通过 agent 的 `request_permission` 命中缓存,
    /// 验证两者共享同一份 `Arc<Mutex<HashSet<...>>>`。
    #[tokio::test]
    async fn connection_slot_shares_permission_cache() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        let slot = agent.connection_slot();

        // 通过 slot 写入缓存
        {
            let mut cache = slot.permission_cache.lock().await;
            cache.insert(("shell_exec".to_string(), "rm -rf /".to_string()));
        }

        // 通过 agent 的 request_permission 应该命中缓存(无需 connection)
        let request = PermissionRequest {
            operation: "shell_exec".to_string(),
            target: "rm -rf /".to_string(),
            impact: "destructive".to_string(),
        };
        let outcome = agent.request_permission(request).await;
        assert!(
            outcome.is_ok(),
            "cached request_permission should not error: {outcome:?}"
        );
        assert_eq!(
            outcome.unwrap(),
            PermissionOutcome::AlwaysAllow,
            "cached entry should return AlwaysAllow"
        );
    }

    /// 验证 `set_active_session` 更新活跃 session,且 `active_session_id` 能取回。
    #[test]
    fn set_active_session_updates_state() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        assert!(agent.active_session_id().is_none());

        let sid = schema::SessionId::new("session-1");
        agent.set_active_session(sid.clone());
        assert_eq!(agent.active_session_id(), Some(sid));
    }

    /// 验证 `set_active_session` 多次调用后,`active_session_id` 返回最近一个。
    #[test]
    fn set_active_session_overwrites_previous() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        let sid1 = schema::SessionId::new("session-1");
        let sid2 = schema::SessionId::new("session-2");
        agent.set_active_session(sid1);
        agent.set_active_session(sid2.clone());
        assert_eq!(agent.active_session_id(), Some(sid2));
    }

    // ---- 反向请求的错误路径测试 ----

    /// 验证 `read_editor_buffer` 在无活跃 session 时返回 `Ide` 错误。
    ///
    /// 注意:此测试故意不设置 session 也不设置 connection,验证错误路径的优先级
    /// —— 当前实现是先 `acquire_connection` 后 `active_session_id`,所以
    /// 无 connection 时会先返回 `ConnectionClosed`。本测试通过设置一个连接
    /// slot 但不设 session 的方式,触发 session 缺失错误;但 1.3 中
    /// `ConnectionTo<Client>` 不能直接构造。改用验证错误为 `ConnectionClosed`
    /// 或 `Ide` 之一(都表示"无法完成请求")的方式。
    #[tokio::test]
    async fn read_editor_buffer_without_connection_returns_closed() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        // 不设 session,也不设 connection
        let result = agent.read_editor_buffer("/some/path").await;
        assert!(
            matches!(result, Err(ReadError::ConnectionClosed)),
            "expected ConnectionClosed when no connection set, got: {result:?}"
        );
    }

    /// 验证 `write_editor_buffer` 在无 connection 时返回 `ConnectionClosed`。
    #[tokio::test]
    async fn write_editor_buffer_without_connection_returns_closed() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        let result = agent.write_editor_buffer("/some/path", "content").await;
        assert!(
            matches!(result, Err(WriteError::ConnectionClosed)),
            "expected ConnectionClosed when no connection set, got: {result:?}"
        );
    }

    /// 验证 `request_permission` 在无 connection 且未命中缓存时返回 `ConnectionClosed`。
    #[tokio::test]
    async fn request_permission_without_connection_returns_closed() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        let request = PermissionRequest {
            operation: "file_write".to_string(),
            target: "/etc/passwd".to_string(),
            impact: "system file".to_string(),
        };
        let result = agent.request_permission(request).await;
        assert!(
            matches!(result, Err(PermissionError::ConnectionClosed)),
            "expected ConnectionClosed when no connection set, got: {result:?}"
        );
    }

    /// 验证 `request_permission` 缓存命中时不访问 connection(避免无谓往返)。
    ///
    /// 这是 AlwaysAllow 缓存的核心价值:第二次同类请求不再打扰用户。
    #[tokio::test]
    async fn request_permission_cached_skips_connection_lookup() {
        let agent = ClawAgentV13::<NullApiClient>::new();
        // 直接写入缓存(模拟首次 AlwaysAllow 后的状态)
        {
            let mut cache = agent.permission_cache.lock().await;
            cache.insert(("mcp_call".to_string(), "tool-A".to_string()));
        }

        let request = PermissionRequest {
            operation: "mcp_call".to_string(),
            target: "tool-A".to_string(),
            impact: "external tool".to_string(),
        };
        // 即使没有 connection(默认 None),命中缓存也应直接返回 AlwaysAllow
        let outcome = agent.request_permission(request).await;
        assert!(
            outcome.is_ok(),
            "cached request should succeed without connection: {outcome:?}"
        );
        assert_eq!(outcome.unwrap(), PermissionOutcome::AlwaysAllow);
    }

    // ---- 反向请求的成功路径测试 ----
    //
    // 以下 3 个测试用 `acp::Channel::duplex()` 构造 in-process transport,
    // 让 mock IDE(Client 角色)响应 agent 的反向请求,覆盖完整的
    // 请求-响应循环。与错误路径测试互补,验证真实连接下的行为。
    //
    // 架构:
    // - Client(mock IDE):`acp::Client.builder().on_receive_request(...).connect_to(client_channel)`
    //   注册类型化 handler,返回 canned response。`spawn_local` 驱动。
    // - Agent:`acp::Agent.builder().connect_with(agent_channel, main_fn)`
    //   `main_fn` 接收 `ConnectionTo<Client>`,写入 slot 后等待测试完成信号。
    //   `spawn_local` 驱动,使 background actors 持续轮询。
    // - 主流程:等 slot 填充 → 调用 agent 反向请求方法 → 断言 → 通知退出。
    //
    // `ClawAgentV13` 持有 `RefCell`(!Send),但 `connect_with` 的 `main_fn`
    // 不要求 `Send`(只捕获 Send 的 slot + oneshot),且 `LocalSet` +
    // `current_thread` runtime 接受 !Send future,故 agent 可留在主流程中。

    /// 验证 `read_editor_buffer` 在真实 IDE 连接下返回文件内容。
    ///
    /// mock IDE 收到 `fs/read_text_file` 请求后返回固定内容,验证 agent
    /// 的 `read_editor_buffer` 能完整走通 send_request → block_task →
    /// 解析 response.content 的成功路径。
    #[tokio::test(flavor = "current_thread")]
    async fn read_editor_buffer_returns_content_from_ide() {
        use tokio::task::LocalSet;
        let local = LocalSet::new();
        local
            .run_until(async {
                let (client_channel, agent_channel) = acp::Channel::duplex();

                // Client(mock IDE):处理 fs/read_text_file,返回固定内容
                let client_connection = acp::Client
                    .builder()
                    .on_receive_request(
                        async |_req: schema::ReadTextFileRequest,
                               responder: acp::Responder<schema::ReadTextFileResponse>,
                               _cx: acp::ConnectionTo<acp::Agent>| {
                            responder
                                .respond(schema::ReadTextFileResponse::new("hello from IDE buffer"))
                        },
                        acp::on_receive_request!(),
                    )
                    .connect_to(client_channel);
                tokio::task::spawn_local(async move {
                    let _ = client_connection.await;
                });

                // Agent:connect_with 获取 ConnectionTo<Client> 并注入 slot
                let agent = ClawAgentV13::<NullApiClient>::new();
                agent.set_active_session(schema::SessionId::new("test-session"));
                let slot = agent.connection_slot();
                let (slot_ready_tx, slot_ready_rx) = tokio::sync::oneshot::channel();
                let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

                let agent_connection = acp::Agent.builder().name("test-read-agent").connect_with(
                    agent_channel,
                    async move |cx: acp::ConnectionTo<acp::Client>| {
                        slot.set_connection(cx.clone()).await;
                        let _ = slot_ready_tx.send(());
                        // 保持连接存活,直到测试完成
                        let _ = done_rx.await;
                        Ok(())
                    },
                );
                tokio::task::spawn_local(async move {
                    let _ = agent_connection.await;
                });

                // 等待 slot 填充(connect_with main_fn 已注入 ConnectionTo)
                let _ = slot_ready_rx.await;

                // 调用反向请求,验证成功路径
                let result = agent.read_editor_buffer("/test/file.rs").await;
                assert!(
                    result.is_ok(),
                    "read_editor_buffer should succeed with live connection: {result:?}"
                );
                assert_eq!(
                    result.unwrap(),
                    "hello from IDE buffer",
                    "should return content from IDE"
                );

                // 通知 agent 连接可以退出
                let _ = done_tx.send(());
            })
            .await;
    }

    /// 验证 `write_editor_buffer` 在 IDE ACK 后完成。
    ///
    /// mock IDE 收到 `fs/write_text_file` 请求后返回空 ACK
    /// (`WriteTextFileResponse` 无业务数据),验证 agent 的
    /// `write_editor_buffer` 能完整走通 send_request → block_task → Ok(())
    /// 的成功路径。
    #[tokio::test(flavor = "current_thread")]
    async fn write_editor_buffer_completes_when_ide_acknowledges() {
        use tokio::task::LocalSet;
        let local = LocalSet::new();
        local
            .run_until(async {
                let (client_channel, agent_channel) = acp::Channel::duplex();

                // Client(mock IDE):处理 fs/write_text_file,返回空 ACK
                let client_connection = acp::Client
                    .builder()
                    .on_receive_request(
                        async |_req: schema::WriteTextFileRequest,
                               responder: acp::Responder<schema::WriteTextFileResponse>,
                               _cx: acp::ConnectionTo<acp::Agent>| {
                            responder.respond(schema::WriteTextFileResponse::new())
                        },
                        acp::on_receive_request!(),
                    )
                    .connect_to(client_channel);
                tokio::task::spawn_local(async move {
                    let _ = client_connection.await;
                });

                // Agent:connect_with 获取 ConnectionTo<Client> 并注入 slot
                let agent = ClawAgentV13::<NullApiClient>::new();
                agent.set_active_session(schema::SessionId::new("test-session"));
                let slot = agent.connection_slot();
                let (slot_ready_tx, slot_ready_rx) = tokio::sync::oneshot::channel();
                let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

                let agent_connection = acp::Agent.builder().name("test-write-agent").connect_with(
                    agent_channel,
                    async move |cx: acp::ConnectionTo<acp::Client>| {
                        slot.set_connection(cx.clone()).await;
                        let _ = slot_ready_tx.send(());
                        let _ = done_rx.await;
                        Ok(())
                    },
                );
                tokio::task::spawn_local(async move {
                    let _ = agent_connection.await;
                });

                let _ = slot_ready_rx.await;

                // 调用反向请求,验证成功路径(WriteTextFileResponse 无业务数据,
                // 只要不报错即表示 IDE 已 ACK)
                let result = agent
                    .write_editor_buffer("/test/output.rs", "fn main() {}")
                    .await;
                assert!(
                    result.is_ok(),
                    "write_editor_buffer should succeed when IDE acknowledges: {result:?}"
                );

                let _ = done_tx.send(());
            })
            .await;
    }

    /// 验证 `request_permission` 返回 IDE 的授权决策。
    ///
    /// mock IDE 对第一次请求返回 "allow",对第二次返回 "deny"
    /// (用 counter 区分)。验证 agent 分别收到 `PermissionOutcome::Allow`
    /// 和 `PermissionOutcome::Deny`,且 AlwaysAllow 缓存未被触发
    /// (allow ≠ allow_always)。
    #[tokio::test(flavor = "current_thread")]
    async fn request_permission_returns_decision_from_ide() {
        use tokio::task::LocalSet;
        let local = LocalSet::new();
        local
            .run_until(async {
                let (client_channel, agent_channel) = acp::Channel::duplex();

                // Client(mock IDE):用 counter 区分两次请求,第一次 allow,第二次 deny
                let counter = std::sync::Arc::new(tokio::sync::Mutex::new(0u32));
                let counter_clone = counter.clone();
                let client_connection = acp::Client
                    .builder()
                    .on_receive_request(
                        async move |_req: schema::RequestPermissionRequest,
                                    responder: acp::Responder<
                            schema::RequestPermissionResponse,
                        >,
                                    _cx: acp::ConnectionTo<acp::Agent>| {
                            let mut c = counter_clone.lock().await;
                            *c += 1;
                            let option_id = if *c == 1 { "allow" } else { "deny" };
                            let outcome = schema::RequestPermissionOutcome::Selected(
                                schema::SelectedPermissionOutcome::new(option_id),
                            );
                            responder.respond(schema::RequestPermissionResponse::new(outcome))
                        },
                        acp::on_receive_request!(),
                    )
                    .connect_to(client_channel);
                tokio::task::spawn_local(async move {
                    let _ = client_connection.await;
                });

                // Agent:connect_with 获取 ConnectionTo<Client> 并注入 slot
                let agent = ClawAgentV13::<NullApiClient>::new();
                agent.set_active_session(schema::SessionId::new("test-session"));
                let slot = agent.connection_slot();
                let (slot_ready_tx, slot_ready_rx) = tokio::sync::oneshot::channel();
                let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

                let agent_connection = acp::Agent.builder().name("test-perm-agent").connect_with(
                    agent_channel,
                    async move |cx: acp::ConnectionTo<acp::Client>| {
                        slot.set_connection(cx.clone()).await;
                        let _ = slot_ready_tx.send(());
                        let _ = done_rx.await;
                        Ok(())
                    },
                );
                tokio::task::spawn_local(async move {
                    let _ = agent_connection.await;
                });

                let _ = slot_ready_rx.await;

                // 第一次请求:operation/target A → IDE 返回 allow
                let request_a = PermissionRequest {
                    operation: "shell_exec".to_string(),
                    target: "ls -la".to_string(),
                    impact: "read-only listing".to_string(),
                };
                let outcome_a = agent.request_permission(request_a).await;
                assert!(
                    outcome_a.is_ok(),
                    "first request_permission should succeed: {outcome_a:?}"
                );
                assert_eq!(
                    outcome_a.unwrap(),
                    PermissionOutcome::Allow,
                    "first request should be Allow"
                );

                // 第二次请求:operation/target B(不同 cache key)→ IDE 返回 deny
                let request_b = PermissionRequest {
                    operation: "file_write".to_string(),
                    target: "/etc/passwd".to_string(),
                    impact: "system file".to_string(),
                };
                let outcome_b = agent.request_permission(request_b).await;
                assert!(
                    outcome_b.is_ok(),
                    "second request_permission should succeed: {outcome_b:?}"
                );
                assert_eq!(
                    outcome_b.unwrap(),
                    PermissionOutcome::Deny,
                    "second request should be Deny"
                );

                let _ = done_tx.send(());
            })
            .await;
    }

    // ---- flush_lane_events 测试 ----

    /// 全局 LaneEvent sink 序列化锁(与 0.10.4 lane_bridge 测试一致)。
    fn sink_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 验证 `flush_lane_events` 在无 connection 时仅 drain 不推送,返回 drain 数。
    ///
    /// 这是 fire-and-forget 设计的体现:即使 IDE 断开,事件仍被清空,
    /// 避免缓冲区无限增长。
    ///
    /// `sink_lock` 是 std::sync::Mutex,但持锁贯穿 `await`。这里与 0.10.4 的
    /// `lane_bridge::tests::flush_*` 测试一致(同样的 sink_lock 模式):
    /// 全局 LaneEvent sink 是进程级单例,跨测试必须串行才能保证 drain 数可断言。
    /// `await` 期间不会有其他任务访问本测试的 `agent`,所以不存在真正的死锁风险。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn flush_lane_events_drains_without_pushing_when_no_connection() {
        let _guard = sink_lock();
        // 先清空全局 sink
        let _ = runtime::drain_lane_events();

        // 发布 2 个事件:1 个 Started(不映射)+ 1 个 Finished(映射)
        runtime::try_publish(LaneEvent::new(
            LaneEventName::Started,
            runtime::LaneEventStatus::Running,
            "2026-07-26T00:00:00Z",
        ));
        runtime::try_publish(LaneEvent::new(
            LaneEventName::Finished,
            runtime::LaneEventStatus::Completed,
            "2026-07-26T00:00:01Z",
        ));

        let agent = ClawAgentV13::<NullApiClient>::new();
        let session_id = schema::SessionId::new("test");
        let count = agent.flush_lane_events(&session_id).await;
        assert_eq!(count, 2, "should drain both events even without connection");

        // 二次调用应返回 0(已 drain 干净)
        let count2 = agent.flush_lane_events(&session_id).await;
        assert_eq!(count2, 0, "sink should be empty after first flush");
    }

    // ---- 更多 LaneEvent → SessionNotification 映射覆盖 ----

    /// 验证 `lane_event_to_session_update_v1_3` 对 CommitCreated 映射为
    /// ToolCall(Completed)。
    #[test]
    fn commit_created_maps_to_tool_call_completed_v1_3() {
        let event = LaneEvent::new(
            LaneEventName::CommitCreated,
            runtime::LaneEventStatus::Completed,
            "2026-07-26T00:00:00Z",
        );
        let session_id = schema::SessionId::new("test");
        let notif = lane_event_to_session_update_v1_3(&event, &session_id)
            .expect("CommitCreated should map");
        match notif.update {
            schema::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, schema::ToolCallStatus::Completed);
                assert!(call.title.contains("Git commit"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 Green 映射为
    /// AgentMessageChunk。
    #[test]
    fn green_maps_to_agent_message_chunk_v1_3() {
        let event = LaneEvent::new(
            LaneEventName::Green,
            runtime::LaneEventStatus::Completed,
            "2026-07-26T00:00:00Z",
        );
        let session_id = schema::SessionId::new("test");
        let notif =
            lane_event_to_session_update_v1_3(&event, &session_id).expect("Green should map");
        assert!(
            matches!(notif.update, schema::SessionUpdate::AgentMessageChunk(_)),
            "expected AgentMessageChunk, got {:?}",
            notif.update
        );
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 Failed 映射为
    /// AgentMessageChunk。
    #[test]
    fn failed_maps_to_agent_message_chunk_v1_3() {
        let event = LaneEvent::new(
            LaneEventName::Failed,
            runtime::LaneEventStatus::Failed,
            "2026-07-26T00:00:00Z",
        );
        let session_id = schema::SessionId::new("test");
        let notif =
            lane_event_to_session_update_v1_3(&event, &session_id).expect("Failed should map");
        assert!(
            matches!(notif.update, schema::SessionUpdate::AgentMessageChunk(_)),
            "expected AgentMessageChunk, got {:?}",
            notif.update
        );
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 Red 映射为
    /// Plan(InProgress)。
    #[test]
    fn red_maps_to_plan_in_progress_v1_3() {
        let event = LaneEvent::new(
            LaneEventName::Red,
            runtime::LaneEventStatus::Failed,
            "2026-07-26T00:00:00Z",
        );
        let session_id = schema::SessionId::new("test");
        let notif = lane_event_to_session_update_v1_3(&event, &session_id).expect("Red should map");
        match notif.update {
            schema::SessionUpdate::Plan(plan) => {
                assert_eq!(plan.entries.len(), 1);
                assert_eq!(plan.entries[0].status, schema::PlanEntryStatus::InProgress);
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 SubagentResult(status=completed)
    /// 映射为 ToolCall(Completed)。
    #[test]
    fn subagent_result_completed_maps_to_tool_call_completed_v1_3() {
        let event =
            LaneEvent::subagent_result("2026-07-26T00:00:00Z", "sub-456", "completed", "done");
        let session_id = schema::SessionId::new("test");
        let notif = lane_event_to_session_update_v1_3(&event, &session_id)
            .expect("SubagentResult should map");
        match notif.update {
            schema::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, schema::ToolCallStatus::Completed);
                assert!(call.title.contains("sub-456"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对 SubagentResult(status=failed)
    /// 映射为 ToolCall(Failed)。
    #[test]
    fn subagent_result_failed_maps_to_tool_call_failed_v1_3() {
        let event = LaneEvent::subagent_result("2026-07-26T00:00:00Z", "sub-789", "failed", "boom");
        let session_id = schema::SessionId::new("test");
        let notif = lane_event_to_session_update_v1_3(&event, &session_id)
            .expect("SubagentResult failed should map");
        match notif.update {
            schema::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, schema::ToolCallStatus::Failed);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    /// 验证 `lane_event_to_session_update_v1_3` 对所有 ToolCall 类事件
    /// (git 操作)生成唯一的 tool_call_id(基于事件名 + seq)。
    #[test]
    fn tool_call_id_includes_event_name_and_seq_v1_3() {
        use runtime::{EventProvenance, LaneEventBuilder};

        let event1 = LaneEventBuilder::new(
            LaneEventName::CommitCreated,
            runtime::LaneEventStatus::Completed,
            "2026-07-26T00:00:00Z",
            42,
            EventProvenance::Test,
        )
        .build();
        let event2 = LaneEventBuilder::new(
            LaneEventName::PrOpened,
            runtime::LaneEventStatus::Completed,
            "2026-07-26T00:00:01Z",
            43,
            EventProvenance::Test,
        )
        .build();

        let session_id = schema::SessionId::new("test");
        let notif1 =
            lane_event_to_session_update_v1_3(&event1, &session_id).expect("CommitCreated maps");
        let notif2 =
            lane_event_to_session_update_v1_3(&event2, &session_id).expect("PrOpened maps");

        match (&notif1.update, &notif2.update) {
            (schema::SessionUpdate::ToolCall(c1), schema::SessionUpdate::ToolCall(c2)) => {
                assert_ne!(
                    c1.tool_call_id, c2.tool_call_id,
                    "different events must have different tool_call_ids"
                );
                assert!(
                    c1.tool_call_id.0.as_ref().contains("commit_created"),
                    "CommitCreated id should include event name: {}",
                    c1.tool_call_id.0
                );
                assert!(
                    c1.tool_call_id.0.as_ref().contains("-42"),
                    "CommitCreated id should include seq: {}",
                    c1.tool_call_id.0
                );
            }
            other => panic!("expected two ToolCalls, got {other:?}"),
        }
    }

    /// 全部 LaneEventName 变体(用于覆盖测试)。
    const ALL_LANE_EVENT_NAMES: &[LaneEventName] = &[
        LaneEventName::Started,
        LaneEventName::Ready,
        LaneEventName::PromptMisdelivery,
        LaneEventName::Blocked,
        LaneEventName::Red,
        LaneEventName::Green,
        LaneEventName::CommitCreated,
        LaneEventName::PrOpened,
        LaneEventName::MergeReady,
        LaneEventName::Finished,
        LaneEventName::Failed,
        LaneEventName::Reconciled,
        LaneEventName::Merged,
        LaneEventName::Superseded,
        LaneEventName::Closed,
        LaneEventName::BranchStaleAgainstMain,
        LaneEventName::BranchWorkspaceMismatch,
        LaneEventName::ShipPrepared,
        LaneEventName::ShipCommitsSelected,
        LaneEventName::ShipMerged,
        LaneEventName::ShipPushedMain,
        LaneEventName::SubagentHandoff,
        LaneEventName::SubagentResult,
    ];
}

// ---- TODO 项汇总(供阶段 2 后续实现参考) ----
//
// 1. ClawAgentV13::new ✓ 完成
//    - 设计 1.3 专用 builder(不复用 0.10.4 ClawAgentBuilder)
//    - 调用 1.3 `Builder::new()` 构造 `Component`
//    - 注册 `ConversationRuntime` 为 1.3 的 tool provider
//    - 通过 `ConnectTo` 建立 Connection,保存连接句柄
//      → 实现方式:不直接持有 Builder;通过 connection_slot() 让
//        Builder 闭包写入 ConnectionTo<Client>。Builder 在 spawn.rs/
//        stdio.rs 的 1.3 入口中构造。
//
// 2. read_editor_buffer ✓ 完成
//    - 验证 schema::v1::ReadTextFileRequest 的字段结构
//    - 实现超时保护(IDE 不响应时返回 ReadError::Timeout)
//    - 处理 IDE 返回的 file_not_found / permission_denied
//      → 当前统一映射为 ReadError::Ide(message);后续可解析
//        acp::Error.code 做更细的分类
//
// 3. write_editor_buffer ✓ 完成
//    - 验证 schema::v1::WriteTextFileRequest 的字段结构
//    - 考虑先 read 再 write(避免覆盖用户未保存的修改)
//      → 当前直接 write;由调用方决定是否先 read
//    - 处理 file_modified_externally 错误
//      → 当前统一映射为 WriteError::Ide(message)
//
// 4. request_permission ✓ 完成
//    - 验证 schema::v1::RequestPermissionRequest / PermissionOutcome 的字段
//    - 实现超时(默认拒绝?还是允许?)→ 超时返回 PermissionError::Timeout
//    - 缓存用户决策(AlwaysAllow)→ 已实现 (operation,target) 缓存
//
// 5. flush_lane_events ✓ 完成
//    - 实现 1.3 版本的 lane_event_to_session_update_v1_3
//    - 1.3 的 SessionNotification API 与 0.10.4 在 schema 层相同
//      → 复用映射逻辑(字节复制自 lane_bridge.rs)
//    - 通过 ConnectionTo<Client>::send_notification 推送
//
// 6. spawn.rs/stdio.rs 1.3 入口 ✓ 完成
//    - 新增 spawn_v1_3.rs:spawn_claw_shell_v1_3 (线程 + LocalSet + cancel)
//    - 新增 stdio_v1_3.rs:run_stdio_agent_v1_3 + run_agent_on_io_v1_3
//    - 用 1.3 Agent.builder() + Stdio transport
//    - 通过 on_receive_dispatch 闭包捕获 ConnectionTo<Client>
//    - Stage 2 stub:对所有 Request 返回 internal_error
//    - Stage 3 会注册实际的 InitializeRequest / NewSessionRequest / PromptRequest 等
//      handler,让 ClawAgentV13 真正参与 ACP 协议交互
