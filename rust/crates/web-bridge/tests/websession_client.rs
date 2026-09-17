//! DeepSeek 网页 API 客户端端到端测试：本地 axum stub 模拟
//! `users/current` / `chat_session/create` / `create_pow_challenge(sha256)` /
//! `chat/completion(SSE+FINISHED)`，验证客户端全链路（无需真实登录）。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use sha2::Digest;
use web_bridge::deepseek_web::DeepseekWebClient;
use web_bridge::web_session::WebAuth;
use web_bridge::WebExchange;

#[derive(Clone)]
struct Stub {
    completion_hits: Arc<std::sync::atomic::AtomicU32>,
}

async fn users_current() -> Json<Value> {
    Json(json!({ "data": { "biz_data": { "token": "tok-12345" } } }))
}

async fn session_create() -> Json<Value> {
    Json(json!({ "data": { "biz_data": { "id": "sess-test-001" } } }))
}

async fn pow_challenge() -> Json<Value> {
    Json(json!({ "data": { "biz_data": { "challenge": {
        "algorithm": "sha256",
        "challenge": "challenge-abc",
        "difficulty": 4,
        "salt": "salt-xyz",
        "signature": ""
    } } } }))
}

fn sse_frame(payload: &str) -> String {
    format!("data: {payload}\n\n")
}

async fn chat_completion(
    State(stub): State<Stub>,
    headers: HeaderMap,
    body: Json<Value>,
) -> Result<axum::response::Response, StatusCode> {
    stub.completion_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    // 校验 PoW 头存在且 answer 正确（用 sha256 重算验证）。
    let pow_header = headers
        .get("x-ds-pow-response")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, pow_header)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let obj: Value = serde_json::from_slice(&decoded).map_err(|_| StatusCode::BAD_REQUEST)?;
    assert_eq!(obj["algorithm"], "sha256");
    assert_eq!(obj["target_path"], "/api/v0/chat/completion");
    let nonce = obj["answer"].as_u64().expect("sha256 answer");
    let input = format!("{}{}{}", obj["salt"].as_str().unwrap(), obj["challenge"].as_str().unwrap(), nonce);
    let digest = sha2::Sha256::digest(input.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let leading = hex.chars().next().unwrap().to_digit(16).unwrap();
    assert!(leading >= 4, "pow invalid: {hex}");

    let prompt = body["prompt"].as_str().unwrap_or("");
    let reply = format!(
        "STUB-WEB 收到提示({} 字符)。\n\n```claw\nawait bash(\"echo hi\");\n```\n完成。",
        prompt.len()
    );
    let stream = format!(
        "{}{}{}",
        sse_frame(&json!({"p":"response/status","v":"RUNNING"}).to_string()),
        sse_frame(&json!({"p":"response/fragments/0/content","v":reply}).to_string()),
        sse_frame(&json!({"p":"response/status","v":"FINISHED"}).to_string()),
    );
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        stream,
    )
        .into_response())
}

#[tokio::test(flavor = "multi_thread")]
async fn deepseek_web_client_full_round_trip_via_stub() {
    std::env::set_var("DEEPSEEK_WEB_BASE", "http://127.0.0.1:1"); // 占位，下面覆盖
    let app = Router::new()
        .route("/api/v0/users/current", post(users_current))
        .route("/api/v0/chat_session/create", post(session_create))
        .route("/api/v0/chat/create_pow_challenge", post(pow_challenge))
        .route("/api/v0/chat/completion", post(chat_completion))
        .with_state(Stub {
            completion_hits: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        });

    let listener = tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    std::env::set_var("DEEPSEEK_WEB_BASE", format!("http://{addr}"));

    let auth = WebAuth {
        cookie: "d_id=abc; ds_session_id=xyz".to_string(),
        bearer: "tok-12345".to_string(),
        user_agent: web_bridge::web_session::DEFAULT_USER_AGENT.to_string(),
        captured_at: None,
    };
    let mut client = DeepseekWebClient::new(auth);
    let reply = client.round_trip("你好，帮我读一个文件").expect("round trip ok");
    assert!(reply.contains("完成。"), "回复应含结尾: {reply}");
    assert!(reply.contains("```claw"), "回复应含工具代码块: {reply}");

    handle.abort();
}