use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::http_client::{
    build_http_client_or_default, build_http_client_with_opts, ProxyConfig, TimeoutConfig,
};
use crate::types::{
    ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
    InputContentBlock, InputMessage, MessageDelta, MessageDeltaEvent, MessageRequest,
    MessageResponse, MessageStartEvent, MessageStopEvent, OutputContentBlock, StreamEvent,
    SystemContent, ToolChoice, ToolDefinition, ToolResultContentBlock, Usage,
};

use super::preflight_message_request;

pub const DEFAULT_DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
const REQUEST_ID_HEADER: &str = "request-id";
const ALT_REQUEST_ID_HEADER: &str = "x-request-id";
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(128);
const DEFAULT_MAX_RETRIES: u32 = 8;

/// Minimal placeholder echoed back as `reasoning_content` for DeepSeek
/// thinking-mode tool-call turns (`call_0N_*`, N ≥ 1).
///
/// Empirically verified on 2026-08-15 (deepseek-v4 endpoint) that the API only
/// checks the **presence** of `reasoning_content`, never its content: values
/// like `"x"*200`, `"thinking"`, `"1"`, `"…"` all return 200, while an omitted
/// field returns 400. So instead of echoing the full thinking text (~1/3 of
/// assistant tokens in thinking mode) we send a single-character placeholder.
/// The model never reads it back — it is dead context the API only validates
/// for existence.
pub const REASONING_PLACEHOLDER: &str = "…";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiCompatConfig {
    pub provider_name: &'static str,
    pub api_key_env: &'static str,
    pub base_url_env: &'static str,
    pub default_base_url: &'static str,
    /// Maximum request body size in bytes.
    pub max_request_body_bytes: usize,
}

const DEEPSEEK_ENV_VARS: &[&str] = &["DEEPSEEK_API_KEY"];

// DeepSeek has no documented body size limit; use 100MB as a safe default.
const DEEPSEEK_MAX_REQUEST_BODY_BYTES: usize = 104_857_600; // 100MB

impl OpenAiCompatConfig {
    /// DeepSeek API (deepseek-v4-pro, deepseek-v4-flash, etc.)
    /// Uses the OpenAI-compatible REST shape at api.deepseek.com/v1.
    /// Auth via DEEPSEEK_API_KEY.
    #[must_use]
    pub const fn deepseek() -> Self {
        Self {
            provider_name: "DeepSeek",
            api_key_env: "DEEPSEEK_API_KEY",
            base_url_env: "DEEPSEEK_BASE_URL",
            default_base_url: DEFAULT_DEEPSEEK_BASE_URL,
            max_request_body_bytes: DEEPSEEK_MAX_REQUEST_BODY_BYTES,
        }
    }

    #[must_use]
    pub fn credential_env_vars(self) -> &'static [&'static str] {
        DEEPSEEK_ENV_VARS
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    api_key: String,
    config: OpenAiCompatConfig,
    base_url: String,
    max_retries: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl OpenAiCompatClient {
    const fn config(&self) -> OpenAiCompatConfig {
        self.config
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    #[must_use]
    pub fn new(api_key: impl Into<String>, config: OpenAiCompatConfig) -> Self {
        Self {
            http: build_http_client_or_default(),
            api_key: api_key.into(),
            config,
            base_url: read_base_url(config),
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }

    pub fn from_env(config: OpenAiCompatConfig) -> Result<Self, ApiError> {
        let api_key = read_env_non_empty(config.api_key_env)?.ok_or_else(|| {
            ApiError::missing_credentials(config.provider_name, config.credential_env_vars())
        })?;
        Ok(Self::new(api_key, config))
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    #[must_use]
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    #[must_use]
    pub fn with_retry_policy(
        mut self,
        max_retries: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        self.max_retries = max_retries;
        self.initial_backoff = initial_backoff;
        self.max_backoff = max_backoff;
        self
    }

    /// Replace the underlying `reqwest::Client` with one built from the
    /// provided [`TimeoutConfig`] (and the current process proxy config).
    /// This is the config-file-driven counterpart to the env-var-based
    /// defaults applied by `build_http_client_or_default`.
    ///
    /// Errors only occur when the proxy configuration is malformed; in that
    /// case the previous client is retained and the error is logged to stderr
    /// so the caller retains a working client.
    #[must_use]
    pub fn with_timeout(mut self, timeout: &TimeoutConfig) -> Self {
        match build_http_client_with_opts(&ProxyConfig::from_env(), timeout) {
            Ok(client) => self.http = client,
            Err(error) => {
                eprintln!(
                    "warn: failed to apply custom TimeoutConfig (connect={:?}, request={:?}): {error}; keeping existing client",
                    timeout.connect_timeout,
                    timeout.request_timeout
                );
            }
        }
        self
    }

    pub async fn send_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        let request = MessageRequest {
            stream: false,
            ..request.clone()
        };
        preflight_message_request(&request)?;
        let response = self.send_with_retry(&request).await?;
        let request_id = request_id_from_headers(response.headers());
        let body = response.text().await.map_err(ApiError::from)?;
        // Some backends return {"error":{"message":"...","type":"...","code":...}}
        // instead of a valid completion object. Check for this before attempting
        // full deserialization so the user sees the actual error, not a cryptic
        // "missing field 'id'" parse failure.
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(err_obj) = raw.get("error") {
                let msg = err_obj
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("provider returned an error")
                    .to_string();
                let code = err_obj
                    .get("code")
                    .and_then(serde_json::Value::as_u64)
                    .map(|c| c as u16);
                return Err(ApiError::Api {
                    status: reqwest::StatusCode::from_u16(code.unwrap_or(400))
                        .unwrap_or(reqwest::StatusCode::BAD_REQUEST),
                    error_type: err_obj
                        .get("type")
                        .and_then(|t| t.as_str())
                        .map(str::to_owned),
                    message: Some(msg),
                    request_id,
                    body,
                    retryable: false,
                    suggested_action: suggested_action_for_status(
                        reqwest::StatusCode::from_u16(code.unwrap_or(400))
                            .unwrap_or(reqwest::StatusCode::BAD_REQUEST),
                    ),
                    retry_after: None,
                });
            }
        }
        let payload = serde_json::from_str::<ChatCompletionResponse>(&body).map_err(|error| {
            ApiError::json_deserialize(self.config.provider_name, &request.model, &body, error)
        })?;
        let mut normalized = normalize_response(&request.model, payload)?;
        if normalized.request_id.is_none() {
            normalized.request_id = request_id;
        }
        Ok(normalized)
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        preflight_message_request(request)?;
        let response = self
            .send_with_retry(&request.clone().with_streaming())
            .await?;
        Ok(MessageStream {
            request_id: request_id_from_headers(response.headers()),
            response,
            parser: OpenAiSseParser::with_context(self.config.provider_name, request.model.clone()),
            pending: VecDeque::new(),
            done: false,
            state: StreamState::new(request.model.clone(), self.base_url.clone()),
        })
    }

    async fn send_with_retry(
        &self,
        request: &MessageRequest,
    ) -> Result<reqwest::Response, ApiError> {
        let mut attempts = 0;

        let last_error = loop {
            attempts += 1;
            let retryable_error = match self.send_raw_request(request).await {
                Ok(response) => match expect_success(response).await {
                    Ok(response) => return Ok(response),
                    Err(error) if error.is_retryable() && attempts <= self.max_retries + 1 => error,
                    Err(error) => return Err(error),
                },
                Err(error) if error.is_retryable() && attempts <= self.max_retries + 1 => error,
                Err(error) => return Err(error),
            };

            if attempts > self.max_retries {
                break retryable_error;
            }

            // Honour the server's `Retry-After` advisory when present; only
            // fall back to local exponential backoff when the response did not
            // carry one. This keeps us aligned with provider rate-limit windows
            // instead of guessing.
            let sleep_duration = retryable_error
                .retry_after()
                .map(Ok)
                .unwrap_or_else(|| self.jittered_backoff_for_attempt(attempts))?;
            tokio::time::sleep(sleep_duration).await;
        };

        Err(ApiError::RetriesExhausted {
            attempts,
            last_error: Box::new(last_error),
        })
    }

    async fn send_raw_request(
        &self,
        request: &MessageRequest,
    ) -> Result<reqwest::Response, ApiError> {
        // Pre-flight check: verify request body size against provider limits
        check_request_body_size_for_base_url(request, self.config(), &self.base_url)?;

        let request_url = chat_completions_endpoint(&self.base_url);
        self.http
            .post(&request_url)
            .header("content-type", "application/json")
            .bearer_auth(&self.api_key)
            .json(&build_chat_completion_request_for_base_url(
                request,
                self.config(),
                &self.base_url,
            ))
            .send()
            .await
            .map_err(ApiError::from)
    }

    fn backoff_for_attempt(&self, attempt: u32) -> Result<Duration, ApiError> {
        let Some(multiplier) = 1_u32.checked_shl(attempt.saturating_sub(1)) else {
            return Err(ApiError::BackoffOverflow {
                attempt,
                base_delay: self.initial_backoff,
            });
        };
        Ok(self
            .initial_backoff
            .checked_mul(multiplier)
            .map_or(self.max_backoff, |delay| delay.min(self.max_backoff)))
    }

    fn jittered_backoff_for_attempt(&self, attempt: u32) -> Result<Duration, ApiError> {
        let base = self.backoff_for_attempt(attempt)?;
        Ok(base + jitter_for_base(base))
    }
}

/// Process-wide counter that guarantees distinct jitter samples even when
/// the system clock resolution is coarser than consecutive retry sleeps.
static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns a random additive jitter in `[0, base]` to decorrelate retries
/// Deserialize a JSON field as a `Vec<T>`, treating an explicit `null` value
/// the same as a missing field (i.e. as an empty vector).
/// Some OpenAI-compatible providers emit `"tool_calls": null` instead of
/// omitting the field or using `[]`, which serde's `#[serde(default)]` alone
/// does not tolerate — `default` only handles absent keys, not null values.
fn deserialize_null_as_empty_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// from multiple concurrent clients. Entropy is drawn from the nanosecond
/// wall clock mixed with a monotonic counter and run through a splitmix64
/// finalizer; adequate for retry jitter (no cryptographic requirement).
fn jitter_for_base(base: Duration) -> Duration {
    let base_nanos = u64::try_from(base.as_nanos()).unwrap_or(u64::MAX);
    if base_nanos == 0 {
        return Duration::ZERO;
    }
    let raw_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        });
    let tick = JITTER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut mixed = raw_nanos
        .wrapping_add(tick)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^= mixed >> 31;
    let jitter_nanos = mixed % base_nanos.saturating_add(1);
    Duration::from_nanos(jitter_nanos)
}

