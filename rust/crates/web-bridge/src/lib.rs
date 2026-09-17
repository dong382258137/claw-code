//! Web Bridge:把 DeepSeek 网页版(登录态、零 Token)当作 claw 引擎的"模型后端"。
//!
//! 一次性登录(`login`/`import`)保存凭证后,运行时直接调用 chat.deepseek.com
//! 内部 API(async round_trip):PoW(`sha256`/`DeepSeekHashV1`,wasmtime 执行官方
//! wasm) + `chat/completion` SSE 流(以服务端 FINISHED 为完成信号)。
//!
//! 架构:
//! ```text
//! claw agent 循环(`runtime::ApiClient` 接缝,复用全部工具/权限)
//!   └─ WebApiClient ─ DeepseekWebClient(WebExchange, worker 线程 HTTP)
//!        └─ chat.deepseek.com 内部 API(cookie + userToken 鉴权)
//! ```
//!
//! 说明:早期 M0(模拟回复)/M1(浏览器扩展 + WS 网关)/Electron 壳链路已全部退役,
//! 现仅保留直连网页 API 一条路径。

pub mod deepseek_web;
pub mod driver;
pub mod login;
pub mod pow;
pub mod protocol;
pub mod safe_commands;
pub mod tools;
pub mod web_session;

use ::runtime::{ApiRequest, AssistantEvent, RuntimeError, TokenUsage};

/// 模型后端对接抽象:一次"把增强提示发给网页并取回整段回复"的往返。
pub trait WebExchange: Send {
    fn round_trip(&mut self, prompt: &str) -> Result<String, String>;
}

/// 实现 `runtime::ApiClient` 的网页版模型客户端。
///
/// 网页版聊天没有原生 tool-calling,所以把"对话记录 + 调用协议"渲染成
/// 一段增强提示发给网页;回复按 [`protocol::parse_reply`] 解析出工具调用
/// 与最终文本,转成 `AssistantEvent` 交给 agent 循环执行。
///
/// 工具可用性不在客户端侧声明——协议白名单(`protocol::PROTOCOL_SUFFIX`)
/// 是唯一事实源,不再从 registry 广告全量工具名(曾导致双工具协议冲突)。
pub struct WebApiClient {
    exchange: Box<dyn WebExchange>,
    tool_use_counter: u64,
    /// TUI 状态转发:网页后端一次性返回完整回复,没有原生流式事件;
    /// TUI 的回复显示完全依赖 StatusEvent(见 tui/app.rs 对
    /// StatusEvent::TextDelta 的消费),因此需把「整段文本 + 收尾事件」
    /// 模拟成流式事件转发出去,否则 TUI 输出区永远空白(CLI 不受影响)。
    status_emitter: Option<runtime::StatusEmitter>,
}

impl WebApiClient {
    #[must_use]
    pub fn new(exchange: Box<dyn WebExchange>) -> Self {
        Self {
            exchange,
            tool_use_counter: 0,
            status_emitter: None,
        }
    }

    fn next_tool_id(&mut self) -> String {
        self.tool_use_counter += 1;
        format!("toolu_web_{}", self.tool_use_counter)
    }
}

impl runtime::ApiClient for WebApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let prompt = protocol::render_enhanced_prompt(&request.system_prompt, &request.messages);

        let reply = self
            .exchange
            .round_trip(&prompt)
            .map_err(|e| RuntimeError::new(format!("web round-trip failed: {e}")))?;
        crate::deepseek_web::debug_log(&format!(
            "WebApiClient::stream 拿到 reply ({} chars)",
            reply.len()
        ));

        let outcome = protocol::parse_reply(&reply);
        let text = outcome.text;
        crate::deepseek_web::debug_log(&format!(
            "parse_reply: text={} chars calls={} unparsed={} truncated={}",
            text.len(),
            outcome.calls.len(),
            outcome.unparsed.len(),
            outcome.truncated
        ));

        let mut events: Vec<AssistantEvent> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        for call in outcome.calls {
            // 命令执行前过危险预检;命中即拒绝,并把原因作为文本回传模型。
            if call.name == "bash" {
                let command = call
                    .args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if let Some(desc) = safe_commands::check(command) {
                    tracing::warn!("web turn: blocked bash command: {desc}: {command}");
                    blocked.push(format!("命令被安全策略拒绝({desc})"));
                    continue;
                }
            }
            events.push(AssistantEvent::ToolUse {
                id: self.next_tool_id(),
                name: call.name,
                input: serde_json::to_string(&call.args).unwrap_or_else(|_| "{}".to_string()),
            });
        }

        // 组装给模型看的反馈:最终文本 + 截断/解析失败/被拒命令的提示。
        let mut feedback = text;
        if outcome.truncated && events.is_empty() {
            feedback = format!(
                "{feedback}\n\n[claw-system] 检测到工具代码块被截断,请在下一轮输出完整、闭合的代码块。"
            );
        }
        for u in &outcome.unparsed {
            feedback = format!(
                "{feedback}\n\n[claw-system] 工具调用无法解析({u});请改用字符串/数字字面量作为参数,不要使用变量或模板字符串。"
            );
        }
        for b in blocked {
            feedback = format!("{feedback}\n\n[claw-system] {b};请改用更安全的方式完成任务。");
        }
        if !feedback.trim().is_empty() {
            events.insert(0, AssistantEvent::TextDelta(feedback));
        }
        events.push(AssistantEvent::Usage(TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }));
        events.push(AssistantEvent::MessageStop);

        // TUI 适配:网页后端无原生流式事件,把「整段回复文本 + usage + 收尾」
        // 模拟成 StatusEvent 序列转发,驱动 TUI 输出区/状态栏实时显示
        // (CLI 模式 emitter 为 None,零影响)。
        if let Some(emitter) = &self.status_emitter {
            crate::deepseek_web::debug_log("emitter 存在,转发 StatusEvent 序列");
            emitter(runtime::StatusEvent::StreamStart);
            let mut forwarded = 0;
            for text in events.iter().filter_map(|e| match e {
                AssistantEvent::TextDelta(t) => Some(t),
                _ => None,
            }) {
                emitter(runtime::StatusEvent::TextDelta(text.clone()));
                forwarded += 1;
            }
            crate::deepseek_web::debug_log(&format!("转发 {forwarded} 个 TextDelta 事件"));
            if let Some(AssistantEvent::Usage(u)) = events
                .iter()
                .rfind(|e| matches!(e, AssistantEvent::Usage(_)))
            {
                emitter(runtime::StatusEvent::Usage(*u));
            }
            emitter(runtime::StatusEvent::MessageStop);
        } else {
            crate::deepseek_web::debug_log("emitter 为 None(未设置)");
        }
        Ok(events)
    }

    fn set_status_emitter(&mut self, emitter: runtime::StatusEmitter) {
        crate::deepseek_web::debug_log("WebApiClient::set_status_emitter 被调用");
        self.status_emitter = Some(emitter);
    }
}
