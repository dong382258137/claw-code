//! Native Trae chat provider.
//!
//! Claw talks to Trae's `/api/agent/v3/llm_utils_chat` endpoint directly. The
//! endpoint uses a Trae-specific SSE envelope (not OpenAI Chat Completions),
//! so this module owns request conversion, SSE decoding and event mapping.
//!
//! Credentials come from the first of these that yields a non-expired token:
//!
//! 1. `TRAE_ACCESS_TOKEN` (manual override for one-off experiments);
//! 2. the Trae desktop client's `storage.json`
//!    (`%APPDATA%\Trae CN\User\globalStorage\storage.json` on Windows), whose
//!    `iCubeAuthInfo://icube.cloudide` value is the account credential,
//!    sealed with a fixed-salt AES-128-CBC envelope;
//! 3. the legacy DSH connector cache (`~/.dsh/.trae-auth.cn.json`), written by
//!    `dsh-connect-trae`.
//!
//! The desktop client refreshes (2) while it is running, so it is the only
//! source that stays valid without re-running another tool.

use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes::Aes128;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha512};

use crate::error::ApiError;
use crate::http_client::build_http_client_or_default;
use crate::types::{
    ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
    InputContentBlock, InputMessage, MessageDelta, MessageDeltaEvent, MessageRequest,
    MessageResponse, MessageStartEvent, MessageStopEvent, OutputContentBlock, StreamEvent,
    SystemContent, ToolResultContentBlock, Usage,
};

const DEFAULT_BASE_URL: &str = "https://trae-api-cn.mchost.guru";
const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
const DEFAULT_FUNCTION: &str = "solo_work_remote";
const VERSION_CODE: &str = "20260716";

/// send 阶段瞬时失败的重试预算。
///
/// Trae 上游对高频重发较敏感（429 `TRAE_4011` 即限流），且外层 runtime 还有
/// 一次 Provider 恢复重试，因此比 `openai_compat` 的 8 次收窄：4 次重试，
/// 1s→2s→4s→8s 指数退避 + ±25% 抖动，并优先使用 `Retry-After`。
const MAX_SEND_RETRIES: u32 = 4;
const SEND_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const SEND_MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub struct TraeClient {
    http: reqwest::Client,
    base_url: String,
    access_token: String,
    user_id: String,
    machine_id: Option<String>,
    device_id: Option<String>,
    app_version: String,
    os_version: Option<String>,
    function: String,
}

