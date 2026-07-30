use crate::session::Session;

const DEFAULT_INPUT_COST_PER_MILLION: f64 = 15.0;
const DEFAULT_OUTPUT_COST_PER_MILLION: f64 = 75.0;
const DEFAULT_CACHE_CREATION_COST_PER_MILLION: f64 = 18.75;
const DEFAULT_CACHE_READ_COST_PER_MILLION: f64 = 1.5;

/// CNY 兑 USD 汇率（用于本地化显示）。
/// 内部存储始终用 USD，仅在显示层根据地区转换为 CNY。
/// 取值 7.2（2026-07 近似汇率）。
pub const CNY_TO_USD_RATE: f64 = 7.2;

/// Per-million-token pricing used for cost estimation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_cost_per_million: f64,
    pub output_cost_per_million: f64,
    pub cache_creation_cost_per_million: f64,
    pub cache_read_cost_per_million: f64,
}

impl ModelPricing {
    #[must_use]
    pub const fn default_sonnet_tier() -> Self {
        Self {
            input_cost_per_million: DEFAULT_INPUT_COST_PER_MILLION,
            output_cost_per_million: DEFAULT_OUTPUT_COST_PER_MILLION,
            cache_creation_cost_per_million: DEFAULT_CACHE_CREATION_COST_PER_MILLION,
            cache_read_cost_per_million: DEFAULT_CACHE_READ_COST_PER_MILLION,
        }
    }
}

/// Token counters accumulated for a conversation turn or session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub cache_read_input_tokens: u32,
}

/// Estimated dollar cost derived from a [`TokenUsage`] sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageCostEstimate {
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub cache_creation_cost_usd: f64,
    pub cache_read_cost_usd: f64,
}

impl UsageCostEstimate {
    #[must_use]
    pub fn total_cost_usd(self) -> f64 {
        self.input_cost_usd
            + self.output_cost_usd
            + self.cache_creation_cost_usd
            + self.cache_read_cost_usd
    }
}

