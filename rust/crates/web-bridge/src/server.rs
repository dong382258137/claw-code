//! 本地 bridge server:把浏览器扩展与 WebApiClient 的进程内 turn 路由接起来。
//!
//! - `GET /health` — 健康检查
//! - `GET /ws/ext?token=...` — 扩展连接(WebSocket 升级;token 校验失败即拒绝)
//!
//! ## 消息流
//!
//! ```text
//! 扩展 ──UserRun{text}──▶ broker.user_run → runner(agent 回合)
//! agent ──turn_bus──▶ run_turn_loop ──InjectPrompt{turn_id,text}──▶ 扩展
//! 扩展 ──Reply{turn_id,text}──▶ broker.resolve_turn ──回转──▶ 阻塞中的 round_trip
//! 回合结束 ──Final{text}──▶ 扩展(测试断言/UI 提示)
//! ```
//!
//! 仅回环监听 + token 鉴权(单扩展连接,后连覆盖先连)。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use crate::ws_exchange::{TurnRequest, TurnRunner};

/// server → 扩展 的消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToExt {
    /// 告诉扩展:把 `text` 填入网页输入框并发送,完成后回 `Reply`。
    InjectPrompt { turn_id: String, text: String },
    /// 整个用户回合结束(最终执行结果,测试断言 / UI 提示用)。
    Final { text: String },
    /// 握手成功。
    HelloOk,
    /// 服务端错误(未连接扩展、单回合并发等)。
    Error { message: String },
}

/// 扩展 → server 的消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtToServer {
    /// 用户点击"claw 执行"按钮,提交任务文本。
    UserRun { text: String },
    /// 针对某次 InjectPrompt 的回复(最终稳定文本)。
    Reply { turn_id: String, text: String },
    /// 握手(连接 query 已带 token,此消息可选)。
    Hello,
    /// 扩展侧自动诊断上报(注入状态,供守护进程日志排查)。
    Diag { payload: String },
}

/// WebSocket 升级 query 参数。
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    #[serde(default)]
    pub token: Option<String>,
}

/// 共享状态。
pub struct AppState {
    pub broker: Arc<TurnBroker>,
    pub token: String,
}

/// 回合路由器:维护扩展连接、pending 的注入回合、单飞行限制。
pub struct TurnBroker {
    /// 当前扩展连接的发送端。
    ext_tx: Mutex<Option<mpsc::UnboundedSender<ServerToExt>>>,
    /// turn_id → 等待中的标准库回执通道。
    pending: Mutex<HashMap<String, std_mpsc::Sender<Result<String, String>>>>,
    next_turn_id: AtomicU64,
    /// 用户任务处理器(完整 agent 回合)。
    runner: TurnRunner,
    /// 是否已有用户回合在执行(M1 单飞行)。
    active_run: Mutex<bool>,
}

impl TurnBroker {
    fn new(runner: TurnRunner) -> Arc<Self> {
        Arc::new(Self {
            ext_tx: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next_turn_id: AtomicU64::new(1),
            runner,
            active_run: Mutex::new(false),
        })
    }

    fn next_turn_id(&self) -> String {
        format!("turn_{}", self.next_turn_id.fetch_add(1, Ordering::Relaxed))
    }

    /// 更新扩展连接(置 None = 断开)。
    async fn set_ext(&self, tx: Option<mpsc::UnboundedSender<ServerToExt>>) {
        *self.ext_tx.lock().await = tx;
    }