impl TraeClient {
    pub fn from_env() -> Result<Self, ApiError> {
        let cached = read_desktop_credential().or_else(read_cached_credential);
        let access_token = env::var("TRAE_ACCESS_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| cached.as_ref().map(|credential| credential.access_token.clone()))
            .ok_or_else(|| {
                ApiError::missing_credentials_with_hint(
                    "Trae",
                    &["TRAE_ACCESS_TOKEN", "TRAE_AUTH_FILE", "TRAE_STORAGE_FILE"],
                    "登录 Trae CN 客户端并保持其运行（它会持续刷新 storage.json 里的凭证），或设置 TRAE_ACCESS_TOKEN",
                )
            })?;
        let user_id = cached
            .as_ref()
            .and_then(|credential| credential.user_id.clone())
            .or_else(|| env::var("TRAE_USER_ID").ok())
            .unwrap_or_default();
        let identity = read_identity();
        let app_version = identity
            .app_version
            .clone()
            .or_else(|| env::var("TRAE_APP_VERSION").ok())
            .unwrap_or_else(|| "2.3.98528".to_string());

        Ok(Self {
            http: build_http_client_or_default(),
            base_url: env::var("TRAE_BASE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            access_token,
            user_id,
            machine_id: identity.machine_id,
            device_id: identity.device_id,
            app_version,
            os_version: identity.os_version,
            function: env::var("TRAE_FUNCTION")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_FUNCTION.to_string()),
        })
    }

    pub async fn send_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        let mut stream = self.stream_message(request).await?;
        let mut blocks: BTreeMap<u32, OutputContentBlock> = BTreeMap::new();
        let mut order: Vec<u32> = Vec::new();
        let mut usage = Usage::default();
        let mut stop_reason = None;
        let mut id = String::from("trae-message");
        let mut model = request.model.clone();
        while let Some(event) = stream.next_event().await? {
            match event {
                StreamEvent::MessageStart(start) => {
                    id = start.message.id;
                    model = start.message.model;
                }
                StreamEvent::ContentBlockStart(start) => {
                    if !blocks.contains_key(&start.index) {
                        order.push(start.index);
                    }
                    blocks.insert(start.index, start.content_block);
                }
                StreamEvent::ContentBlockDelta(delta) => {
                    if let Some(block) = blocks.get_mut(&delta.index) {
                        apply_delta(block, &delta.delta);
                    }
                }
                StreamEvent::MessageDelta(delta) => {
                    usage = delta.usage;
                    if delta.delta.stop_reason.is_some() {
                        stop_reason = delta.delta.stop_reason;
                    }
                }
                StreamEvent::ContentBlockStop(_) | StreamEvent::MessageStop(_) => {}
            }
        }
        order.sort_unstable();
        let content = order
            .into_iter()
            .filter_map(|index| blocks.remove(&index))
            .collect();
        Ok(MessageResponse {
            id,
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content,
            model,
            stop_reason,
            stop_sequence: None,
            usage,
            request_id: None,
        })
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        let trace = trace_id();
        let model = strip_trae_prefix(&request.model);
        let body = build_request_body(request, &model, &self.function);
        if let Ok(path) = env::var("TRAE_DEBUG_BODY") {
            let _ = fs::write(
                format!("{path}.{trace}.json"),
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            );
        }
        let mut headers = HeaderMap::new();
        insert_header(
            &mut headers,
            "Authorization",
            &format!("Cloud-IDE-JWT {}", self.access_token),
        );
        insert_header(&mut headers, "X-Ide-Token", &self.access_token);
        insert_header(&mut headers, "X-Cloudide-Token", &self.access_token);
        if !self.user_id.is_empty() {
            insert_header(&mut headers, "x-uid", &self.user_id);
        }
        insert_header(&mut headers, "x-plugin-channel", "icube-ai");
        insert_header(
            &mut headers,
            "User-Agent",
            &format!("Trae/{}", self.app_version),
        );
        insert_header(&mut headers, "x-app-id", APP_ID);
        if let Some(machine_id) = &self.machine_id {
            insert_header(&mut headers, "x-machine-id", machine_id);
        }
        if let Some(device_id) = &self.device_id {
            insert_header(&mut headers, "x-device-id", device_id);
        }
        insert_header(
            &mut headers,
            "x-device-type",
            if cfg!(windows) {
                "windows"
            } else {
                std::env::consts::OS
            },
        );
        if let Some(os_version) = &self.os_version {
            insert_header(&mut headers, "x-os-version", os_version);
        }
        insert_header(&mut headers, "x-app-version", &self.app_version);
        insert_header(&mut headers, "x-ide-version", &self.app_version);
        insert_header(&mut headers, "x-app-version-code", VERSION_CODE);
        insert_header(&mut headers, "x-ide-version-code", VERSION_CODE);
        insert_header(&mut headers, "x-ide-version-type", "stable");
        insert_header(&mut headers, "x-custom-trace-id", &trace);
        insert_header(
            &mut headers,
            "x-flow-traceparent",
            &format!("04-{trace}-{}-01", &trace[..16]),
        );
        let request_id = dashed_id(&trace);
        insert_header(&mut headers, "x-request-id", &request_id);
        insert_header(&mut headers, "x-trae-request-id", &request_id);
        insert_header(&mut headers, "request-traffic-type", "prod");
        insert_header(&mut headers, "Accept", "text/event-stream");
        insert_header(&mut headers, "Content-Type", "application/json");

        let url = format!(
            "{}/api/agent/v3/llm_utils_chat",
            self.base_url.trim_end_matches('/')
        );
        let response = self.send_stream_request(&url, &headers, &body).await?;

        Ok(MessageStream {
            response,
            parser: TraeSseParser::default(),
            pending: VecDeque::new(),
            state: TraeState::new(model, trace.clone()),
            trace,
            done: false,
            ttft: Instant::now(),
            first_chunk: None,
        })
    }

