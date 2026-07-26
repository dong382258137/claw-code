//! Feishu (Lark) connector: webhook handler + message sending.
//!
//! Implements two parts of the Feishu bot integration:
//! 1. Webhook endpoint (URL verification + event callback)
//! 2. Message sending (via Feishu Open API)
//!
//! References:
//! - <https://open.feishu.cn/document/server-docs/event-subscription-guide/event-subscription-configure-/request-url-configuration>
//! - <https://open.feishu.cn/document/server-docs/im-v1/message-content-and-send/message_content_overview>

use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::FeishuConfig;

// ── Webhook Request/Response types ──────────────────────────

/// Top-level webhook body from Feishu.
/// Can be a URL verification challenge or an event callback.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum FeishuWebhookBody {
    Challenge(FeishuChallenge),
    EventCallback(FeishuEventCallback),
}

/// URL verification challenge (step 1 of event subscription setup).
#[derive(Debug, Deserialize)]
pub struct FeishuChallenge {
    #[serde(default)]
    pub challenge: String,
    #[serde(default)]
    pub token: String,
    #[serde(rename = "type")]
    pub event_type: String,
}

/// Event callback wrapper (V2.0 schema).
#[derive(Debug, Deserialize)]
pub struct FeishuEventCallback {
    pub schema: Option<String>,
    pub header: Option<FeishuEventHeader>,
    pub event: Option<FeishuEvent>,
    /// Legacy V1.0 challenge field (some older apps).
    pub challenge: Option<String>,
    pub token: Option<String>,
    #[serde(rename = "type")]
    pub event_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEventHeader {
    pub event_id: Option<String>,
    pub event_type: Option<String>,
    pub create_time: Option<String>,
    pub token: Option<String>,
    pub app_id: Option<String>,
    pub tenant_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEvent {
    pub sender: Option<FeishuSender>,
    pub message: Option<FeishuMessageEvent>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSender {
    pub sender_id: Option<FeishuSenderId>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSenderId {
    pub open_id: Option<String>,
    pub user_id: Option<String>,
    pub union_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuMessageEvent {
    pub message_id: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub chat_id: String,
    pub chat_type: String,
    pub create_time: Option<String>,
    pub message_type: String,
    pub content: String, // JSON string, parsed separately
}

// ── Message Content types ──────────────────────────────────

/// Parsed message content from `FeishuMessageEvent.content`.
#[derive(Debug, Deserialize)]
pub struct FeishuMessageContent {
    pub text: Option<String>,
}

// ── Extracted user message ─────────────────────────────────

/// A user message extracted from a Feishu webhook event.
#[derive(Debug, Clone)]
pub struct FeishuUserMessage {
    pub chat_id: String,
    pub user_id: String,
    pub message_id: String,
    pub text: String,
}

// ── API Response types ─────────────────────────────────────

/// Tenant access token response.
#[derive(Debug, Deserialize)]
struct TenantAccessTokenResponse {
    code: i32,
    msg: Option<String>,
    tenant_access_token: Option<String>,
    expire: Option<i32>,
}

/// Send message request body.
#[derive(Debug, Serialize)]
struct SendMessageRequest {
    receive_id: String,
    msg_type: String,
    content: String,
}

/// Send message response.
#[derive(Debug, Deserialize)]
struct SendMessageResponse {
    code: i32,
    msg: Option<String>,
}

// ── FeishuClient ───────────────────────────────────────────

/// Feishu API client: handles token management and message sending.
#[derive(Clone)]
pub struct FeishuClient {
    config: Arc<FeishuConfig>,
    http: Client,
    /// Cached tenant access token + expiry.
    token_state: Arc<tokio::sync::Mutex<TokenState>>,
}

impl std::fmt::Debug for FeishuClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuClient")
            .field("app_id", &self.config.app_id)
            .finish_non_exhaustive()
    }
}

struct TokenState {
    token: String,
    expires_at: Instant,
}

impl FeishuClient {
    pub fn new(config: FeishuConfig) -> Self {
        Self {
            config: Arc::new(config),
            http: Client::new(),
            token_state: Arc::new(tokio::sync::Mutex::new(TokenState {
                token: String::new(),
                expires_at: Instant::now(),
            })),
        }
    }

    /// Whether signature verification is required (encrypt_key is configured).
    ///
    /// When `true`, the webhook handler MUST verify the `X-Lark-Signature`
    /// header before processing the event.
    pub fn requires_signature(&self) -> bool {
        self.config.encrypt_key.is_some()
    }

    /// Verify the Feishu event signature (if encrypt_key is configured).
    ///
    /// Feishu signature format: SHA256(timestamp + nonce + encrypt_key + body)
    /// where `body` is the raw request body string.
    ///
    /// If `encrypt_key` is None, verification is skipped (dev mode).
    pub fn verify_event_signature(
        &self,
        timestamp: &str,
        nonce: &str,
        body: &str,
        signature: &str,
    ) -> bool {
        match &self.config.encrypt_key {
            Some(key) => {
                let data = format!("{timestamp}{nonce}{key}{body}");
                let mut hasher = Sha256::new();
                hasher.update(data.as_bytes());
                let computed = hex::encode(hasher.finalize());
                // Constant-time-ish comparison
                computed == signature
            }
            None => true, // No encrypt key → skip verification (dev mode)
        }
    }

    /// Extract a user message from a Feishu event callback.
    pub fn extract_message(event: &FeishuEventCallback) -> Option<FeishuUserMessage> {
        let event = event.event.as_ref()?;
        let sender = event.sender.as_ref()?;
        let msg = event.message.as_ref()?;

        // Only handle text messages for now
        if msg.message_type != "text" {
            tracing::debug!("ignoring non-text message type: {}", msg.message_type);
            return None;
        }

        let sender_id = sender
            .sender_id
            .as_ref()
            .and_then(|s| s.open_id.clone().or_else(|| s.user_id.clone()))
            .unwrap_or_else(|| "unknown".to_string());

        let content: FeishuMessageContent = serde_json::from_str(&msg.content).ok()?;
        let text = content.text?;

        // Skip empty messages
        if text.trim().is_empty() {
            return None;
        }

        Some(FeishuUserMessage {
            chat_id: msg.chat_id.clone(),
            user_id: sender_id,
            message_id: msg.message_id.clone(),
            text,
        })
    }

    /// Get a valid tenant access token, refreshing if needed.
    async fn get_token(&self) -> Result<String, String> {
        let mut state = self.token_state.lock().await;

        // Return cached token if still valid (with 60s buffer)
        if !state.token.is_empty() && state.expires_at > Instant::now() + Duration::from_secs(60) {
            return Ok(state.token.clone());
        }

        // Request new token
        let url = "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal";
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "app_id": self.config.app_id,
                "app_secret": self.config.app_secret,
            }))
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?;

        let body: TenantAccessTokenResponse = resp
            .json()
            .await
            .map_err(|e| format!("token parse failed: {e}"))?;

        if body.code != 0 {
            return Err(format!(
                "token request error (code {}): {}",
                body.code,
                body.msg.as_deref().unwrap_or("unknown")
            ));
        }

        let token = body.tenant_access_token.ok_or("no token in response")?;
        let expire_secs = body.expire.unwrap_or(7200) as u64;
        state.token = token.clone();
        state.expires_at = Instant::now() + Duration::from_secs(expire_secs);

        Ok(token)
    }

    /// Send a text message to a Feishu chat.
    pub async fn send_text_message(&self, chat_id: &str, text: &str) -> Result<(), String> {
        let token = self.get_token().await?;

        let content = serde_json::json!({
            "text": text
        });

        let req = SendMessageRequest {
            receive_id: chat_id.to_string(),
            msg_type: "text".to_string(),
            content: serde_json::to_string(&content).map_err(|e| format!("json error: {e}"))?,
        };

        let url = "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id";
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("send message failed: {e}"))?;

        let body: SendMessageResponse = resp
            .json()
            .await
            .map_err(|e| format!("send message parse failed: {e}"))?;

        if body.code != 0 {
            return Err(format!(
                "send message error (code {}): {}",
                body.code,
                body.msg.as_deref().unwrap_or("unknown")
            ));
        }

        tracing::info!("message sent to chat {chat_id}");
        Ok(())
    }
}
