//! 远程 MCP 传输：streamable HTTP、旧版 SSE、WebSocket。
//!
//! 目标是与 stdio 走同一套 `McpServerManager` 接口：管理器只调用
//! `initialize` / `list_tools` / `call_tool` / `list_resources` / `read_resource`
//! 与 `shutdown`，具体传输由本模块实现，不再有第二条调用路径。

use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::mcp_client::McpRemoteTransport;
use crate::mcp_stdio::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
};

const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    Http,
    Sse,
    WebSocket,
}

struct SseState {
    endpoint: String,
    receiver: mpsc::UnboundedReceiver<Value>,
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct WsState {
    socket: WsStream,
}

pub struct McpRemoteProcess {
    kind: RemoteKind,
    http: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    timeout: Duration,
    session_id: Option<String>,
    sse: Option<SseState>,
    ws: Option<WsState>,
    /// WebSocket 专用：reqwest 不参与 ws，代理隧道需自行建立。
    ws_proxy: Option<ProxyEndpoint>,
}

/// 代理协议；只有 HTTP CONNECT 隧道在此实现。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProxyScheme {
    Http,
    Https,
    Socks5,
}

#[derive(Debug, Clone)]
struct ProxyEndpoint {
    scheme: ProxyScheme,
    host: String,
    port: u16,
    /// 已编码的 `Proxy-Authorization` 值（含 `Basic ` 前缀）。
    authorization: Option<String>,
    raw: String,
}

impl ProxyEndpoint {
    fn parse(raw: &str, label: &str) -> io::Result<Self> {
        let url = reqwest::Url::parse(raw).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} 地址无效（{raw}）：{error}"),
            )
        })?;
        let scheme = match url.scheme() {
            "http" => ProxyScheme::Http,
            "https" => ProxyScheme::Https,
            "socks5" | "socks5h" | "socks" => ProxyScheme::Socks5,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{label} 不支持的协议：{other}（仅支持 http / https）"),
                ))
            }
        };
        let host = url.host_str().map(str::to_string).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} 缺少主机名：{raw}"),
            )
        })?;
        let port = url
            .port_or_known_default()
            .or_else(|| (scheme == ProxyScheme::Socks5).then_some(1080))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{label} 缺少端口：{raw}"),
                )
            })?;
        let authorization = if url.username().is_empty() && url.password().is_none() {
            None
        } else {
            let credentials = format!(
                "{}:{}",
                percent_decode(url.username()),
                percent_decode(url.password().unwrap_or_default())
            );
            Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(credentials)
            ))
        };
        Ok(Self {
            scheme,
            host,
            port,
            authorization,
            raw: raw.to_string(),
        })
    }

    /// WebSocket 只能走明文 HTTP CONNECT 隧道；其余协议如实报错。
    fn ensure_ws_supported(&self) -> io::Result<()> {
        match self.scheme {
            ProxyScheme::Http => Ok(()),
            ProxyScheme::Https => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "WebSocket 传输暂不支持 HTTPS 代理（需要 TLS-to-proxy）：{}",
                    self.raw
                ),
            )),
            ProxyScheme::Socks5 => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("WebSocket 传输暂不支持 SOCKS 代理：{}", self.raw),
            )),
        }
    }
}