    async fn send_ext(&self, msg: ServerToExt) -> bool {
        let mut guard = self.ext_tx.lock().await;
        match guard.as_mut() {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// 用户回合主入口:校验单飞行 → spawn runner → 结果回 Final。
    async fn user_run(&self, text: String) {
        {
            let mut active = self.active_run.lock().await;
            if *active {
                let _ = self
                    .send_ext(ServerToExt::Error {
                        message: "another claw turn is already running".to_string(),
                    })
                    .await;
                return;
            }
            *active = true;
        }
        let text_for_run = text.clone();
        let runner_result = (self.runner)(text_for_run).await;
        {
            let mut active = self.active_run.lock().await;
            *active = false;
        }
        let text = match runner_result {
            Ok(t) => t,
            Err(e) => format!("claw error: {e}"),
        };
        let _ = self.send_ext(ServerToExt::Final { text }).await;
    }

    /// 处理扩展的 Reply:找到 pending 回执并投递;找不到则忽略(超时已清理)。
    async fn resolve_turn(&self, turn_id: &str, text: String) {
        tracing::info!("web turn {turn_id}: reply arrived ({} chars)", text.len());
        let reply = self.pending.lock().await.remove(turn_id);
        if let Some(reply) = reply {
            let _ = reply.send(Ok(text));
        }
    }
}

/// 启动 bridge server,返回托管的 broker 与 turn 请求总线。
///
/// - `runner`:用户任务处理器
/// - 返回值:`(broker, turn_bus)` —— 调用方把 `turn_bus` 写进
///   [`crate::ws_exchange::make_user_turn_runner`] 需要的 `OnceLock`。
pub fn spawn_bridge(runner: TurnRunner) -> (Arc<TurnBroker>, mpsc::UnboundedSender<TurnRequest>) {
    let broker = TurnBroker::new(runner);
    let (turn_bus, turn_rx) = mpsc::unbounded_channel();
    let turn_loop_broker = broker.clone();
    tokio::spawn(async move {
        turn_loop_broker.run_turn_loop(turn_rx).await;
    });
    (broker, turn_bus)
}

impl TurnBroker {
    /// 消费 `TurnRequest`:生成 turn_id,登记回执,向扩展注入提示,等 Reply。
    async fn run_turn_loop(&self, mut turn_rx: mpsc::UnboundedReceiver<TurnRequest>) {
        while let Some(req) = turn_rx.recv().await {
            let turn_id = self.next_turn_id();
            // 扩展未连接:立即失败,避免阻塞 agent 线程直到超时。
            if !self.ext_tx.lock().await.is_some() {
                tracing::warn!("web turn {turn_id}: extension not connected");
                let _ = req
                    .reply_tx
                    .send(Err("extension not connected".to_string()));
                continue;
            }
            self.pending
                .lock()
                .await
                .insert(turn_id.clone(), req.reply_tx);
            let ok = self
                .send_ext(ServerToExt::InjectPrompt {
                    turn_id: turn_id.clone(),
                    text: req.prompt.clone(),
                })
                .await;
            tracing::info!(
                "web turn {turn_id}: injected ({} chars, delivered={ok})",
                req.prompt.len()
            );
            if !ok {
                if let Some(tx) = self.pending.lock().await.remove(&turn_id) {
                    let _ = tx.send(Err("extension disconnected while injecting".to_string()));
                }
            }
        }
    }
}

/// HTTP 服务入口。`addr` 应为 127.0.0.1 + 具体端口(测试用 0 端口自动分配,
/// 由返回的 `resolved_addr` 获知)。
pub async fn serve(
    addr: SocketAddr,
    broker: Arc<TurnBroker>,
    token: String,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    let state = Arc::new(AppState { broker, token });
    let app = Router::new()
        .route("/health", get(health))
        .route("/ws/ext", get(ext_ws))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind {addr}: {e}"))?;
    let resolved = listener
        .local_addr()
        .map_err(|e| format!("failed to read local addr: {e}"))?;

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("web bridge http server error: {e}");
        }
    });
    Ok((resolved, handle))
}

async fn health() -> &'static str {
    "ok"
}

/// `/ws/ext` 升级处理器:校验 token 后进入扩展会话循环。
async fn ext_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WsQuery>,
) -> Response {
    let provided = query.token.as_deref().unwrap_or_default();
    if provided != state.token {
        tracing::warn!("web bridge ws token mismatch (rejected extension)");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let broker = state.broker.clone();
    ws.on_upgrade(move |socket| ext_session(socket, broker))
}

/// 单条扩展连接的会话循环:
/// 建 sink 转发任务(ServerToExt → ws)+ recv 循环(ws → 路由到 broker)。
async fn ext_session(socket: WebSocket, broker: Arc<TurnBroker>) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (ext_tx, mut ext_rx) = mpsc::unbounded_channel::<ServerToExt>();

    // 转发任务:broker 广播 → ws 写出。循环/发送失败即退出。
    let sink_task = tokio::spawn(async move {
        while let Some(msg) = ext_rx.recv().await {
            let payload = serde_json::to_string(&msg).unwrap_or_else(|_| "{}".to_string());
            match ws_sink.send(WsMessage::Text(payload.into())).await {
                Ok(()) => {}
                Err(_) => break,
            }
        }
    });

    broker.set_ext(Some(ext_tx)).await;
    tracing::info!("web bridge: extension (壳) connected");
    let _ = broker.send_ext(ServerToExt::HelloOk).await;

    while let Some(Ok(msg)) = ws_stream.next().await {
        let text = match msg.into_text() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let parsed: ExtToServer = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("web bridge ws: unparsable message: {e}");
                continue;
            }
        };
        match parsed {
            // user_run 必须后台执行:它 await 完整 agent 回合,若在此循环里
            // 阻塞,扩展的 Reply 就没有收循环处理,所有 round_trip 都会卡死。
            ExtToServer::UserRun { text } => {
                let broker = broker.clone();
                tokio::spawn(async move {
                    broker.user_run(text).await;
                });
            }
            ExtToServer::Reply { turn_id, text } => broker.resolve_turn(&turn_id, text).await,
            ExtToServer::Hello => {}
            ExtToServer::Diag { payload } => {
                tracing::info!("extension diag: {payload}");
            }
        }
    }

    // 断开:清空扩展连接,停止 sink。
    broker.set_ext(None).await;
    tracing::info!("web bridge: extension disconnected");
    sink_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn turn_id_is_unique_and_monotonic() {
        let broker = TurnBroker::new(Box::new(|_: String| {
            Box::pin(async { Ok("ok".to_string()) })
        }));
        let a = broker.next_turn_id();
        let b = broker.next_turn_id();
        assert_ne!(a, b);
    }
}
