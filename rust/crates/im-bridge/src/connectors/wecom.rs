//! WeCom (企业微信) smart bot connector.
//!
//! Implements the smart bot callback protocol:
//! 1. URL verification (GET): verify signature + decrypt echostr
//! 2. Message callback (POST): decrypt, submit to agent, return passive ack within 5s
//! 3. Active push: send response via webhook URL when agent completes
//!
//! Key constraint: WeCom smart bot requires HTTP response within 5 seconds.
//! So we return acknowledgment immediately and push results via webhook asynchronously.
//!
//! References:
//! - <https://developer.work.weixin.qq.com/document/path/100719>

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::WeComConfig;
use crate::connectors::wecom_crypto::{
    build_passive_response_xml, WeComCrypto, WeComEncryptedBody, WeComMessage, WeComUserMessage,
};

/// WeCom bot client: handles crypto, URL verification, and message pushing.
#[derive(Clone)]
pub struct WeComClient {
    config: Arc<WeComConfig>,
    crypto: Arc<WeComCrypto>,
    http: Client,
    /// Access token + expiry for webhook pushes.
    token_state: Arc<tokio::sync::Mutex<WeComTokenState>>,
}

impl std::fmt::Debug for WeComClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeComClient")
            .field("corp_id", &self.config.corp_id)
            .finish_non_exhaustive()
    }
}

struct WeComTokenState {
    token: String,
    expires_at: std::time::Instant,
}

impl WeComClient {
    pub fn new(config: WeComConfig) -> Result<Self, String> {
        let crypto = WeComCrypto::new(
            &config.token,
            &config.encoding_aes_key,
            &config.corp_id,
        )?;

        Ok(Self {
            config: Arc::new(config),
            crypto: Arc::new(crypto),
            http: Client::new(),
            token_state: Arc::new(tokio::sync::Mutex::new(WeComTokenState {
                token: String::new(),
                expires_at: std::time::Instant::now(),
            })),
        })
    }

    /// Handle URL verification (GET from WeCom).
    ///
    /// Query params: msg_signature, timestamp, nonce, echostr
    ///
    /// Returns the decrypted echostr to prove we own the URL.
    pub fn verify_url(
        &self,
        msg_signature: &str,
        timestamp: &str,
        nonce: &str,
        echostr: &str,
    ) -> Result<String, String> {
        if !self
            .crypto
            .verify_signature(timestamp, nonce, echostr, msg_signature)
        {
            return Err("URL verification: signature mismatch".to_string());
        }

        let decrypted = self.crypto.decrypt(echostr)?;
        Ok(decrypted)
    }

    /// Decrypt and parse an incoming WeCom message callback body.
    ///
    /// Returns the extracted user message if valid.
    pub fn parse_message_callback(
        &self,
        body_xml: &str,
    ) -> Result<Option<WeComUserMessage>, String> {
        // Parse the encrypted XML wrapper
        let encrypted: WeComEncryptedBody = quick_xml::de::from_str(body_xml)
            .map_err(|e| format!("failed to parse wecom encrypted body: {e}"))?;

        // Verify signature if present
        if let (Some(sig), Some(ts), Some(nonce)) = (
            &encrypted.msg_signature,
            &encrypted.time_stamp,
            &encrypted.nonce,
        ) {
            if !self.crypto.verify_signature(ts, nonce, &encrypted.encrypt, sig) {
                return Err("message callback: signature mismatch".to_string());
            }
        }

        // Decrypt
        let decrypted_xml = self.crypto.decrypt(&encrypted.encrypt)?;

        tracing::debug!("wecom decrypted XML: {decrypted_xml}");

        // Parse inner message
        let msg: WeComMessage = quick_xml::de::from_str(&decrypted_xml)
            .map_err(|e| format!("failed to parse wecom message: {e}"))?;

        Ok(msg.into_user_message(self.config.webhook_url.clone()))
    }

    /// Build a passive response XML for immediate return (within 5s).
    ///
    /// WeCom smart bot expects the response body to be the message reply.
    /// If we can't reply immediately, return empty ack and push later via webhook.
    pub fn build_passive_response(
        &self,
        reply_text: Option<&str>,
    ) -> Result<String, String> {
        use rand::Rng;

        let inner_xml = build_passive_response_xml(reply_text);
        let encrypted = self.crypto.encrypt(&inner_xml)?;

        let timestamp = chrono::Utc::now().timestamp().to_string();
        let nonce: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();

        let msg_signature =
            self.crypto
                .generate_signature(&timestamp, &nonce, &encrypted);

        let response_xml = format!(
            r#"<xml><Encrypt><![CDATA[{encrypted}]]></Encrypt><MsgSignature><![CDATA[{msg_signature}]]></MsgSignature><TimeStamp>{timestamp}</TimeStamp><Nonce><![CDATA[{nonce}]]></Nonce></xml>"#
        );

        Ok(response_xml)
    }

