//! Web Bridge:把 DeepSeek 网页版当作 claw 引擎的"模型后端"。
//!
//! 早期 M0(模拟回复)/M1(浏览器扩展 + WS 网关)/Electron 桌面壳链路已全部退役并
//! 移除,现仅保留 `login`/`import`(凭证)与直连网页 API 的模型后端一条路径。

pub mod deepseek_web;
pub mod driver;
pub mod login;
pub mod pow;
pub mod protocol;
pub mod safe_commands;
pub mod tools;
pub mod web_session;

use ::tools::GlobalToolRegistry;
use runtime::{ApiRequest, AssistantEvent, RuntimeError, TokenUsage};

/// 模型后端对接抽象:一次"把增强提示发给网页并取回整段回复"的往返。
///
/// 唯一实现为 [`deepseek_web::DeepseekWebClient`](直连 chat.deepseek.com 内部 API);
/// 早期 M0 的脚本化模拟与 M1 的 WS 实现均已退役。
pub trait WebExchange: Send {
    fn round_trip(&mut self, prompt: &str) -> Result<String, String>;
}

/// 实现 `runtime::ApiClient` 的网页版模型客户端。
///
/// 网页版聊天没有原生 tool-calling,所以把"可用工具 + 对话记录 + 调用协议"
/// 渲染成一段增强提示发给网页;回复按 [`protocol::parse_reply`] 解析出
/// 工具调用与最终文本,转成 `AssistantEvent` 交给 agent 循环执行。
pub struct WebApiClient {
    exchange: Box<dyn WebExchange>,
    registry: GlobalToolRegistry,
    /// 是否把工具清单渲染进增强提示(广告 = 可执行)。
    enable_tools: bool,
    tool_use_counter: u64,
}

impl WebApiClient {
    #[must_use]
    pub fn new(exchange: Box<dyn WebExchange>, registry: GlobalToolRegistry) -> Self {
        Self {
            exchange,
            registry,
            enable_tools: true,
            tool_use_counter: 0,
        }
    }

    fn next_tool_id(&mut self) -> String {
        self.tool_use_counter += 1;
        format!("toolu_web_{}", self.tool_use_counter)
    }
}

impl runtime::ApiClient for WebApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let tool_names = if self.enable_tools {
            let mut names = self
                .registry
                .definitions(None)
                .into_iter()
                .filter(|def| !crate::tools::WEB_BRIDGE_EXCLUDED_TOOLS.contains(&def.name.as_str()))
                .map(|def| def.name)
                .collect::<Vec<_>>();
            names.sort();
            names.join(", ")
        } else {
            String::new()
        };

        let prompt = protocol::render_enhanced_prompt(
            &request.system_prompt,
            &request.messages,
            &tool_names,
        );

        let reply = self
            .exchange
            .round_trip(&prompt)
            .map_err(|e| RuntimeError::new(format!("web round-trip failed: {e}")))?;

        let (outcome) = protocol::parse_reply(&reply);
        let text = outcome.text;

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
        Ok(events)
    }
}
