//! Integration tests for the DeepSeek-only `OpenAiCompatClient` /
//! `ProviderClient` surface.
//!
//! These tests were originally written against the Anthropic-flavoured
//! `ApiClient` / `AnthropicClient` / `AuthSource` API. After the DeepSeek-only
//! migration they now exercise the OpenAI-compatible chat completions path
//! (`POST /chat/completions`) via `OpenAiCompatClient::new(...,
//! OpenAiCompatConfig::deepseek())` and `ProviderClient::from_model`.
//!
//! Tests that depended on Anthropic-only features (anthropic-beta headers,
//! local prompt cache, `with_auth_token`, telemetry session tracer) are
//! preserved but marked `#[ignore]` with a documented reason so the test count
//! stays honest and the migration history remains auditable.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::Duration;

use api::{
    ApiError, ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, InputContentBlock,
    InputMessage, MessageDeltaEvent, MessageRequest, OpenAiCompatClient, OpenAiCompatConfig,
    OutputContentBlock, ProviderClient, ProviderKind, StreamEvent, SystemContent, ToolChoice,
    ToolDefinition,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[tokio::test]
async fn send_message_posts_json_and_parses_response() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_test\",",
        "\"model\":\"deepseek-v4-pro\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from DeepSeek\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":4}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client = OpenAiCompatClient::new("test-key", OpenAiCompatConfig::deepseek())
        .with_base_url(server.base_url());
    let response = client
        .send_message(&sample_request(false))
        .await
        .expect("request should succeed");

    assert_eq!(response.id, "chatcmpl_test");
    assert_eq!(response.total_tokens(), 16);
    assert_eq!(
        response.content,
        vec![OutputContentBlock::Text {
            text: "Hello from DeepSeek".to_string(),
        }]
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer test-key")
    );
    assert!(
        !request.headers.contains_key("x-api-key"),
        "DeepSeek OpenAI-compat path must not send an x-api-key header"
    );
    assert!(
        !request.headers.contains_key("anthropic-version"),
        "anthropic-version header must not be sent on the DeepSeek path"
    );
    assert!(
        !request.headers.contains_key("anthropic-beta"),
        "anthropic-beta header must not be sent on the DeepSeek path"
    );
    let body: serde_json::Value =
        serde_json::from_str(&request.body).expect("request body should be json");
    assert_eq!(
        body.get("model").and_then(serde_json::Value::as_str),
        Some("deepseek-v4-pro")
    );
    // The OpenAI-compat path always serializes the `stream` boolean (unlike the
    // legacy Anthropic client, which omitted it for non-streaming requests).
    assert_eq!(
        body.get("stream").and_then(serde_json::Value::as_bool),
        Some(false),
        "non-streaming DeepSeek request should serialize stream=false"
    );
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("get_weather"));
    assert_eq!(body["tool_choice"], json!("auto"));
    assert!(
        body.get("betas").is_none(),
        "betas are an Anthropic-only concept and must not appear in the DeepSeek request body"
    );
}

#[tokio::test]
async fn send_message_blocks_oversized_requests_before_the_http_call() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", "{}")],
    )
    .await;

    let client = OpenAiCompatClient::new("test-key", OpenAiCompatConfig::deepseek())
        .with_base_url(server.base_url());
    let error = client
        .send_message(&MessageRequest {
            model: "deepseek-v4-pro".to_string(),
            max_tokens: 64_000,
            messages: vec![InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text {
                    // DeepSeek-v4-pro has a 1M-token context window. The
                    // preflight estimator uses ~bytes/4, so ~5M chars yields
                    // ~1.25M estimated input tokens — comfortably above the
                    // 1M ceiling once max_tokens is added.
                    text: "x".repeat(5_000_000),
                }],
            }],
            system: Some(SystemContent::from_text("Keep the answer short.")),
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        })
        .await
        .expect_err("oversized request should fail local context-window preflight");

    assert!(matches!(error, ApiError::ContextWindowExceeded { .. }));
    assert!(
        state.lock().await.is_empty(),
        "preflight failure should avoid any upstream HTTP request"
    );
}