    /// Get an access token for API calls.
    async fn get_access_token(&self) -> Result<String, String> {
        let mut state = self.token_state.lock().await;

        // Return cached token if still valid (with 5min buffer)
        if !state.token.is_empty()
            && state.expires_at > std::time::Instant::now() + std::time::Duration::from_secs(300)
        {
            return Ok(state.token.clone());
        }

        let url = format!(
            "https://qyapi.weixin.qq.com/cgi-bin/gettoken?corpid={}&corpsecret={}",
            self.config.corp_id, self.config.secret
        );

        #[derive(Deserialize)]
        struct TokenResponse {
            errcode: i32,
            errmsg: Option<String>,
            access_token: Option<String>,
            expires_in: Option<i64>,
        }

        let resp: TokenResponse = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("token parse failed: {e}"))?;

        if resp.errcode != 0 {
            return Err(format!(
                "token error (code {}): {}",
                resp.errcode,
                resp.errmsg.as_deref().unwrap_or("unknown")
            ));
        }

        let token = resp
            .access_token
            .ok_or("no access_token in response")?;
        let expires_in = resp.expires_in.unwrap_or(7200) as u64;
        state.token = token.clone();
        state.expires_at =
            std::time::Instant::now() + std::time::Duration::from_secs(expires_in);

        Ok(token)
    }

    /// Push a text message to a WeCom chat via webhook URL.
    ///
    /// Uses the smart bot webhook (passive response channel) or
    /// the generic message-send API with access token.
    pub async fn push_text_message(&self, _chat_id: &str, text: &str) -> Result<(), String> {
        // Prefer webhook if configured (simpler, no token needed)
        if let Some(ref webhook_url) = self.config.webhook_url {
            self.push_via_webhook(webhook_url, text).await
        } else {
            // Fall back to API-based send
            self.push_via_api(text).await
        }
    }

    /// Push via smart bot webhook URL.
    async fn push_via_webhook(&self, webhook_url: &str, text: &str) -> Result<(), String> {
        #[derive(Serialize)]
        struct WebhookBody<'a> {
            msgtype: &'a str,
            text: WebhookText<'a>,
        }

        #[derive(Serialize)]
        struct WebhookText<'a> {
            content: &'a str,
        }

        let body = WebhookBody {
            msgtype: "text",
            text: WebhookText { content: text },
        };

        let resp = self
            .http
            .post(webhook_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("webhook push failed: {e}"))?;

        #[derive(Deserialize)]
        struct WebhookResponse {
            errcode: i32,
            errmsg: Option<String>,
        }

        let result: WebhookResponse = resp
            .json()
            .await
            .map_err(|e| format!("webhook response parse failed: {e}"))?;

        if result.errcode != 0 {
            return Err(format!(
                "webhook push error (code {}): {}",
                result.errcode,
                result.errmsg.as_deref().unwrap_or("unknown")
            ));
        }

        tracing::info!("wecom webhook message sent");
        Ok(())
    }

    /// Push via WeCom API (requires access token).
    async fn push_via_api(&self, text: &str) -> Result<(), String> {
        let token = self.get_access_token().await?;

        #[derive(Serialize)]
        struct ApiBody<'a> {
            touser: &'a str,
            msgtype: &'a str,
            agentid: i64,
            text: ApiText<'a>,
        }

        #[derive(Serialize)]
        struct ApiText<'a> {
            content: &'a str,
        }

        let body = ApiBody {
            touser: "@all",
            msgtype: "text",
            agentid: self.config.agent_id.unwrap_or(0),
            text: ApiText { content: text },
        };

        let url = format!(
            "https://qyapi.weixin.qq.com/cgi-bin/message/send?access_token={token}"
        );

        #[derive(Deserialize)]
        struct ApiResponse {
            errcode: i32,
            errmsg: Option<String>,
        }

        let resp: ApiResponse = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("api push failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("api response parse failed: {e}"))?;

        if resp.errcode != 0 {
            return Err(format!(
                "api push error (code {}): {}",
                resp.errcode,
                resp.errmsg.as_deref().unwrap_or("unknown")
            ));
        }

        tracing::info!("wecom api message sent");
        Ok(())
    }
}