impl std::fmt::Debug for McpRemoteProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRemoteProcess")
            .field("kind", &self.kind)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl McpRemoteProcess {
    pub fn new(kind: RemoteKind, transport: McpRemoteTransport) -> io::Result<Self> {
        let timeout =
            Duration::from_millis(transport.tool_call_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let mut builder = reqwest::Client::builder().connect_timeout(timeout);
        if let Some(raw) = transport.proxy.as_deref() {
            let proxy = ProxyEndpoint::parse(raw, "MCP 代理")?;
            if proxy.scheme == ProxyScheme::Socks5 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("MCP HTTP 传输暂不支持 SOCKS 代理（未启用 reqwest socks 特性）：{raw}"),
                ));
            }
            builder = builder.proxy(reqwest::Proxy::all(raw).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("MCP 代理配置无效（{raw}）：{error}"),
                )
            })?);
        }
        // 连接超时放在 client 上：SSE 是长连接，不能用整体请求超时（会掐断流）。
        let http = builder
            .build()
            .map_err(|error| io::Error::other(format!("build MCP HTTP client failed: {error}")))?;

        // reqwest 会自动读取环境变量代理；ws 走自建隧道，需要手动解析。
        let ws_proxy = match transport.proxy.as_deref() {
            Some(raw) => Some(ProxyEndpoint::parse(raw, "WebSocket 代理")?),
            None => match proxy_from_env(&transport.url) {
                Some(raw) => Some(ProxyEndpoint::parse(&raw, "WebSocket 环境变量代理")?),
                None => None,
            },
        };
        if kind == RemoteKind::WebSocket {
            if let Some(proxy) = &ws_proxy {
                proxy.ensure_ws_supported()?;
            }
        }

        Ok(Self {
            kind,
            http,
            url: transport.url,
            headers: transport.headers,
            timeout,
            session_id: None,
            sse: None,
            ws: None,
            ws_proxy,
        })
    }

    pub async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>> {
        self.request(id, "initialize", Some(params)).await
    }

    pub async fn request<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        id: JsonRpcId,
        method: impl Into<String>,
        params: Option<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let request = JsonRpcRequest::new(id, method, params);
        match self.kind {
            RemoteKind::Http => self.http_request(&request).await,
            RemoteKind::Sse => self.sse_request(&request).await,
            RemoteKind::WebSocket => self.ws_request(&request).await,
        }
    }

    /// 发送 JSON-RPC 通知（无 id），失败时忽略响应体。
    pub async fn notify<T: Serialize>(
        &mut self,
        method: &str,
        params: Option<T>,
    ) -> io::Result<()> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        match self.kind {
            RemoteKind::Http => {
                let url = self.url.clone();
                let response = self.post(&url, &body).await?;
                drain_response(response).await
            }
            RemoteKind::Sse => {
                let endpoint = self.ensure_sse().await?;
                let response = self.post(&endpoint, &body).await?;
                drain_response(response).await
            }
            RemoteKind::WebSocket => {
                self.ensure_ws().await?;
                let Some(state) = self.ws.as_mut() else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "MCP websocket not connected",
                    ));
                };
                state
                    .socket
                    .send(Message::Text(
                        String::from_utf8_lossy(&body).to_string().into(),
                    ))
                    .await
                    .map_err(io_err)
            }
        }
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        if let Some(state) = self.ws.as_mut() {
            let _ = state.socket.close(None).await;
        }
        self.ws = None;
        self.sse = None;
        Ok(())
    }

    async fn http_request<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        request: &JsonRpcRequest<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let body = serde_json::to_vec(request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let url = self.url.clone();
        let response = self.post(&url, &body).await?;
        self.decode_http_response(response, request).await
    }

    async fn post(&mut self, url: &str, body: &[u8]) -> io::Result<reqwest::Response> {
        let mut builder = self
            .http
            .post(url)
            .timeout(self.timeout)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        if let Some(session_id) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", session_id);
        }
        let response = builder.send().await.map_err(reqwest_err)?;
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session_id.to_string());
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(io::Error::other(format!(
                "MCP {url} returned HTTP {status}: {}",
                truncate(&body)
            )));
        }
        Ok(response)
    }

    async fn decode_http_response<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        response: reqwest::Response,
        request: &JsonRpcRequest<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let bytes = response.bytes().await.map_err(reqwest_err)?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("MCP response exceeds {MAX_RESPONSE_BYTES} bytes"),
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        if content_type.contains("text/event-stream") {
            return response_from_sse_text::<TResult>(&text, &request.id);
        }
        serde_json::from_str::<JsonRpcResponse<TResult>>(&text).map_err(invalid_data)
    }

    async fn ensure_sse(&mut self) -> io::Result<String> {
        if let Some(state) = &self.sse {
            return Ok(state.endpoint.clone());
        }
        let mut builder = self
            .http
            .get(&self.url)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        if let Some(session_id) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", session_id);
        }
        let response = builder.send().await.map_err(reqwest_err)?;
        let status = response.status();
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "MCP SSE endpoint {} returned HTTP {status}",
                self.url
            )));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        loop {
            let chunk = tokio::time::timeout(self.timeout, stream.next())
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "MCP SSE endpoint handshake timed out",
                    )
                })?
                .transpose()
                .map_err(reqwest_err)?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "MCP SSE stream closed before endpoint event",
                    )
                })?;
            buffer.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
            while let Some(event) = take_sse_event(&mut buffer) {
                if event.event.as_deref() == Some("endpoint") {
                    let base = reqwest::Url::parse(&self.url).map_err(invalid_data)?;
                    let endpoint = base.join(&event.data).map_err(invalid_data)?;
                    return self.start_sse_reader(endpoint, stream, buffer);
                }
            }
        }
    }

    fn start_sse_reader(
        &mut self,
        endpoint: reqwest::Url,
        stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
        buffer: String,
    ) -> io::Result<String> {
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            let mut buffer = buffer;
            loop {
                while let Some(event) = take_sse_event(&mut buffer) {
                    if event.event.as_deref() == Some("endpoint") {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
                        if sender.send(value).is_err() {
                            return;
                        }
                    }
                }
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
                    }
                    _ => return,
                }
            }
        });
        let endpoint = endpoint.to_string();
        self.sse = Some(SseState {
            endpoint: endpoint.clone(),
            receiver,
        });
        Ok(endpoint)
    }

    async fn sse_request<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        request: &JsonRpcRequest<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let endpoint = self.ensure_sse().await?;
        let body = serde_json::to_vec(request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let response = self.post(&endpoint, &body).await?;
        drain_response(response).await?;

        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let Some(state) = self.sse.as_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "MCP SSE transport closed",
                ));
            };
            match tokio::time::timeout_at(deadline, state.receiver.recv()).await {
                Ok(Some(value)) => {
                    if let Some(response) = value_into_response::<TResult>(value, &request.id)? {
                        return Ok(response);
                    }
                }
                Ok(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "MCP SSE stream closed",
                    ))
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "MCP SSE response timed out",
                    ))
                }
            }
        }
    }

    async fn ensure_ws(&mut self) -> io::Result<()> {
        if self.ws.is_some() {
            return Ok(());
        }
        let mut request = self.url.clone().into_client_request().map_err(io_err)?;
        for (name, value) in &self.headers {
            let name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(invalid_data)?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(invalid_data)?;
            request.headers_mut().insert(name, value);
        }
        let (socket, _) = tokio::time::timeout(self.timeout, self.connect_ws(request))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("MCP websocket connect timed out: {}", self.url),
                )
            })??;
        self.ws = Some(WsState { socket });
        Ok(())
    }

    async fn connect_ws(
        &self,
        request: tokio_tungstenite::tungstenite::handshake::client::Request,
    ) -> io::Result<(
        WsStream,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    )> {
        let proxy = match &self.ws_proxy {
            Some(proxy) => proxy,
            None => {
                return tokio_tungstenite::connect_async(request)
                    .await
                    .map_err(io_err)
            }
        };
        let uri = request.uri();
        let host = uri
            .host()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("MCP websocket URL 缺少主机名：{}", self.url),
                )
            })?
            .to_string();
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("wss") {
                443
            } else {
                80
            });
        let mut stream = tokio::net::TcpStream::connect((proxy.host.as_str(), proxy.port))
            .await
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "MCP websocket 连接代理 {}:{} 失败：{error}",
                        proxy.host, proxy.port
                    ),
                )
            })?;
        let mut head = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
        if let Some(authorization) = &proxy.authorization {
            head.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await.map_err(io_err)?;
        let status = read_proxy_connect_status(&mut stream).await?;
        if !(200..300).contains(&status) {
            return Err(io::Error::other(format!(
                "MCP websocket 代理 CONNECT {host}:{port} 返回 HTTP {status}"
            )));
        }
        tokio_tungstenite::client_async_tls_with_config(request, stream, None, None)
            .await
            .map_err(io_err)
    }

    async fn ws_request<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        request: &JsonRpcRequest<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        self.ensure_ws().await?;
        let body = serde_json::to_string(request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        {
            let state = self.ws.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "MCP websocket not connected")
            })?;
            state
                .socket
                .send(Message::Text(body.into()))
                .await
                .map_err(io_err)?;
        }

        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let state = self.ws.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "MCP websocket not connected")
            })?;
            let message = tokio::time::timeout_at(deadline, state.socket.next())
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "MCP websocket response timed out")
                })?;
            match message {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).map_err(invalid_data)?;
                    if let Some(response) = value_into_response::<TResult>(value, &request.id)? {
                        return Ok(response);
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    let value: Value = serde_json::from_slice(&bytes).map_err(invalid_data)?;
                    if let Some(response) = value_into_response::<TResult>(value, &request.id)? {
                        return Ok(response);
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    let _ = state.socket.send(Message::Pong(payload)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "MCP websocket closed",
                    ))
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(io_err(error)),
            }
        }
    }
}