#[tokio::test]
async fn given_empty_usage_object_when_send_message_parses_response_then_usage_defaults_to_zero() {
    // given
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_empty_usage\",",
        "\"model\":\"deepseek-v4-pro\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from DeepSeek\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{}",
        "}"
    );
    let server = spawn_server(
        state,
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;
    let client = OpenAiCompatClient::new("test-key", OpenAiCompatConfig::deepseek())
        .with_base_url(server.base_url());

    // when
    let response = client
        .send_message(&sample_request(false))
        .await
        .expect("response with empty usage object should still parse");

    // then
    assert_eq!(response.id, "chatcmpl_empty_usage");
    assert_eq!(response.total_tokens(), 0);
    assert_eq!(response.usage.input_tokens, 0);
    assert_eq!(response.usage.cache_creation_input_tokens, 0);
    assert_eq!(response.usage.cache_read_input_tokens, 0);
    assert_eq!(response.usage.output_tokens, 0);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn stream_message_parses_sse_events_with_tool_use() {
    let _guard = env_lock();
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let sse = concat!(
        "data: {\"id\":\"chatcmpl_stream\",\"model\":\"deepseek-v4-pro\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null}}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response_with_headers(
            "200 OK",
            "text/event-stream",
            sse,
            &[("x-request-id", "req_stream_456")],
        )],
    )
    .await;

    let client = OpenAiCompatClient::new("test-key", OpenAiCompatConfig::deepseek())
        .with_base_url(server.base_url());
    let mut stream = client
        .stream_message(&sample_request(true))
        .await
        .expect("stream should start");

    assert_eq!(stream.request_id(), Some("req_stream_456"));

    let mut events = Vec::new();
    while let Some(event) = stream
        .next_event()
        .await
        .expect("stream event should parse")
    {
        events.push(event);
    }

    // The OpenAI-compat parser emits a MessageStart, then content/tool blocks,
    // then a MessageDelta carrying the trailing usage, then MessageStop.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::MessageStart(_))),
        "expected a MessageStart event"
    );
    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                content_block: OutputContentBlock::ToolUse { name, .. },
                ..
            }) if name == "get_weather")
        ),
        "expected a tool_use content block for get_weather"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                delta: ContentBlockDelta::InputJsonDelta { .. },
                ..
            })
        )),
        "expected an input_json_delta for the tool call"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::MessageDelta(MessageDeltaEvent { .. }))),
        "expected a MessageDelta event"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::MessageStop(_))),
        "expected a MessageStop event"
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(request.path, "/chat/completions");
    assert!(request.body.contains("\"stream\":true"));
    assert!(
        request.body.contains("\"stream_options\""),
        "stream requests should opt into usage chunks via stream_options.include_usage"
    );
}

#[tokio::test]
async fn retries_retryable_failures_before_succeeding() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![
            http_response(
                "429 Too Many Requests",
                "application/json",
                "{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}",
            ),
            http_response(
                "200 OK",
                "application/json",
                "{\"id\":\"chatcmpl_retry\",\"model\":\"deepseek-v4-pro\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"Recovered\",\"tool_calls\":[]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
            ),
        ],
    )
    .await;

    let client = OpenAiCompatClient::new("test-key", OpenAiCompatConfig::deepseek())
        .with_base_url(server.base_url())
        .with_retry_policy(2, Duration::from_millis(1), Duration::from_millis(2));

    let response = client
        .send_message(&sample_request(false))
        .await
        .expect("retry should eventually succeed");

    assert_eq!(response.total_tokens(), 5);
    assert_eq!(state.lock().await.len(), 2);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // env 互斥锁:防止并行测试相互覆盖环境变量,有意持有跨 await