    /// 发送流式请求，对建连/超时/传输类瞬时失败做指数退避重试。
    ///
    /// 与 `openai_compat::send_with_retry` 的差异：次数更少（Trae 上游对密集
    /// 重发敏感，且 runtime 层还有一次恢复重试），但同样尊重 `Retry-After`。
    /// send 阶段失败时尚未产生任何 SSE 事件，重发不会造成输出重复。
    async fn send_stream_request(
        &self,
        url: &str,
        headers: &HeaderMap,
        body: &Value,
    ) -> Result<reqwest::Response, ApiError> {
        let mut retries = 0u32;
        loop {
            let error = match self
                .http
                .post(url)
                .headers(headers.clone())
                .json(body)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let retry_after = retry_after_header(&response);
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    http_error(status, body, retry_after)
                }
                Err(error) => ApiError::from(error),
            };
            if !error.is_retryable() || retries >= MAX_SEND_RETRIES {
                return Err(error);
            }
            retries += 1;
            let delay = error.retry_after().unwrap_or_else(|| send_backoff(retries));
            tokio::time::sleep(delay).await;
        }
    }
}

#[derive(Debug)]
pub struct MessageStream {
    response: reqwest::Response,
    parser: TraeSseParser,
    pending: VecDeque<StreamEvent>,
    state: TraeState,
    trace: String,
    done: bool,
    ttft: Instant,
    first_chunk: Option<Instant>,
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        Some(&self.trace)
    }

    #[must_use]
    pub fn time_to_first_chunk_ms(&self) -> Option<u64> {
        self.first_chunk.map(|at| {
            u64::try_from(at.saturating_duration_since(self.ttft).as_millis()).unwrap_or(u64::MAX)
        })
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if self.done {
                self.pending.extend(self.state.finish()?);
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
                return Ok(None);
            }
            match self.response.chunk().await? {
                Some(chunk) => {
                    if self.first_chunk.is_none() {
                        self.first_chunk = Some(Instant::now());
                    }
                    for frame in self.parser.push(&chunk) {
                        self.pending.extend(self.state.ingest(frame)?);
                    }
                }
                None => {
                    for frame in self.parser.finish() {
                        self.pending.extend(self.state.ingest(frame)?);
                    }
                    self.done = true;
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct TraeSseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl TraeSseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<TraeSseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line_bytes: Vec<u8> = self.buffer.drain(..=position).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(frame) = self.consume_line(line) {
                frames.push(frame);
            }
        }
        frames
    }

    fn finish(&mut self) -> Vec<TraeSseFrame> {
        let mut frames = Vec::new();
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).to_string();
            self.buffer.clear();
            if let Some(frame) = self.consume_line(line.trim_end_matches(['\r', '\n'])) {
                frames.push(frame);
            }
        }
        if let Some(frame) = self.dispatch() {
            frames.push(frame);
        }
        frames
    }

    fn consume_line(&mut self, line: &str) -> Option<TraeSseFrame> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<TraeSseFrame> {
        if self.data.is_empty() {
            self.event = None;
            return None;
        }
        let frame = TraeSseFrame {
            event: self.event.take(),
            data: self.data.join("\n"),
        };
        self.data.clear();
        Some(frame)
    }
}

#[derive(Debug)]
struct TraeSseFrame {
    event: Option<String>,
    data: String,
}

#[derive(Debug)]
struct TraeState {
    model: String,
    trace: String,
    message_started: bool,
    thinking_started: bool,
    thinking_finished: bool,
    thinking_index: u32,
    text_started: bool,
    text_finished: bool,
    text_index: u32,
    next_index: u32,
    finished: bool,
    stop_reason: Option<String>,
    usage: Usage,
    tool_calls: BTreeMap<u32, TraeToolCall>,
    saw_tool_calls: bool,
}

impl TraeState {
    fn new(model: String, trace: String) -> Self {
        Self {
            model,
            trace,
            message_started: false,
            thinking_started: false,
            thinking_finished: false,
            thinking_index: 0,
            text_started: false,
            text_finished: false,
            text_index: 0,
            next_index: 0,
            finished: false,
            stop_reason: None,
            usage: Usage::default(),
            tool_calls: BTreeMap::new(),
            saw_tool_calls: false,
        }
    }

