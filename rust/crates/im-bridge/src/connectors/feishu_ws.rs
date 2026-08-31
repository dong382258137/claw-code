//! Feishu (Lark) WebSocket long-connection client.
//!
//! Implements the native "long connection" mode: a full-duplex WebSocket channel
//! to the Feishu open platform, so the bot can receive subscribed events without
//! a public URL / webhook. This is the recommended mode for enterprise self-built
//! apps.
//!
//! Protocol (native, no official SDK):
//! 1. Discover a WebSocket endpoint:
//!    `POST https://open.feishu.cn/callback/ws/endpoint`
//!    body `{ "AppID": ..., "AppSecret": ... }`
//!    → `{ "code":0, "data":{ "URL":"wss://...&device_id=..&service_id=..",
//!                            "ClientConfig":{...} } }`
//! 2. Connect to `URL` via WebSocket.
//! 3. Exchange binary frames. Each frame is a protobuf `Frame` message:
//!    - Header { key=1, value=2 }
//!    - Frame  { SeqID=1, LogID=2, service=3, method=4 (0=CONTROL,1=DATA),
//!      headers=5 (repeated Header), payload_encoding=6,
//!      payload_type=7, payload=8 (bytes), LogIDNew=9 }
//! 4. Client sends a CONTROL ping frame (`type=ping`) every PingInterval.
//! 5. Server pushes DATA frames (`type=event`/`card`); the client must reply to
//!    each DATA frame with a response frame (code 200) within 3s.
//! 6. On connection loss, re-discover an endpoint and reconnect.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::config::FeishuConfig;
use crate::connectors::feishu::{
    FeishuClient, FeishuEvent, FeishuEventCallback, FeishuMessageContent, FeishuUserMessage,
};

// ── Protocol constants ─────────────────────────────────────

const BASE_URL: &str = "https://open.feishu.cn";
const ENDPOINT_URI: &str = "/callback/ws/endpoint";

/// Frame.method values. `0` = CONTROL (ping/pong/ack), `1` = DATA (event).
const FRAME_CONTROL: i32 = 0;

/// Header keys.
const HEADER_TYPE: &str = "type";
const HEADER_MESSAGE_ID: &str = "message_id";
const HEADER_SUM: &str = "sum";
const HEADER_SEQ: &str = "seq";
const HEADER_TRACE_ID: &str = "trace_id";
const HEADER_BIZ_RT: &str = "biz_rt";

/// Message types.
const MSG_EVENT: &str = "event";
const MSG_PING: &str = "ping";
const MSG_PONG: &str = "pong";

/// Runtime defaults (overridable by the returned ClientConfig).
const DEFAULT_PING_INTERVAL: u64 = 120;
const DEFAULT_RECONNECT_COUNT: i32 = -1; // -1 = infinite
const DEFAULT_RECONNECT_INTERVAL: u64 = 120;
const DEFAULT_RECONNECT_NONCE: u64 = 30;

// ── Endpoint discovery response ────────────────────────────

