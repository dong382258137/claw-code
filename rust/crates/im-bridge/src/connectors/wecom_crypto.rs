//! WeCom (企业微信) crypto utilities: XML parsing, AES decrypt/encrypt, SHA1 signature.
//!
//! WeCom smart bot message protocol:
//! - Messages are XML-encoded
//! - Content is encrypted with AES-256-CBC, PKCS#7 padding
//! - Signature: SHA1(sort(token, timestamp, nonce, encrypt_msg))
//! - Encrypted payload format: random(16) + msg_len(4 big-endian) + msg + corp_id
//!
//! References:
//! - <https://developer.work.weixin.qq.com/document/path/100719>

use aes::Aes256;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use serde::Deserialize;
use sha1::{Digest, Sha1};

type Aes256CbcDec = cbc::Decryptor<Aes256>;
type Aes256CbcEnc = cbc::Encryptor<Aes256>;

/// Crypto context for one WeCom bot instance.
pub struct WeComCrypto {
    aes_key: Vec<u8>, // 32 bytes from base64 decode of EncodingAESKey + "="
    corp_id: String,
    token: String,
}

impl WeComCrypto {
    /// Create a new crypto context.
    ///
    /// `encoding_aes_key` is the 43-character base64-encoded AES key.
    /// We append "=" to get a valid base64 string → 32 bytes.
    pub fn new(token: &str, encoding_aes_key: &str, corp_id: &str) -> Result<Self, String> {
        let key_str = format!("{encoding_aes_key}=");
        let aes_key = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &key_str)
            .map_err(|e| format!("invalid encoding_aes_key: {e}"))?;
        if aes_key.len() != 32 {
            return Err(format!(
                "encoding_aes_key must decode to 32 bytes, got {}",
                aes_key.len()
            ));
        }
        Ok(Self {
            aes_key,
            corp_id: corp_id.to_string(),
            token: token.to_string(),
        })
    }

    /// Verify WeCom message signature.
    ///
    /// Signature = SHA1(sort(token, timestamp, nonce, msg_encrypt))
    pub fn verify_signature(
        &self,
        timestamp: &str,
        nonce: &str,
        encrypt_msg: &str,
        signature: &str,
    ) -> bool {
        let mut parts = [self.token.as_str(), timestamp, nonce, encrypt_msg];
        parts.sort();
        let combined = parts.join("");
        let mut hasher = Sha1::new();
        hasher.update(combined.as_bytes());
        let computed = hex::encode(hasher.finalize());
        // Use constant-time-ish comparison
        computed.len() == signature.len()
            && computed
                .bytes()
                .zip(signature.bytes())
                .fold(0, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    /// Decrypt a base64-encoded encrypted message.
    ///
    /// Returns the plaintext XML string.
    pub fn decrypt(&self, encrypted: &str) -> Result<String, String> {
        let ciphertext =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encrypted)
                .map_err(|e| format!("base64 decode failed: {e}"))?;

        // IV = first 16 bytes of AES key
        // IV = first 16 bytes of AES key (key is 32 bytes for AES-256-CBC)
        let iv: [u8; 16] = self.aes_key[..16]
            .try_into()
            .map_err(|_| "AES key too short for IV".to_string())?;

        let plaintext = Aes256CbcDec::new(self.aes_key.as_slice().into(), &iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(&ciphertext)
            .map_err(|e| format!("AES decrypt failed: {e}"))?;

        Self::parse_decrypted_payload(&plaintext, &self.corp_id)
    }

    /// Parse the decrypted payload format: random(16) + msg_len(4) + msg + corp_id
    fn parse_decrypted_payload(data: &[u8], expected_corp_id: &str) -> Result<String, String> {
        if data.len() < 20 {
            return Err("decrypted message too short".to_string());
        }

        let msg_len = u32::from_be_bytes([data[16], data[17], data[18], data[19]]) as usize;

        let msg_start = 20;
        let msg_end = msg_start + msg_len;
        if msg_end > data.len() {
            return Err(format!(
                "message length {msg_len} exceeds decrypted data len {}",
                data.len() - 20
            ));
        }

        let msg = String::from_utf8(data[msg_start..msg_end].to_vec())
            .map_err(|e| format!("UTF-8 decode failed: {e}"))?;

        // Verify corp_id suffix
        let actual_corp_id = String::from_utf8_lossy(&data[msg_end..])
            .trim_end_matches('\0')
            .to_string();
        if actual_corp_id != expected_corp_id {
            tracing::warn!(
                "corp_id mismatch: expected '{}', got '{}'",
                expected_corp_id,
                actual_corp_id
            );
        }

        Ok(msg)
    }

    /// Encrypt a plaintext message into the WeCom encrypted format.
    ///
    /// Returns base64-encoded ciphertext.
    pub fn encrypt(&self, plaintext: &str) -> Result<String, String> {
        use rand::Rng;

        let msg_bytes = plaintext.as_bytes();
        let corp_id_bytes = self.corp_id.as_bytes();

        // Format: random(16) + msg_len(4 BE) + msg + corp_id
        let mut data = Vec::with_capacity(16 + 4 + msg_bytes.len() + corp_id_bytes.len());

        // Random 16 bytes (simple PRNG for padding — cryptographically "random enough")
        let mut rng = rand::thread_rng();
        let random: [u8; 16] = rng.gen();
        data.extend_from_slice(&random);

        // Message length (big-endian u32)
        data.extend_from_slice(&(msg_bytes.len() as u32).to_be_bytes());

        // Message content
        data.extend_from_slice(msg_bytes);

        // Corp ID
        data.extend_from_slice(corp_id_bytes);

        // IV = first 16 bytes of AES key
        // IV = first 16 bytes of AES key (key is 32 bytes for AES-256-CBC)
        let iv: [u8; 16] = self.aes_key[..16]
            .try_into()
            .map_err(|_| "AES key too short for IV".to_string())?;

        let ciphertext = Aes256CbcEnc::new(self.aes_key.as_slice().into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&data);

        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &ciphertext,
        ))
    }

    /// Generate a SHA1 signature for an encrypted message.
    pub fn generate_signature(&self, timestamp: &str, nonce: &str, encrypted: &str) -> String {
        let mut parts = [self.token.as_str(), timestamp, nonce, encrypted];
        parts.sort();
        let combined = parts.join("");
        let mut hasher = Sha1::new();
        hasher.update(combined.as_bytes());
        hex::encode(hasher.finalize())
    }
}