    fn ensure_message_start(&mut self, events: &mut Vec<StreamEvent>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        events.push(StreamEvent::MessageStart(MessageStartEvent {
            message: MessageResponse {
                id: format!("trae-{}", self.trace),
                kind: "message".to_string(),
                role: "assistant".to_string(),
                content: Vec::new(),
                model: self.model.clone(),
                stop_reason: None,
                stop_sequence: None,
                usage: Usage::default(),
                request_id: None,
            },
        }));
    }

    fn start_thinking(&mut self, events: &mut Vec<StreamEvent>) {
        if self.thinking_started || self.thinking_finished {
            return;
        }
        self.ensure_message_start(events);
        self.thinking_started = true;
        self.thinking_index = self.next_index;
        self.next_index += 1;
        events.push(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
            index: self.thinking_index,
            content_block: OutputContentBlock::Thinking {
                thinking: String::new(),
                signature: None,
            },
        }));
    }

    fn close_thinking(&mut self, events: &mut Vec<StreamEvent>) {
        if !self.thinking_started || self.thinking_finished {
            return;
        }
        self.thinking_started = false;
        self.thinking_finished = true;
        events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
            index: self.thinking_index,
        }));
    }

    fn start_text(&mut self, events: &mut Vec<StreamEvent>) {
        if self.text_started || self.text_finished {
            return;
        }
        self.close_thinking(events);
        self.ensure_message_start(events);
        self.text_started = true;
        self.text_index = self.next_index;
        self.next_index += 1;
        events.push(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
            index: self.text_index,
            content_block: OutputContentBlock::Text {
                text: String::new(),
            },
        }));
    }

    fn close_text(&mut self, events: &mut Vec<StreamEvent>) {
        if !self.text_started || self.text_finished {
            return;
        }
        self.text_started = false;
        self.text_finished = true;
        events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
            index: self.text_index,
        }));
    }

    fn ingest(&mut self, frame: TraeSseFrame) -> Result<Vec<StreamEvent>, ApiError> {
        let mut events = Vec::new();
        if frame.data == "[DONE]" {
            return self.finish();
        }
        let payload: Value = match serde_json::from_str(&frame.data) {
            Ok(payload) => payload,
            Err(_) => return Ok(events),
        };
        let record = payload.as_object().cloned().unwrap_or_default();

        if frame.event.as_deref() == Some("error")
            || record
                .get("code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code >= 4000)
        {
            let code = record.get("code").and_then(Value::as_i64).unwrap_or(400);
            let message = record
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Trae upstream error")
                .to_string();
            return Err(trae_error(code, message, frame.data));
        }

        if frame.event.as_deref() == Some("token_usage") || record.contains_key("prompt_tokens") {
            self.usage = usage_from_record(&record);
            return Ok(events);
        }

        if let Some(reasoning) = record.get("reasoning_content").and_then(Value::as_str) {
            if !reasoning.is_empty() {
                self.start_thinking(&mut events);
                events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                    index: self.thinking_index,
                    delta: ContentBlockDelta::ThinkingDelta {
                        thinking: reasoning.to_string(),
                    },
                }));
            }
        }

        if let Some(text) = record.get("response").and_then(Value::as_str) {
            if !text.is_empty() {
                self.start_text(&mut events);
                events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                    index: self.text_index,
                    delta: ContentBlockDelta::TextDelta {
                        text: text.to_string(),
                    },
                }));
            }
        }

        if let Some(calls) = record.get("tool_calls").and_then(Value::as_array) {
            for (fallback_index, raw) in calls.iter().enumerate() {
                if let Some(call) = parse_tool_call(raw, fallback_index) {
                    let index = call.index;
                    self.saw_tool_calls = true;
                    self.tool_calls
                        .entry(index)
                        .and_modify(|existing| existing.merge(&call))
                        .or_insert(call);
                }
            }
        }

        if frame.event.as_deref() == Some("done")
            || record
                .get("finish_reason")
                .and_then(Value::as_str)
                .is_some()
        {
            events.extend(self.finish()?);
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ApiError> {
        if self.finished {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        self.close_thinking(&mut events);
        self.close_text(&mut events);
        let tool_calls = std::mem::take(&mut self.tool_calls);
        for (_, call) in tool_calls {
            let index = self.next_index;
            self.next_index += 1;
            let input = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
            events.push(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                index,
                content_block: OutputContentBlock::ToolUse {
                    id: if call.id.is_empty() {
                        format!("trae_call_{index}")
                    } else {
                        call.id
                    },
                    name: call.name,
                    input,
                },
            }));
            events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                index,
            }));
        }
        let stop_reason = if self.saw_tool_calls {
            "tool_use".to_string()
        } else {
            self.stop_reason
                .clone()
                .unwrap_or_else(|| "end_turn".to_string())
        };
        events.push(StreamEvent::MessageDelta(MessageDeltaEvent {
            delta: MessageDelta {
                stop_reason: Some(stop_reason),
                stop_sequence: None,
            },
            usage: self.usage.clone(),
        }));
        events.push(StreamEvent::MessageStop(MessageStopEvent {}));
        self.finished = true;
        Ok(events)
    }
}