#[derive(Debug, Deserialize)]
struct ClientConfig {
    #[serde(rename = "ReconnectCount")]
    reconnect_count: Option<i32>,
    #[serde(rename = "ReconnectInterval")]
    reconnect_interval: Option<u64>,
    #[serde(rename = "ReconnectNonce")]
    reconnect_nonce: Option<u64>,
    #[serde(rename = "PingInterval")]
    ping_interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct EndpointData {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig")]
    client_config: Option<ClientConfig>,
}

#[derive(Debug, Deserialize)]
struct EndpointResp {
    code: i32,
    msg: Option<String>,
    data: Option<EndpointData>,
}

// ── Frame (protobuf) ───────────────────────────────────────

#[derive(Debug, Clone)]
struct Frame {
    seq_id: u64,
    log_id: u64,
    service: i32,
    method: i32,
    headers: Vec<(String, String)>,
    payload_encoding: Option<String>,
    payload_type: Option<String>,
    payload: Vec<u8>,
    log_id_new: Option<String>,
}

impl Frame {
    fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

// ── Minimal protobuf codec (only what the Frame needs) ─────

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn decode_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = buf[*pos];
        *pos += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

fn write_header(out: &mut Vec<u8>, key: &str, value: &str) {
    out.push(0x0A); // field 1 (key), wire type 2
    encode_varint(key.len() as u64, out);
    out.extend_from_slice(key.as_bytes());
    out.push(0x12); // field 2 (value), wire type 2
    encode_varint(value.len() as u64, out);
    out.extend_from_slice(value.as_bytes());
}

fn write_string(out: &mut Vec<u8>, tag: u8, value: &str) {
    out.push(tag);
    encode_varint(value.len() as u64, out);
    out.extend_from_slice(value.as_bytes());
}

fn encode_frame(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    // Required scalar fields (proto2) are always emitted.
    out.push(0x08); // field 1 SeqID (uint64)
    encode_varint(frame.seq_id, &mut out);
    out.push(0x10); // field 2 LogID (uint64)
    encode_varint(frame.log_id, &mut out);
    out.push(0x18); // field 3 service (int32)
    encode_varint(frame.service as u32 as u64, &mut out);
    out.push(0x20); // field 4 method (int32)
    encode_varint(frame.method as u32 as u64, &mut out);
    for (k, v) in &frame.headers {
        let mut hdr = Vec::new();
        write_header(&mut hdr, k, v);
        out.push(0x2A); // field 5 headers (repeated Header)
        encode_varint(hdr.len() as u64, &mut out);
        out.extend_from_slice(&hdr);
    }
    if let Some(pe) = &frame.payload_encoding {
        write_string(&mut out, 0x32, pe);
    }
    if let Some(pt) = &frame.payload_type {
        write_string(&mut out, 0x3A, pt);
    }
    if !frame.payload.is_empty() {
        out.push(0x42); // field 8 payload (bytes)
        encode_varint(frame.payload.len() as u64, &mut out);
        out.extend_from_slice(&frame.payload);
    }
    if let Some(ln) = &frame.log_id_new {
        write_string(&mut out, 0x4A, ln);
    }
    out
}

fn decode_header(buf: &[u8]) -> Result<(String, String), String> {
    let mut key = String::new();
    let mut value = String::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        let tag = decode_varint(buf, &mut pos);
        let field = tag >> 3;
        let len = decode_varint(buf, &mut pos) as usize;
        if pos + len > buf.len() {
            return Err("header exceeds buffer".to_string());
        }
        match field {
            1 => key = String::from_utf8_lossy(&buf[pos..pos + len]).to_string(),
            2 => value = String::from_utf8_lossy(&buf[pos..pos + len]).to_string(),
            _ => {}
        }
        pos += len;
    }
    Ok((key, value))
}

fn decode_frame(buf: &[u8]) -> Result<Frame, String> {
    let mut frame = Frame {
        seq_id: 0,
        log_id: 0,
        service: 0,
        method: 0,
        headers: Vec::new(),
        payload_encoding: None,
        payload_type: None,
        payload: Vec::new(),
        log_id_new: None,
    };
    let mut pos = 0usize;
    while pos < buf.len() {
        let tag = decode_varint(buf, &mut pos);
        let field = tag >> 3;
        let wire = tag & 0x07;
        match field {
            1 => frame.seq_id = decode_varint(buf, &mut pos),
            2 => frame.log_id = decode_varint(buf, &mut pos),
            3 => frame.service = decode_varint(buf, &mut pos) as i32,
            4 => frame.method = decode_varint(buf, &mut pos) as i32,
            5 => {
                let len = decode_varint(buf, &mut pos) as usize;
                if pos + len > buf.len() {
                    return Err("header field exceeds buffer".to_string());
                }
                let (k, v) = decode_header(&buf[pos..pos + len])?;
                frame.headers.push((k, v));
                pos += len;
            }
            6 => {
                let len = decode_varint(buf, &mut pos) as usize;
                if pos + len > buf.len() {
                    return Err("payload_encoding exceeds buffer".to_string());
                }
                frame.payload_encoding =
                    Some(String::from_utf8_lossy(&buf[pos..pos + len]).to_string());
                pos += len;
            }
            7 => {
                let len = decode_varint(buf, &mut pos) as usize;
                if pos + len > buf.len() {
                    return Err("payload_type exceeds buffer".to_string());
                }
                frame.payload_type =
                    Some(String::from_utf8_lossy(&buf[pos..pos + len]).to_string());
                pos += len;
            }
            8 => {
                let len = decode_varint(buf, &mut pos) as usize;
                if pos + len > buf.len() {
                    return Err("payload exceeds buffer".to_string());
                }
                frame.payload = buf[pos..pos + len].to_vec();
                pos += len;
            }
            9 => {
                let len = decode_varint(buf, &mut pos) as usize;
                if pos + len > buf.len() {
                    return Err("LogIDNew exceeds buffer".to_string());
                }
                frame.log_id_new = Some(String::from_utf8_lossy(&buf[pos..pos + len]).to_string());
                pos += len;
            }
            _ => match wire {
                0 => {
                    decode_varint(buf, &mut pos);
                }
                1 => pos += 8,
                2 => {
                    let len = decode_varint(buf, &mut pos) as usize;
                    pos += len;
                }
                5 => pos += 4,
                _ => return Err(format!("unsupported wire type {wire}")),
            },
        }
    }
    Ok(frame)
}

// ── Multi-frame assembler ──────────────────────────────────

struct FrameAssembler {
    parts: HashMap<String, (u32, Vec<Vec<u8>>)>,
}

impl FrameAssembler {
    fn new() -> Self {
        Self {
            parts: HashMap::new(),
        }
    }

