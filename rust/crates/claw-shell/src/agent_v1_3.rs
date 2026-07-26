//! ACP 1.3 版本的 Agent 实现(骨架)。
//!
//! 1.3 API 重设计:`Client`/`Agent` 从 trait 变为 struct,
//! 引入 `Component`/`Connection`/`Builder` 模型。本模块用 1.3 的新 API
//! 重新实现 `ClawAgent`,提供 IDE 反向请求(fs/read_text_file、
//! fs/write_text_file、session/request_permission)的发送逻辑。
//!
//! ## 状态:骨架
//!
//! 本模块当前仅提供 struct 定义 + 方法签名 + 占位实现。
//! 完整实现需要:
//! 1. 用 1.3 `Builder` API 构造 `Component`
//! 2. 通过 `Connection` 发送反向请求并等待响应
//! 3. 桥接 `ConversationRuntime` 的同步 `run_turn` 到 1.3 的 async 模型
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

use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;

// 1.3 schema:package 名仍是 agent-client-protocol,但通过 rename 区分。
// 1.3 的 schema 类型在 `schema::v1` 路径下;Client/Agent 是 role marker struct
// (位于 `acp::role::acp::`,通过 `acp::Client` / `acp::Agent` re-export)。
use agent_client_protocol_v1 as acp;
use acp::schema::v1 as schema;

use runtime::{ApiClient, ConversationRuntime, StaticToolExecutor};

/// 1.3 版本的 ClawAgent。
///
/// 与 0.10.4 的 `ClawAgent` 主要差异:
/// - 不实现 `acp::Agent` trait(1.3 中 Agent 是 struct,不是 trait)
/// - 持有 1.3 `Component` 模型所需的连接句柄(完整实现时通过 `ConnectTo` 建立)
/// - 提供 3 个反向请求方法,主动向 IDE 请求数据/权限
///
/// ## 反向请求
///
/// 1. `read_editor_buffer` → `fs/read_text_file`:读取 IDE 打开的文件内容
/// 2. `write_editor_buffer` → `fs/write_text_file`:向 IDE 写入文件
/// 3. `request_permission` → `session/request_permission`:请求用户授权
///
/// 这 3 个方法是阶段 2 的核心交付物,让 agent 能主动查询/修改 IDE 状态,
/// 而不仅被动响应 IDE 的 prompt。
pub struct ClawAgentV13<C>
where
    C: ApiClient + 'static,
{
    /// 当前活跃的 runtime。initialize 阶段为 None,new_session 后创建。
    runtime: RefCell<Option<ConversationRuntime<C, StaticToolExecutor>>>,

    /// API client(在 new_session 时移入 runtime,故用 Option)。
    api_client: RefCell<Option<C>>,

    /// Tool executor(在 new_session 时移入 runtime,故用 Option)。
    tool_executor: RefCell<Option<StaticToolExecutor>>,

    /// 活跃 session 的 ID 集合。
    /// 1.3 中 session 通过 `SessionId` 标识,session 状态由 `Component` 管理
    /// (不像 0.10.4 需要 agent 自己持有 Session 对象)。
    /// 完整实现时这里可以替换为 `HashMap<SessionId, SessionState>`。
    sessions: RefCell<HashSet<schema::SessionId>>,

    /// 占位字段,确保 `C` 类型参数被使用。
    /// 完整实现后 `api_client` 字段会持有 C,此处保留以防编译警告。
    _marker: PhantomData<C>,
}