#[derive(Debug, Clone)]
struct TraeToolCall {
    index: u32,
    id: String,
    name: String,
    arguments: String,
}

impl TraeToolCall {
    fn merge(&mut self, other: &Self) {
        if !other.id.is_empty() {
            self.id = other.id.clone();
        }
        if !other.name.is_empty() {
            self.name = other.name.clone();
        }
        if !other.arguments.is_empty() {
            self.arguments.push_str(&other.arguments);
        }
    }
}

fn parse_tool_call(raw: &Value, fallback_index: usize) -> Option<TraeToolCall> {
    let record = raw.as_object()?;
    let function = record
        .get("function_call")
        .and_then(Value::as_object)
        .or_else(|| record.get("function").and_then(Value::as_object));
    let index = record
        .get("index")
        .and_then(Value::as_u64)
        .map_or(fallback_index as u32, |value| value as u32);
    Some(TraeToolCall {
        index,
        id: record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: function
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments: function
            .and_then(|value| value.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn build_request_body(request: &MessageRequest, model: &str, function: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &request.system {
        let text = system_text(system);
        if !text.is_empty() {
            messages.push(json!({
                "role": "system",
                "content": [{"type": "text", "text": text}]
            }));
        }
    }
    for message in &request.messages {
        append_message(&mut messages, message);
    }

    let mut body = json!({
        "messages": messages,
        "model": model,
        "config_name": model,
        "function": function,
        "stream": true
    });
    if let Some(tools) = &request.tools {
        let tools: Vec<Value> = tools
            .iter()
            .filter(|tool| !tool.name.trim().is_empty())
            .map(|tool| {
                let parameters =
                    serde_json::to_string(&tool.input_schema).unwrap_or_else(|_| "{}".to_string());
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description.clone().unwrap_or_default(),
                        "parameters": parameters
                    }
                })
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning_effort"] = Value::String(map_reasoning_effort(effort).to_string());
    }
    body
}

fn append_message(messages: &mut Vec<Value>, message: &InputMessage) {
    let role = message.role.as_str();
    if role == "assistant" {
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                InputContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tool_calls: Vec<Value> = message
            .content
            .iter()
            .filter_map(|block| match block {
                InputContentBlock::ToolUse { id, name, input } => Some(json!({
                    "id": id,
                    "type": "function",
                    "function_call": {
                        "name": name,
                        "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string())
                    }
                })),
                _ => None,
            })
            .collect();
        if !text.is_empty() || !tool_calls.is_empty() {
            let mut payload = json!({
                "role": "assistant",
                "content": if text.is_empty() { json!([]) } else { json!([{"type": "text", "text": text}]) }
            });
            if !tool_calls.is_empty() {
                payload["tool_calls"] = Value::Array(tool_calls);
            }
            messages.push(payload);
        }
        return;
    }

    let mut text_parts = Vec::new();
    let mut tool_results = Vec::new();
    for block in &message.content {
        match block {
            InputContentBlock::Text { text } if !text.is_empty() => text_parts.push(text.clone()),
            InputContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut text = content
                    .iter()
                    .map(|block| match block {
                        ToolResultContentBlock::Text { text } => text.clone(),
                        ToolResultContentBlock::Json { value } => value.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if *is_error {
                    text = format!("Error: {text}");
                }
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": [{"type": "text", "text": text}]
                }));
            }
            _ => {}
        }
    }
    messages.extend(tool_results);
    if !text_parts.is_empty() {
        messages.push(json!({
            "role": if role.is_empty() { "user" } else { role },
            "content": [{"type": "text", "text": text_parts.join("\n")}]
        }));
    }
}

fn system_text(system: &SystemContent) -> String {
    match system {
        SystemContent::Text(text) => text.clone(),
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| block.text.as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn strip_trae_prefix(model: &str) -> String {
    model
        .trim()
        .strip_prefix("trae/")
        .unwrap_or_else(|| model.trim())
        .to_string()
}

fn map_reasoning_effort(effort: &str) -> &'static str {
    match effort.trim().to_ascii_lowercase().as_str() {
        "low" | "light" | "minimal" => "light",
        "max" | "xhigh" | "extra_high" => "extra_high",
        _ => "high",
    }
}

fn usage_from_record(record: &serde_json::Map<String, Value>) -> Usage {
    Usage {
        input_tokens: token_u32(record, "prompt_tokens"),
        cache_creation_input_tokens: token_u32(record, "cache_creation_input_tokens"),
        cache_read_input_tokens: token_u32(record, "cache_read_input_tokens"),
        output_tokens: token_u32(record, "completion_tokens"),
    }
}

fn token_u32(record: &serde_json::Map<String, Value>, key: &str) -> u32 {
    record
        .get(key)
        .and_then(Value::as_u64)
        .map_or(0, |value| u32::try_from(value).unwrap_or(u32::MAX))
}

fn apply_delta(block: &mut OutputContentBlock, delta: &ContentBlockDelta) {
    match (block, delta) {
        (OutputContentBlock::Text { text }, ContentBlockDelta::TextDelta { text: addition }) => {
            text.push_str(addition);
        }
        (
            OutputContentBlock::Thinking { thinking, .. },
            ContentBlockDelta::ThinkingDelta { thinking: addition },
        ) => {
            thinking.push_str(addition);
        }
        _ => {}
    }
}

fn trace_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{nanos:032x}")
}

fn http_error(status: StatusCode, body: String, retry_after: Option<Duration>) -> ApiError {
    ApiError::Api {
        status,
        error_type: None,
        message: Some(body.clone()),
        request_id: None,
        body,
        retryable: status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
        suggested_action: None,
        retry_after,
    }
}

/// 读取响应头里的 `Retry-After`（只解析秒数形式；HTTP-date 形式直接忽略，
/// 交由本地指数退避兜底）。
fn retry_after_header(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// 指数退避：1s→2s→4s→8s（上限后保持 8s），带 ±25% 抖动，避免多会话
/// 在同一时刻重试形成尖峰。
fn send_backoff(retries: u32) -> Duration {
    let exponent = retries.saturating_sub(1).min(3);
    let base = SEND_INITIAL_BACKOFF
        .saturating_mul(1u32 << exponent)
        .min(SEND_MAX_BACKOFF);
    let jitter_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX) / 4;
    if jitter_ms == 0 {
        return base;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::from(elapsed.subsec_nanos()));
    base + Duration::from_millis(nanos % jitter_ms)
}

fn trae_error(code: i64, message: String, body: String) -> ApiError {
    let status = match code {
        4011 => StatusCode::TOO_MANY_REQUESTS,
        1005 => StatusCode::PAYMENT_REQUIRED,
        401 | 403 => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError::Api {
        status,
        error_type: Some(format!("TRAE_{code}")),
        message: Some(message),
        request_id: None,
        body,
        retryable: code == 4011,
        suggested_action: None,
        retry_after: None,
    }
}

#[derive(Debug, Deserialize)]
struct AuthFile {
    credentials: Vec<AuthCredential>,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthCredential {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(default)]
    edition: Option<String>,
    #[serde(default, rename = "expiresAtMs")]
    expires_at_ms: Option<u64>,
    #[serde(default, rename = "userId")]
    user_id: Option<String>,
}

fn read_cached_credential() -> Option<AuthCredential> {
    let path = env::var("TRAE_AUTH_FILE")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(default_auth_path);
    let text = fs::read_to_string(path).ok()?;
    let auth: AuthFile = serde_json::from_str(&text).ok()?;
    let edition = env::var("TRAE_EDITION").unwrap_or_else(|_| "cn".to_string());
    let selected = auth
        .credentials
        .iter()
        .find(|credential| credential.edition.as_deref() == Some(edition.as_str()))
        .or_else(|| auth.credentials.first())?;
    if let Some(expires_at_ms) = selected.expires_at_ms {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        if now_ms >= expires_at_ms {
            return None;
        }
    }
    Some(selected.clone())
}

fn default_auth_path() -> PathBuf {
    let home = env::var("USERPROFILE")
        .or_else(|_| env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".dsh").join(".trae-auth.cn.json")
}

/// Key in the desktop client's `storage.json` that holds the account credential.
const AUTH_STORAGE_KEY: &str = "iCubeAuthInfo://icube.cloudide";

/// Fixed salts for the storage value envelope. The 6-byte header selects the
/// salt pair; every value shipped today uses the first one.
const AUTH_HEADER_AES: [u8; 6] = [116, 99, 5, 16, 0, 0];
const AUTH_HEADER_AES_PRIVATE: [u8; 6] = [18, 57, 32, 32, 2, 3];
const SALT_A: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109,
    139, 209, 37,
];
const SALT_B: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200,
    235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33,
    12, 125,
];
const SALT_C: [u8; 64] = [
    191, 192, 216, 250, 122, 246, 220, 97, 31, 254, 98, 27, 8, 72, 71, 176, 135, 99, 96, 18, 127,
    101, 203, 104, 211, 102, 191, 125, 37, 72, 150, 156, 51, 229, 121, 35, 17, 153, 141, 177, 110,
    131, 150, 128, 172, 255, 254, 6, 18, 140, 55, 62, 236, 249, 135, 64, 135, 12, 117, 4, 89, 149,
    168, 209,
];
const SALT_D: [u8; 64] = [
    246, 204, 26, 232, 232, 70, 129, 109, 223, 146, 169, 242, 23, 241, 105, 145, 50, 196, 165, 42,
    254, 120, 3, 54, 244, 207, 209, 85, 53, 6, 138, 106, 175, 148, 31, 204, 186, 186, 165, 182, 87,
    142, 49, 10, 39, 110, 26, 154, 86, 56, 173, 125, 18, 64, 198, 225, 99, 99, 83, 82, 191, 134,
    76, 170,
];