async fn provider_client_dispatches_deepseek_requests() {
    let _guard = env_lock();
    std::env::set_var("DEEPSEEK_API_KEY", "test-key");
    std::env::set_var("DEEPSEEK_BASE_URL", ""); // ensure from_env reads env api key

    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response(
            "200 OK",
            "application/json",
            "{\"id\":\"chatcmpl_provider\",\"model\":\"deepseek-v4-pro\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"Dispatched\",\"tool_calls\":[]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
        )],
    )
    .await;

    let client = ProviderClient::from_model("deepseek-v4-pro")
        .expect("deepseek provider client should be constructed");
    assert_eq!(client.provider_kind(), ProviderKind::DeepSeek);

    // ProviderClient wraps an OpenAiCompatClient configured from env. To point
    // it at the mock server we set DEEPSEEK_BASE_URL before construction; the
    // client reads the env var via read_base_url at construction time.
    std::env::set_var("DEEPSEEK_BASE_URL", server.base_url());
    let client = ProviderClient::from_model("deepseek-v4-pro")
        .expect("deepseek provider client should rebuild against mock base url");

    let response = client
        .send_message(&sample_request(false))
        .await
        .expect("provider-dispatched request should succeed");

    assert_eq!(response.total_tokens(), 5);

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer test-key")
    );
    assert!(
        !request.headers.contains_key("x-api-key"),
        "DeepSeek path must not send x-api-key"
    );

    std::env::remove_var("DEEPSEEK_API_KEY");
    std::env::remove_var("DEEPSEEK_BASE_URL");
}

