//! Axum HTTP server: receives IM webhooks and routes to the agent.
//!
//! Routes:
//! - `GET  /health`           — health check
//! - `POST /feishu/webhook`   — Feishu event subscription callback
//! - `GET  /wecom/webhook`    — WeCom URL verification
//! - `POST /wecom/webhook`    — WeCom message callback

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::trace::TraceLayer;

use crate::config::ImBridgeConfig;
use crate::connectors::feishu::{FeishuClient, FeishuUserMessage, FeishuWebhookBody};
use crate::connectors::feishu_ws::FeishuWsClient;
use crate::connectors::wecom::WeComClient;
use crate::response::{ResponseCollector, RouteTarget, SessionRouter};
use crate::session::{ChatKey, ImRequest, SessionManager, SpawnKeepAlive, SpawnResult};
use runtime::{bus_now_ms, global_session_bus, BusMessageKind, PeerKind};

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub session_manager: Arc<SessionManager>,
    pub session_router: SessionRouter,
    pub feishu_client: Option<FeishuClient>,
    pub wecom_client: Option<WeComClient>,
}

/// Running server handles.
pub struct ServerHandle {
    pub server_task: tokio::task::JoinHandle<()>,
    pub collector_task: tokio::task::JoinHandle<()>,
    pub persist_task: tokio::task::JoinHandle<()>,
    /// Feishu long-connection (WebSocket) client task, when enabled.
    pub feishu_ws_task: Option<tokio::task::JoinHandle<()>>,
    /// Session Bus 跨进程邮箱轮询任务，配置 `bus_root` 时启用（审查补充 2026-08-12）。
    pub bus_poll_task: Option<tokio::task::JoinHandle<()>>,
    #[allow(dead_code)]
    pub keep_alive: SpawnKeepAlive,
}