/// Read the account credential from the Trae desktop client's `storage.json`.
///
/// The value under [`AUTH_STORAGE_KEY`] is `[6-byte header][32-byte random]
/// [AES-128-CBC ciphertext]`; the key and IV come from
/// `sha512(sha512(random) || salt)` and the plaintext is
/// `sha512(body) || body`. Expired entries are skipped so a running client
/// that has not refreshed yet falls through to the next source.
fn read_desktop_credential() -> Option<AuthCredential> {
    let path = env::var("TRAE_STORAGE_FILE")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(default_storage_path);
    let text = fs::read_to_string(path).ok()?;
    let storage: Value = serde_json::from_str(&text).ok()?;
    let encoded = storage.get(AUTH_STORAGE_KEY)?.as_str()?;
    let auth = decrypt_auth_storage_value(encoded)?;
    let access_token = auth.get("token")?.as_str()?.trim().to_string();
    if access_token.is_empty() {
        return None;
    }
    let user_id = auth
        .get("userId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let expires_at_ms = jwt_expiry_ms(&access_token);
    if expires_at_ms.is_some_and(|expires| now_ms() >= expires) {
        return None;
    }
    Some(AuthCredential {
        access_token,
        edition: Some("cn".to_string()),
        expires_at_ms,
        user_id,
    })
}

fn decrypt_auth_storage_value(encoded: &str) -> Option<Value> {
    let buffer = BASE64_STANDARD.decode(encoded.trim()).ok()?;
    if buffer.len() <= 102 {
        return None;
    }
    let header = &buffer[..6];
    let salt = if header == AUTH_HEADER_AES.as_slice() {
        xor64(&SALT_A, &SALT_B)
    } else if header == AUTH_HEADER_AES_PRIVATE.as_slice() {
        xor64(&SALT_C, &SALT_D)
    } else {
        return None;
    };
    let random = &buffer[6..38];
    let encrypted = &buffer[38..];
    let first = Sha512::digest(random);
    let mut hasher = Sha512::new();
    hasher.update(first);
    hasher.update(salt);
    let derived = hasher.finalize();
    let decryptor =
        cbc::Decryptor::<Aes128>::new_from_slices(&derived[..16], &derived[16..32]).ok()?;
    let mut plaintext = encrypted.to_vec();
    let decrypted = decryptor.decrypt_padded_mut::<Pkcs7>(&mut plaintext).ok()?;
    if decrypted.len() < 64 {
        return None;
    }
    let (expected, body) = decrypted.split_at(64);
    if expected != Sha512::digest(body).as_slice() {
        return None;
    }
    serde_json::from_slice(body).ok()
}

fn xor64(a: &[u8; 64], b: &[u8; 64]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = a[index] ^ b[index];
    }
    out
}

