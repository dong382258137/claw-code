//! 进程内"网页通道"的实现:`WebExchange` 的 WS 桥版本 + 默认 turn runner。
//!
//! 视角误区澄清:`ApiClient::stream` 是同步接口、且运行在 agent 的
//! LocalSet 线程里,不能在内部再建 tokio runtime。所以这里用**进程内通道**
//! 断开等待:本模块把 `TurnRequest` 丢给 bridge server(`server.rs` 的
//! `run_turn_loop`),由 server 经 WebSocket 转发给扩展并等回复;本模块的
//! `round_trip` 只做一次 `recv_timeout` 阻塞等待(标准库通道,无嵌套 runtime)。
//!
//! ```text
//! agent 线程                    server 任务                      浏览器扩展
//! round_trip(prompt) ──channel──▶ run_turn_loop ──ws inject_prompt──▶ content script
//!      ◀──channel── 等待回复      │  ◀──ws reply─────────────────────┘
//! ```

use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::driver::run_prompt;
use crate::WebApiClient;
use crate::WebExchange;

/// 一次网页往返请求(进程内通道载荷)。
pub struct TurnRequest {
    pub prompt: String,
    /// server 侧填充结果(Ok=回复文本, Err=失败原因)。
    pub reply_tx: mpsc::Sender<Result<String, String>>,
}

/// `WebExchange` 的进程内实现:把往返转成 `TurnRequest` 经 channel 交给
/// bridge server 处理,再阻塞等标准库通道的回执。
pub struct WsWebExchange {
    turn_bus: tokio::sync::mpsc::UnboundedSender<TurnRequest>,
    timeout: std::time::Duration,
}

impl WsWebExchange {
    #[must_use]
    pub fn new(
        turn_bus: tokio::sync::mpsc::UnboundedSender<TurnRequest>,
        timeout: Duration,
    ) -> Self {
        Self { turn_bus, timeout }
    }
}

impl WebExchange for WsWebExchange {
    fn round_trip(&mut self, prompt: &str) -> Result<String, String> {
        tracing::info!("web turn: submit prompt ({} chars)", prompt.len());
        let (reply_tx, reply_rx) = mpsc::channel();
        self.turn_bus
            .send(TurnRequest {
                prompt: prompt.to_string(),
                reply_tx,
            })
            .map_err(|e| format!("bridge turn channel closed: {e}"))?;
        let result = match reply_rx.recv_timeout(self.timeout) {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("web round-trip timed out after {:?}", self.timeout))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("bridge server closed the turn channel".to_string())
            }
        };
        match &result {
            Ok(text) => {
                let preview: String = text.chars().take(200).collect();
                tracing::info!(
                    "web turn: reply ok ({} chars) preview: {preview}",
                    text.len()
                );
            }
            Err(e) => tracing::warn!("web turn: reply error: {e}"),
        }
        result
    }
}

/// 构造"用户任务 → 完整 agent 回合"的 runner(二进制与集成测试共用)。
///
/// `turn_slot` 在 bridge server 创建完成后由调用方写入 turn channel sender,
/// 运行时才取值,避免构造期的循环依赖。
pub fn make_user_turn_runner(
    turn_slot: Arc<OnceLock<tokio::sync::mpsc::UnboundedSender<TurnRequest>>>,
    system_prompt: Vec<String>,
) -> TurnRunner {
    let runner = move |text: String| -> BoxFuture<'static, Result<String, String>> {
        let slot = turn_slot.clone();
        let system_prompt = system_prompt.clone();
        Box::pin(async move {
            let turn_bus = slot
                .get()
                .cloned()
                .ok_or_else(|| "bridge turn channel not initialized".to_string())?;
            let exchange = WsWebExchange::new(turn_bus, Duration::from_secs(turn_timeout_secs()));
            let client =
                WebApiClient::new(Box::new(exchange), ::tools::GlobalToolRegistry::builtin());
            run_prompt(client, system_prompt, &text).await
        })
    };
    Box::new(runner)
}

/// 单次网页往返超时(秒),环境变量可覆盖。
fn turn_timeout_secs() -> u64 {
    std::env::var("WEB_BRIDGE_TURN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

/// 用户任务 runner 的类型别名。
pub type TurnRunner =
    Box<dyn Fn(String) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;