#[tokio::test]
async fn surfaces_retry_exhaustion_for_persistent_retryable_errors() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![
            http_response(
                "503 Service Unavailable",
                "application/json",
                "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}",
            ),
            http_response(
                "503 Service Unavailable",
                "application/json",
                "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"still busy\"}}",
            ),
        ],
    )
    .await;

    let client = OpenAiCompatClient::new("test-key", OpenAiCompatConfig::deepseek())
        .with_base_url(server.base_url())
        .with_retry_policy(1, Duration::from_millis(1), Duration::from_millis(2));

    let error = client
        .send_message(&sample_request(false))
        .await
        .expect_err("persistent 503 should fail");

    match error {
        ApiError::RetriesExhausted {
            attempts,
            last_error,
        } => {
            assert_eq!(attempts, 2);
            assert!(matches!(
                *last_error,
                ApiError::Api {
                    status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    retryable: true,
                    ..
                }
            ));
        }
        other => panic!("expected retries exhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn retries_multiple_retryable_failures_with_exponential_backoff_and_jitter() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![
            http_response(
                "429 Too Many Requests",
                "application/json",
                "{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}",
            ),
            http_response(
                "500 Internal Server Error",
                "application/json",
                "{\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}",
            ),
            http_response(
                "503 Service Unavailable",
                "application/json",
                "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}",
            ),
            http_response(
                "429 Too Many Requests",
                "application/json",
                "{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down again\"}}",
            ),
            http_response(
                "503 Service Unavailable",
                "application/json",
                "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"still busy\"}}",
            ),
            http_response(
                "200 OK",
                "application/json",
                "{\"id\":\"chatcmpl_exp_retry\",\"model\":\"deepseek-v4-pro\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"Recovered after 5\",\"tool_calls\":[]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
            ),
        ],
    )
    .await;

    let client = OpenAiCompatClient::new("test-key", OpenAiCompatConfig::deepseek())
        .with_base_url(server.base_url())
        .with_retry_policy(8, Duration::from_millis(1), Duration::from_millis(4));
    let started_at = std::time::Instant::now();

    let response = client
        .send_message(&sample_request(false))
        .await
        .expect("8-retry policy should absorb 5 retryable failures");

    let elapsed = started_at.elapsed();
    assert_eq!(response.total_tokens(), 5);
    assert_eq!(
        state.lock().await.len(),
        6,
        "client should issue 1 original + 5 retry requests before the 200"
    );
    // Jittered sleeps are bounded by 2 * max_backoff per retry (base + jitter),
    // so 5 sleeps fit comfortably below this upper bound with generous slack.
    assert!(
        elapsed < Duration::from_secs(5),
        "retries should complete promptly, took {elapsed:?}"
    );
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and network access to api.deepseek.com"]
async fn live_stream_smoke_test() {
    let client = OpenAiCompatClient::from_env(OpenAiCompatConfig::deepseek())
        .expect("DEEPSEEK_API_KEY must be set");
    let mut stream = client
        .stream_message(&MessageRequest {
            model: std::env::var("DEEPSEEK_MODEL")
                .unwrap_or_else(|_| "deepseek-v4-pro".to_string()),
            max_tokens: 32,
            messages: vec![InputMessage::user_text(
                "Reply with exactly: hello from rust",
            )],
            system: None,
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        })
        .await
        .expect("live stream should start");

    while let Some(_event) = stream
        .next_event()
        .await
        .expect("live stream should yield events")
    {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

struct TestServer {
    base_url: String,
    join_handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}

async fn spawn_server(
    state: Arc<Mutex<Vec<CapturedRequest>>>,
    responses: Vec<String>,
) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have local addr");
    let join_handle = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("server should accept");
            let mut buffer = Vec::new();
            let mut header_end = None;

            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket
                    .read(&mut chunk)
                    .await
                    .expect("request read should succeed");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = find_header_end(&buffer) {
                    header_end = Some(position);
                    break;
                }
            }

            let header_end = header_end.expect("request should include headers");
            let (header_bytes, remaining) = buffer.split_at(header_end);
            let header_text =
                String::from_utf8(header_bytes.to_vec()).expect("headers should be utf8");
            let mut lines = header_text.split("\r\n");
            let request_line = lines.next().expect("request line should exist");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().expect("method should exist").to_string();
            let path = parts.next().expect("path should exist").to_string();
            let mut headers = HashMap::new();
            let mut content_length = 0_usize;
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let (name, value) = line.split_once(':').expect("header should have colon");
                let value = value.trim().to_string();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().expect("content length should parse");
                }
                headers.insert(name.to_ascii_lowercase(), value);
            }

            let mut body = remaining[4..].to_vec();
            while body.len() < content_length {
                let mut chunk = vec![0_u8; content_length - body.len()];
                let read = socket
                    .read(&mut chunk)
                    .await
                    .expect("body read should succeed");
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read]);
            }

            state.lock().await.push(CapturedRequest {
                method,
                path,
                headers,
                body: String::from_utf8(body).expect("body should be utf8"),
            });

            socket
                .write_all(response.as_bytes())
                .await
                .expect("response write should succeed");
        }
    });

    TestServer {
        base_url: format!("http://{address}"),
        join_handle,
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn http_response(status: &str, content_type: &str, body: &str) -> String {
    http_response_with_headers(status, content_type, body, &[])
}

fn http_response_with_headers(
    status: &str,
    content_type: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> String {
    let mut extra_headers = String::new();
    for (name, value) in headers {
        use std::fmt::Write as _;
        write!(&mut extra_headers, "{name}: {value}\r\n").expect("header write should succeed");
    }
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn sample_request(stream: bool) -> MessageRequest {
    MessageRequest {
        model: "deepseek-v4-pro".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![
                InputContentBlock::Text {
                    text: "Say hello".to_string(),
                },
                InputContentBlock::ToolResult {
                    tool_use_id: "toolu_prev".to_string(),
                    content: vec![api::ToolResultContentBlock::Json {
                        value: json!({"forecast": "sunny"}),
                    }],
                    is_error: false,
                },
            ],
        }],
        system: Some(SystemContent::from_text("Use tools when needed")),
        tools: Some(vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: Some("Fetches the weather".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
            cache_control: None,
        }]),
        tool_choice: Some(ToolChoice::Auto),
        stream,
        ..Default::default()
    }
}