/// Start the HTTP server and response collector.
pub async fn run_server(
    config: &ImBridgeConfig,
    spawn_result: SpawnResult,
    session_router: SessionRouter,
) -> Result<ServerHandle, String> {
    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .map_err(|e| format!("invalid listen address '{}': {e}", config.listen_addr))?;

    let feishu_client = config.feishu.clone().map(FeishuClient::new);
    let use_feishu_ws = config
        .feishu
        .as_ref()
        .map(|c| c.mode == "ws")
        .unwrap_or(false);

    let wecom_client = match &config.wecom {
        Some(cfg) => Some(WeComClient::new(cfg.clone())?),
        None => None,
    };

    let state = AppState {
        session_manager: spawn_result.manager,
        session_router: session_router.clone(),
        feishu_client,
        wecom_client,
    };

    // Epic 3 Session Bus hub：进程内唯一总线，IM 频道注册为 `im:*` peer。
    // 放行 hub 内互通：`im:im`（IM 频道互发）与 `ide:*`（外部进程 → IM 频道），
    // 保持 deny-by-default 的其余规则（Main/Subagent 不受影响）。
    // 审查补充(2026-08-12):追加 `im:ide` 放行——IM 频道可经文件事件队列投递到
    // 远端邮箱(TUI 主会话 / IDE 面板),打通跨进程反向通道(否则 Im→Ide 默认拒绝,
    // IM 用户 `/bus send main:xxx` 连文件都不写)。
    let bus = global_session_bus();
    bus.set_allow(PeerKind::Im, PeerKind::Im, true);
    bus.set_allow(PeerKind::Im, PeerKind::Ide, true);
    bus.set_allow(PeerKind::Ide, PeerKind::Im, true);
    bus.set_allow(PeerKind::Ide, PeerKind::Ide, true);

    // Spawn response collector
    let collector = ResponseCollector::new(
        spawn_result.notification_rx,
        spawn_result.completion_rx,
        session_router,
    );
    let collector_task = tokio::spawn(collector.run());

    // Periodic session persistence (every 60s)
    let persist_mgr = state.session_manager.clone();
    let persist_task = tokio::spawn(async move {
        let interval = persist_mgr.persistence().save_interval();
        loop {
            tokio::time::sleep(interval).await;
            persist_mgr.persist_sessions().await;
        }
    });

    // Feishu long-connection (WebSocket) mode: subscribe events via a
    // full-duplex channel instead of the HTTP webhook. Requires no public URL.
    let feishu_ws_task = if use_feishu_ws {
        let feishu_cfg = config
            .feishu
            .clone()
            .ok_or("feishu ws mode requires a [feishu] config section")?;
        let (ws_tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel::<FeishuUserMessage>();
        let ws_client = FeishuWsClient::new(feishu_cfg, ws_tx);
        let ws_task = tokio::spawn(async move {
            if let Err(e) = ws_client.run().await {
                tracing::error!("feishu long connection client stopped: {e}");
            }
        });
        let consumer_state = state.clone();
        let consumer_task = tokio::spawn(async move {
            while let Some(msg) = ws_rx.recv().await {
                process_feishu_message(&consumer_state, msg).await;
            }
        });
        tracing::info!("feishu long connection (ws) mode enabled");
        Some(tokio::spawn(async move {
            let _ = ws_task.await;
            let _ = consumer_task.await;
        }))
    } else {
        None
    };

    // 审查补充(2026-08-12):配置 `bus_root` 时启用跨进程文件事件队列——
    // TUI 主会话 ↔ IM 频道互通:本进程消费 IM 频道邮箱并直发真实 IM(广播/定向),
    // IM 用户 `/bus send` 反向经文件投递到 TUI 主会话邮箱。
    // 注意:须在 `.with_state(state)` 消费 state 之前启动(此处只 clone 字段)。
    let bus_poll_task = if let Some(root) = config.bus_root.clone() {
        let manager = state.session_manager.clone();
        bus.set_bus_root(root.clone());
        tracing::info!("Session Bus cross-process mailbox polling enabled at {}", root.display());
        Some(manager.start_bus_mailbox_poller(root))
    } else {
        None
    };

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/feishu/webhook", post(feishu_webhook))
        .route("/wecom/webhook", get(wecom_verify_url))
        .route("/wecom/webhook", post(wecom_message_callback))
        // Epic 3 Session Bus hub API：跨进程/跨频道互通
        .route("/api/bus/send", post(bus_send_handler))
        .route("/api/bus/list", get(bus_list_handler))
        .route("/api/bus/poll", get(bus_poll_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    tracing::info!("IM bridge listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind to {addr}: {e}"))?;

    let server_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("HTTP server error: {e}");
        }
    });

    Ok(ServerHandle {
        server_task,
        collector_task,
        persist_task,
        feishu_ws_task,
        bus_poll_task,
        keep_alive: spawn_result.keep_alive,
    })
}

// ── Health check ────────────────────────────────────────────

async fn health_handler() -> &'static str {
    "ok"
}

// ── Epic 3 Session Bus hub API ─────────────────────────────

/// `POST /api/bus/send` 请求体。
#[derive(Deserialize)]
struct BusSendRequest {
    /// 发送方 peer id；缺省时 hub 自动注册为 `ext:<n>`（外部进程身份，kind=Ide）。
    #[serde(default)]
    from: Option<String>,
    /// 目标 peer id（`im:{platform}:{chat_id}`）或 `*` 广播。
    to: String,
    /// 消息种类，缺省 `message`。
    #[serde(default)]
    kind: Option<String>,
    /// 消息文本。
    text: String,
}