impl<C> ClawAgentV13<C>
where
    C: ApiClient + Send + 'static,
{
    /// 构造 1.3 ClawAgent 骨架。
    ///
    /// # TODO(1.3 完整实现)
    /// - 设计 1.3 专用的 builder(不复用 0.10.4 的 ClawAgentBuilder)
    /// - 调用 1.3 `Builder::new()` 构造 `Component`
    /// - 注册 `ConversationRuntime` 为 1.3 的 tool provider
    /// - 创建 `Connection` 并保存到 `client_connection`
    ///
    /// 当前实现:所有字段初始化为 `None`/空,所有反向请求方法
    /// 返回 `Err(NotImplemented)`。调用方需显式指定类型参数 `C`:
    /// ```ignore
    /// let agent = ClawAgentV13::<MyClient>::new();
    /// ```
    pub fn new() -> Self {
        Self {
            runtime: RefCell::new(None),
            api_client: RefCell::new(None),
            tool_executor: RefCell::new(None),
            sessions: RefCell::new(HashSet::new()),
            _marker: PhantomData,
        }
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
    /// 通过 `client_connection` 的 `read_text_file` 方法发送请求:
    /// ```ignore
    /// let req = schema::ReadTextFileRequest::new(uri);
    /// let resp = client.read_text_file(req).await?;
    /// Ok(resp.content)
    /// ```
    ///
    /// # TODO
    /// 当前返回 `NotImplemented`。需要:
    /// 1. 验证 1.3 schema 中 `ReadTextFileRequest` 的字段结构
    /// 2. 处理 IDE 返回的错误(file not found、permission denied)
    /// 3. 添加超时保护(IDE 不响应时不能无限等待)
    pub async fn read_editor_buffer(&self, path: &str) -> Result<String, ReadError> {
        let _ = path;
        Err(ReadError::NotImplemented)
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
    /// ```ignore
    /// let req = schema::WriteTextFileRequest::new(uri, content);
    /// let _resp = client.write_text_file(req).await?;
    /// ```
    ///
    /// # TODO
    /// 当前返回 `NotImplemented`。需要:
    /// 1. 验证 1.3 schema 中 `WriteTextFileRequest` 的字段结构
    /// 2. 处理 IDE 拒绝写入的情况(文件被外部修改、权限不足)
    /// 3. 考虑是否需要先 read 再 write(避免覆盖用户未保存的修改)
    pub async fn write_editor_buffer(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), WriteError> {
        let _ = (path, content);
        Err(WriteError::NotImplemented)
    }

    /// 发起 `session/request_permission` 反向请求。
    ///
    /// 让 agent 在执行敏感操作前主动请求用户授权,典型场景:
    /// - 执行 shell 命令前请求确认
    /// - 修改工作区外的文件前请求确认
    /// - 调用外部 MCP 工具前请求确认
    ///
    /// # 参数
    /// - `request`:权限请求描述(操作类型、影响范围等)
    ///
    /// # 1.3 API 实现要点
    /// ```ignore
    /// let req = schema::RequestPermissionRequest::new(
    ///     session_id,
    ///     permission_request,
    /// );
    /// let resp = client.request_permission(req).await?;
    /// match resp.outcome {
    ///     schema::PermissionOutcome::Allow => Ok(PermissionOutcome::Allow),
    ///     schema::PermissionOutcome::Deny => Ok(PermissionOutcome::Deny),
    /// }
    /// ```
    ///
    /// # TODO
    /// 当前返回 `NotImplemented`。需要:
    /// 1. 验证 1.3 schema 中 `RequestPermissionRequest` / `PermissionOutcome` 的字段
    /// 2. 处理 IDE 超时(默认拒绝?还是允许?)
    /// 3. 缓存用户决策(同一操作不重复询问)
    pub async fn request_permission(
        &self,
        request: PermissionRequest,
    ) -> Result<PermissionOutcome, PermissionError> {
        let _ = request;
        Err(PermissionError::NotImplemented)
    }

    /// 刷新 LaneEvent 到 1.3 的 SessionNotification。
    ///
    /// 与 0.10.4 的 `lane_bridge::flush_lane_events_to_acp` 类似,
    /// 但通过 1.3 Connection 推送。
    ///
    /// # TODO
    /// 当前为占位。1.3 的 SessionNotification API 与 0.10.4 略有不同,
    /// 需要单独实现 `lane_event_to_session_update_v1_3`。
    pub fn flush_lane_events(&self, _session_id: &schema::SessionId) -> usize {
        // TODO(1.3):实现 1.3 版本的 LaneEvent → SessionNotification 映射
        // 当前仅 drain 不推送
        runtime::drain_lane_events().len()
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

// ---- 反向请求的错误/请求/结果类型 ----
//
// 这些类型独立于 1.3 schema,让 agent_v1_3.rs 在 1.3 API 未完整接入时
// 也能编译。完整实现时应替换为 1.3 schema 中的对应类型。

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
    /// IDE 返回的其他错误。
    #[error("IDE error: {0}")]
    Ide(String),
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
    /// IDE 返回的其他错误。
    #[error("IDE error: {0}")]
    Ide(String),
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

/// 权限请求描述。
///
/// 完整实现时应映射到 1.3 schema 的 `PermissionRequest`。
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
///
/// 完整实现时应映射到 1.3 schema 的 `PermissionOutcome`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// 用户允许。
    Allow,
    /// 用户拒绝。
    Deny,
    /// 用户选择"总是允许"(后续同类操作不再询问)。
    AlwaysAllow,
}

// ---- TODO 项汇总(供阶段 2 后续实现参考) ----
//
// 1. ClawAgentV13::new
//    - 设计 1.3 专用 builder(不复用 0.10.4 ClawAgentBuilder)
//    - 调用 1.3 `Builder::new()` 构造 `Component`
//    - 注册 `ConversationRuntime` 为 1.3 的 tool provider
//    - 通过 `ConnectTo` 建立 Connection,保存连接句柄
//      (当前 struct 没有 connection 字段,完整实现时需要添加)
//
// 2. read_editor_buffer
//    - 验证 schema::v1::ReadTextFileRequest 的字段结构
//    - 实现超时保护(IDE 不响应时返回错误)
//    - 处理 IDE 返回的 file_not_found / permission_denied
//
// 3. write_editor_buffer
//    - 验证 schema::v1::WriteTextFileRequest 的字段结构
//    - 考虑先 read 再 write(避免覆盖用户未保存的修改)
//    - 处理 file_modified_externally 错误
//
// 4. request_permission
//    - 验证 schema::v1::RequestPermissionRequest / PermissionOutcome 的字段
//    - 实现超时(默认拒绝?还是允许?)
//    - 缓存用户决策(AlwaysAllow)
//
// 5. flush_lane_events
//    - 实现 1.3 版本的 lane_event_to_session_update_v1_3
//    - 1.3 的 SessionNotification API 与 0.10.4 略有不同,需单独映射
//    - 当前仅 drain 不推送(无 connection 句柄)
//
// 6. 整体 cfg-gating(已完成 ✓)
//    - lib.rs 已根据 feature 选择 0.10.4 / 1.3 入口
//    - agent.rs / spawn.rs / stdio.rs / lane_bridge.rs 用 `#[cfg(feature = "acp-0_10")]` gate
//    - agent_v1_3.rs 用 `#[cfg(feature = "acp-1_5")]` gate
//    - 后续:让 spawn.rs / stdio.rs 支持 1.3 Connection(目前 1.3 模式下无入口)