/// Expiry (`exp`) from a JWT payload, in milliseconds since the epoch.
fn jwt_expiry_ms(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value.get("exp")?.as_u64().map(|seconds| seconds * 1_000)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[derive(Debug, Default)]
struct TraeIdentity {
    machine_id: Option<String>,
    device_id: Option<String>,
    app_version: Option<String>,
    os_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StorageFile {
    #[serde(default, rename = "telemetry.machineId")]
    machine_id: Option<String>,
    #[serde(default, rename = "telemetry.devDeviceId")]
    device_id: Option<String>,
    #[serde(default, rename = "iCubeLastVersion")]
    app_version: Option<String>,
}

fn read_identity() -> TraeIdentity {
    let path = env::var("TRAE_STORAGE_FILE")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(default_storage_path);
    let parsed = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<StorageFile>(&text).ok());
    TraeIdentity {
        machine_id: parsed
            .as_ref()
            .and_then(|storage| storage.machine_id.clone())
            .filter(|value| !value.trim().is_empty()),
        device_id: parsed
            .as_ref()
            .and_then(|storage| storage.device_id.clone())
            .filter(|value| !value.trim().is_empty()),
        app_version: parsed
            .as_ref()
            .and_then(|storage| storage.app_version.clone())
            .filter(|value| !value.trim().is_empty()),
        os_version: detect_os_version(),
    }
}

fn default_storage_path() -> PathBuf {
    let appdata = env::var("APPDATA")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            env::var("USERPROFILE")
                .ok()
                .map(|home| PathBuf::from(home).join("AppData").join("Roaming"))
        })
        .unwrap_or_default();
    appdata
        .join("Trae CN")
        .join("User")
        .join("globalStorage")
        .join("storage.json")
}