/// `POST /api/bus/send` — 外部进程（IDE / 其他 CLAW 实例 / 脚本）经 hub 向总线发布消息。
///
/// 发送方若未注册会被自动注册为 `Ide` peer（外部进程身份）；发布受 `session_bus.allow`
/// 权限约束（hub 默认放行 `im:im` / `ide:*`，见 `run_server` 初始化）。
/// 目标为本 hub 已注册的 IM 频道时，同时向该频道直发真实 IM 消息。
async fn bus_send_handler(
    State(state): State<AppState>,
    body: String,
) -> Result<Json<Value>, StatusCode> {
    let req: BusSendRequest = serde_json::from_str(&body).map_err(|e| {
        tracing::warn!("bus send: invalid body: {e}");
        StatusCode::BAD_REQUEST
    })?;

    // 外部进程身份：注册为 Ide peer（幂等，统一路由层入口）
    let from = match req.from {
        Some(f) if !f.trim().is_empty() => f,
        _ => format!("ext:{}", bus_now_ms()),
    };
    let bus = global_session_bus();
    if !bus.ensure_external_peer(&from) {
        return Ok(Json(json!({ "ok": false, "error": "invalid sender id" })));
    }

    // 统一外部入口：kind 解析 + 文本消息构造 + 发布（含权限校验 + 限流）
    let kind = BusMessageKind::from_str(req.kind.as_deref().unwrap_or("message"));
    let delivered = match bus.publish_text(&from, &req.to, kind, &req.text) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("bus send rejected: {e}");
            return Ok(Json(json!({ "ok": false, "error": e })));
        }
    };

    // 目标为本 hub 已注册的 IM 频道 → 直发真实 IM 消息。
    // 注意：publish 已在上方完成，此处仅做直发（bus_send_and_push 的 publish 幂等——
    // 同消息再发布只会让 unread 重复，故直接复用其直发逻辑会重复投递。这里改为
    // 仅当目标是本地 IM 频道时执行直发，不重复 publish）。
    let mut pushed_im = false;
    if req.to.starts_with("im:") {
        let route = state
            .session_manager
            .bus_route_for(&req.to);
        if route {
            pushed_im = state
                .session_manager
                .push_im_route(&req.to, &req.text, &from)
                .await;
        }
    }

    Ok(Json(json!({
        "ok": true,
        "delivered": delivered,
        "pushed_im": pushed_im,
    })))
}

/// `GET /api/bus/list` — 列出全部 peer（供外部进程发现会话）。
async fn bus_list_handler(State(_state): State<AppState>) -> Json<Value> {
    let bus = global_session_bus();
    let peers: Vec<Value> = bus
        .peers_snapshot()
        .into_iter()
        .map(|p| {
            json!({
                "session_id": p.session_id,
                "label": p.label,
                "kind": p.kind.as_str(),
                "status": p.status.as_str(),
                "unread": p.unread,
            })
        })
        .collect();
    Json(json!({ "peers": peers }))
}

/// `GET /api/bus/poll?session_id=X` — 轮询某 peer 的未读消息（外部订阅方拉取）。
///
/// 返回该 peer 未读的 `BusMessage` 列表（时间升序）并标记已读（消费确认）。
async fn bus_poll_handler(
    State(_state): State<AppState>,
    Query(params): Query<BusPollParams>,
) -> Json<Value> {
    let bus = global_session_bus();
    let msgs: Vec<Value> = bus
        .unread_messages(&params.session_id)
        .into_iter()
        .map(|m| {
            json!({
                "from": m.from,
                "to": m.to,
                "kind": m.kind.as_str(),
                "payload": m.payload,
                "hop": m.hop,
                "ts_ms": m.ts_ms,
            })
        })
        .collect();
    bus.mark_read(&params.session_id);
    Json(json!({ "messages": msgs }))
}

/// `GET /api/bus/poll` 查询参数。
#[derive(Deserialize)]
struct BusPollParams {
    session_id: String,
}

// ── Feishu webhook ─────────────────────────────────────────