#[derive(Debug)]
pub struct MessageStream {
    request_id: Option<String>,
    response: reqwest::Response,
    parser: OpenAiSseParser,
    pending: VecDeque<StreamEvent>,
    done: bool,
    state: StreamState,
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
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
                    for parsed in self.parser.push(&chunk)? {
                        self.pending.extend(self.state.ingest_chunk(parsed)?);
                    }
                }
                None => {
                    self.done = true;
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct OpenAiSseParser {
    buffer: Vec<u8>,
    provider: String,
    model: String,
}

impl OpenAiSseParser {
    fn with_context(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            buffer: Vec::new(),
            provider: provider.into(),
            model: model.into(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<ChatCompletionChunk>, ApiError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(frame) = next_sse_frame(&mut self.buffer) {
            if let Some(event) = parse_sse_frame(&frame, &self.provider, &self.model)? {
                events.push(event);
            }
        }

        Ok(events)
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct StreamState {
    model: String,
    base_url: String,
    message_started: bool,
    text_started: bool,
    text_finished: bool,
    finished: bool,
    stop_reason: Option<String>,
    usage: Option<Usage>,
    tool_calls: BTreeMap<u32, ToolCallState>,
    thinking_started: bool,
    thinking_finished: bool,
}

impl StreamState {
    fn new(model: String, base_url: String) -> Self {
        Self {
            model,
            base_url,
            message_started: false,
            text_started: false,
            text_finished: false,
            finished: false,
            stop_reason: None,
            usage: None,
            tool_calls: BTreeMap::new(),
            thinking_started: false,
            thinking_finished: false,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_chunk(&mut self, chunk: ChatCompletionChunk) -> Result<Vec<StreamEvent>, ApiError> {
        let mut events = Vec::new();
        if !self.message_started {
            self.message_started = true;
            events.push(StreamEvent::MessageStart(MessageStartEvent {
                message: MessageResponse {
                    id: chunk.id.clone(),
                    kind: "message".to_string(),
                    role: "assistant".to_string(),
                    content: Vec::new(),
                    model: chunk.model.clone().unwrap_or_else(|| self.model.clone()),
                    stop_reason: None,
                    stop_sequence: None,
                    usage: Usage {
                        input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        output_tokens: 0,
                    },
                    request_id: None,
                },
            }));
        }

        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.normalized(Some(&self.model), Some(&self.base_url)));
        }

        for choice in chunk.choices {
            if let Some(reasoning) = choice
                .delta
                .reasoning_content
                .filter(|value| !value.is_empty())
            {
                if !self.thinking_started {
                    self.thinking_started = true;
                    events.push(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                        index: 0,
                        content_block: OutputContentBlock::Thinking {
                            thinking: String::new(),
                            signature: None,
                        },
                    }));
                }
                events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::ThinkingDelta {
                        thinking: reasoning,
                    },
                }));
            }

            if let Some(content) = choice.delta.content.filter(|value| !value.is_empty()) {
                self.close_thinking(&mut events);
                if !self.text_started {
                    self.text_started = true;
                    events.push(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                        index: self.text_block_index(),
                        content_block: OutputContentBlock::Text {
                            text: String::new(),
                        },
                    }));
                }
                events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                    index: self.text_block_index(),
                    delta: ContentBlockDelta::TextDelta { text: content },
                }));
            }

            for tool_call in choice.delta.tool_calls {
                self.close_thinking(&mut events);
                let tool_index_offset = self.tool_index_offset();
                let state = self.tool_calls.entry(tool_call.index).or_default();
                state.apply(tool_call);
                let block_index = state.block_index(tool_index_offset);
                if !state.started {
                    if let Some(start_event) = state.start_event(tool_index_offset)? {
                        state.started = true;
                        events.push(StreamEvent::ContentBlockStart(start_event));
                    } else {
                        continue;
                    }
                }
                if let Some(delta_event) = state.delta_event(tool_index_offset) {
                    events.push(StreamEvent::ContentBlockDelta(delta_event));
                }
                if choice.finish_reason.as_deref() == Some("tool_calls") && !state.stopped {
                    state.stopped = true;
                    events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                        index: block_index,
                    }));
                }
            }

            if let Some(finish_reason) = choice.finish_reason {
                self.stop_reason = Some(normalize_finish_reason(&finish_reason));
                if finish_reason == "tool_calls" {
                    let tool_index_offset = self.tool_index_offset();
                    for state in self.tool_calls.values_mut() {
                        if state.started && !state.stopped {
                            state.stopped = true;
                            events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                                index: state.block_index(tool_index_offset),
                            }));
                        }
                    }
                }
            }
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ApiError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;

        let mut events = Vec::new();
        self.close_thinking(&mut events);
        if self.text_started && !self.text_finished {
            self.text_finished = true;
            events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                index: self.text_block_index(),
            }));
        }

        let tool_index_offset = self.tool_index_offset();
        for state in self.tool_calls.values_mut() {
            if !state.started {
                if let Some(start_event) = state.start_event(tool_index_offset)? {
                    state.started = true;
                    events.push(StreamEvent::ContentBlockStart(start_event));
                    if let Some(delta_event) = state.delta_event(tool_index_offset) {
                        events.push(StreamEvent::ContentBlockDelta(delta_event));
                    }
                }
            }
            if state.started && !state.stopped {
                state.stopped = true;
                events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                    index: state.block_index(tool_index_offset),
                }));
            }
        }

        if self.message_started {
            events.push(StreamEvent::MessageDelta(MessageDeltaEvent {
                delta: MessageDelta {
                    stop_reason: Some(
                        self.stop_reason
                            .clone()
                            .unwrap_or_else(|| "end_turn".to_string()),
                    ),
                    stop_sequence: None,
                },
                usage: self.usage.clone().unwrap_or(Usage {
                    input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    output_tokens: 0,
                }),
            }));
            events.push(StreamEvent::MessageStop(MessageStopEvent {}));
        }
        Ok(events)
    }

    fn close_thinking(&mut self, events: &mut Vec<StreamEvent>) {
        if self.thinking_started && !self.thinking_finished {
            self.thinking_finished = true;
            events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                index: 0,
            }));
        }
    }

    const fn text_block_index(&self) -> u32 {
        if self.thinking_started {
            1
        } else {
            0
        }
    }

    const fn tool_index_offset(&self) -> u32 {
        if self.thinking_started {
            2
        } else {
            1
        }
    }
}

#[derive(Debug, Default)]
struct ToolCallState {
    openai_index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    emitted_len: usize,
    started: bool,
    stopped: bool,
}

impl ToolCallState {
    fn apply(&mut self, tool_call: DeltaToolCall) {
        self.openai_index = tool_call.index;
        if let Some(id) = tool_call.id {
            self.id = Some(id);
        }
        if let Some(name) = tool_call.function.name {
            self.name = Some(name);
        }
        if let Some(arguments) = tool_call.function.arguments {
            self.arguments.push_str(&arguments);
        }
    }

    const fn block_index(&self, offset: u32) -> u32 {
        self.openai_index + offset
    }

    #[allow(clippy::unnecessary_wraps)]
    fn start_event(&self, offset: u32) -> Result<Option<ContentBlockStartEvent>, ApiError> {
        let Some(name) = self.name.clone() else {
            return Ok(None);
        };
        let id = self
            .id
            .clone()
            .unwrap_or_else(|| format!("tool_call_{}", self.openai_index));
        Ok(Some(ContentBlockStartEvent {
            index: self.block_index(offset),
            content_block: OutputContentBlock::ToolUse {
                id,
                name,
                input: json!({}),
            },
        }))
    }

