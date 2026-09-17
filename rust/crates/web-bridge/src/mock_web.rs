//! M0 试点用的"模拟网页客户端":脚本化模拟 DeepSeek 网页回复,无浏览器。
//!
//! 行为(确定性,便于测试断言):
//! - 若提示中已出现 `<|tool_result|>` **且** 包含 `fixture_text`
//!   (即本地工具已真实执行并返回了内容)→ 返回最终回复 `MOCK_FINAL: <fixture_text>`;
//! - 否则返回一条 `read_file` 工具调用围栏(最多 `max_turns` 轮,防 agent 死循环)。
//!
//! 校验设计:工具**入参**是文件路径,`fixture_text` 只存在于文件的**内容**里,
//! 因此它只能通过工具执行结果进入提示 —— 若工具没真实执行,第二轮提示里
//! 不会同时出现 `<|tool_result|>` 与 `fixture_text`,mock 会一直要求调用工具
//! 直至超过 `max_turns` 触发 [Self::MOCK_STALL] 保护,测试据此判失败。

use crate::WebExchange;

/// 死循环保护时返回的文本(测试断言不出现)。
pub const MOCK_STALL: &str = "[mock-stall] agent kept requesting tools but no tool result was seen";

/// 见 [module](self) 文档。
pub struct MockWebExchange {
    fixture_path: String,
    fixture_text: String,
    max_turns: u32,
    turns: u32,
}

impl MockWebExchange {
    #[must_use]
    pub fn new(
        fixture_path: impl Into<String>,
        fixture_text: impl Into<String>,
        max_turns: u32,
    ) -> Self {
        Self {
            fixture_path: fixture_path.into(),
            fixture_text: fixture_text.into(),
            max_turns,
            turns: 0,
        }
    }
}

impl WebExchange for MockWebExchange {
    fn round_trip(&mut self, prompt: &str) -> Result<String, String> {
        Ok(self.scripted_reply(prompt))
    }
}

impl MockWebExchange {
    /// 纯函数化的"网页回复剧本":M0 测试与 M1 扩展桩共用同一套决策逻辑。
    ///
    /// - 提示含 `<|tool_result|>` 且含 `fixture_text` → 最终答复
    /// - 超过 `max_turns` → 死循环保护文本
    /// - 否则 → 要求执行 `read_file`
    fn scripted_reply(&mut self, prompt: &str) -> String {
        let tool_ran = prompt.contains("<|tool_result|>") && prompt.contains(&self.fixture_text);
        if tool_ran {
            return format!("MOCK_FINAL: {}", self.fixture_text);
        }
        if self.turns >= self.max_turns {
            return MOCK_STALL.to_string();
        }
        self.turns += 1;
        let call = serde_json::json!([{
            "name": "read_file",
            "args": { "path": self.fixture_path },
        }]);
        format!("```tool_calls\n{call}\n```")
    }
}