async fn feishu_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, StatusCode> {
    // Guard: ensure feishu client is configured
    let feishu = state
        .feishu_client
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // P1-3: Verify event signature when encrypt_key is configured.
    // Feishu sends three headers: X-Lark-Signature, X-Lark-Request-Timestamp,
    // X-Lark-Request-Nonce. If encrypt_key is set but headers are missing or
    // the signature doesn't match, reject the request as unauthorized.
    if feishu.requires_signature() {
        let sig = headers
            .get("x-lark-signature")
            .and_then(|v| v.to_str().ok());
        let ts = headers
            .get("x-lark-request-timestamp")
            .and_then(|v| v.to_str().ok());
        let nonce = headers
            .get("x-lark-request-nonce")
            .and_then(|v| v.to_str().ok());

        match (sig, ts, nonce) {
            (Some(s), Some(t), Some(n)) => {
                if !feishu.verify_event_signature(t, n, &body, s) {
                    tracing::warn!("feishu signature verification failed");
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
            _ => {
                tracing::warn!("feishu signature headers missing (encrypt_key configured)");
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
    }

    let webhook: FeishuWebhookBody = serde_json::from_str(&body).map_err(|e| {
        tracing::warn!("failed to parse feishu webhook body: {e}");
        StatusCode::BAD_REQUEST
    })?;

    match webhook {
        FeishuWebhookBody::Challenge(challenge) => {
            tracing::info!("feishu URL verification challenge received");
            Ok(Json(json!({ "challenge": challenge.challenge })))
        }
        FeishuWebhookBody::EventCallback(callback) => {
            let msg = FeishuClient::extract_message(&callback).ok_or(StatusCode::OK)?;
            process_feishu_message(&state, msg).await;
            Ok(Json(json!({})))
        }
    }
}

/// Process a Feishu user message: run commands directly, otherwise submit to
/// the agent and register the route for the response.
async fn process_feishu_message(state: &AppState, msg: FeishuUserMessage) {
    let chat_key = ChatKey::new("feishu", &msg.chat_id);
    let req = ImRequest {
        chat_key: chat_key.clone(),
        user_id: msg.user_id,
        text: msg.text,
    };

    match state.session_manager.handle_command(&req).await {
        Ok(Some(cmd_response)) => {
            tracing::info!(
                "command handled for feishu chat {}",
                msg.chat_id
            );
            if let Some(client) = &state.feishu_client {
                if let Err(e) = client
                    .send_text_message(&msg.chat_id, &cmd_response)
                    .await
                {
                    tracing::error!("failed to send feishu command response: {e}");
                }
            }
        }
        Ok(None) => {
            let Some(feishu) = state.feishu_client.clone() else {
                tracing::error!("feishu_client not initialized");
                return;
            };
            match state.session_manager.process_request(req).await {
                Ok(session_id) => {
                    let target = RouteTarget::Feishu {
                        client: feishu,
                        chat_id: msg.chat_id.clone(),
                    };
                    let mut router = state.session_router.lock().await;
                    router.insert(session_id, target.clone());
                    drop(router);
                    // Epic 3：同步注册 bus 直发路由（跨频道 `/bus send` 直达本频道）
                    state
                        .session_manager
                        .register_bus_route(&chat_key, target)
                        .await;
                }
                Err(e) => {
                    tracing::error!("failed to process feishu message: {e}");
                    let _ = feishu
                        .send_text_message(&msg.chat_id, &format!("Error: {e}"))
                        .await;
                }
            }
        }
        Err(e) => {
            tracing::error!("command handling error for feishu: {e}");
        }
    }
}

// ── WeCom webhook ──────────────────────────────────────────

/// Query params for WeCom URL verification.
#[derive(serde::Deserialize)]
struct WeComVerifyParams {
    msg_signature: String,
    timestamp: String,
    nonce: String,
    echostr: String,
}

/// GET /wecom/webhook — URL verification.
async fn wecom_verify_url(
    State(state): State<AppState>,
    Query(params): Query<WeComVerifyParams>,
) -> Result<String, StatusCode> {
    let wecom = state
        .wecom_client
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    match wecom.verify_url(
        &params.msg_signature,
        &params.timestamp,
        &params.nonce,
        &params.echostr,
    ) {
        Ok(decrypted) => {
            tracing::info!("wecom URL verification succeeded");
            Ok(decrypted)
        }
        Err(e) => {
            tracing::error!("wecom URL verification failed: {e}");
            Err(StatusCode::FORBIDDEN)
        }
    }
}

/// POST /wecom/webhook — message callback.
///
/// Must return within 5 seconds — returns passive acknowledgment immediately,
/// then pushes actual response via webhook asynchronously.
async fn wecom_message_callback(
    State(state): State<AppState>,
    body: String,
) -> Result<Html<String>, StatusCode> {
    let wecom = state
        .wecom_client
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // Parse and decrypt the incoming message
    let msg = match wecom.parse_message_callback(&body) {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            // Non-text message or empty — return empty ack
            let ack = wecom.build_passive_response(None).unwrap_or_default();
            return Ok(Html(ack));
        }
        Err(e) => {
            tracing::error!("wecom message parse failed: {e}");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let chat_id = msg.chat_id.clone();
    let user_id = msg.user_id.clone();
    let text = msg.text.clone();

    handle_im_message(
        &state,
        "wecom",
        &chat_id,
        &user_id,
        &text,
        move |state, chat_key, req| async move {
            let Some(wecom) = state.wecom_client.as_ref() else {
                tracing::error!("wecom_client not initialized in callback");
                return;
            };
            match state.session_manager.process_request(req).await {
                Ok(session_id) => {
                    let target = RouteTarget::WeCom {
                        client: wecom.clone(),
                        chat_id: chat_key.chat_id.clone(),
                    };
                    let mut router = state.session_router.lock().await;
                    router.insert(session_id, target.clone());
                    drop(router);
                    // Epic 3：同步注册 bus 直发路由（跨频道 `/bus send` 直达本频道）
                    state
                        .session_manager
                        .register_bus_route(&chat_key, target)
                        .await;
                }
                Err(e) => {
                    tracing::error!("failed to process wecom message: {e}");
                    let _ = wecom
                        .push_text_message(&chat_key.chat_id, &format!("Error: {e}"))
                        .await;
                }
            }
        },
    )
    .await;

    // Return passive ack immediately (WeCom 5s constraint)
    let ack = wecom.build_passive_response(None).unwrap_or_default();
    Ok(Html(ack))
}

// ── Common message handler ─────────────────────────────────

/// Handle an IM message, checking for commands first.
/// If the message is a command, reply directly without invoking the agent.
/// Otherwise, calls `process_fn` to submit to the agent.
async fn handle_im_message<F, Fut>(
    state: &AppState,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    text: &str,
    process_fn: F,
) where
    F: FnOnce(AppState, ChatKey, ImRequest) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let chat_key = ChatKey::new(platform, chat_id);
    let req = ImRequest {
        chat_key: chat_key.clone(),
        user_id: user_id.to_string(),
        text: text.to_string(),
    };

    // Check if it's a command
    match state.session_manager.handle_command(&req).await {
        Ok(Some(cmd_response)) => {
            // Command handled — reply directly based on platform
            tracing::info!("command '{}' handled for {platform} chat {chat_id}", text);

            match platform {
                "feishu" => {
                    if let Some(client) = &state.feishu_client {
                        if let Err(e) = client.send_text_message(chat_id, &cmd_response).await {
                            tracing::error!("failed to send command response to feishu: {e}");
                        }
                    }
                }
                "wecom" => {
                    if let Some(client) = &state.wecom_client {
                        if let Err(e) = client.push_text_message(chat_id, &cmd_response).await {
                            tracing::error!("failed to send command response to wecom: {e}");
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(None) => {
            // Not a command — submit to agent
            process_fn(state.clone(), chat_key, req).await;
        }
        Err(e) => {
            tracing::error!("command handling error for {platform}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::SessionBus;

    fn im_peer(id: &str) -> BusPeer {
        BusPeer {
            session_id: id.to_string(),
            label: format!("IM {id}"),
            kind: PeerKind::Im,
            status: PeerStatus::Idle,
            unread: 0,
            last_seen_ms: bus_now_ms(),
            config_path: None,
        }
    }

    /// Epic 3 hub 权限模型：模拟 `run_server` 中 `set_allow(im:im)` 后，
    /// 两个 IM 频道可以互发；缺省 deny-by-default 下不可。
    #[test]
    fn hub_allow_im_to_im_enables_cross_channel() {
        let bus = SessionBus::new();
        bus.register(im_peer("im:feishu:oc_a")).unwrap();
        bus.register(im_peer("im:wecom:chat_b")).unwrap();

        // 缺省：Im → Im 被拒（deny by default）
        let msg = BusMessage {
            from: "im:feishu:oc_a".into(),
            to: "im:wecom:chat_b".into(),
            kind: BusMessageKind::Message,
            payload: json!({ "text": "hi" }),
            hop: 0,
            ts_ms: bus_now_ms(),
        };
        assert!(bus.publish(msg.clone()).unwrap().is_empty());

        // 放行 im:im（run_server 初始化）后互通
        bus.set_allow(PeerKind::Im, PeerKind::Im, true);
        let delivered = bus.publish(msg).unwrap();
        assert_eq!(delivered, vec!["im:wecom:chat_b".to_string()]);
    }

    /// Epic 3：外部进程（Ide）经 hub 向 IM 频道投递需放行 ide:im。
    #[test]
    fn hub_allow_ide_to_im_enables_external_send() {
        let bus = SessionBus::new();
        bus.register(im_peer("im:feishu:oc_a")).unwrap();
        bus.register(BusPeer {
            session_id: "ext:panel-1".into(),
            label: "external:panel-1".into(),
            kind: PeerKind::Ide,
            status: PeerStatus::Idle,
            unread: 0,
            last_seen_ms: bus_now_ms(),
            config_path: None,
        })
        .unwrap();

        let msg = BusMessage {
            from: "ext:panel-1".into(),
            to: "im:feishu:oc_a".into(),
            kind: BusMessageKind::Message,
            payload: json!({ "text": "from ide" }),
            hop: 0,
            ts_ms: bus_now_ms(),
        };
        // 缺省拒绝
        assert!(bus.publish(msg.clone()).unwrap().is_empty());
        // hub 放行 ide:im 后可达
        bus.set_allow(PeerKind::Ide, PeerKind::Im, true);
        let delivered = bus.publish(msg).unwrap();
        assert_eq!(delivered, vec!["im:feishu:oc_a".to_string()]);
    }

    /// Epic 3：外部进程未注册时，`/api/bus/send` 自动注册为 Ide peer。
    #[test]
    fn bus_send_auto_registers_external_sender() {
        let bus = SessionBus::new();
        bus.set_allow(PeerKind::Ide, PeerKind::Im, true);
        bus.register(im_peer("im:feishu:oc_a")).unwrap();

        // 模拟 bus_send_handler 的自动注册逻辑
        let from = "ext:auto-1";
        bus.register(BusPeer {
            session_id: from.into(),
            label: format!("external:{from}"),
            kind: PeerKind::Ide,
            status: PeerStatus::Idle,
            unread: 0,
            last_seen_ms: bus_now_ms(),
            config_path: None,
        })
        .unwrap();

        let msg = BusMessage {
            from: from.into(),
            to: "im:feishu:oc_a".into(),
            kind: BusMessageKind::Message,
            payload: json!({ "text": "hello" }),
            hop: 0,
            ts_ms: bus_now_ms(),
        };
        let delivered = bus.publish(msg).unwrap();
        assert_eq!(delivered, vec!["im:feishu:oc_a".to_string()]);
        assert_eq!(bus.unread_messages("im:feishu:oc_a").len(), 1);
    }

    /// 审查补充(2026-08-12):IM → 远端跨进程反向通道。IM 频道 `/bus send` 到
    /// TUI 主会话邮箱需放行 im:ide(文件路由目标 kind=Ide);run_server 已配置。
    #[test]
    fn hub_allow_im_to_ide_enables_reverse_file_route() {
        let bus = SessionBus::new();
        bus.register(im_peer("im:feishu:oc_a")).unwrap();
        // TUI 主会话邮箱(远端,本进程未注册其 peer)
        let root = std::env::temp_dir().join(format!("bus-im-test-{}", bus_now_ms()));
        SessionBus::ensure_mailbox(&root, "main-abc").expect("ensure mailbox");
        bus.set_bus_root(root.clone());

        let msg = BusMessage {
            from: "im:feishu:oc_a".into(),
            to: "main-abc".into(),
            kind: BusMessageKind::Message,
            payload: json!({ "text": "from im" }),
            hop: 0,
            ts_ms: bus_now_ms(),
        };
        // 缺省:Im → Ide 拒绝 → 进程内与文件路由都不投递
        assert!(bus.publish(msg.clone()).unwrap().is_empty());
        let mailbox = SessionBus::mailbox_dir(&root, "main-abc");
        let count = |p: &std::path::Path| -> usize {
            std::fs::read_dir(p).map(|rd| rd.flatten().count()).unwrap_or(0)
        };
        assert_eq!(count(&mailbox), 0, "denied: no file written");
        // 放行 im:ide(run_server 初始化)后文件路由生效
        bus.set_allow(PeerKind::Im, PeerKind::Ide, true);
        assert!(bus.publish(msg).unwrap().is_empty(), "进程内仍不投递(未注册)");
        assert_eq!(count(&mailbox), 1, "file routed to main mailbox");
        std::fs::remove_dir_all(&root).ok();
    }
}