    fn delta_event(&mut self, offset: u32) -> Option<ContentBlockDeltaEvent> {
        if self.emitted_len >= self.arguments.len() {
            return None;
        }
        let delta = self.arguments[self.emitted_len..].to_string();
        self.emitted_len = self.arguments.len();
        Some(ContentBlockDeltaEvent {
            index: self.block_index(offset),
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: delta,
            },
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    id: String,
    model: String,
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Debug, Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseToolFunction,
}

#[derive(Debug, Deserialize)]
struct ResponseToolFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    // DeepSeek 原生字段：直接平铺在 usage 对象上，不在 prompt_tokens_details 里。
    // 优先级高于 OpenAI 标准 cached_tokens，因为它是 DeepSeek 自己的命中计数。
    #[serde(default)]
    prompt_cache_hit_tokens: u32,
    #[serde(default)]
    prompt_cache_miss_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct OpenAiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

impl OpenAiUsage {
    pub(crate) fn normalized(&self, model: Option<&str>, base_url: Option<&str>) -> Usage {
        // DeepSeek 原生字段优先：当 hit > 0 时直接用 DeepSeek 的语义。
        // - cache_read_input_tokens = prompt_cache_hit_tokens (命中缓存读取)
        // - cache_creation_input_tokens = miss (未命中写入缓存,用于命中率统计)
        // - input_tokens = 0 (DeepSeek 的 prompt 全归入 hit+miss,无"既不缓存也不读"的裸输入)
        //
        // prompt_cache_miss_tokens 兼容:部分 API 版本只返回 hit 不返回 miss,
        // 此时从 prompt_tokens - hit 反推 miss; 若 prompt_tokens 也为 0 (流式
        // trailing chunk 可能不包含), 则从 total_tokens - completion_tokens - hit 兜底。
        //
        // 否则回退到 OpenAI 标准：从 prompt_tokens_details.cached_tokens 推导。
        if self.prompt_cache_hit_tokens > 0 {
            let miss = if self.prompt_cache_miss_tokens > 0 {
                self.prompt_cache_miss_tokens
            } else if self.prompt_tokens > 0 {
                self.prompt_tokens
                    .saturating_sub(self.prompt_cache_hit_tokens)
            } else {
                self.total_tokens
                    .saturating_sub(self.completion_tokens)
                    .saturating_sub(self.prompt_cache_hit_tokens)
            };
            Usage {
                input_tokens: 0,
                cache_creation_input_tokens: miss,
                cache_read_input_tokens: self.prompt_cache_hit_tokens,
                output_tokens: self.completion_tokens,
            }
        } else {
            let cached_tokens = self
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |details| details.cached_tokens);
            // DeepSeek V4 uses OpenAI-standard cached_tokens field but
            // semantically all prompt tokens are either cache-hit or
            // cache-miss — there is no "uncategorised" input bucket.
            let is_deepseek = model
                .is_some_and(|name| name.to_ascii_lowercase().contains("deepseek"))
                || base_url.is_some_and(|url| url.to_ascii_lowercase().contains("deepseek"));
            if is_deepseek {
                Usage {
                    input_tokens: 0,
                    cache_creation_input_tokens: self.prompt_tokens.saturating_sub(cached_tokens),
                    cache_read_input_tokens: cached_tokens,
                    output_tokens: self.completion_tokens,
                }
            } else {
                Usage {
                    input_tokens: self.prompt_tokens.saturating_sub(cached_tokens),
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: cached_tokens,
                    output_tokens: self.completion_tokens,
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    delta: ChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_as_empty_vec")]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: DeltaFunction,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(rename = "type")]
    error_type: Option<String>,
    message: Option<String>,
}

/// Returns true for models known to reject tuning parameters like temperature,
/// `top_p`, `frequency_penalty`, and `presence_penalty`. These are typically
/// reasoning/chain-of-thought models with fixed sampling.
/// DeepSeek-reasoner is handled via `model_requires_reasoning_content_in_history`
/// and does not rely on this function.
/// Public for benchmarking and testing purposes.
#[must_use]
pub fn is_reasoning_model(_model: &str) -> bool {
    false
}

/// Returns whether an OpenAI-compatible model requires prior assistant
/// reasoning to be echoed back as `reasoning_content` in history.
///
/// Historically DeepSeek V4 (thinking mode) rejected requests that omitted
/// `reasoning_content` on tool-call turns (400: "reasoning_content ... must be
/// passed back"). Empirically verified on 2026-08-15 that the current
/// deepseek-v4 build no longer enforces this for ordinary turns, so we strip
/// prior thinking from the request to avoid carrying ~1/3 of assistant tokens
/// as dead context. However, turns whose tool calls carry the `call_0N_*`
/// prefix (N ≥ 1, thinking mode) still enforce passback — see
/// [`has_thinking_mode_tool_call`], which is the per-message gate that
/// actually decides whether `reasoning_content` is emitted.
/// Returns `false` for all models.
#[must_use]
pub fn model_requires_reasoning_content_in_history(_model: &str) -> bool {
    false
}

/// Returns true when any tool call id uses the `call_0N_` (N ≥ 1) prefix,
/// which DeepSeek reserves for thinking-mode tool calls.
///
/// Empirically verified on the deepseek-v4 endpoint:
/// - assistant turn with a `call_01_*` / `call_02_*` tool call and NO
///   `reasoning_content` in history → 400 "The `reasoning_content` in the
///   thinking mode must be passed back to the API" (call_01_ verified
///   2026-08-15; call_02_ verified 2026-08-30, session-1788025620097).
/// - the same turn WITH `reasoning_content` → 200.
/// - `call_00_*` tool calls / pure-text turns (no tool call) → 200 without
///   reasoning. So the thinking-mode numbering starts at 1; match `call_0N_`
///   for N ≥ 1 rather than hard-coding a single number, leaving call_00_
///   (non-thinking) and Anthropic-style ids (toolu_*) out.
#[must_use]
pub fn has_thinking_mode_tool_call<I, S>(tool_call_ids: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    tool_call_ids.into_iter().any(|id| {
        let b = id.as_ref().as_bytes();
        b.len() >= 8
            && &b[0..6] == b"call_0"
            && b[6].is_ascii_digit()
            && b[6] != b'0'
            && b[7] == b'_'
    })
}

/// Strip routing prefix (e.g., "deepseek/deepseek-v4-pro" → "deepseek-v4-pro")
/// for the wire. The prefix is used only to select transport; the backend
/// expects the bare model id.
#[allow(dead_code)]
fn strip_routing_prefix(model: &str) -> &str {
    if let Some(pos) = model.find('/') {
        let prefix = &model[..pos];
        if prefix.eq_ignore_ascii_case("deepseek") {
            return &model[pos + 1..];
        }
    }
    model
}

fn wire_model_for_base_url<'a>(
    model: &'a str,
    _config: OpenAiCompatConfig,
    _base_url: &str,
) -> Cow<'a, str> {
    let Some(pos) = model.find('/') else {
        return Cow::Borrowed(model);
    };
    let prefix = &model[..pos];
    if prefix.eq_ignore_ascii_case("deepseek") {
        return Cow::Borrowed(&model[pos + 1..]);
    }
    Cow::Borrowed(model)
}

/// Estimate the serialized JSON size of a request payload in bytes.
/// This is a pre-flight check to avoid hitting provider-specific size limits.
#[must_use]
pub fn estimate_request_body_size(request: &MessageRequest, config: OpenAiCompatConfig) -> usize {
    estimate_request_body_size_for_base_url(request, config, &read_base_url(config))
}

fn estimate_request_body_size_for_base_url(
    request: &MessageRequest,
    config: OpenAiCompatConfig,
    base_url: &str,
) -> usize {
    let payload = build_chat_completion_request_for_base_url(request, config, base_url);
    // serde_json::to_vec gives us the exact byte size of the serialized JSON
    serde_json::to_vec(&payload).map_or(0, |v| v.len())
}

/// Pre-flight check for request body size against provider limits.
/// Returns Ok(()) if the request is within limits, or an error with
/// a clear message about the size limit being exceeded.
pub fn check_request_body_size(
    request: &MessageRequest,
    config: OpenAiCompatConfig,
) -> Result<(), ApiError> {
    check_request_body_size_for_base_url(request, config, &read_base_url(config))
}

fn check_request_body_size_for_base_url(
    request: &MessageRequest,
    config: OpenAiCompatConfig,
    base_url: &str,
) -> Result<(), ApiError> {
    let estimated_bytes = estimate_request_body_size_for_base_url(request, config, base_url);
    let max_bytes = config.max_request_body_bytes;

    if estimated_bytes > max_bytes {
        Err(ApiError::RequestBodySizeExceeded {
            estimated_bytes,
            max_bytes,
            provider: config.provider_name,
        })
    } else {
        Ok(())
    }
}

/// Builds a chat completion request payload from a `MessageRequest`.
/// Public for benchmarking purposes.
#[must_use]
pub fn build_chat_completion_request(
    request: &MessageRequest,
    config: OpenAiCompatConfig,
) -> Value {
    build_chat_completion_request_for_base_url(request, config, &read_base_url(config))
}

/// 将 `SystemContent` 转换为 1~2 个 OpenAI 兼容的 system message。
///
/// 专为 DeepSeek 等隐式前缀缓存模型优化：
/// - `SystemContent::Text`：单字符串，作为单个 system message。
/// - `SystemContent::Blocks`：按 `cache_control` 标记位置拆分。最后一个带
///   `cache_control` 的 block 视为静态/动态边界（Anthropic 路径的
///   `build_system_blocks` 在最后一个 static block 上标 `cache_control`）。
///   静态段（含该标记 block 及其之前的所有 block）拼成第一个 system message
///   —— token 序列稳定，命中 DeepSeek 前缀缓存；动态段（标记之后的 blocks）
///   拼成第二个 system message —— 内容会变化，但放在静态段之后不破坏前缀。
///
/// 此外，OpenAI/DeepSeek 的 system message `content` 必须是 string，不能是
/// `SystemBlock` 数组。直接 `content: system` 会让 `SystemContent::Blocks`
/// 序列化成数组，导致 400 错误或被强制 stringify（增加 token 浪费 + 破坏前缀
/// 缓存）。本函数显式提取每个 block 的 `text` 字段并用 `\n\n` 拼接成字符串。
fn split_system_content_to_openai_messages(system: &Option<SystemContent>) -> Vec<Value> {
    let Some(content) = system.as_ref().filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    match content {
        SystemContent::Text(text) => {
            if text.is_empty() {
                return Vec::new();
            }
            vec![json!({ "role": "system", "content": text })]
        }
        SystemContent::Blocks(blocks) => {
            if blocks.is_empty() {
                return Vec::new();
            }
            // 找到最后一个带 cache_control 的 block 作为静态/动态边界。
            // 没有任何 cache_control 标记时，全部视为静态段（单个 system message）。
            let boundary = blocks.iter().rposition(|b| b.cache_control.is_some());
            match boundary {
                None => {
                    let text = blocks
                        .iter()
                        .map(|b| b.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    vec![json!({ "role": "system", "content": text })]
                }
                Some(idx) => {
                    let static_text = blocks
                        .iter()
                        .take(idx + 1)
                        .map(|b| b.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let dynamic_text = blocks
                        .iter()
                        .skip(idx + 1)
                        .map(|b| b.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let mut result = Vec::with_capacity(2);
                    if !static_text.is_empty() {
                        result.push(json!({ "role": "system", "content": static_text }));
                    }
                    if !dynamic_text.is_empty() {
                        result.push(json!({ "role": "system", "content": dynamic_text }));
                    }
                    result
                }
            }
        }
    }
}

fn build_chat_completion_request_for_base_url(
    request: &MessageRequest,
    config: OpenAiCompatConfig,
    base_url: &str,
) -> Value {
    let mut messages = Vec::new();
    for system_message in split_system_content_to_openai_messages(&request.system) {
        messages.push(system_message);
    }
    // Resolve the transport routing prefix into the wire model. Custom
    // OpenAI-compatible gateways may require slash-containing slugs intact.
    let wire_model = wire_model_for_base_url(&request.model, config, base_url);
    let wire_model = wire_model.as_ref();
    for message in &request.messages {
        messages.extend(translate_message(message, wire_model));
    }
    // Sanitize: drop any `role:"tool"` message that does not have a valid
    // paired `role:"assistant"` with a `tool_calls` entry carrying the same
    // `id` immediately before it (directly or as part of a run of tool
    // results). OpenAI-compatible backends return 400 for orphaned tool
    // messages regardless of how they were produced (compaction, session
    // editing, resume, etc.). We drop rather than error so the request can
    // still proceed with the remaining history intact.
    messages = sanitize_tool_message_pairing(messages);

    let mut payload = json!({
        "model": wire_model,
        "max_tokens": request.max_tokens,
        "messages": messages,
        "stream": request.stream,
    });

    if request.stream {
        payload["stream_options"] = json!({ "include_usage": true });
    }

    if let Some(tools) = &request.tools {
        payload["tools"] =
            Value::Array(tools.iter().map(openai_tool_definition).collect::<Vec<_>>());
    }
    if let Some(tool_choice) = &request.tool_choice {
        payload["tool_choice"] = openai_tool_choice(tool_choice);
    }

    // OpenAI-compatible tuning parameters — only included when explicitly set.
    if !is_reasoning_model(&request.model) {
        if let Some(temperature) = request.temperature {
            payload["temperature"] = json!(temperature);
        }
        if let Some(top_p) = request.top_p {
            payload["top_p"] = json!(top_p);
        }
        if let Some(frequency_penalty) = request.frequency_penalty {
            payload["frequency_penalty"] = json!(frequency_penalty);
        }
        if let Some(presence_penalty) = request.presence_penalty {
            payload["presence_penalty"] = json!(presence_penalty);
        }
    }
    // stop is generally safe for all providers
    if let Some(stop) = &request.stop {
        if !stop.is_empty() {
            payload["stop"] = json!(stop);
        }
    }
    // reasoning_effort for DeepSeek reasoning models (deepseek-reasoner, etc.)
    if let Some(effort) = &request.reasoning_effort {
        payload["reasoning_effort"] = json!(effort);
    }

    for (key, value) in &request.extra_body {
        if is_protected_extra_body_key(key) {
            continue;
        }
        payload[key] = value.clone();
    }

    payload
}

fn is_protected_extra_body_key(key: &str) -> bool {
    matches!(
        key,
        "model" | "messages" | "stream" | "tools" | "tool_choice" | "max_tokens"
    )
}

/// Translates an `InputMessage` into OpenAI-compatible message format.
/// Public for benchmarking purposes.
#[must_use]
pub fn translate_message(message: &InputMessage, model: &str) -> Vec<Value> {
    match message.role.as_str() {
        "assistant" => {
            let mut text = String::new();
            let mut has_reasoning = false;
            let mut tool_calls = Vec::new();
            let mut tool_call_ids = Vec::new();
            for block in &message.content {
                match block {
                    InputContentBlock::Text { text: value } => text.push_str(value),
                    InputContentBlock::Thinking {
                        thinking: value, ..
                    } => {
                        if !value.is_empty() {
                            has_reasoning = true;
                        }
                    }
                    InputContentBlock::ToolUse { id, name, input } => {
                        tool_call_ids.push(id.clone());
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input.to_string(),
                            }
                        }))
                    }
                    InputContentBlock::ToolResult { .. } => {}
                }
            }
            // DeepSeek thinking-mode tool calls (`call_0N_*`, N ≥ 1) require
            // the prior reasoning to be echoed back as `reasoning_content`;
            // omitting it yields 400. Non-thinking turns don't need it, so we
            // only pay the context cost when the API actually enforces passback.
            //
            // 修复(2026-08-29):回传条件**不依赖 has_reasoning**。会话压缩/微压缩
            // 会剥离 thinking block,但保留 call_0N_* tool call —— 此时
            // has_reasoning=false 会导致占位符不回传,DeepSeek 仍返回 400
            // ("reasoning_content ... must be passed back to the API",线上复现)。
            // 修复(2026-08-30):前缀从 call_01_* 放宽到 call_0N_*(N ≥ 1)——
            // deepseek-v4 已在 thinking 模式生成 call_02_* id(线上复现 400,
            // 见 has_thinking_mode_tool_call)。call_0N_* 前缀本身就是 thinking
            // 模式的强信号,只要存在即回传。
            let include_reasoning = model_requires_reasoning_content_in_history(model)
                || has_thinking_mode_tool_call(&tool_call_ids);
            if text.is_empty() && tool_calls.is_empty() && !include_reasoning {
                Vec::new()
            } else {
                let mut msg = serde_json::json!({
                    "role": "assistant",
                    "content": (!text.is_empty()).then_some(text),
                });
                if include_reasoning {
                    // API 只校验 presence,不读内容 — 见 `REASONING_PLACEHOLDER`。
                    // 回传最小占位符而非完整 thinking 文本,节省 input token。
                    msg["reasoning_content"] = json!(REASONING_PLACEHOLDER);
                }
                // Only include tool_calls when non-empty: some providers reject
                // assistant messages with an explicit empty tool_calls array.
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                vec![msg]
            }
        }
        _ => message
            .content
            .iter()
            .filter_map(|block| match block {
                InputContentBlock::Text { text } => Some(json!({
                    "role": "user",
                    "content": text,
                })),
                InputContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": flatten_tool_result_content(content),
                    "is_error": is_error,
                })),
                InputContentBlock::Thinking { .. } | InputContentBlock::ToolUse { .. } => None,
            })
            .collect(),
    }
}