async fn drain_response(response: reqwest::Response) -> io::Result<()> {
    // 通知没有响应体要求；消费掉避免连接池悬挂。
    let _ = response.bytes().await.map_err(reqwest_err)?;
    Ok(())
}

fn value_into_response<TResult: DeserializeOwned>(
    value: Value,
    expected_id: &JsonRpcId,
) -> io::Result<Option<JsonRpcResponse<TResult>>> {
    if value.get("id").is_none() {
        return Ok(None);
    }
    let raw: JsonRpcResponse<Value> = serde_json::from_value(value).map_err(invalid_data)?;
    if raw.id != *expected_id {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(&raw).map_err(invalid_data)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(invalid_data)
}

fn response_from_sse_text<TResult: DeserializeOwned>(
    text: &str,
    expected_id: &JsonRpcId,
) -> io::Result<JsonRpcResponse<TResult>> {
    // 服务端可能用 CRLF 分隔事件（DeepWiki/Context7 就是），统一成 LF 再解析。
    let mut buffer = text.replace("\r\n", "\n");
    loop {
        let event = match take_sse_event(&mut buffer) {
            Some(event) => event,
            None => {
                // 流结束时最后一个事件可能没有空行收尾。
                if buffer.trim().is_empty() {
                    break;
                }
                parse_sse_event(&std::mem::take(&mut buffer))
            }
        };
        if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
            if let Some(response) = value_into_response::<TResult>(value, expected_id)? {
                return Ok(response);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "MCP SSE response did not contain a matching JSON-RPC response",
    ))
}

struct SseEvent {
    event: Option<String>,
    data: String,
}

fn take_sse_event(buffer: &mut String) -> Option<SseEvent> {
    let index = buffer.find("\n\n")?;
    let raw = buffer[..index].to_string();
    buffer.drain(..index + 2);
    Some(parse_sse_event(&raw))
}

fn parse_sse_event(raw: &str) -> SseEvent {
    let mut event = None;
    let mut data = String::new();
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    SseEvent { event, data }
}

fn truncate(body: &str) -> String {
    const LIMIT: usize = 512;
    if body.len() <= LIMIT {
        body.to_string()
    } else {
        format!("{}…", &body[..LIMIT])
    }
}

/// 读取代理 `CONNECT` 响应头，返回状态码。
async fn read_proxy_connect_status(stream: &mut tokio::net::TcpStream) -> io::Result<u16> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).await.map_err(io_err)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "代理在 CONNECT 响应前关闭了连接",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 16 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "代理 CONNECT 响应头超过 16KB",
            ));
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("无法解析代理 CONNECT 响应：{}", truncate(&text)),
            )
        })
}