fn detect_os_version() -> Option<String> {
    if let Ok(value) = env::var("TRAE_OS_VERSION") {
        if !value.trim().is_empty() {
            return Some(value);
        }
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd")
            .args(["/C", "ver"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let version = text.split("Version ").nth(1)?.split(']').next()?.trim();
        if !version.is_empty() {
            return Some(version.to_string());
        }
    }
    None
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn dashed_id(trace: &str) -> String {
    if trace.len() >= 32 {
        format!(
            "{}-{}-{}-{}-{}",
            &trace[0..8],
            &trace[8..12],
            &trace[12..16],
            &trace[16..20],
            &trace[20..32]
        )
    } else {
        trace.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_backoff_grows_and_caps() {
        let first = send_backoff(1);
        assert!(first >= Duration::from_secs(1));
        assert!(first < Duration::from_millis(1_300));

        // 超过指数上限后固定在 8s 基准（抖动最多再 +25%）。
        let capped = send_backoff(9);
        assert!(capped >= Duration::from_secs(8));
        assert!(capped < Duration::from_secs(10));
    }

    #[test]
    fn decrypts_desktop_auth_storage_value() {
        // 合成密文：同样的盐/格式，明文是 {"token":"fixture-token","userId":"42"}。
        let encoded = "dGMFEAAABwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBweRKPSg6g63I9MjtfJao11QobNmPTgXi9nEuMFe1ortri6AKueuxxeoS+4lQboYAIywuuBb83XQ5/msMXnMspNWScxdrzleQZuQ8iK0FbFpdrZwJXPgLeeiAeBMULBKE4ox2adp6uoRMTZ7mbD+AAQ/";
        let decoded = decrypt_auth_storage_value(encoded).expect("fixture must decrypt");
        assert_eq!(
            decoded.get("token").and_then(Value::as_str),
            Some("fixture-token")
        );
        assert_eq!(decoded.get("userId").and_then(Value::as_str), Some("42"));
    }
}