/// Remove `role:"tool"` messages from `messages` that have no valid paired
/// `role:"assistant"` message with a matching `tool_calls[].id` immediately
/// preceding them. This is a last-resort safety net at the request-building
/// layer — the compaction boundary fix (6e301c8) prevents the most common
/// producer path, but resume, session editing, or future compaction variants
/// could still create orphaned tool messages.
///
/// Algorithm: scan left-to-right. For each `role:"tool"` message, check the
/// immediately preceding non-tool message. If it's `role:"assistant"` with a
/// `tool_calls` array containing an entry whose `id` matches the tool
/// message's `tool_call_id`, the pair is valid and both are kept. Otherwise
/// the tool message is dropped.
/// Remove `role:"tool"` messages from `messages` that have no valid paired
/// `role:"assistant"` message with a matching `tool_calls[].id` immediately
/// preceding them. Public for benchmarking purposes.
pub fn sanitize_tool_message_pairing(messages: Vec<Value>) -> Vec<Value> {
    // Collect indices of tool messages that are orphaned.
    let mut drop_indices = std::collections::HashSet::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        let tool_call_id = msg
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Find the nearest preceding non-tool message.
        let preceding = messages[..i]
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|v| v.as_str()) != Some("tool"));
        // A tool message is considered paired when:
        // (a) the nearest preceding non-tool message is an assistant message
        //     whose `tool_calls` array contains an entry with the matching id, OR
        // (b) there's no clear preceding context (e.g. the message comes right
        //     after a user turn — this can happen with translated mixed-content
        //     user messages). In case (b) we allow the message through rather
        //     than silently dropping potentially valid history.
        let preceding_role = preceding
            .and_then(|m| m.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Only apply sanitization when the preceding message is an assistant
        // turn (the invariant is: assistant-with-tool_calls must precede tool).
        // If the preceding is something else (user, system) don't drop — it
        // may be a valid translation artifact or a path we don't understand.
        if preceding_role != "assistant" {
            continue;
        }
        let paired = preceding
            .and_then(|m| m.get("tool_calls").and_then(|tc| tc.as_array()))
            .is_some_and(|tool_calls| {
                tool_calls
                    .iter()
                    .any(|tc| tc.get("id").and_then(|v| v.as_str()) == Some(tool_call_id))
            });
        if !paired {
            drop_indices.insert(i);
        }
    }
    if drop_indices.is_empty() {
        return messages;
    }
    messages
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !drop_indices.contains(i))
        .map(|(_, m)| m)
        .collect()
}