// ── XML types for WeCom smart bot messages ─────────────────

/// Incoming encrypted XML body from WeCom callback.
#[derive(Debug, Deserialize)]
pub struct WeComEncryptedBody {
    #[serde(rename = "Encrypt")]
    pub encrypt: String,
    #[serde(rename = "MsgSignature")]
    pub msg_signature: Option<String>,
    #[serde(rename = "TimeStamp")]
    pub time_stamp: Option<String>,
    #[serde(rename = "Nonce")]
    pub nonce: Option<String>,
}

/// Decrypted WeCom message XML (smart bot callback).
#[derive(Debug, Deserialize)]
pub struct WeComMessage {
    #[serde(rename = "ToUserName")]
    pub to_user_name: Option<String>,
    #[serde(rename = "AgentType")]
    pub agent_type: Option<String>,
    #[serde(rename = "MsgType")]
    pub msg_type: Option<String>,
    #[serde(rename = "Content")]
    pub content: Option<String>,
    #[serde(rename = "MsgId")]
    pub msg_id: Option<String>,
    #[serde(rename = "ChatId")]
    pub chat_id: Option<String>,
    #[serde(rename = "ChatType")]
    pub chat_type: Option<String>,
    #[serde(rename = "FromUser")]
    pub from_user: Option<String>,
    #[serde(rename = "GetChatInfoUrl")]
    pub get_chat_info_url: Option<String>,
}

/// Extracted user message from WeCom.
#[derive(Debug, Clone)]
pub struct WeComUserMessage {
    pub chat_id: String,
    pub user_id: String,
    pub msg_id: String,
    pub text: String,
    /// The webhook URL for active push (received during bot setup).
    pub webhook_url: Option<String>,
}

impl WeComMessage {
    /// Convert to a simplified user message.
    pub fn into_user_message(self, webhook_url: Option<String>) -> Option<WeComUserMessage> {
        let msg_type = self.msg_type.as_deref().unwrap_or("");
        if msg_type != "text" {
            tracing::debug!("ignoring non-text wecom message type: {msg_type}");
            return None;
        }

        let text = self.content?;
        if text.trim().is_empty() {
            return None;
        }

        Some(WeComUserMessage {
            chat_id: self.chat_id.unwrap_or_else(|| "unknown".to_string()),
            user_id: self.from_user.unwrap_or_else(|| "unknown".to_string()),
            msg_id: self
                .msg_id
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            text,
            webhook_url,
        })
    }
}

/// Passive response XML (encrypted, for the 5-second acknowledgment).
#[derive(Debug)]
pub struct WeComPassiveResponse {
    pub encrypted: String,
    pub msg_signature: String,
    pub timestamp: String,
    pub nonce: String,
}

/// Build a passive response XML for WeCom.
///
/// If `reply_text` is Some, includes the reply; if None, returns empty (acknowledgment only).
pub fn build_passive_response_xml(reply_text: Option<&str>) -> String {
    if let Some(text) = reply_text {
        format!(
            r#"<xml><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[{text}]]></Content></xml>"#
        )
    } else {
        // Empty response = acknowledgment only, no reply
        r#"<xml><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[]]></Content></xml>"#
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wecom_crypto_roundtrip() {
        // Valid 43-char base64 encoding of a 32-byte key.
        // 32 zero bytes → base64 "AAAA..." (44 chars with padding), take first 43.
        let token = "test_token";
        let encoding_aes_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let corp_id = "test_corp";

        let crypto = WeComCrypto::new(token, encoding_aes_key, corp_id).unwrap();

        let plaintext = "hello world";
        let encrypted = crypto.encrypt(plaintext).unwrap();
        let decrypted = crypto.decrypt(&encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_wecom_crypto_signature() {
        let token = "test_token";
        let encoding_aes_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let corp_id = "test_corp";

        let crypto = WeComCrypto::new(token, encoding_aes_key, corp_id).unwrap();

        let encrypted = crypto.encrypt("test message").unwrap();
        let timestamp = "1234567890";
        let nonce = "test_nonce";

        let sig = crypto.generate_signature(timestamp, nonce, &encrypted);
        assert!(crypto.verify_signature(timestamp, nonce, &encrypted, &sig));
    }

    #[test]
    fn test_build_passive_response() {
        let xml = build_passive_response_xml(Some("hello"));
        assert!(xml.contains("hello"));
        assert!(xml.contains("text"));

        let xml_empty = build_passive_response_xml(None);
        assert!(xml_empty.contains("text"));
    }
}