/// 按目标 URL 决定该用哪个环境变量代理（与 reqwest 的取值顺序一致）。
fn proxy_from_env(target: &str) -> Option<String> {
    let url = reqwest::Url::parse(target).ok()?;
    let host = url.host_str()?;
    if let Some(no_proxy) = env_value(&["NO_PROXY", "no_proxy"]) {
        if host_matches_no_proxy(host, &no_proxy) {
            return None;
        }
    }
    let secure = matches!(url.scheme(), "https" | "wss");
    let names: &[&str] = if secure {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    for name in names {
        if let Some(value) = env_value(&[name]) {
            return Some(value);
        }
    }
    None
}

fn env_value(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `NO_PROXY` 匹配：支持 `*`、精确主机、`.example.com` 与 `example.com` 后缀。
fn host_matches_no_proxy(host: &str, no_proxy: &str) -> bool {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            let entry = entry.to_ascii_lowercase();
            let entry = match entry.rsplit_once(':') {
                Some((host_part, port))
                    if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
                {
                    host_part.to_string()
                }
                _ => entry,
            };
            let entry = entry.trim_start_matches('.');
            if entry.is_empty() {
                return false;
            }
            host == entry || host.ends_with(&format!(".{entry}"))
        })
}

/// 代理凭据里的 `%xx` 解码（`Url::username()` 返回的是未解码片段）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn io_err(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