/// Flattens tool result content blocks into a single string.
/// Optimized to pre-allocate capacity and avoid intermediate `Vec` construction.
#[must_use]
pub fn flatten_tool_result_content(content: &[ToolResultContentBlock]) -> String {
    // Pre-calculate total capacity needed to avoid reallocations
    let total_len: usize = content
        .iter()
        .map(|block| match block {
            ToolResultContentBlock::Text { text } => text.len(),
            ToolResultContentBlock::Json { value } => value.to_string().len(),
        })
        .sum();

    // Add capacity for newlines between blocks
    let capacity = total_len + content.len().saturating_sub(1);

    let mut result = String::with_capacity(capacity);
    for (i, block) in content.iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        match block {
            ToolResultContentBlock::Text { text } => result.push_str(text),
            ToolResultContentBlock::Json { value } => {
                // Use write! to append without creating intermediate String
                result.push_str(&value.to_string());
            }
        }
    }
    result
}

/// Recursively ensure every object-type node in a JSON Schema has
/// `"properties"` (at least `{}`) and `"additionalProperties": false`.
/// The `OpenAI` `/responses` endpoint validates schemas strictly and rejects
/// objects that omit these fields; `/chat/completions` is lenient but also
/// accepts them, so we normalise unconditionally.
fn normalize_object_schema(schema: &mut Value) {
    if let Some(obj) = schema.as_object_mut() {
        if obj.get("type").and_then(Value::as_str) == Some("object") {
            obj.entry("properties").or_insert_with(|| json!({}));
            obj.entry("additionalProperties")
                .or_insert(Value::Bool(false));
        }
        // Recurse into properties values
        if let Some(props) = obj.get_mut("properties") {
            if let Some(props_obj) = props.as_object_mut() {
                let keys: Vec<String> = props_obj.keys().cloned().collect();
                for k in keys {
                    if let Some(v) = props_obj.get_mut(&k) {
                        normalize_object_schema(v);
                    }
                }
            }
        }
        // Recurse into items (arrays)
        if let Some(items) = obj.get_mut("items") {
            normalize_object_schema(items);
        }
    }
}

fn openai_tool_definition(tool: &ToolDefinition) -> Value {
    let mut parameters = tool.input_schema.clone();
    normalize_object_schema(&mut parameters);
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": parameters,
        }
    })
}

fn openai_tool_choice(tool_choice: &ToolChoice) -> Value {
    match tool_choice {
        ToolChoice::Auto => Value::String("auto".to_string()),
        ToolChoice::Any => Value::String("required".to_string()),
        ToolChoice::Tool { name } => json!({
            "type": "function",
            "function": { "name": name },
        }),
    }
}

fn normalize_response(
    model: &str,
    response: ChatCompletionResponse,
) -> Result<MessageResponse, ApiError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(ApiError::InvalidSseFrame(
            "chat completion response missing choices",
        ))?;
    let mut content = Vec::new();
    if let Some(thinking) = choice
        .message
        .reasoning_content
        .filter(|value| !value.is_empty())
    {
        content.push(OutputContentBlock::Thinking {
            thinking,
            signature: None,
        });
    }
    if let Some(text) = choice.message.content.filter(|value| !value.is_empty()) {
        content.push(OutputContentBlock::Text { text });
    }
    for tool_call in choice.message.tool_calls {
        content.push(OutputContentBlock::ToolUse {
            id: tool_call.id,
            name: tool_call.function.name,
            input: parse_tool_arguments(&tool_call.function.arguments),
        });
    }

    // Extract response model before move for usage normalization
    let response_model = if response.model.is_empty() {
        model.to_string()
    } else {
        response.model.clone()
    };
    Ok(MessageResponse {
        id: response.id,
        kind: "message".to_string(),
        role: choice.message.role,
        content,
        model: response.model.if_empty_then(model.to_string()),
        stop_reason: choice
            .finish_reason
            .map(|value| normalize_finish_reason(&value)),
        stop_sequence: None,
        usage: response.usage.as_ref().map_or_else(Usage::default, |u| {
            u.normalized(Some(&response_model), None)
        }),
        request_id: None,
    })
}

fn parse_tool_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "raw": arguments }))
}

fn next_sse_frame(buffer: &mut Vec<u8>) -> Option<String> {
    let separator = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2))
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| (position, 4))
        })?;

    let (position, separator_len) = separator;
    let frame = buffer.drain(..position + separator_len).collect::<Vec<_>>();
    let frame_len = frame.len().saturating_sub(separator_len);
    Some(String::from_utf8_lossy(&frame[..frame_len]).into_owned())
}

fn parse_sse_frame(
    frame: &str,
    provider: &str,
    model: &str,
) -> Result<Option<ChatCompletionChunk>, ApiError> {
    let trimmed = frame.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut data_lines = Vec::new();
    for line in trimmed.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }
    if data_lines.is_empty() {
        return Ok(None);
    }
    let payload = data_lines.join("\n");
    if payload == "[DONE]" {
        return Ok(None);
    }
    // Some backends embed an error object in a data: frame instead of using an
    // HTTP error status. Surface the error message directly rather than letting
    // ChatCompletionChunk deserialization fail with a cryptic 'missing field' error.
    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&payload) {
        if let Some(err_obj) = raw.get("error") {
            let msg = err_obj
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("provider returned an error in stream")
                .to_string();
            let code = err_obj
                .get("code")
                .and_then(serde_json::Value::as_u64)
                .map(|c| c as u16);
            let status = reqwest::StatusCode::from_u16(code.unwrap_or(400))
                .unwrap_or(reqwest::StatusCode::BAD_REQUEST);
            return Err(ApiError::Api {
                status,
                error_type: err_obj
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(str::to_owned),
                message: Some(msg),
                request_id: None,
                body: payload.clone(),
                retryable: false,
                suggested_action: suggested_action_for_status(status),
                retry_after: None,
            });
        }
    }
    serde_json::from_str::<ChatCompletionChunk>(&payload)
        .map(Some)
        .map_err(|error| ApiError::json_deserialize(provider, model, &payload, error))
}

fn read_env_non_empty(key: &str) -> Result<Option<String>, ApiError> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(super::dotenv_value(key)),
        Err(error) => Err(ApiError::from(error)),
    }
}

#[must_use]
pub fn has_api_key(key: &str) -> bool {
    read_env_non_empty(key)
        .ok()
        .and_then(std::convert::identity)
        .is_some()
}

#[must_use]
pub fn read_base_url(config: OpenAiCompatConfig) -> String {
    std::env::var(config.base_url_env).unwrap_or_else(|_| config.default_base_url.to_string())
}

fn chat_completions_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

fn request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_ID_HEADER)
        .or_else(|| headers.get(ALT_REQUEST_ID_HEADER))
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// Parse the `Retry-After` header into a `Duration`. Only the delta-seconds
/// form is supported (the form OpenAI/Anthropic emit). Negative, non-numeric,
/// or overflowing values are ignored so the caller falls back to exponential
/// backoff. Values are clamped to one hour to prevent a misbehaving gateway
/// from stalling the retry loop indefinitely.
fn retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let header_value = headers.get(reqwest::header::RETRY_AFTER)?;
    let text = header_value.to_str().ok()?;
    let trimmed = text.trim();
    // Reject negative-looking values (e.g. "-1") before parsing — u64::parse
    // would also reject them, but checking explicitly makes the intent obvious
    // and survives any future relaxation of the numeric type.
    if trimmed.starts_with('-') {
        return None;
    }
    // Clamp absurdly large values to one hour so a misbehaving gateway
    // cannot stall the retry loop indefinitely.
    let seconds: u64 = trimmed.parse().ok()?;
    Some(Duration::from_secs(seconds.min(3_600)))
}

async fn expect_success(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    // Read headers before `text()` consumes the response so we can honour the
    // server's `Retry-After` advisory during the retry loop.
    let headers = response.headers().clone();
    let request_id = request_id_from_headers(&headers);
    let retry_after = retry_after_from_headers(&headers);
    let body = response.text().await.unwrap_or_default();
    let parsed_error = serde_json::from_str::<ErrorEnvelope>(&body).ok();
    let retryable = is_retryable_status(status) || is_retryable_400(status, &body);

    let suggested_action = suggested_action_for_status(status);

    Err(ApiError::Api {
        status,
        error_type: parsed_error
            .as_ref()
            .and_then(|error| error.error.error_type.clone()),
        message: parsed_error
            .as_ref()
            .and_then(|error| error.error.message.clone()),
        request_id,
        body,
        retryable,
        suggested_action,
        retry_after,
    })
}

const fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504)
}