    /// Feed one chunk; returns the assembled payload once all parts arrived.
    fn push(&mut self, msg_id: &str, total: u32, seq: u32, payload: Vec<u8>) -> Option<Vec<u8>> {
        if total <= 1 {
            return Some(payload);
        }
        let entry = self
            .parts
            .entry(msg_id.to_string())
            .or_insert_with(|| (total, vec![Vec::new(); total as usize]));
        if entry.0 != total {
            entry.0 = total;
            entry.1 = vec![Vec::new(); total as usize];
        }
        let idx = (seq.saturating_sub(1)) as usize;
        if idx < entry.1.len() {
            entry.1[idx] = payload;
        }
        let received = entry.1.iter().filter(|p| !p.is_empty()).count();
        if received == total as usize {
            let assembled = entry.1.concat();
            self.parts.remove(msg_id);
            Some(assembled)
        } else {
            None
        }
    }
}

// ── Runtime settings (server-provided, with defaults) ─────

#[derive(Clone)]
struct WsSettings {
    ping_interval: Duration,
    reconnect_count: i32,
    reconnect_interval: Duration,
    reconnect_nonce: u64,
}

impl Default for WsSettings {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(DEFAULT_PING_INTERVAL),
            reconnect_count: DEFAULT_RECONNECT_COUNT,
            reconnect_interval: Duration::from_secs(DEFAULT_RECONNECT_INTERVAL),
            reconnect_nonce: DEFAULT_RECONNECT_NONCE,
        }
    }
}

impl WsSettings {
    fn apply(&mut self, cfg: Option<&ClientConfig>) {
        let Some(cfg) = cfg else { return };
        if let Some(v) = cfg.ping_interval {
            if v > 0 {
                self.ping_interval = Duration::from_secs(v);
            }
        }
        if let Some(v) = cfg.reconnect_count {
            self.reconnect_count = v;
        }
        if let Some(v) = cfg.reconnect_interval {
            if v > 0 {
                self.reconnect_interval = Duration::from_secs(v);
            }
        }
        if let Some(v) = cfg.reconnect_nonce {
            self.reconnect_nonce = v;
        }
    }
}

// ── Long-connection client ─────────────────────────────────

/// Feishu WebSocket long-connection client.
///
/// Emits extracted user messages through an unbounded channel, and reconnects
/// automatically on connection loss.
pub struct FeishuWsClient {
    config: Arc<FeishuConfig>,
    sender: mpsc::UnboundedSender<FeishuUserMessage>,
}

