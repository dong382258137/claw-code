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
use serde_json::{json, Value};
use tower_http::trace::TraceLayer;

use crate::config::ImBridgeConfig;
use crate::connectors::feishu::{FeishuClient, FeishuWebhookBody};
use crate::connectors::wecom::WeComClient;
use crate::response::{ResponseCollector, RouteTarget, SessionRouter};
use crate::session::{ChatKey, ImRequest, SessionManager, SpawnKeepAlive, SpawnResult};

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

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/feishu/webhook", post(feishu_webhook))
        .route("/wecom/webhook", get(wecom_verify_url))
        .route("/wecom/webhook", post(wecom_message_callback))
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
        keep_alive: spawn_result.keep_alive,
    })
}

// ── Health check ────────────────────────────────────────────

async fn health_handler() -> &'static str {
    "ok"
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
            handle_im_message(
                &state,
                "feishu",
                &msg.chat_id,
                &msg.user_id,
                &msg.text,
                move |state, chat_key, req| async move {
                    let Some(feishu) = state.feishu_client.as_ref() else {
                        tracing::error!("feishu_client not initialized in callback");
                        return;
                    };
                    match state.session_manager.process_request(req).await {
                        Ok(session_id) => {
                            let mut router = state.session_router.lock().await;
                            router.insert(
                                session_id,
                                RouteTarget::Feishu {
                                    client: feishu.clone(),
                                    chat_id: chat_key.chat_id.clone(),
                                },
                            );
                        }
                        Err(e) => {
                            tracing::error!("failed to process feishu message: {e}");
                            let _ = feishu
                                .send_text_message(&chat_key.chat_id, &format!("Error: {e}"))
                                .await;
                        }
                    }
                },
            )
            .await;
            Ok(Json(json!({})))
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
                    let mut router = state.session_router.lock().await;
                    router.insert(
                        session_id,
                        RouteTarget::WeCom {
                            client: wecom.clone(),
                            chat_id: chat_key.chat_id.clone(),
                        },
                    );
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