/// Some providers return HTTP 400 with an unparseable body when a gateway
/// or proxy flakes (e.g. "HTTP 400 from backend (no parseable body)").
/// These are transient network blips, not actual bad requests, and should
/// be retried. We detect them by checking the body for known gateway error
/// phrases.
fn is_retryable_400(status: reqwest::StatusCode, body: &str) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    lowered.contains("no parseable body")
        || lowered.contains("connection reset")
        || lowered.contains("broken pipe")
        || lowered.contains("empty reply from server")
}

/// Generate a suggested user action based on the HTTP status code and error context.
/// This provides actionable guidance when API requests fail.
fn suggested_action_for_status(status: reqwest::StatusCode) -> Option<String> {
    match status.as_u16() {
        401 => Some("Check API key is set correctly and has not expired".to_string()),
        403 => Some("Verify API key has required permissions for this operation".to_string()),
        413 => Some("Reduce prompt size or context window before retrying".to_string()),
        429 => Some("Wait a moment before retrying; consider reducing request rate".to_string()),
        500 => Some("Provider server error - retry after a brief wait".to_string()),
        502..=504 => Some("Provider gateway error - retry after a brief wait".to_string()),
        _ => None,
    }
}

fn normalize_finish_reason(value: &str) -> String {
    match value {
        "stop" => "end_turn",
        "tool_calls" => "tool_use",
        other => other,
    }
    .to_string()
}

trait StringExt {
    fn if_empty_then(self, fallback: String) -> String;
}

impl StringExt for String {
    fn if_empty_then(self, fallback: String) -> String {
        if self.is_empty() {
            fallback
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_chat_completion_request, chat_completions_endpoint, has_thinking_mode_tool_call,
        is_reasoning_model, model_requires_reasoning_content_in_history, normalize_finish_reason,
        normalize_response, openai_tool_choice, parse_tool_arguments, OpenAiCompatClient,
        OpenAiCompatConfig, StreamState, REASONING_PLACEHOLDER,
    };
    use crate::error::ApiError;
    use crate::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
        InputContentBlock, InputMessage, MessageRequest, OutputContentBlock, StreamEvent,
        SystemContent, ToolChoice, ToolDefinition, ToolResultContentBlock,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn request_translation_uses_openai_compatible_shape() {
        let payload = build_chat_completion_request(
            &MessageRequest {
                model: "deepseek-v4-pro".to_string(),
                max_tokens: 64,
                messages: vec![InputMessage {
                    role: "user".to_string(),
                    content: vec![
                        InputContentBlock::Text {
                            text: "hello".to_string(),
                        },
                        InputContentBlock::ToolResult {
                            tool_use_id: "tool_1".to_string(),
                            content: vec![ToolResultContentBlock::Json {
                                value: json!({"ok": true}),
                            }],
                            is_error: false,
                        },
                    ],
                }],
                system: Some(SystemContent::from_text("be helpful")),
                tools: Some(vec![ToolDefinition {
                    name: "weather".to_string(),
                    description: Some("Get weather".to_string()),
                    input_schema: json!({"type": "object"}),
                    cache_control: None,
                }]),
                tool_choice: Some(ToolChoice::Auto),
                stream: false,
                ..Default::default()
            },
            OpenAiCompatConfig::deepseek(),
        );

        assert_eq!(payload["messages"][0]["role"], json!("system"));
        assert_eq!(payload["messages"][1]["role"], json!("user"));
        assert_eq!(payload["messages"][2]["role"], json!("tool"));
        assert_eq!(payload["tools"][0]["type"], json!("function"));
        assert_eq!(payload["tool_choice"], json!("auto"));
    }

    #[test]
    fn model_requires_reasoning_content_in_history_is_false_for_all_models() {
        // DeepSeek V4 no longer requires reasoning_content echo-back (verified
        // empirically 2026-08-15); thinking is stripped to save context tokens.
        let models = [
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "deepseek/deepseek-v4-pro",
            "deepseek/deepseek-v4-flash",
            "deepseek-reasoner",
            "deepseek-chat",
            "unknown-model",
        ];

        for model in models {
            assert!(!model_requires_reasoning_content_in_history(model));
        }
    }

    #[test]
    fn legacy_deepseek_reasoner_request_omits_reasoning_content_for_assistant_history() {
        // Given an assistant history turn containing thinking.
        let request = assistant_history_with_thinking_request("deepseek-reasoner");

        // When serializing for legacy deepseek-reasoner.
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());

        // Then reasoning_content is omitted.
        let assistant = &payload["messages"][0];
        assert_eq!(assistant["role"], json!("assistant"));
        assert!(assistant.get("reasoning_content").is_none());
    }

    #[test]
    fn deepseek_v4_pro_request_omits_reasoning_content_for_assistant_history() {
        // Given an assistant history turn containing thinking.
        let request = assistant_history_with_thinking_request("openai/deepseek-v4-pro");

        // When serializing for DeepSeek V4 Pro.
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());