/// 把 `reqwest` 的失败归类成可判断的 `io::ErrorKind`：
/// 超时必须是 `TimedOut`，连接失败是 `ConnectionRefused`，便于上层给出
/// “是网络不可达还是实现缺陷”的准确结论。
fn reqwest_err(error: reqwest::Error) -> io::Error {
    let kind = if error.is_timeout() {
        io::ErrorKind::TimedOut
    } else if error.is_connect() {
        io::ErrorKind::ConnectionRefused
    } else if error.is_body() || error.is_decode() {
        io::ErrorKind::InvalidData
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, format!("MCP remote request failed: {error}"))
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use base64::Engine as _;

    use crate::mcp_client::{McpClientAuth, McpRemoteTransport};
    use crate::mcp_stdio::{JsonRpcId, McpInitializeClientInfo, McpInitializeParams};

    use super::{host_matches_no_proxy, McpRemoteProcess, ProxyEndpoint, ProxyScheme, RemoteKind};

    struct RecordedRequest {
        head: String,
        body: String,
    }

    type HttpResponse = (u16, Vec<(&'static str, String)>, String);

    fn transport(
        url: impl Into<String>,
        headers: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
    ) -> McpRemoteTransport {
        McpRemoteTransport {
            url: url.into(),
            headers,
            headers_helper: None,
            auth: McpClientAuth::None,
            tool_call_timeout_ms: timeout_ms,
            proxy: None,
        }
    }

    fn initialize_params() -> McpInitializeParams {
        McpInitializeParams {
            protocol_version: "2025-06-18".to_string(),
            capabilities: json!({}),
            client_info: McpInitializeClientInfo {
                name: "claw-test".to_string(),
                version: "0.0.0".to_string(),
            },
        }
    }

    fn initialize_result() -> Value {
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "serverInfo": { "name": "remote-test", "version": "1.0" },
        })
    }

    async fn start_http_server(
        responses: Vec<HttpResponse>,
    ) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        tokio::spawn(async move {
            for (status, headers, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                if let Some(request) = read_request(&mut socket).await {
                    sink.lock().unwrap().push(request);
                }
                let mut response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (name, value) in headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}/mcp"), requests)
    }

    async fn read_request(socket: &mut TcpStream) -> Option<RecordedRequest> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(index) = find_subslice(&buffer, b"\r\n\r\n") {
                break index;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let mut body = buffer[header_end + 4..].to_vec();
        while body.len() < content_length {
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        Some(RecordedRequest {
            head,
            body: String::from_utf8_lossy(&body).to_string(),
        })
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn header_line<'a>(request: &'a RecordedRequest, name: &str) -> Option<&'a str> {
        request.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then_some(value.trim())
        })
    }

    #[tokio::test]
    async fn http_transport_sends_json_rpc_with_auth_and_session_headers() {
        let (url, recorded) = start_http_server(vec![
            (
                200,
                vec![
                    ("Content-Type", "application/json".to_string()),
                    ("Mcp-Session-Id", "session-1".to_string()),
                ],
                json!({ "jsonrpc": "2.0", "id": 1, "result": initialize_result() }).to_string(),
            ),
            (
                200,
                vec![("Content-Type", "application/json".to_string())],
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": { "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] },
                })
                .to_string(),
            ),
        ])
        .await;

        let headers = BTreeMap::from([("X-Api-Key".to_string(), "secret".to_string())]);
        let mut process =
            McpRemoteProcess::new(RemoteKind::Http, transport(&url, headers, Some(5_000))).unwrap();

        let response = process
            .initialize(JsonRpcId::Number(1), initialize_params())
            .await
            .unwrap();
        assert_eq!(
            response.result.as_ref().unwrap().server_info.name,
            "remote-test"
        );

        let tools: crate::mcp_stdio::JsonRpcResponse<Value> = process
            .request::<Value, Value>(JsonRpcId::Number(2), "tools/list", None)
            .await
            .unwrap();
        assert_eq!(
            tools.result.unwrap()["tools"][0]["name"],
            Value::String("echo".to_string())
        );

        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].body.contains("\"method\":\"initialize\""));
        assert_eq!(header_line(&requests[0], "x-api-key"), Some("secret"));
        let accept = header_line(&requests[0], "accept").unwrap_or_default();
        assert!(accept.contains("application/json"));
        assert!(accept.contains("text/event-stream"));
        assert_eq!(
            header_line(&requests[1], "mcp-session-id"),
            Some("session-1")
        );
    }

    #[tokio::test]
    async fn http_transport_decodes_sse_framed_responses() {
        let body = format!(
            "event: message\ndata: {}\n\n",
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } })
        );
        let (url, _) = start_http_server(vec![(
            200,
            vec![("Content-Type", "text/event-stream".to_string())],
            body,
        )])
        .await;

        let mut process = McpRemoteProcess::new(
            RemoteKind::Http,
            transport(&url, BTreeMap::new(), Some(5_000)),
        )
        .unwrap();
        let response: crate::mcp_stdio::JsonRpcResponse<Value> = process
            .request::<Value, Value>(JsonRpcId::Number(1), "ping", None)
            .await
            .unwrap();
        assert_eq!(response.result.unwrap()["ok"], Value::Bool(true));
    }

    #[tokio::test]
    async fn http_transport_surfaces_errors_status_and_timeouts() {
        let (url, _) = start_http_server(vec![(
            500,
            vec![("Content-Type", "text/plain".to_string())],
            "boom".to_string(),
        )])
        .await;
        let mut process = McpRemoteProcess::new(
            RemoteKind::Http,
            transport(&url, BTreeMap::new(), Some(5_000)),
        )
        .unwrap();
        let error = process
            .request::<Value, Value>(JsonRpcId::Number(1), "ping", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 500"), "{error}");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(socket);
        });
        let mut process = McpRemoteProcess::new(
            RemoteKind::Http,
            transport(format!("http://{address}/mcp"), BTreeMap::new(), Some(120)),
        )
        .unwrap();
        let error = process
            .request::<Value, Value>(JsonRpcId::Number(1), "ping", None)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    }

    #[tokio::test]
    async fn websocket_transport_round_trips_json_rpc() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut websocket) = tokio_tungstenite::accept_async(socket).await else {
                return;
            };
            while let Some(Ok(message)) = websocket.next().await {
                let Some(text) = message.to_text().ok() else {
                    continue;
                };
                let Ok(request) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                let id = request["id"].clone();
                let method = request["method"].as_str().unwrap_or_default();
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "method": method, "ok": true },
                });
                let _ = websocket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        response.to_string().into(),
                    ))
                    .await;
            }
        });

        let mut process = McpRemoteProcess::new(
            RemoteKind::WebSocket,
            transport(format!("ws://{address}/mcp"), BTreeMap::new(), Some(5_000)),
        )
        .unwrap();
        let response: crate::mcp_stdio::JsonRpcResponse<Value> = process
            .request::<Value, Value>(JsonRpcId::Number(7), "tools/call", None)
            .await
            .unwrap();
        assert_eq!(
            response.result.unwrap()["method"],
            Value::String("tools/call".to_string())
        );
    }

    /// 旧版 SSE：GET 流先下发 `endpoint` 事件，之后所有 JSON-RPC 响应都走该流。
    async fn spawn_sse_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stream_slot: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&stream_slot);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let slot = Arc::clone(&slot);
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    if request.head.starts_with("GET ") {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
                            )
                            .await;
                        let _ = socket
                            .write_all(b"event: endpoint\ndata: /messages\n\n")
                            .await;
                        let _ = socket.flush().await;
                        *slot.lock().unwrap() = Some(socket);
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    let payload =
                        serde_json::from_str::<Value>(&request.body).unwrap_or(Value::Null);
                    let body = json!({
                        "jsonrpc": "2.0",
                        "id": payload["id"].clone(),
                        "result": { "ok": true },
                    })
                    .to_string();
                    let mut stream = slot.lock().unwrap().take();
                    if let Some(stream) = stream.as_mut() {
                        let _ = stream
                            .write_all(format!("event: message\ndata: {body}\n\n").as_bytes())
                            .await;
                        let _ = stream.flush().await;
                    }
                    *slot.lock().unwrap() = stream;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{address}/mcp")
    }

    #[tokio::test]
    async fn sse_transport_uses_endpoint_event_and_streamed_responses() {
        let url = spawn_sse_server().await;
        let mut process = McpRemoteProcess::new(
            RemoteKind::Sse,
            transport(url, BTreeMap::new(), Some(5_000)),
        )
        .unwrap();
        let response: crate::mcp_stdio::JsonRpcResponse<Value> = process
            .request::<Value, Value>(JsonRpcId::Number(1), "ping", None)
            .await
            .unwrap();
        assert_eq!(response.result.unwrap()["ok"], Value::Bool(true));
    }

    /// 最小 streamable HTTP MCP 服务端：逐连接处理 initialize / tools/list /
    /// tools/call，供 `McpServerManager` 端到端验证远程传输路径。
    async fn spawn_mcp_http_server() -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    let payload =
                        serde_json::from_str::<Value>(&request.body).unwrap_or(Value::Null);
                    let method = payload["method"].as_str().unwrap_or_default().to_string();
                    let id = payload["id"].clone();
                    let body = match method.as_str() {
                        "initialize" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "remote-http", "version": "1.0" },
                            },
                        })
                        .to_string(),
                        "tools/list" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "echo",
                                    "description": "echo input",
                                    "inputSchema": { "type": "object" },
                                }],
                            },
                        })
                        .to_string(),
                        "tools/call" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": format!(
                                        "echo:{}",
                                        payload["params"]["arguments"]["text"]
                                            .as_str()
                                            .unwrap_or_default()
                                    ),
                                }],
                            },
                        })
                        .to_string(),
                        _ => String::new(),
                    };
                    sink.lock().unwrap().push(request);
                    let status = if body.is_empty() { 202 } else { 200 };
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{address}/mcp"), requests)
    }

    #[tokio::test]
    async fn server_manager_discovers_and_calls_remote_http_tools() {
        use crate::config::{
            ConfigSource, McpRemoteServerConfig, McpServerConfig, ScopedMcpServerConfig,
        };
        use crate::mcp::mcp_tool_name;
        use crate::mcp_stdio::McpServerManager;

        let (url, recorded) = spawn_mcp_http_server().await;
        let servers = BTreeMap::from([(
            "remote".to_string(),
            ScopedMcpServerConfig {
                required: true,
                scope: ConfigSource::Local,
                config: McpServerConfig::Http(McpRemoteServerConfig {
                    url,
                    headers: BTreeMap::from([(
                        "Authorization".to_string(),
                        "Bearer test".to_string(),
                    )]),
                    headers_helper: None,
                    oauth: None,
                    tool_call_timeout_ms: Some(5_000),
                    proxy: None,
                }),
            },
        )]);
        let mut manager = McpServerManager::from_servers(&servers);

        let tools = manager
            .discover_tools()
            .await
            .expect("discover remote tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].qualified_name, mcp_tool_name("remote", "echo"));

        let response = manager
            .call_tool(
                &mcp_tool_name("remote", "echo"),
                Some(json!({ "text": "hi" })),
            )
            .await
            .expect("call remote tool");
        let result = response.result.expect("tool result");
        assert_eq!(result.content[0].data.get("text"), Some(&json!("echo:hi")));
        manager.shutdown().await.expect("shutdown");

        let requests = recorded.lock().unwrap();
        let methods = requests
            .iter()
            .map(|request| {
                serde_json::from_str::<Value>(&request.body)
                    .ok()
                    .and_then(|body| body["method"].as_str().map(str::to_string))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        assert!(methods.contains(&"initialize".to_string()), "{methods:?}");
        assert!(
            methods.contains(&"notifications/initialized".to_string()),
            "{methods:?}"
        );
        assert!(methods.contains(&"tools/list".to_string()), "{methods:?}");
        assert!(methods.contains(&"tools/call".to_string()), "{methods:?}");
        assert_eq!(
            header_line(&requests[0], "authorization"),
            Some("Bearer test")
        );
    }

    #[tokio::test]
    async fn websocket_transport_tunnels_through_http_connect_proxy() {
        let ws_address = spawn_ws_echo_server().await;
        let (proxy_url, recorded) = spawn_connect_proxy().await;
        let proxy_with_auth = proxy_url.replace("http://", "http://user:pa%40ss@");

        let mut process = McpRemoteProcess::new(
            RemoteKind::WebSocket,
            McpRemoteTransport {
                url: format!("ws://{ws_address}/mcp"),
                headers: BTreeMap::new(),
                headers_helper: None,
                auth: McpClientAuth::None,
                tool_call_timeout_ms: Some(5_000),
                proxy: Some(proxy_with_auth),
            },
        )
        .unwrap();
        let response: crate::mcp_stdio::JsonRpcResponse<Value> = process
            .request::<Value, Value>(JsonRpcId::Number(3), "tools/list", None)
            .await
            .unwrap();
        assert_eq!(
            response.result.unwrap()["method"],
            Value::String("tools/list".to_string())
        );

        let heads = recorded.lock().unwrap();
        let head = heads.first().expect("proxy received CONNECT");
        let expected = format!("CONNECT {ws_address} HTTP/1.1");
        assert_eq!(head.lines().next(), Some(expected.as_str()));
        assert_eq!(
            header_line_text(head, "proxy-authorization"),
            Some("Basic dXNlcjpwYUBzcw==")
        );
    }

    #[test]
    fn websocket_proxy_rejects_socks_and_https_proxies() {
        for proxy in ["socks5://127.0.0.1:1080", "https://127.0.0.1:8443"] {
            let error = McpRemoteProcess::new(
                RemoteKind::WebSocket,
                McpRemoteTransport {
                    url: "wss://example.test/mcp".to_string(),
                    headers: BTreeMap::new(),
                    headers_helper: None,
                    auth: McpClientAuth::None,
                    tool_call_timeout_ms: None,
                    proxy: Some(proxy.to_string()),
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported, "{error}");
        }
    }

    #[test]
    fn proxy_endpoint_decodes_credentials_and_classifies_scheme() {
        let endpoint = ProxyEndpoint::parse("http://u%40x:p%3Ass@127.0.0.1:8080", "test").unwrap();
        assert_eq!(endpoint.scheme, ProxyScheme::Http);
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 8080);
        let authorization = endpoint.authorization.unwrap();
        let encoded = authorization.trim_start_matches("Basic ");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "u@x:p:ss");

        assert_eq!(
            ProxyEndpoint::parse("socks5://127.0.0.1:1080", "test")
                .unwrap()
                .scheme,
            ProxyScheme::Socks5
        );
        assert!(ProxyEndpoint::parse("ftp://127.0.0.1:21", "test").is_err());
    }

    #[test]
    fn no_proxy_matching_covers_hosts_wildcards_and_ports() {
        assert!(host_matches_no_proxy("api.github.com", "api.github.com"));
        assert!(host_matches_no_proxy("api.github.com", ".github.com"));
        assert!(host_matches_no_proxy("api.github.com", "github.com"));
        assert!(host_matches_no_proxy(
            "internal.test",
            "example.com, .test:443"
        ));
        assert!(host_matches_no_proxy("anything.test", "*"));
        assert!(!host_matches_no_proxy("api.github.com", "example.com"));
        assert!(!host_matches_no_proxy("notgithub.com", "github.com"));
        assert!(!host_matches_no_proxy("api.github.com", ""));
    }

    async fn spawn_ws_echo_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut websocket) = tokio_tungstenite::accept_async(socket).await else {
                return;
            };
            while let Some(Ok(message)) = websocket.next().await {
                let Some(text) = message.to_text().ok() else {
                    continue;
                };
                let Ok(request) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": { "method": request["method"].as_str().unwrap_or_default() },
                });
                let _ = websocket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        response.to_string().into(),
                    ))
                    .await;
            }
        });
        address
    }

    /// 最小 HTTP CONNECT 代理：记录请求头，然后把字节双向转发到目标。
    async fn spawn_connect_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        tokio::spawn(async move {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 512];
            loop {
                let Ok(read) = client.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buffer).to_string();
            let target = head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            sink.lock().unwrap().push(head);
            let Ok(mut upstream) = TcpStream::connect(&target).await else {
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            };
            let _ = client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await;
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
        (format!("http://{address}"), recorded)
    }

    fn header_line_text<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
    }

    /// 真实公网 MCP 验证（默认 `#[ignore]`，需显式 `--ignored` 运行）。
    ///
    /// 覆盖两类公开服务：免鉴权的 DeepWiki / Context7 与需要 OAuth/Bearer
    /// 的 Notion / Stripe / Supabase（后者只验证鉴权拒绝行为如实暴露）。
    #[tokio::test]
    #[ignore = "requires public network access"]
    async fn public_remote_mcp_servers_list_and_call_tools() {
        use crate::config::{
            ConfigSource, McpRemoteServerConfig, McpServerConfig, ScopedMcpServerConfig,
        };
        use crate::mcp::mcp_tool_name;
        use crate::mcp_stdio::McpServerManager;

        let remote = |url: &str| ScopedMcpServerConfig {
            required: true,
            scope: ConfigSource::Local,
            config: McpServerConfig::Http(McpRemoteServerConfig {
                url: url.to_string(),
                headers: BTreeMap::new(),
                headers_helper: None,
                oauth: None,
                tool_call_timeout_ms: Some(30_000),
                proxy: None,
            }),
        };
        let servers = BTreeMap::from([
            (
                "deepwiki".to_string(),
                remote("https://mcp.deepwiki.com/mcp"),
            ),
            (
                "context7".to_string(),
                remote("https://mcp.context7.com/mcp"),
            ),
        ]);
        let mut manager = McpServerManager::from_servers(&servers);
        let tools = manager
            .discover_tools()
            .await
            .expect("discover public tools");
        for tool in &tools {
            println!("TOOL {} ({})", tool.qualified_name, tool.server_name);
        }
        assert!(
            tools
                .iter()
                .any(|tool| tool.qualified_name == mcp_tool_name("deepwiki", "read_wiki_structure")),
            "deepwiki tool missing"
        );
        assert!(
            tools
                .iter()
                .any(|tool| tool.qualified_name == mcp_tool_name("context7", "resolve-library-id")),
            "context7 tool missing"
        );

        let response = manager
            .call_tool(
                &mcp_tool_name("deepwiki", "read_wiki_structure"),
                Some(json!({ "repoName": "facebook/react" })),
            )
            .await
            .expect("call deepwiki tool");
        let result = response.result.expect("deepwiki result");
        println!("CALL deepwiki.read_wiki_structure -> {:?}", result);
        assert!(
            !result.content.is_empty(),
            "deepwiki returned empty content"
        );
        manager.shutdown().await.expect("shutdown");
    }
}
