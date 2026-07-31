//! stdio ACP 服务器:在 LocalSet 上运行 agent,通过 stdin/stdout 桥接 JSON-RPC。
//!
//! 架构参考:grok-build `xai-grok-shell/src/agent/app.rs::run_stdio_agent`。
//!
//! 关键组件:
//! - `LineReaderStream`:把 `mpsc::Receiver<Vec<u8>>` 适配为 `tokio::io::AsyncRead`
//! - `run_stdio_agent`:入口函数,创建 LocalSet + AgentSideConnection + GatewayReceiver
//!
//! 与 `spawn_claw_shell` 的区别:
//! - `spawn_claw_shell` 在独立线程跑 agent,通过 mpsc channel 与前端通信
//! - `run_stdio_agent` 在调用方线程跑 agent,通过 stdin/stdout 与外部客户端通信
//!   (外部客户端 = Zed / VS Code 等 ACP 编辑器)

use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use agent_client_protocol as acp;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use claw_acp::{spawn_stdin_line_reader, AcpGatewayReceiver, AcpGatewaySender};

use crate::agent::ClawAgentBuilder;

/// 把 `mpsc::Receiver<Vec<u8>>` 适配为 `tokio::io::AsyncRead`。
///
/// 每条 channel 消息被视为一段字节流(通常是一行 JSON-RPC 含尾换行)。
/// 按顺序填充到 `poll_read` 的 `ReadBuf`,channel 关闭时返回 EOF(Ok)。
///
/// `Unpin`:因为 `mpsc::Receiver` 和 `Vec<u8>` 都是 `Unpin`。
pub struct LineReaderStream {
    rx: mpsc::Receiver<Vec<u8>>,
    /// 当前正在消费的行(已读取部分)。
    current: Vec<u8>,
    /// `current` 中下一个待读字节的位置。
    pos: usize,
}

impl LineReaderStream {
    pub fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            current: Vec::new(),
            pos: 0,
        }
    }
}

impl AsyncRead for LineReaderStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 当前行已耗尽:尝试拉取下一行。
        if self.pos >= self.current.len() {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(line)) => {
                    self.current = line;
                    self.pos = 0;
                }
                Poll::Ready(None) => {
                    // channel 关闭 = EOF
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // 从 current 复制到 buf,不跨越行边界(简化语义:每次 poll_read 至多返回一行)。
        let available = &self.current[self.pos..];
        let n = std::cmp::min(available.len(), buf.remaining());
        buf.put_slice(&available[..n]);
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

/// 在调用方线程上运行 stdio ACP 服务器。
///
/// # 流程
/// 1. 创建 `current_thread` runtime + `LocalSet`(agent 持有 `Rc<RefCell<...>>`,非 `Send`)
/// 2. 在 LocalSet 内:
///    - 创建 mpsc channel + `AcpGatewaySender<acp::AgentSide>`
///    - 构造 agent(`builder.build(client_gateway)`)
///    - 启动 stdin 行读取线程
///    - 创建 `acp::AgentSideConnection::new(agent_rc, stdout, line_reader, spawn_fn)`
///    - spawn `AcpGatewayReceiver::new(rx, conn).run()`(转发 agent → client 推送)
///    - 等待 `handle_io` 完成(stdin EOF)或 `cancel` 触发
///
/// # 参数
/// - `builder`:agent 构造器(已配置 api_client + 系统 prompt)
/// - `cancel`:取消令牌;触发时退出 LocalSet
///
/// # 返回
/// - `Ok(())`:stdin EOF 或 cancel 触发,正常退出
/// - `Err(io::Error)`:runtime 创建失败或线程 panic
///
/// # 阻塞
/// 此函数会阻塞调用方线程直到退出。调用方应在专用线程或 main 中调用,
/// 不要在 async 上下文中调用(会阻塞 runtime)。
pub fn run_stdio_agent<C>(
    builder: ClawAgentBuilder<C>,
    cancel: CancellationToken,
) -> Result<(), io::Error>
where
    C: runtime::ApiClient + Send + 'static,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();

    let result = local.block_on(&rt, async move {
        // stdin:专用 OS 线程读取,通过 mpsc 投递,再用 LineReaderStream 适配为 AsyncRead
        let stdin_rx = spawn_stdin_line_reader();
        // ACP 用 futures::io::AsyncRead,需要用 tokio_util::compat 桥接 tokio AsyncRead
        let incoming = LineReaderStream::new(stdin_rx).compat();
        // stdout:tokio::io::Stdout 是 tokio AsyncWrite,用 compat_write 桥接
        let outgoing = tokio::io::stdout().compat_write();
        run_agent_on_io(builder, cancel, incoming, outgoing).await
    });

    result
}

