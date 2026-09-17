//! M1 集成测试:起真实 bridge server,用"扩展桩"(tokio-tungstenite 客户端)
//! 按 WS 协议与真实 agent 回合全链路交互:
//!
//! UserRun(桩发送)→ 桥内 spawn 真实 agent 回合 → 桩收到 InjectPrompt
//! → 剧本回复(MockWebExchange)→ Reply → 工具真实执行 → 桥发 Final → 断言。
//!
//! 该测试不依赖浏览器与真实 DeepSeek,但验证了 M1 本地守护 + 协议的全部逻辑。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use web_bridge::mock_web::MockWebExchange;
use web_bridge::server::{serve, spawn_bridge, ExtToServer, ServerToExt};
use web_bridge::ws_exchange::{make_user_turn_runner, TurnRequest};
use web_bridge::WebExchange;

fn unique_text() -> String {
    format!(
        "m1fixture-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extension_stub_full_round_trip_via_real_ws() {
    // 回合超时压短,避免默认 300s 掩盖问题;edition 2021,set_var 安全。
    std::env::set_var("WEB_BRIDGE_TURN_TIMEOUT_SECS", "60");

    let crate_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = crate_dir.join("target");
    std::fs::create_dir_all(&target_dir).expect("create target dir");

    let fixture_text = unique_text();
    let fixture_path = target_dir.join("m1_fixture.txt");
    std::fs::write(&fixture_path, &fixture_text).expect("write fixture");

    let system_prompt = vec![
        "You are claw-code, an AI coding assistant. Respond concisely in Chinese.".to_string(),
    ];
    let turn_slot: Arc<OnceLock<tokio::sync::mpsc::UnboundedSender<TurnRequest>>> =
        Arc::new(OnceLock::new());
    let runner = make_user_turn_runner(turn_slot.clone(), system_prompt);
    let (broker, turn_bus) = spawn_bridge(runner);
    let _ = turn_slot.set(turn_bus);
    eprintln!("[m1-debug] bridge spawned");

    let token = "test-token".to_string();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let (resolved, _http_handle) = serve(addr, broker, token.clone())
        .await
        .expect("serve bridge");

    let url = format!("ws://{resolved}/ws/ext?token={token}");
    let (mut ws, _resp) = connect_async(&url).await.expect("extension stub connect");

    // 用户任务:先提交 UserRun(服务器收到后才开始 agent 回合、注入提示)。
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(&ExtToServer::UserRun {
            text: format!("请读取本地文件 {:?} 并告诉我内容。", fixture_path),
        })
        .expect("serialize user_run"),
    ))
    .await
    .expect("send user_run");

    // 收 Final 的通道 + 扩展桩循环(需同时读写,split 后移入任务)。
    let (final_tx, mut final_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (mut ws_sink, mut ws_stream) = ws.split();

    let stub_path = fixture_path.to_string_lossy().to_string();
    let stub_text = fixture_text.clone();
    tokio::spawn(async move {
        let mut mock = MockWebExchange::new(stub_path, stub_text, 6);
        while let Some(Ok(msg)) = ws_stream.next().await {
            let Ok(text) = msg.into_text() else {
                continue;
            };
            let parsed: ServerToExt = match serde_json::from_str(&text) {
                Ok(p) => p,
                Err(_) => continue,
            };
            match parsed {
                ServerToExt::InjectPrompt { turn_id, text } => {
                    let reply = mock
                        .round_trip(&text)
                        .unwrap_or_else(|e| format!("[mock-error] {e}"));
                    let out = serde_json::to_string(&ExtToServer::Reply {
                        turn_id,
                        text: reply,
                    })
                    .expect("serialize reply");
                    if ws_sink
                        .send(tokio_tungstenite::tungstenite::Message::Text(out))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                ServerToExt::Final { text } => {
                    let _ = final_tx.send(text);
                    return;
                }
                _ => {}
            }
        }
    });

    let final_text = tokio::time::timeout(Duration::from_secs(90), final_rx.recv())
        .await
        .expect("M1 round trip should finish within 90s")
        .expect("final message arrived");

    let expected = format!("MOCK_FINAL: {fixture_text}");
    assert!(
        final_text.contains(&expected),
        "Final 应包含 stub 的最终答复(证明整条 WS 链路 + 本地工具执行闭环);实际:\n{final_text:#?}"
    );

    let _ = std::fs::remove_file(&fixture_path);
}