impl FeishuWsClient {
    pub fn new(config: FeishuConfig, sender: mpsc::UnboundedSender<FeishuUserMessage>) -> Self {
        Self {
            config: Arc::new(config),
            sender,
        }
    }

    /// Run forever, reconnecting as needed. Returns Err only on a fatal,
    /// non-recoverable condition (e.g. reconnect attempts exhausted).
    pub async fn run(self) -> Result<(), String> {
        let mut settings = WsSettings::default();
        loop {
            let result = self.connect_once(&mut settings).await;
            match result {
                Ok(()) => tracing::info!("feishu long connection closed; reconnecting"),
                Err(e) => tracing::error!("feishu long connection error: {e}; reconnecting"),
            }

            if settings.reconnect_count == 0 {
                return Err("feishu long connection reconnect attempts exhausted".to_string());
            }

            // Jitter first, then the fixed reconnect interval.
            if settings.reconnect_nonce > 0 {
                let jitter = rand::thread_rng().gen_range(0.0..settings.reconnect_nonce as f64);
                tokio::time::sleep(Duration::from_secs_f64(jitter)).await;
            }
            tokio::time::sleep(settings.reconnect_interval).await;
        }
    }

    /// Establish one connection and drive it until it breaks.
    async fn connect_once(&self, settings: &mut WsSettings) -> Result<(), String> {
        let endpoint = fetch_endpoint(&self.config).await?;
        settings.apply(
            endpoint
                .data
                .as_ref()
                .and_then(|d| d.client_config.as_ref()),
        );
        let url = endpoint
            .data
            .as_ref()
            .ok_or("endpoint response missing data")?
            .url
            .clone();
        let service_id =
            query_param(&url, "service_id").ok_or("endpoint url missing service_id")?;

        let (ws_stream, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| format!("websocket connect failed: {e}"))?;
        tracing::info!("feishu long connection established (service_id={service_id})");

        let (mut sink, mut stream) = ws_stream.split();
        let mut assembler = FrameAssembler::new();
        let mut ping_timer = tokio::time::interval(settings.ping_interval);
        ping_timer.tick().await; // consume immediate first tick

        loop {
            tokio::select! {
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Binary(data))) => {
                            self.handle_frame(&mut sink, &mut assembler, &data, service_id, settings).await?;
                        }
                        Some(Ok(WsMessage::Text(text))) => {
                            self.handle_frame(&mut sink, &mut assembler, text.as_bytes(), service_id, settings).await?;
                        }
                        Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => {
                            // tungstenite replies to ping automatically; nothing to do.
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(format!("websocket recv error: {e}")),
                        None => return Ok(()), // connection closed by peer
                    }
                }
                _ = ping_timer.tick() => {
                    let frame = Frame {
                        seq_id: 0,
                        log_id: 0,
                        service: service_id,
                        method: FRAME_CONTROL,
                        headers: vec![(HEADER_TYPE.to_string(), MSG_PING.to_string())],
                        payload_encoding: None,
                        payload_type: None,
                        payload: Vec::new(),
                        log_id_new: None,
                    };
                    if let Err(e) = sink.send(WsMessage::Binary(encode_frame(&frame))).await {
                        return Err(format!("websocket ping send error: {e}"));
                    }
                }
            }
        }
    }

    async fn handle_frame<S>(
        &self,
        sink: &mut S,
        assembler: &mut FrameAssembler,
        data: &[u8],
        service_id: i32,
        settings: &mut WsSettings,
    ) -> Result<(), String>
    where
        S: SinkExt<WsMessage> + Unpin,
        S::Error: std::fmt::Display,
    {
        let frame = match decode_frame(data) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("failed to decode feishu ws frame: {e}");
                return Ok(());
            }
        };

        if frame.method == FRAME_CONTROL {
            let msg_type = frame.header(HEADER_TYPE).unwrap_or("");
            if msg_type == MSG_PONG {
                // Pong may carry an updated ClientConfig in its payload.
                if !frame.payload.is_empty() {
                    if let Ok(cfg) = serde_json::from_slice::<ClientConfig>(&frame.payload) {
                        settings.apply(Some(&cfg));
                    }
                }
            }
            return Ok(());
        }

        // DATA frame: extract headers.
        let msg_id = frame.header(HEADER_MESSAGE_ID).unwrap_or("").to_string();
        let trace_id = frame.header(HEADER_TRACE_ID).unwrap_or("").to_string();
        let sum: u32 = frame.header(HEADER_SUM).unwrap_or("1").parse().unwrap_or(1);
        let seq: u32 = frame.header(HEADER_SEQ).unwrap_or("1").parse().unwrap_or(1);
        let msg_type = frame.header(HEADER_TYPE).unwrap_or("").to_string();

        // Only events are interesting; others (card, etc.) are acked and skipped.
        let payload = if msg_type == MSG_EVENT {
            assembler.push(&msg_id, sum, seq, frame.payload.clone())
        } else {
            None
        };

        if let Some(payload) = payload {
            match extract_ws_message(&payload) {
                Some(msg) => {
                    tracing::info!(
                        "feishu ws event: message_id={} trace_id={}",
                        msg.message_id,
                        trace_id
                    );
                    let _ = self.sender.send(msg);
                }
                None => {
                    tracing::debug!("feishu ws event produced no text message");
                }
            }
        }

        // Acknowledge every DATA frame with a 200 response within 3s.
        let start_ms = std::time::Instant::now();
        let rt_ms = start_ms.elapsed().as_millis().to_string();
        let mut resp = frame.clone();
        resp.method = FRAME_CONTROL;
        resp.service = service_id;
        resp.headers.push((HEADER_BIZ_RT.to_string(), rt_ms));
        resp.payload = br#"{"code":200}"#.to_vec();
        sink.send(WsMessage::Binary(encode_frame(&resp)))
            .await
            .map_err(|e| format!("websocket ack send error: {e}"))?;

        Ok(())
    }
}
// ── Helpers ────────────────────────────────────────────────