        // Then reasoning_content is omitted (thinking stripped to save tokens).
        let assistant = &payload["messages"][0];
        assert!(assistant.get("reasoning_content").is_none());
        assert_eq!(assistant["content"], json!("answer"));
    }

    #[test]
    fn deepseek_v4_flash_request_omits_reasoning_content_for_assistant_history() {
        // Given an assistant history turn containing thinking.
        let request = assistant_history_with_thinking_request("deepseek-v4-flash");

        // When serializing for DeepSeek V4 Flash.
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());

        // Then reasoning_content is omitted.
        let assistant = &payload["messages"][0];
        assert!(assistant.get("reasoning_content").is_none());
    }

    #[test]
    fn thinking_mode_tool_call_requires_reasoning_content_passback() {
        // call_01_* 前缀(DeepSeek thinking 模式)必须回传 reasoning_content。
        // Given an assistant turn with a call_01_* tool call + thinking.
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage {
                role: "assistant".to_string(),
                content: vec![
                    InputContentBlock::Thinking {
                        thinking: "prior reasoning".to_string(),
                        signature: None,
                    },
                    InputContentBlock::ToolUse {
                        id: "call_01_SeY7wrVwpFOzzZR2vM2c9683".to_string(),
                        name: "grep_search".to_string(),
                        input: json!({"pattern": "MACD"}),
                    },
                ],
            }],
            stream: false,
            ..Default::default()
        };

        // When serializing for the DeepSeek endpoint.
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());

        // Then reasoning_content must be present (else the API returns 400).
        // API 只校验 presence 不读内容,故回传占位符即可。
        let assistant = &payload["messages"][0];
        assert_eq!(assistant["reasoning_content"], json!(REASONING_PLACEHOLDER));
        assert_eq!(
            assistant["tool_calls"][0]["id"],
            json!("call_01_SeY7wrVwpFOzzZR2vM2c9683")
        );
    }

    #[test]
    fn thinking_tool_call_passback_survives_stripped_thinking() {
        // 回归(2026-08-29,线上复现):会话压缩/微压缩剥离 thinking block 后,
        // assistant 消息只剩 call_01_* tool call。旧实现要求 has_reasoning=true
        // 才回传 reasoning_content,此时 has_reasoning=false → 不回传 → DeepSeek
        // 400 ("reasoning_content ... must be passed back to the API")。
        // call_01_* 前缀本身即 thinking 模式强信号,占位符必须无条件回传。
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage {
                role: "assistant".to_string(),
                content: vec![
                    // 无 Thinking block —— 压缩后被剥离的真实形态
                    InputContentBlock::ToolUse {
                        id: "call_01_SrippedThinkingToolCall0001".to_string(),
                        name: "notebook_update".to_string(),
                        input: json!({"section": "attempted"}),
                    },
                ],
            }],
            stream: false,
            ..Default::default()
        };

        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());

        let assistant = &payload["messages"][0];
        assert_eq!(
            assistant["reasoning_content"],
            json!(REASONING_PLACEHOLDER),
            "thinking 被剥离后 call_01_* tool call 仍必须回传占位符"
        );
        assert_eq!(
            assistant["tool_calls"][0]["id"],
            json!("call_01_SrippedThinkingToolCall0001")
        );
    }

    #[test]
    fn call_02_tool_call_passback_reasoning_content() {
        // 回归(2026-08-30,线上复现 session-1788025620097):deepseek-v4 在
        // thinking 模式生成 `call_02_*` tool call id(此处以真实 id
        // call_02_vFVFAg79gxanJZdXeWHb5001 复现)。旧实现 has_thinking_mode_
        // tool_call 只认 call_01_* 前缀 → include_reasoning=false →
        // reasoning_content 不回传 → API 400。放宽到 call_0N_(N ≥ 1)后
        // 必须回传占位符。
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage {
                role: "assistant".to_string(),
                content: vec![InputContentBlock::ToolUse {
                    id: "call_02_vFVFAg79gxanJZdXeWHb5001".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command": "ls"}),
                }],
            }],
            stream: false,
            ..Default::default()
        };

        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());

        let assistant = &payload["messages"][0];
        assert_eq!(
            assistant["reasoning_content"],
            json!(REASONING_PLACEHOLDER),
            "call_02_* tool call 必须回传占位符(call_0N_ N≥1 均为 thinking 模式)"
        );
        assert_eq!(
            assistant["tool_calls"][0]["id"],
            json!("call_02_vFVFAg79gxanJZdXeWHb5001")
        );
    }

    #[test]
    fn non_thinking_tool_call_omits_reasoning_content() {
        // call_00_* 前缀(非 thinking 模式)不需要回传 reasoning_content。
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage {
                role: "assistant".to_string(),
                content: vec![
                    InputContentBlock::Thinking {
                        thinking: "prior reasoning".to_string(),
                        signature: None,
                    },
                    InputContentBlock::ToolUse {
                        id: "call_00_JvIHk6LO4kk0M9XZHAYR0592".to_string(),
                        name: "grep_search".to_string(),
                        input: json!({"pattern": "fn main"}),
                    },
                ],
            }],
            stream: false,
            ..Default::default()
        };

        // When serializing for the DeepSeek endpoint.
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());

        // Then reasoning_content is omitted (stripped to save context tokens).
        let assistant = &payload["messages"][0];
        assert!(assistant.get("reasoning_content").is_none());
        assert_eq!(
            assistant["tool_calls"][0]["id"],
            json!("call_00_JvIHk6LO4kk0M9XZHAYR0592")
        );
    }

    #[test]
    fn has_thinking_mode_tool_call_detects_call_0n_prefix() {
        assert!(has_thinking_mode_tool_call(&[
            "call_01_SeY7wrVwpFOzzZR2vM2c9683"
        ]));
        assert!(has_thinking_mode_tool_call(&[
            "call_02_vFVFAg79gxanJZdXeWHb5001"
        ]));
        assert!(has_thinking_mode_tool_call(&["call_09_future"]));
        assert!(!has_thinking_mode_tool_call(&[
            "call_00_JvIHk6LO4kk0M9XZHAYR0592"
        ]));
        assert!(!has_thinking_mode_tool_call(&["call_0_short"]));
        assert!(!has_thinking_mode_tool_call(std::iter::empty::<&str>()));
        assert!(!has_thinking_mode_tool_call(&["toolu_01_abc"]));
    }

    #[test]
    fn non_streaming_response_with_reasoning_content_emits_thinking_block_first() {
        // Given a non-streaming OpenAI-compatible response with reasoning_content.
        let response = super::ChatCompletionResponse {
            id: "chatcmpl_reasoning".to_string(),
            model: "deepseek-v4-pro".to_string(),
            choices: vec![super::ChatChoice {
                message: super::ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("final answer".to_string()),
                    reasoning_content: Some("hidden thought".to_string()),
                    tool_calls: Vec::new(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };

        // When normalizing the provider response.
        let normalized = normalize_response("deepseek-v4-pro", response).expect("normalized");

        // Then Thinking is the first content block, before text.
        assert_eq!(
            normalized.content,
            vec![
                OutputContentBlock::Thinking {
                    thinking: "hidden thought".to_string(),
                    signature: None,
                },
                OutputContentBlock::Text {
                    text: "final answer".to_string(),
                },
            ]
        );
    }

    #[test]
    fn streaming_chunks_with_reasoning_content_emit_thinking_block_events_before_text() {
        // Given streaming chunks with reasoning_content followed by text.
        let mut state = StreamState::new(
            "deepseek-v4-pro".to_string(),
            "https://api.deepseek.com/v1".to_string(),
        );
        let mut events = state
            .ingest_chunk(super::ChatCompletionChunk {
                id: "chatcmpl_stream_reasoning".to_string(),
                model: Some("deepseek-v4-pro".to_string()),
                choices: vec![super::ChunkChoice {
                    delta: super::ChunkDelta {
                        content: None,
                        reasoning_content: Some("think".to_string()),
                        tool_calls: Vec::new(),
                    },
                    finish_reason: None,
                }],
                usage: None,
            })
            .expect("reasoning chunk");
        events.extend(
            state
                .ingest_chunk(super::ChatCompletionChunk {
                    id: "chatcmpl_stream_reasoning".to_string(),
                    model: None,
                    choices: vec![super::ChunkChoice {
                        delta: super::ChunkDelta {
                            content: Some(" answer".to_string()),
                            reasoning_content: None,
                            tool_calls: Vec::new(),
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: None,
                })
                .expect("text chunk"),
        );
        events.extend(state.finish().expect("finish"));

        // When reading normalized stream events.
        // Then Thinking starts at index 0, text is offset to index 1.
        assert!(matches!(events[0], StreamEvent::MessageStart(_)));
        assert!(matches!(
            events[1],
            StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                index: 0,
                content_block: OutputContentBlock::Thinking { .. },
            })
        ));
        assert!(matches!(
            events[2],
            StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                index: 0,
                delta: ContentBlockDelta::ThinkingDelta { .. },
            })
        ));
        assert!(matches!(
            events[3],
            StreamEvent::ContentBlockStop(ContentBlockStopEvent { index: 0 })
        ));
        assert!(matches!(
            events[4],
            StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                index: 1,
                content_block: OutputContentBlock::Text { .. },
            })
        ));
        assert!(matches!(
            events[5],
            StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                index: 1,
                delta: ContentBlockDelta::TextDelta { .. },
            })
        ));
        assert!(matches!(
            events[6],
            StreamEvent::ContentBlockStop(ContentBlockStopEvent { index: 1 })
        ));
    }

    #[test]
    fn tool_schema_object_gets_strict_fields_for_responses_endpoint() {
        // OpenAI /responses endpoint rejects object schemas missing
        // "properties" and "additionalProperties". Verify normalize_object_schema
        // fills them in so the request shape is strict-validator-safe.
        use super::normalize_object_schema;

        // Bare object — no properties at all
        let mut schema = json!({"type": "object"});
        normalize_object_schema(&mut schema);
        assert_eq!(schema["properties"], json!({}));
        assert_eq!(schema["additionalProperties"], json!(false));

        // Nested object inside properties
        let mut schema2 = json!({
            "type": "object",
            "properties": {
                "location": {"type": "object", "properties": {"lat": {"type": "number"}}}
            }
        });
        normalize_object_schema(&mut schema2);
        assert_eq!(schema2["additionalProperties"], json!(false));
        assert_eq!(
            schema2["properties"]["location"]["additionalProperties"],
            json!(false)
        );

        // Existing properties/additionalProperties should not be overwritten
        let mut schema3 = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "additionalProperties": true
        });
        normalize_object_schema(&mut schema3);
        assert_eq!(
            schema3["additionalProperties"],
            json!(true),
            "must not overwrite existing"
        );
    }

    #[test]
    fn reasoning_effort_is_included_when_set() {
        let payload = build_chat_completion_request(
            &MessageRequest {
                model: "deepseek-v4-pro".to_string(),
                max_tokens: 1024,
                messages: vec![InputMessage::user_text("think hard")],
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            },
            OpenAiCompatConfig::deepseek(),
        );
        assert_eq!(payload["reasoning_effort"], json!("high"));
    }

    #[test]
    fn reasoning_effort_omitted_when_not_set() {
        let payload = build_chat_completion_request(
            &MessageRequest {
                model: "deepseek-v4-flash".to_string(),
                max_tokens: 64,
                messages: vec![InputMessage::user_text("hello")],
                ..Default::default()
            },
            OpenAiCompatConfig::deepseek(),
        );
        assert!(payload.get("reasoning_effort").is_none());
    }

    #[test]
    fn openai_streaming_requests_include_usage_opt_in() {
        let payload = build_chat_completion_request(
            &MessageRequest {
                model: "deepseek-v4-pro".to_string(),
                max_tokens: 64,
                messages: vec![InputMessage::user_text("hello")],
                system: None,
                tools: None,
                tool_choice: None,
                stream: true,
                ..Default::default()
            },
            OpenAiCompatConfig::deepseek(),
        );

        assert_eq!(payload["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn tool_choice_translation_supports_required_function() {
        assert_eq!(openai_tool_choice(&ToolChoice::Any), json!("required"));
        assert_eq!(
            openai_tool_choice(&ToolChoice::Tool {
                name: "weather".to_string(),
            }),
            json!({"type": "function", "function": {"name": "weather"}})
        );
    }

    #[test]
    fn parses_tool_arguments_fallback() {
        assert_eq!(
            parse_tool_arguments("{\"city\":\"Paris\"}"),
            json!({"city": "Paris"})
        );
        assert_eq!(parse_tool_arguments("not-json"), json!({"raw": "not-json"}));
    }

    #[test]
    fn missing_deepseek_api_key_is_detected() {
        let _lock = env_lock();
        std::env::remove_var("DEEPSEEK_API_KEY");
        let error = OpenAiCompatClient::from_env(OpenAiCompatConfig::deepseek())
            .expect_err("missing key should error");
        assert!(matches!(
            error,
            ApiError::MissingCredentials {
                provider: "DeepSeek",
                ..
            }
        ));
    }

    #[test]
    fn endpoint_builder_accepts_base_urls_and_full_endpoints() {
        assert_eq!(
            chat_completions_endpoint("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://api.deepseek.com/v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    fn assistant_history_with_thinking_request(model: &str) -> MessageRequest {
        MessageRequest {
            model: model.to_string(),
            max_tokens: 100,
            messages: vec![InputMessage {
                role: "assistant".to_string(),
                content: vec![
                    InputContentBlock::Thinking {
                        thinking: "prior reasoning".to_string(),
                        signature: None,
                    },
                    InputContentBlock::Text {
                        text: "answer".to_string(),
                    },
                ],
            }],
            stream: false,
            ..Default::default()
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock")
    }

    #[test]
    fn normalizes_stop_reasons() {
        assert_eq!(normalize_finish_reason("stop"), "end_turn");
        assert_eq!(normalize_finish_reason("tool_calls"), "tool_use");
    }

    #[test]
    fn tuning_params_included_in_payload_when_set() {
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 1024,
            messages: vec![],
            system: None,
            tools: None,
            tool_choice: None,
            stream: false,
            temperature: Some(0.7),
            top_p: Some(0.9),
            frequency_penalty: Some(0.5),
            presence_penalty: Some(0.3),
            stop: Some(vec!["\n".to_string()]),
            reasoning_effort: None,
            extra_body: BTreeMap::new(),
        };
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());
        assert_eq!(payload["temperature"], 0.7);
        assert_eq!(payload["top_p"], 0.9);
        assert_eq!(payload["frequency_penalty"], 0.5);
        assert_eq!(payload["presence_penalty"], 0.3);
        assert_eq!(payload["stop"], json!(["\n"]));
    }

    #[test]
    fn extra_body_params_are_passed_through_without_overriding_core_fields() {
        let mut extra_body = BTreeMap::new();
        extra_body.insert(
            "web_search_options".to_string(),
            json!({"search_context_size": "medium"}),
        );
        extra_body.insert("parallel_tool_calls".to_string(), json!(false));
        extra_body.insert("model".to_string(), json!("bad-override"));
        extra_body.insert("messages".to_string(), json!([]));
        extra_body.insert("max_tokens".to_string(), json!(1));

        let payload = build_chat_completion_request(
            &MessageRequest {
                model: "deepseek-v4-flash".to_string(),
                max_tokens: 1024,
                messages: vec![InputMessage::user_text("hello")],
                extra_body,
                ..Default::default()
            },
            OpenAiCompatConfig::deepseek(),
        );

        assert_eq!(payload["model"], json!("deepseek-v4-flash"));
        assert_eq!(payload["max_tokens"], json!(1024));
        assert_eq!(payload["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            payload["web_search_options"],
            json!({"search_context_size": "medium"})
        );
        assert_eq!(payload["parallel_tool_calls"], json!(false));
    }

    #[test]
    fn is_reasoning_model_always_returns_false_for_deepseek() {
        // DeepSeek does not use fixed-sampling reasoning models; reasoning is
        // controlled via reasoning_effort and reasoning_content, not by
        // stripping tuning parameters.
        assert!(!is_reasoning_model("deepseek-v4-pro"));
        assert!(!is_reasoning_model("deepseek-v4-flash"));
        assert!(!is_reasoning_model("deepseek-chat"));
        assert!(!is_reasoning_model("deepseek-reasoner"));
    }

    #[test]
    fn tuning_params_omitted_from_payload_when_none() {
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            ..Default::default()
        };
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());
        assert!(
            payload.get("temperature").is_none(),
            "temperature should be absent"
        );
        assert!(payload.get("top_p").is_none(), "top_p should be absent");
        assert!(payload.get("frequency_penalty").is_none());
        assert!(payload.get("presence_penalty").is_none());
        assert!(payload.get("stop").is_none());
    }

    /// Regression test: some OpenAI-compatible providers emit `"tool_calls": null`
    /// in stream delta chunks instead of omitting the field or using `[]`.
    /// Before the fix this produced: `invalid type: null, expected a sequence`.
    #[test]
    fn delta_with_null_tool_calls_deserializes_as_empty_vec() {
        use super::deserialize_null_as_empty_vec;

        #[allow(dead_code)]
        #[derive(serde::Deserialize, Debug)]
        struct Delta {
            content: Option<String>,
            #[serde(default, deserialize_with = "deserialize_null_as_empty_vec")]
            tool_calls: Vec<super::DeltaToolCall>,
        }

        // Simulate the exact shape observed in the wild (internal repro 2026-04-09)
        let json = r#"{
            "content": "",
            "function_call": null,
            "refusal": null,
            "role": "assistant",
            "tool_calls": null
        }"#;
        let delta: Delta = serde_json::from_str(json)
            .expect("delta with tool_calls:null must deserialize without error");
        assert!(
            delta.tool_calls.is_empty(),
            "tool_calls:null must produce an empty vec, not an error"
        );
    }

    /// Regression: when building a multi-turn request where a prior assistant
    /// turn has no tool calls, the serialized assistant message must NOT include
    /// `tool_calls: []`. Some providers reject requests that carry an empty
    /// `tool_calls` array on assistant turns (internal repro 2026-04-09).
    #[test]
    fn assistant_message_without_tool_calls_omits_tool_calls_field() {
        use crate::types::{InputContentBlock, InputMessage};

        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage {
                role: "assistant".to_string(),
                content: vec![InputContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            }],
            stream: false,
            ..Default::default()
        };
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());
        let messages = payload["messages"].as_array().unwrap();
        let assistant_msg = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message must be present");
        assert!(
            assistant_msg.get("tool_calls").is_none(),
            "assistant message without tool calls must omit tool_calls field: {assistant_msg:?}"
        );
    }

    /// Regression: assistant messages WITH tool calls must still include
    /// the `tool_calls` array (normal multi-turn tool-use flow).
    #[test]
    fn assistant_message_with_tool_calls_includes_tool_calls_field() {
        use crate::types::{InputContentBlock, InputMessage};

        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage {
                role: "assistant".to_string(),
                content: vec![InputContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "/tmp/test"}),
                }],
            }],
            stream: false,
            ..Default::default()
        };
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());
        let messages = payload["messages"].as_array().unwrap();
        let assistant_msg = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message must be present");
        let tool_calls = assistant_msg
            .get("tool_calls")
            .expect("assistant message with tool calls must include tool_calls field");
        assert!(tool_calls.is_array());
        assert_eq!(tool_calls.as_array().unwrap().len(), 1);
    }

    /// Orphaned tool messages (no preceding assistant `tool_calls`) must be
    /// dropped by the request-builder sanitizer. Regression for the second
    /// layer of the tool-pairing invariant fix (internal repro 2026-04-10).
    #[test]
    fn sanitize_drops_orphaned_tool_messages() {
        use super::sanitize_tool_message_pairing;

        // Valid pair: assistant with tool_calls → tool result
        let valid = vec![
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "search", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "result"}),
        ];
        let out = sanitize_tool_message_pairing(valid);
        assert_eq!(out.len(), 2, "valid pair must be preserved");

        // Orphaned tool message: no preceding assistant tool_calls
        let orphaned = vec![
            json!({"role": "assistant", "content": "hi"}),
            json!({"role": "tool", "tool_call_id": "call_2", "content": "orphaned"}),
        ];
        let out = sanitize_tool_message_pairing(orphaned);
        assert_eq!(out.len(), 1, "orphaned tool message must be dropped");
        assert_eq!(out[0]["role"], json!("assistant"));

        // Mismatched tool_call_id
        let mismatched = vec![
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_3", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_WRONG", "content": "bad"}),
        ];
        let out = sanitize_tool_message_pairing(mismatched);
        assert_eq!(out.len(), 1, "tool message with wrong id must be dropped");

        // Two tool results both valid (same preceding assistant)
        let two_results = vec![
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_a", "type": "function", "function": {"name": "fa", "arguments": "{}"}},
                {"id": "call_b", "type": "function", "function": {"name": "fb", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "call_a", "content": "ra"}),
            json!({"role": "tool", "tool_call_id": "call_b", "content": "rb"}),
        ];
        let out = sanitize_tool_message_pairing(two_results);
        assert_eq!(out.len(), 3, "both valid tool results must be preserved");
    }

    #[test]
    fn deepseek_uses_max_tokens() {
        // DeepSeek models use `max_tokens`.
        let request = MessageRequest {
            model: "deepseek-v4-pro".to_string(),
            max_tokens: 512,
            messages: vec![],
            stream: false,
            ..Default::default()
        };
        let payload = build_chat_completion_request(&request, OpenAiCompatConfig::deepseek());
        assert_eq!(payload["max_tokens"], json!(512));
        assert!(
            payload.get("max_completion_tokens").is_none(),
            "deepseek-v4-pro must not emit max_completion_tokens"
        );
    }

    // ============================================================================
    // US-021: Request body size pre-flight check tests
    // ============================================================================

    #[test]
    fn estimate_request_body_size_returns_reasonable_estimate() {
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage::user_text("Hello world".to_string())],
            stream: false,
            ..Default::default()
        };

        let size = super::estimate_request_body_size(&request, OpenAiCompatConfig::deepseek());
        // Should be non-zero and reasonable for a small request
        assert!(size > 0, "estimated size should be positive");
        assert!(size < 10_000, "small request should be under 10KB");
    }

    #[test]
    fn check_request_body_size_passes_for_small_requests() {
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 100,
            messages: vec![InputMessage::user_text("Hello".to_string())],
            stream: false,
            ..Default::default()
        };

        // Should pass for DeepSeek with a small request
        assert!(super::check_request_body_size(&request, OpenAiCompatConfig::deepseek()).is_ok());
    }

    #[test]
    fn deepseek_size_limit_is_100mb() {
        assert_eq!(
            OpenAiCompatConfig::deepseek().max_request_body_bytes,
            104_857_600
        ); // 100MB
    }

    #[test]
    fn strip_routing_prefix_strips_deepseek_provider_prefix() {
        assert_eq!(
            super::strip_routing_prefix("deepseek/deepseek-v4-pro"),
            "deepseek-v4-pro"
        );
        assert_eq!(
            super::strip_routing_prefix("deepseek-v4-pro"),
            "deepseek-v4-pro"
        ); // no prefix, unchanged
    }
}