/// Returns pricing metadata for a known model alias or family.
///
/// 覆盖范围:
/// - Anthropic 系列:`haiku`/`opus`/`sonnet`
/// - OpenAI 系列:`gpt-5`/`gpt-4o`/`gpt-4o-mini`
/// - xAI 系列:`grok-3`/`grok-2`
/// - 阿里通义系列:`qwen-max`/`qwen-plus`/`qwen-turbo`
/// - DeepSeek 系列:`deepseek-chat`/`deepseek-reasoner`
///
/// 非 Anthropic 系列不原生支持 prompt caching,
/// `cache_creation`/`cache_read` 设为 `0.0`。
///
/// **注意**:价格为公开渠道截至 2025 年的估值,可能与最新官方价格有差异。
/// 准确定价请以厂商官方公告为准。
#[must_use]
pub fn pricing_for_model(model: &str) -> Option<ModelPricing> {
    let normalized = model.to_ascii_lowercase();
    if normalized.contains("haiku") {
        return Some(ModelPricing {
            input_cost_per_million: 1.0,
            output_cost_per_million: 5.0,
            cache_creation_cost_per_million: 1.25,
            cache_read_cost_per_million: 0.1,
        });
    }
    if normalized.contains("opus") {
        return Some(ModelPricing {
            input_cost_per_million: 15.0,
            output_cost_per_million: 75.0,
            cache_creation_cost_per_million: 18.75,
            cache_read_cost_per_million: 1.5,
        });
    }
    if normalized.contains("sonnet") {
        return Some(ModelPricing::default_sonnet_tier());
    }
    // OpenAI 系列
    if normalized.contains("gpt-5") || normalized.contains("gpt5") {
        return Some(ModelPricing {
            input_cost_per_million: 5.0,
            output_cost_per_million: 15.0,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    if normalized.contains("gpt-4o-mini") || normalized.contains("gpt4o-mini") {
        return Some(ModelPricing {
            input_cost_per_million: 0.15,
            output_cost_per_million: 0.6,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    if normalized.contains("gpt-4o") || normalized.contains("gpt4o") {
        return Some(ModelPricing {
            input_cost_per_million: 2.5,
            output_cost_per_million: 10.0,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    // xAI Grok 系列
    if normalized.contains("grok-3") || normalized.contains("grok3") {
        return Some(ModelPricing {
            input_cost_per_million: 5.0,
            output_cost_per_million: 15.0,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    if normalized.contains("grok-2") || normalized.contains("grok2") {
        return Some(ModelPricing {
            input_cost_per_million: 2.0,
            output_cost_per_million: 10.0,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    // 阿里通义 Qwen 系列
    if normalized.contains("qwen-max") || normalized.contains("qwenmax") {
        return Some(ModelPricing {
            input_cost_per_million: 2.5,
            output_cost_per_million: 10.0,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    if normalized.contains("qwen-plus") || normalized.contains("qwenplus") {
        return Some(ModelPricing {
            input_cost_per_million: 0.4,
            output_cost_per_million: 1.2,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    if normalized.contains("qwen-turbo") || normalized.contains("qwenturbo") {
        return Some(ModelPricing {
            input_cost_per_million: 0.05,
            output_cost_per_million: 0.2,
            cache_creation_cost_per_million: 0.0,
            cache_read_cost_per_million: 0.0,
        });
    }
    // DeepSeek 系列（2026-07 官方价目，按汇率 7.2 CNY/USD 换算）
    // 官方价格页：https://api-docs.deepseek.com/zh-cn/quick_start/pricing
    //
    // prompt_cache_miss_tokens 按 input 价计费，映射上 miss 同时填入
    // input_tokens=0 + cache_creation_input_tokens=miss (见 openai_compat.rs),
    // 所以 cache_creation_cost_per_million 必须等于 input_cost_per_million,
    // 否则 miss 部分不计费会导致成本严重低估。
    //
    // v4-pro (原 deepseek-reasoner, 思考模式):
    //   官方: ¥3/M input miss, ¥0.025/M cache hit, ¥6/M output
    //   换算: $0.417 / $0.00347 / $0.833
    //   缓存命中 = input 价的 1/120
    if normalized.contains("deepseek-reasoner")
        || normalized.contains("deepseek-r1")
        || normalized.contains("deepseekr1")
        || normalized.contains("deepseek-v4-pro")
        || normalized.contains("deepseekv4-pro")
    {
        return Some(ModelPricing {
            input_cost_per_million: 0.417,
            output_cost_per_million: 0.833,
            cache_creation_cost_per_million: 0.417,
            cache_read_cost_per_million: 0.00347,
        });
    }
    // v4-flash (原 deepseek-chat, 非思考模式):
    //   官方: ¥1/M input miss, ¥0.02/M cache hit, ¥2/M output
    //   换算: $0.139 / $0.00278 / $0.278
    //   缓存命中 = input 价的 1/50
    if normalized.contains("deepseek-chat")
        || normalized.contains("deepseek-v3")
        || normalized.contains("deepseekv3")
        || normalized.contains("deepseek-v4-flash")
        || normalized.contains("deepseekv4-flash")
    {
        return Some(ModelPricing {
            input_cost_per_million: 0.139,
            output_cost_per_million: 0.278,
            cache_creation_cost_per_million: 0.139,
            cache_read_cost_per_million: 0.00278,
        });
    }
    None
}

impl TokenUsage {
    #[must_use]
    pub fn total_tokens(self) -> u32 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }

    /// 估算当前消耗的上下文窗口 Token 数（不含 output tokens）。
    ///
    /// 兼容两种 API 语义：
    /// - Anthropic 风格：`input_tokens` 已包含 cache 子字段，
    ///   `cache_creation` + `cache_read` 是其子集。
    /// - DeepSeek 风格：`input_tokens` 恒为 0，
    ///   所有 prompt tokens 都在 `cache_creation` + `cache_read` 中。
    ///
    /// 取 `input_tokens.max(cache_creation + cache_read)` 统一处理，
    /// 确保 output tokens 不计入上下文窗口用量。
    #[must_use]
    pub fn context_tokens(self) -> u32 {
        self.input_tokens
            .max(self.cache_creation_input_tokens + self.cache_read_input_tokens)
    }

    #[must_use]
    pub fn estimate_cost_usd(self) -> UsageCostEstimate {
        self.estimate_cost_usd_with_pricing(ModelPricing::default_sonnet_tier())
    }

    #[must_use]
    pub fn estimate_cost_usd_with_pricing(self, pricing: ModelPricing) -> UsageCostEstimate {
        UsageCostEstimate {
            input_cost_usd: cost_for_tokens(self.input_tokens, pricing.input_cost_per_million),
            output_cost_usd: cost_for_tokens(self.output_tokens, pricing.output_cost_per_million),
            cache_creation_cost_usd: cost_for_tokens(
                self.cache_creation_input_tokens,
                pricing.cache_creation_cost_per_million,
            ),
            cache_read_cost_usd: cost_for_tokens(
                self.cache_read_input_tokens,
                pricing.cache_read_cost_per_million,
            ),
        }
    }

    #[must_use]
    pub fn summary_lines(self, label: &str) -> Vec<String> {
        self.summary_lines_for_model(label, None)
    }

    #[must_use]
    pub fn summary_lines_for_model(self, label: &str, model: Option<&str>) -> Vec<String> {
        let pricing = model.and_then(pricing_for_model);
        let cost = pricing.map_or_else(
            || self.estimate_cost_usd(),
            |pricing| self.estimate_cost_usd_with_pricing(pricing),
        );
        let model_suffix =
            model.map_or_else(String::new, |model_name| format!(" model={model_name}"));
        let pricing_suffix = if pricing.is_some() {
            ""
        } else if model.is_some() {
            " pricing=estimated-default"
        } else {
            ""
        };
        vec![
            format!(
                "{label}: total_tokens={} input={} output={} cache_write={} cache_read={} estimated_cost={}{}{}",
                self.total_tokens(),
                self.input_tokens,
                self.output_tokens,
                self.cache_creation_input_tokens,
                self.cache_read_input_tokens,
                format_usd(cost.total_cost_usd()),
                model_suffix,
                pricing_suffix,
            ),
            format!(
                "  cost breakdown: input={} output={} cache_write={} cache_read={}",
                format_usd(cost.input_cost_usd),
                format_usd(cost.output_cost_usd),
                format_usd(cost.cache_creation_cost_usd),
                format_usd(cost.cache_read_cost_usd),
            ),
        ]
    }
}

fn cost_for_tokens(tokens: u32, usd_per_million_tokens: f64) -> f64 {
    f64::from(tokens) / 1_000_000.0 * usd_per_million_tokens
}

#[must_use]
/// Formats a dollar-denominated value for CLI display.
pub fn format_usd(amount: f64) -> String {
    format!("${amount:.4}")
}

/// 根据地区格式化费用显示。
///
/// 内部存储始终为 USD，此函数仅在显示层做转换：
/// - `use_cny = true`（中国大陆地区）：转换为 CNY，显示为 `¥X.XXXX`
/// - `use_cny = false`（其他地区）：保持 USD，显示为 `$X.XXXX`
///
/// 转换使用 [`CNY_TO_USD_RATE`] 常量汇率。
#[must_use]
pub fn format_cost_localized(usd_amount: f64, use_cny: bool) -> String {
    if use_cny {
        let cny = usd_amount * CNY_TO_USD_RATE;
        format!("¥{cny:.4}")
    } else {
        format!("${usd_amount:.4}")
    }
}

/// Aggregates token usage across a running session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTracker {
    latest_turn: TokenUsage,
    cumulative: TokenUsage,
    turns: u32,
}

impl UsageTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_session(session: &Session) -> Self {
        let mut tracker = Self::new();
        for message in &session.messages {
            if let Some(usage) = message.usage {
                tracker.record(usage);
            }
        }
        tracker
    }

    pub fn record(&mut self, usage: TokenUsage) {
        self.latest_turn = usage;
        self.cumulative.input_tokens += usage.input_tokens;
        self.cumulative.output_tokens += usage.output_tokens;
        self.cumulative.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        self.cumulative.cache_read_input_tokens += usage.cache_read_input_tokens;
        self.turns += 1;
    }

    #[must_use]
    pub fn current_turn_usage(&self) -> TokenUsage {
        self.latest_turn
    }

    #[must_use]
    pub fn cumulative_usage(&self) -> TokenUsage {
        self.cumulative
    }

    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns
    }
}

#[cfg(test)]
mod tests {
    use super::{format_usd, pricing_for_model, TokenUsage, UsageTracker};
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

    #[test]
    fn tracks_true_cumulative_usage() {
        let mut tracker = UsageTracker::new();
        tracker.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 1,
        });
        tracker.record(TokenUsage {
            input_tokens: 20,
            output_tokens: 6,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 2,
        });

        assert_eq!(tracker.turns(), 2);
        assert_eq!(tracker.current_turn_usage().input_tokens, 20);
        assert_eq!(tracker.current_turn_usage().output_tokens, 6);
        assert_eq!(tracker.cumulative_usage().output_tokens, 10);
        assert_eq!(tracker.cumulative_usage().input_tokens, 30);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 48);
    }

    #[test]
    fn computes_cost_summary_lines() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_creation_input_tokens: 100_000,
            cache_read_input_tokens: 200_000,
        };

        let cost = usage.estimate_cost_usd();
        assert_eq!(format_usd(cost.input_cost_usd), "$15.0000");
        assert_eq!(format_usd(cost.output_cost_usd), "$37.5000");
        let lines = usage.summary_lines_for_model("usage", Some("deepseek-v4-pro"));
        assert!(lines[0].contains("estimated_cost=$0.8759"));
        assert!(lines[0].contains("model=deepseek-v4-pro"));
        assert!(lines[1].contains("cache_read=$0.0007"));
    }

    #[test]
    fn supports_model_specific_pricing() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };

        let flash = pricing_for_model("deepseek-v4-flash").expect("flash pricing");
        let pro = pricing_for_model("deepseek-v4-pro").expect("pro pricing");
        let flash_cost = usage.estimate_cost_usd_with_pricing(flash);
        let pro_cost = usage.estimate_cost_usd_with_pricing(pro);
        // flash: $0.139/M input + $0.278/M output = $0.139 + $0.139 = $0.278
        assert_eq!(format_usd(flash_cost.total_cost_usd()), "$0.2780");
        // pro: $0.417/M input + $0.833/M output = $0.417 + $0.4165 = $0.8335
        assert_eq!(format_usd(pro_cost.total_cost_usd()), "$0.8335");
    }

    #[test]
    fn supports_deepseek_model_pricing() {
        // DeepSeek v4-flash（原 deepseek-chat）和 v4-pro（原 deepseek-reasoner）
        let ds_flash = pricing_for_model("deepseek-chat").expect("deepseek-chat pricing");
        let ds_pro = pricing_for_model("deepseek-reasoner").expect("deepseek-reasoner pricing");
        let ds_v4_flash =
            pricing_for_model("deepseek-v4-flash").expect("deepseek-v4-flash pricing");
        let ds_v4_pro = pricing_for_model("deepseek-v4-pro").expect("deepseek-v4-pro pricing");

        // DeepSeek 2026-07 官方价目（按汇率 7.2 换算）
        // v4-flash: ¥1/M input, ¥2/M output, ¥0.02/M cache hit
        assert_eq!(ds_flash.input_cost_per_million, 0.139);
        assert_eq!(ds_flash.output_cost_per_million, 0.278);
        assert_eq!(ds_flash.cache_read_cost_per_million, 0.00278);
        assert_eq!(ds_flash.cache_creation_cost_per_million, 0.139); // miss = input 价
                                                                     // v4-pro: ¥3/M input, ¥6/M output, ¥0.025/M cache hit
        assert_eq!(ds_pro.input_cost_per_million, 0.417);
        assert_eq!(ds_pro.output_cost_per_million, 0.833);
        assert_eq!(ds_pro.cache_read_cost_per_million, 0.00347);
        assert_eq!(ds_pro.cache_creation_cost_per_million, 0.417);

        // 验证别名映射：旧名等价新名
        assert_eq!(
            ds_v4_flash.input_cost_per_million,
            ds_flash.input_cost_per_million
        );
        assert_eq!(
            ds_v4_pro.input_cost_per_million,
            ds_pro.input_cost_per_million
        );
        // deepseek-v3 仍映射到 flash 价（向后兼容）
        let ds_v3_alias = pricing_for_model("deepseek-v3").expect("deepseek-v3 alias");
        assert_eq!(
            ds_v3_alias.input_cost_per_million,
            ds_flash.input_cost_per_million
        );
        // deepseek-v4 裸名已删除，应返回 None（避免歧义）
        assert!(pricing_for_model("deepseek-v4").is_none());

        // 未知模型仍返回 None
        assert!(pricing_for_model("some-unknown-model-v999").is_none());
    }

    #[test]
    fn context_tokens_excludes_output_tokens() {
        // Anthropic 风格：input_tokens 已包含 cache 子字段
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 200,
        };
        // output_tokens 不计入 context，cache 是 input 的子集，不应重复计数
        assert_eq!(usage.context_tokens(), 1000);
        // total_tokens 仍包含 output
        assert_eq!(usage.total_tokens(), 2000);
    }

    #[test]
    fn context_tokens_handles_deepseek_style() {
        // DeepSeek 风格：input_tokens=0，cache 字段承载所有 prompt tokens
        let usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 400,
            cache_creation_input_tokens: 800,
            cache_read_input_tokens: 200,
        };
        assert_eq!(usage.context_tokens(), 1000);
        // total_tokens 包含 output
        assert_eq!(usage.total_tokens(), 1400);
    }

    #[test]
    fn formats_cost_localized_by_region() {
        // USD 显示
        assert_eq!(super::format_cost_localized(1.0, false), "$1.0000");
        // CNY 显示（汇率 7.2）
        assert_eq!(super::format_cost_localized(1.0, true), "¥7.2000");
        assert_eq!(super::format_cost_localized(0.0, true), "¥0.0000");
    }

    #[test]
    fn marks_unknown_model_pricing_as_fallback() {
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 100,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let lines = usage.summary_lines_for_model("usage", Some("custom-model"));
        assert!(lines[0].contains("pricing=estimated-default"));
    }

    #[test]
    fn reconstructs_usage_from_session_messages() {
        let mut session = Session::new();
        session.messages = vec![ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
            usage: Some(TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 0,
            }),
        }];

        let tracker = UsageTracker::from_session(&session);
        assert_eq!(tracker.turns(), 1);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 8);
    }
}