/// Discover a long-connection WebSocket endpoint.
async fn fetch_endpoint(config: &FeishuConfig) -> Result<EndpointResp, String> {
    let url = format!("{BASE_URL}{ENDPOINT_URI}");
    let body = serde_json::json!({
        "AppID": config.app_id,
        "AppSecret": config.app_secret,
    });
    let client = Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("endpoint request failed: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read endpoint response failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("endpoint request status {status}: {text}"));
    }
    let parsed: EndpointResp =
        serde_json::from_str(&text).map_err(|e| format!("endpoint response parse failed: {e}"))?;
    if parsed.code != 0 {
        return Err(format!(
            "endpoint error (code {}): {}",
            parsed.code,
            parsed.msg.as_deref().unwrap_or("unknown")
        ));
    }
    Ok(parsed)
}

/// Extract a query parameter from a URL string.
fn query_param(url: &str, key: &str) -> Option<i32> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return v.parse().ok();
        }
    }
    None
}

/// Extract a text user message from a WebSocket event payload.
///
/// The payload is the V2 event body `{ "event": { "sender": ..., "message": ... } }`
/// (some versions send the inner object directly). We tolerate both shapes.
fn extract_ws_message(payload: &[u8]) -> Option<FeishuUserMessage> {
    let text = std::str::from_utf8(payload).ok()?;
    if let Ok(cb) = serde_json::from_str::<FeishuEventCallback>(text) {
        if let Some(msg) = FeishuClient::extract_message(&cb) {
            return Some(msg);
        }
    }
    // Fallback: payload is the inner event `{ sender, message }`.
    let ev: FeishuEvent = serde_json::from_str(text).ok()?;
    let sender = ev.sender?;
    let msg = ev.message?;
    if msg.message_type != "text" {
        return None;
    }
    let sender_id = sender
        .sender_id
        .as_ref()
        .and_then(|s| s.open_id.clone().or_else(|| s.user_id.clone()))
        .unwrap_or_else(|| "unknown".to_string());
    let content: FeishuMessageContent = serde_json::from_str(&msg.content).ok()?;
    let text = content.text?;
    if text.trim().is_empty() {
        return None;
    }
    Some(FeishuUserMessage {
        chat_id: msg.chat_id,
        user_id: sender_id,
        message_id: msg.message_id,
        text,
    })
}