/// 在自定义 IO 上运行 ACP agent(核心逻辑,与 stdin/stdout 解耦)。
///
/// `run_stdio_agent` 的可测试版本:接受任意的 `futures::io::AsyncRead` +
/// `futures::io::AsyncWrite`,用于测试中用内存管道替换 stdin/stdout。
///
/// # 参数
/// - `incoming`:ACP 客户端 → agent 的字节流(JSON-RPC 请求,每行一条)
/// - `outgoing`:agent → ACP 客户端的字节流(JSON-RPC 响应/通知)
///
/// # 返回
/// - `Ok(())`:`incoming` EOF 或 cancel 触发
/// - `Err(io::Error)`:IO 错误
///
/// # 运行环境要求
/// 必须在 `tokio::task::LocalSet` 上运行(因 agent 持有 `Rc<RefCell<...>>`,非 `Send`)。
/// 调用方负责创建 runtime + LocalSet。
pub async fn run_agent_on_io<C, R, W>(
    builder: ClawAgentBuilder<C>,
    cancel: CancellationToken,
    incoming: R,
    outgoing: W,
) -> Result<(), io::Error>
where
    C: runtime::ApiClient + Send + 'static,
    R: futures::AsyncRead + Unpin,
    W: futures::AsyncWrite + Unpin,
{
    // 1. 创建 agent ↔ connection 之间的 mpsc channel
    //    agent 通过 client_gateway 推送 AcpClientMessage(SessionNotification 等)
    //    AcpGatewayReceiver 消费这些消息并转发到 AgentSideConnection → outgoing
    //
    //    构造顺序(参考 spawn.rs / grok-build spawn_agent_local):
    //    a) 先 mpsc channel + AcpGatewaySender::new(tx) —— 因 agent 需持有 sender
    //    b) builder.build(sender) 构造 agent
    //    c) AgentSideConnection::new(agent, outgoing, incoming, spawn_fn) 返回 (conn, handle_io)
    //    d) AcpGatewayReceiver::new(rx, conn) 在此 move conn,spawn run()
    let (gw_tx, gw_rx) = mpsc::unbounded_channel();
    let client_gateway = AcpGatewaySender::<acp::AgentSide>::new(gw_tx).with_tracing(true);

    // 2. 构造 agent(builder.build 在 LocalSet 内完成,因 StaticToolExecutor 非 Send)
    let agent = builder.build(client_gateway);
    let agent_rc = Rc::new(agent);

    // 3. 创建 AgentSideConnection
    //    - agent_rc 实现 acp::Agent → 自动实现 MessageHandler<AgentSide>
    //    - spawn_fn 用 spawn_local(LocalSet 上)
    //    - incoming/outgoing 已是 futures::io::{AsyncRead, AsyncWrite}
    let (conn, handle_io) = acp::AgentSideConnection::new(agent_rc, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });

    // 4. spawn GatewayReceiver:消费 agent 推送的 AcpClientMessage,转发到 conn
    //    AcpGatewayReceiver::new(rx, conn) 在此 move conn
    let gateway_receiver = AcpGatewayReceiver::new(gw_rx, conn).with_tracing(true);
    tokio::task::spawn_local(gateway_receiver.run());

    // 5. yield 一次,让 gateway receiver 先注册
    tokio::task::yield_now().await;

    // 6. 等待 handle_io 完成(incoming EOF)或 cancel 触发
    //    select! 取最先完成的分支;cancel 优先级略低(若同时 ready,先返回 handle_io)
    let io_result = tokio::select! {
        biased;
        r = handle_io => r,
        _ = cancel.cancelled() => {
            tracing::debug!("claw-stdio-agent: cancellation received, exiting");
            return Ok(());
        }
    };

    match io_result {
        Ok(()) => {
            tracing::debug!("claw-stdio-agent: incoming EOF, exiting");
            Ok(())
        }
        Err(e) => {
            tracing::warn!("claw-stdio-agent: io error: {e}");
            Err(io::Error::other(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use runtime::{
        ApiClient, ApiRequest, AssistantEvent, PermissionMode, PermissionPolicy, RuntimeError,
    };

    /// 测试用 `ApiClient`:返回固定的 assistant 事件序列。
    /// 必须是 `Send`(`ClawAgentBuilder<C>` 要求 `C: Send + 'static`)。
    struct MockApiClient {
        events: Vec<AssistantEvent>,
    }

    impl MockApiClient {
        fn new() -> Self {
            Self {
                events: vec![
                    AssistantEvent::TextDelta("mock response".into()),
                    AssistantEvent::MessageStop,
                ],
            }
        }
    }

    impl ApiClient for MockApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(self.events.clone())
        }
    }

    fn test_builder() -> ClawAgentBuilder<MockApiClient> {
        ClawAgentBuilder::new(
            MockApiClient::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["test system prompt".to_string()],
        )
    }

    #[tokio::test]
    async fn line_reader_stream_yields_lines_in_order() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
        tx.send(b"first\n".to_vec()).await.unwrap();
        tx.send(b"second\n".to_vec()).await.unwrap();
        drop(tx);

        let mut stream = LineReaderStream::new(rx);
        let mut buf = [0u8; 64];

        // 第一次 poll_read 应返回 "first\n"
        use tokio::io::AsyncReadExt;
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"first\n");

        // 第二次应返回 "second\n"
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"second\n");

        // 第三次应返回 0(EOF)
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn line_reader_stream_returns_partial_when_buf_small() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(4);
        tx.send(b"hello world\n".to_vec()).await.unwrap();
        drop(tx);

        let mut stream = LineReaderStream::new(rx);
        let mut buf = [0u8; 5];
        use tokio::io::AsyncReadExt;

        // 第一次只读 5 字节(受 buf 大小限制)
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");

        // 第二次继续读剩下的(至多 5 字节)
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b" worl");

        // 第三次读完剩余
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"d\n");

        // EOF
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn line_reader_stream_pending_when_channel_empty() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
        let mut stream = LineReaderStream::new(rx);

        // 在另一个 task 中延迟发送
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tx.send(b"delayed\n".to_vec()).await.unwrap();
        });

        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 64];
        // 应等待 50ms 后返回数据
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"delayed\n");
    }

    /// 端到端测试:通过内存管道向 `run_agent_on_io` 发送 JSON-RPC `initialize`
    /// 请求,验证收到包含 `protocolVersion` 和 `authMethods` 的正确响应。
    ///
    /// 这验证了 stdio ACP 服务器的核心 IO 桥接逻辑:
    /// - `LineReaderStream` + `.compat()` → `futures::AsyncRead`(incoming)
    /// - `tokio::io::stdout().compat_write()` 的等价物 → `futures::AsyncWrite`(outgoing)
    /// - `AgentSideConnection::new` + `AcpGatewayReceiver` 正确路由消息
    /// - `ClawAgent::initialize` 返回符合 ACP 协议的响应
    #[tokio::test]
    async fn run_agent_on_io_handles_initialize_handshake() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // 1. 两个 duplex:client→agent(incoming)和 agent→client(outgoing)
                let (mut client_tx, agent_rx) = tokio::io::duplex(8192);
                let (agent_tx, mut client_rx) = tokio::io::duplex(8192);

                // 2. 构造 builder + cancel
                let builder = test_builder();
                let cancel = CancellationToken::new();
                let agent_cancel = cancel.clone();

                // 3. 在 LocalSet 内启动 agent(stdio.rs 的核心被测函数)
                let agent_task = tokio::task::spawn_local(async move {
                    let incoming = agent_rx.compat();
                    let outgoing = agent_tx.compat_write();
                    run_agent_on_io(builder, agent_cancel, incoming, outgoing).await
                });

                // 4. 客户端发送 JSON-RPC initialize 请求(NDJSON,每行一条)
                let init_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "initialize",
                    "params": {
                        "protocolVersion": 1
                    },
                    "id": 1
                });
                let req_line = format!("{}\n", serde_json::to_string(&init_req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                // 5. 客户端读取响应(带 5s 超时,防止 hang)
                let mut reader = tokio::io::BufReader::new(&mut client_rx);
                let mut line = String::new();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await;

                assert!(
                    read_result.is_ok(),
                    "should receive initialize response within 5s"
                );
                assert!(!line.is_empty(), "response line should not be empty");

                let resp: serde_json::Value = serde_json::from_str(line.trim())
                    .unwrap_or_else(|e| panic!("response should be valid JSON: {e}, got: {line}"));

                assert_eq!(resp["jsonrpc"], "2.0", "jsonrpc field: {}", resp);
                assert_eq!(resp["id"], 1, "id field: {}", resp);
                assert_eq!(
                    resp["result"]["protocolVersion"], 1,
                    "protocolVersion: {}",
                    resp
                );
                assert!(
                    resp["result"]["authMethods"].is_array(),
                    "authMethods should be array: {}",
                    resp
                );
                assert!(
                    !resp["result"]["authMethods"].as_array().unwrap().is_empty(),
                    "authMethods should not be empty: {}",
                    resp
                );

                // 6. 取消 agent,等待干净退出
                cancel.cancel();
                let agent_result = agent_task.await;
                assert!(agent_result.is_ok(), "agent task should not panic");
                assert!(
                    agent_result.unwrap().is_ok(),
                    "agent should exit cleanly on cancel"
                );
            })
            .await;
    }

    /// 验证 `run_agent_on_io` 在 incoming EOF 时正常退出(返回 `Ok(())`)。
    ///
    /// 这对应 stdio 服务器在 stdin 关闭时的行为:agent 应检测到 EOF 并优雅退出,
    /// 而不是 hang 或报错。
    #[tokio::test]
    async fn run_agent_on_io_exits_cleanly_on_eof() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // 1. 两个 duplex
                let (client_tx, agent_rx) = tokio::io::duplex(8192);
                let (agent_tx, mut _client_rx) = tokio::io::duplex(8192);

                // 2. drop client_tx 立即触发 incoming EOF
                drop(client_tx);

                let builder = test_builder();
                let cancel = CancellationToken::new();

                // 3. 启动 agent,带超时防止 hang
                let agent_task = tokio::task::spawn_local(async move {
                    let incoming = agent_rx.compat();
                    let outgoing = agent_tx.compat_write();
                    run_agent_on_io(builder, cancel, incoming, outgoing).await
                });

                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(3), agent_task).await;

                assert!(result.is_ok(), "agent should exit within 3s on EOF");
                let join_result = result.unwrap();
                assert!(join_result.is_ok(), "agent task should not panic");
                assert!(
                    join_result.unwrap().is_ok(),
                    "agent should return Ok(()) on EOF"
                );
            })
            .await;
    }

    /// 验证 `run_agent_on_io` 在 `CancellationToken` 触发时正常退出。
    ///
    /// 这对应外部调用方(如 `claw acp serve` 被 Ctrl+C 中断)主动取消 agent 的场景。
    #[tokio::test]
    async fn run_agent_on_io_exits_cleanly_on_cancel() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // 1. 两个 duplex,client 端不关闭(保持 incoming 不 EOF)
                let (_client_tx, agent_rx) = tokio::io::duplex(8192);
                let (agent_tx, _client_rx) = tokio::io::duplex(8192);

                let builder = test_builder();
                let cancel = CancellationToken::new();
                let cancel_clone = cancel.clone();

                // 2. 启动 agent
                let agent_task = tokio::task::spawn_local(async move {
                    let incoming = agent_rx.compat();
                    let outgoing = agent_tx.compat_write();
                    run_agent_on_io(builder, cancel_clone, incoming, outgoing).await
                });

                // 3. 给 agent 一点时间启动,然后取消
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                cancel.cancel();

                // 4. 等待退出(带超时)
                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(3), agent_task).await;

                assert!(result.is_ok(), "agent should exit within 3s on cancel");
                let join_result = result.unwrap();
                assert!(join_result.is_ok(), "agent task should not panic");
                assert!(
                    join_result.unwrap().is_ok(),
                    "agent should return Ok(()) on cancel"
                );
            })
            .await;
    }

    /// A6.4:错误路径 — 发送无效 JSON,验证 agent 返回 `-32700 parse_error`
    /// 错误响应且通道保持可用(后续合法请求仍可正常处理)。
    ///
    /// **行为变迁**:原 ACP 0.10.4 上游库对 parse 失败仅 `log::error!`
    /// 并 silent drop。本项目通过 `rust/forks/agent-client-protocol` patch
    /// 修复此行为,遵循 JSON-RPC 2.0 规范返回 `id=null` 的 parse_error 响应。
    #[tokio::test]
    async fn run_agent_on_io_returns_parse_error_on_invalid_json() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut client_tx, agent_rx) = tokio::io::duplex(8192);
                let (agent_tx, mut client_rx) = tokio::io::duplex(8192);

                let builder = test_builder();
                let cancel = CancellationToken::new();
                let agent_cancel = cancel.clone();

                let agent_task = tokio::task::spawn_local(async move {
                    let incoming = agent_rx.compat();
                    let outgoing = agent_tx.compat_write();
                    run_agent_on_io(builder, agent_cancel, incoming, outgoing).await
                });

                // 1. 发送一行无效 JSON(缺少闭合引号和括号)
                client_tx
                    .write_all(b"{\"jsonrpc\":\"2.0\",\"method\n")
                    .await
                    .unwrap();
                client_tx.flush().await.unwrap();

                // 2. 验证 agent 返回 -32700 parse_error 响应(id=null)
                let mut reader = tokio::io::BufReader::new(&mut client_rx);
                let mut line = String::new();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    reader.read_line(&mut line),
                )
                .await;
                assert!(
                    read_result.is_ok(),
                    "agent should send parse_error response for invalid JSON"
                );
                let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(resp["jsonrpc"], "2.0");
                assert_eq!(resp["id"], serde_json::Value::Null);
                assert_eq!(resp["error"]["code"], -32700);
                assert_eq!(resp["error"]["message"], "Parse error");

                // 3. 发送合法的 initialize 请求,验证 agent 仍可响应
                let init_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "initialize",
                    "params": { "protocolVersion": 1 },
                    "id": 1
                });
                let req_line = format!("{}\n", serde_json::to_string(&init_req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                line.clear();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await;
                assert!(
                    read_result.is_ok(),
                    "agent should respond to valid initialize after parse error"
                );
                let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(resp["jsonrpc"], "2.0");
                assert_eq!(resp["id"], 1);
                assert_eq!(resp["result"]["protocolVersion"], 1);

                cancel.cancel();
                let _ = agent_task.await;
            })
            .await;
    }

    /// A6.4:错误路径 — 发送缺少 `method` 字段的 JSON-RPC 消息,
    /// 验证 agent 不崩溃且通道保持可用。
    ///
    /// **ACP 0.10.4 行为**:合法 JSON 但缺少 `method` 字段时,若 `id` 存在,
    /// 库会尝试当作 Response 处理(查找 pending_responses)。找不到对应 id 时
    /// `log::error!("received response for unknown request id")` 并 silent drop,
    /// **不发送 error response**。此测试固化该行为。
    #[tokio::test]
    async fn run_agent_on_io_silently_drops_missing_method_field() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut client_tx, agent_rx) = tokio::io::duplex(8192);
                let (agent_tx, mut client_rx) = tokio::io::duplex(8192);

                let builder = test_builder();
                let cancel = CancellationToken::new();
                let agent_cancel = cancel.clone();

                let agent_task = tokio::task::spawn_local(async move {
                    let incoming = agent_rx.compat();
                    let outgoing = agent_tx.compat_write();
                    run_agent_on_io(builder, agent_cancel, incoming, outgoing).await
                });

                // 1. 发送合法 JSON 但缺少 method 字段(只有 id)
                let req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 99
                });
                let req_line = format!("{}\n", serde_json::to_string(&req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                // 2. 等待处理
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                // 3. 验证 silent drop
                let mut reader = tokio::io::BufReader::new(&mut client_rx);
                let mut line = String::new();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    reader.read_line(&mut line),
                )
                .await;
                assert!(
                    read_result.is_err(),
                    "agent should NOT send response for missing method (ACP 0.10.4 silent drop)"
                );

                // 4. 验证 agent 仍可处理合法请求
                let init_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "initialize",
                    "params": { "protocolVersion": 1 },
                    "id": 2
                });
                let req_line = format!("{}\n", serde_json::to_string(&init_req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                line.clear();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await;
                assert!(
                    read_result.is_ok(),
                    "agent should respond to valid initialize after missing method"
                );

                cancel.cancel();
                let _ = agent_task.await;
            })
            .await;
    }

    /// A6.4:错误路径 — 发送未知 `method` 名的 JSON-RPC 请求,
    /// 验证服务器返回 `code: -32601` (Method not found) error。
    ///
    /// **ACP 0.10.4 行为**:`Local::decode_request(&method, params)` 失败时,
    /// 库会构造 `Response::Error { id, error }` 并写回 outgoing。这是唯一
    /// 会返回 error response 的错误路径(与 parse failure / missing method 不同)。
    #[tokio::test]
    async fn run_agent_on_io_returns_error_on_unknown_method() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (mut client_tx, agent_rx) = tokio::io::duplex(8192);
                let (agent_tx, mut client_rx) = tokio::io::duplex(8192);

                let builder = test_builder();
                let cancel = CancellationToken::new();
                let agent_cancel = cancel.clone();

                let agent_task = tokio::task::spawn_local(async move {
                    let incoming = agent_rx.compat();
                    let outgoing = agent_tx.compat_write();
                    run_agent_on_io(builder, agent_cancel, incoming, outgoing).await
                });

                // 合法 JSON + 合法结构,但 method 名不存在于 ACP 协议
                let req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/nonexistentMethod",
                    "params": {},
                    "id": 42
                });
                let req_line = format!("{}\n", serde_json::to_string(&req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                let mut reader = tokio::io::BufReader::new(&mut client_rx);
                let mut line = String::new();
                let read_result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await;
                assert!(
                    read_result.is_ok(),
                    "should receive error response within 5s"
                );

                let resp: serde_json::Value =
                    serde_json::from_str(line.trim()).expect("error response should be valid JSON");
                assert_eq!(resp["jsonrpc"], "2.0");
                assert_eq!(resp["id"], 42, "id should match request: {resp}");
                assert!(
                    resp.get("error").is_some(),
                    "should have error field: {resp}"
                );
                // Method not found code is -32601
                assert_eq!(
                    resp["error"]["code"], -32601,
                    "method not found code should be -32601: {resp}"
                );

                cancel.cancel();
                let _ = agent_task.await;
            })
            .await;
    }

    /// A6.4:单元测试 — `ClawAgent::cancel` 是 stub,直接调用应返回 `Ok(())`
    ///
    /// Phase A 中 `cancel`(agent.rs:345-350)不做任何事,仅记录 warn 日志后返回 Ok。
    /// 此测试固化该行为。未来 cancel 被实现后,需更新断言以验证实际的取消语义。
    #[tokio::test]
    async fn claw_agent_cancel_returns_ok_as_stub() {
        use acp::Agent;
        use agent_client_protocol as acp;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // 构造 agent(需要 gateway sender,但 cancel 不使用它)
                let builder = test_builder();
                let (gw_tx, _gw_rx) = mpsc::unbounded_channel();
                let client_gateway = AcpGatewaySender::<acp::AgentSide>::new(gw_tx);
                let agent = builder.build(client_gateway);

                // cancel 是 stub:无论 session 是否存在,都返回 Ok(())
                let notif = acp::CancelNotification::new("fake-session-id");
                let result = agent.cancel(notif).await;

                assert!(result.is_ok(), "cancel stub should return Ok: {:?}", result);
            })
            .await;
    }

    /// A6.4:集成测试 — cancel during prompt turn(当前 stub 行为)
    ///
    /// 验证场景:在 agent 处理 `session/prompt` 期间(或紧随其后)发送
    /// `session/cancel` 通知,验证当前行为:
    ///
    /// - **当前(Phase A)**:cancel 是 stub,不中断正在进行的 turn;
    ///   prompt 正常完成并返回 `StopReason::EndTurn`
    /// - **未来**:cancel 应中断 turn,prompt 应返回 `StopReason::Cancelled`
    ///
    /// 架构说明:`run_turn` 是同步阻塞 API,会阻塞 LocalSet 线程。
    /// 因此 `session/cancel` 通知实际上会在 turn 完成后才被处理
    /// (cancel handler 是 no-op,无副作用)。此测试固化该时序行为。
    ///
    /// 未来 `run_turn` 改为 async + CancellationToken 后,取消语义将改变,
    /// 需将 `end_turn` 断言改为 `cancelled`。
    #[tokio::test]
    async fn run_agent_on_io_cancel_during_prompt_does_not_interrupt_turn() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // 1. 两个 duplex:client→agent(incoming)和 agent→client(outgoing)
                let (mut client_tx, agent_rx) = tokio::io::duplex(8192);
                let (agent_tx, mut client_rx) = tokio::io::duplex(8192);

                // 2. 构造 builder + cancel
                let builder = test_builder();
                let cancel = CancellationToken::new();
                let agent_cancel = cancel.clone();

                // 3. 启动 agent
                let agent_task = tokio::task::spawn_local(async move {
                    let incoming = agent_rx.compat();
                    let outgoing = agent_tx.compat_write();
                    run_agent_on_io(builder, agent_cancel, incoming, outgoing).await
                });

                let mut reader = tokio::io::BufReader::new(&mut client_rx);
                let mut line = String::new();

                // 4. initialize 请求
                let init_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "initialize",
                    "params": { "protocolVersion": 1 },
                    "id": 1
                });
                let req_line = format!("{}\n", serde_json::to_string(&init_req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                line.clear();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await
                .expect("should receive initialize response within 5s");
                let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(resp["id"], 1, "initialize response id: {resp}");
                assert_eq!(resp["result"]["protocolVersion"], 1, "protocolVersion: {resp}");

                // 5. authenticate 请求
                let auth_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "authenticate",
                    "params": { "methodId": "api_key" },
                    "id": 2
                });
                let req_line = format!("{}\n", serde_json::to_string(&auth_req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                line.clear();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await
                .expect("should receive authenticate response within 5s");
                let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(resp["id"], 2, "authenticate response id: {resp}");

                // 6. session/new 请求
                let new_session_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/new",
                    "params": {
                        "cwd": ".",
                        "mcpServers": []
                    },
                    "id": 3
                });
                let req_line = format!("{}\n", serde_json::to_string(&new_session_req).unwrap());
                client_tx.write_all(req_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                line.clear();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await
                .expect("should receive session/new response within 5s");
                let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(resp["id"], 3, "session/new response id: {resp}");
                let session_id = resp["result"]["sessionId"]
                    .as_str()
                    .expect("sessionId should be in response")
                    .to_string();

                // 7. 发送 session/prompt 请求(立即紧跟 session/cancel 通知)
                //    cancel 是 JSON-RPC notification(无 id 字段),agent 不返回响应。
                //    由于 run_turn 同步阻塞 LocalSet,cancel 实际在 turn 完成后才被处理。
                let prompt_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/prompt",
                    "params": {
                        "sessionId": session_id,
                        "prompt": [{ "type": "text", "text": "hello" }]
                    },
                    "id": 4
                });
                let prompt_line = format!("{}\n", serde_json::to_string(&prompt_req).unwrap());
                client_tx.write_all(prompt_line.as_bytes()).await.unwrap();

                // 立即发送 cancel notification(无 id = notification)
                let cancel_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": { "sessionId": session_id }
                });
                let cancel_line = format!("{}\n", serde_json::to_string(&cancel_req).unwrap());
                client_tx.write_all(cancel_line.as_bytes()).await.unwrap();
                client_tx.flush().await.unwrap();

                // 8. 读取 agent 发送的消息:
                //    预期先收到 session/update notification(AgentMessageChunk),
                //    然后收到 session/prompt response(id=4, stopReason=end_turn)。
                //    cancel notification 是 silent 的(无响应)。
                let mut found_prompt_response = false;
                for _ in 0..5 {
                    line.clear();
                    let read_result = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        reader.read_line(&mut line),
                    )
                    .await;
                    assert!(
                        read_result.is_ok(),
                        "should receive message within 10s after prompt+cancel"
                    );

                    let msg: serde_json::Value = serde_json::from_str(line.trim())
                        .unwrap_or_else(|e| panic!("invalid JSON: {e}, got: {line}"));

                    // 检查是否是 prompt response(id=4)
                    if msg.get("id") == Some(&serde_json::Value::from(4)) {
                        found_prompt_response = true;
                        let stop_reason = msg["result"]["stopReason"]
                            .as_str()
                            .unwrap_or_else(|| panic!("stopReason missing: {msg}"));
                        // 当前 stub 行为:cancel 不中断 turn,prompt 正常完成
                        // 未来实现 cancel 后,此断言应改为 "cancelled"
                        assert_eq!(
                            stop_reason, "end_turn",
                            "cancel stub should not interrupt turn; expected end_turn, got {stop_reason}"
                        );
                        break;
                    }
                    // 否则是 notification(session/update 等),跳过
                }

                assert!(
                    found_prompt_response,
                    "should receive prompt response with id=4"
                );

                // 9. 清理
                cancel.cancel();
                let _ = agent_task.await;
            })
            .await;
    }
}
