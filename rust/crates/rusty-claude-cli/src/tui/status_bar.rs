//! Shared state for the persistent status bar.
//!
//! `StatusBarState` is the single source of truth the TUI reads to render
//! the bottom status bar. It is updated by:
//! - `LiveCli::accumulate_usage` (after each turn, cumulative totals)
//! - `StatusEmitter` callback in `AnthropicRuntimeClient` (live during stream)
//!
//! Rendering to a ratatui `Frame` happens in `render_status_bar` (added in Task 4).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use std::sync::{Arc, Mutex};

use api;
use runtime::TokenUsage;

/// Snapshot of everything the status bar displays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StatusBarState {
    /// Resolved model name (e.g. `claude-opus-4-6`). 显示在底栏。
    pub model: String,
    /// Provider label (e.g. `Anthropic`, `OpenAI`, `xAI`). 保留字段但不再在 TUI 显示。
    pub provider: String,
    /// Short cwd path (e.g. `~/projects/claw`).
    pub cwd: String,
    /// Current git branch, or empty if not in a repo.
    pub git_branch: String,
    /// Active permission mode label.
    pub permission_mode: String,
    /// Session id.
    pub session_id: String,
    /// Cumulative token usage across all turns in this session.
    pub cumulative_usage: TokenUsage,
    /// Delta usage observed *during* the current streaming turn (resets per turn).
    pub turn_usage: TokenUsage,
    /// Elapsed millis since the current turn started (0 when idle).
    pub turn_elapsed_ms: u64,
    /// True when a streaming turn is in progress.
    pub streaming: bool,
    /// Goal badge text (e.g. `🎯 goal` / `⚠ goal (1/3)`), or empty when paused/no goal.
    pub goal_badge: String,
    /// Poor-mode active flag.
    pub poor_mode: bool,
    /// 当前 reasoning effort 设置（None=默认，Some("low"/"medium"/"high")=已设置）。
    pub reasoning_effort: Option<String>,
    /// 累计 AI 思考轮次（每个 turn +1）。
    pub turn_count: u32,
    /// 标记当前 turn 是否已开始（用于多轮工具调用中避免重复 reset）。
    pub turn_in_progress: bool,
    /// Git 工作区摘要（e.g. "clean", "±3", "±3 a:1 b:2"）。空表示未获取。
    pub git_status: String,
    /// 会话启动时间戳（毫秒，TUI 启动时设一次）。
    pub session_start_ms: u64,
    /// 本会话累计消息数（user + assistant）。
    pub message_count: u32,
    /// 本会话累计成功工具调用次数。
    pub tool_success_count: u32,
    /// 本会话累计失败工具调用次数。
    pub tool_error_count: u32,
    /// 上一轮完成时的 context token 数（用于 idle 状态显示，
    /// 避免 `cumulative.context_tokens()` 跨 turn 重复计数导致 100% 误报）。
    pub last_ctx: u128,
}

impl StatusBarState {
    pub(crate) fn shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    /// Total tokens (cumulative + current turn delta).
    pub(crate) fn total_tokens(&self) -> u128 {
        let cumulative = self.cumulative_usage.total_tokens() as u128;
        let turn = self.turn_usage.total_tokens() as u128;
        cumulative + turn
    }

    /// 缓存命中率（0.0~1.0），口径与侧栏原算法一致：
    /// 命中 = cache_read，未命中 = cache_creation，均为累计 + 当前轮 delta。
    /// 无缓存数据（命中 + 未命中 = 0）时返回 None，调用方不渲染该段。
    pub(crate) fn cache_hit_rate(&self) -> Option<f64> {
        let hit = (self.cumulative_usage.cache_read_input_tokens as u64)
            + (self.turn_usage.cache_read_input_tokens as u64);
        let miss = (self.cumulative_usage.cache_creation_input_tokens as u64)
            + (self.turn_usage.cache_creation_input_tokens as u64);
        let sum = hit + miss;
        if sum == 0 {
            None
        } else {
            Some(hit as f64 / sum as f64)
        }
    }

    /// 上下文窗口实际消耗的 Token 数（不含 output tokens）。
    ///
    /// 与进度条分母 `context_window_tokens` 口径一致：
    /// 只计 prompt 侧 token（input + cache），排除 completion 侧。
    ///
    /// **修正（2026-08）**：原实现返回 `cumulative.context_tokens() + turn.context_tokens()`，
    /// 但每次 API 返回的 `context_tokens()` 已是该 turn 的**完整 prompt 量**（包含全部历史）。
    /// 按 turn 累加会导致对话历史被重复计数 N 次，在 ~15 轮后误报 100%。
    /// 修正：使用当前 turn 的 `context_tokens()` 作为上下文窗口占用量，
    /// 空闲时使用上一轮完成时保存的快照。
    pub(crate) fn context_tokens(&self) -> u128 {
        if self.turn_in_progress {
            // 流式输出中：以 API 返回的最新 prompt 量为准。
            let turn_ctx = self.turn_usage.context_tokens() as u128;
            if turn_ctx > 0 {
                return turn_ctx;
            }
            // turn 刚开始、API 尚未返回 Usage 事件时，turn_usage 被
            // reset_turn() 清零了。此时兜底用上一轮完成时的快照，
            // 避免 CTX% 进度条短暂跳变到 0%，视觉上像"每发一次就重置"。
        }
        // 空闲态，或 turn 刚开始尚无 API 数据时：复用上一轮快照。
        self.last_ctx
    }

    pub(crate) fn reset_turn(&mut self) {
        if self.turn_in_progress {
            self.streaming = true;
            return;
        }
        self.turn_usage = TokenUsage::default();
        self.turn_elapsed_ms = 0;
        self.streaming = true;
        self.turn_in_progress = true;
    }

    pub(crate) fn finish_turn(&mut self) {
        self.streaming = false;
        self.turn_in_progress = false;
        self.cumulative_usage.input_tokens += self.turn_usage.input_tokens;
        self.cumulative_usage.output_tokens += self.turn_usage.output_tokens;
        self.cumulative_usage.cache_creation_input_tokens +=
            self.turn_usage.cache_creation_input_tokens;
        self.cumulative_usage.cache_read_input_tokens += self.turn_usage.cache_read_input_tokens;
        // 保存本轮 context 用量到快照，用于 idle 状态的 ctx% 显示
        self.last_ctx = self.turn_usage.context_tokens() as u128;
        self.turn_usage = TokenUsage::default();
        self.turn_elapsed_ms = 0;
    }
}

/// Ratatui widget that renders the persistent status bar.
///
/// Renders a single line at the bottom of the terminal showing:
/// `│ 🤖 model │ 📁 cwd │ 💰 cost │ ctx: 45% ████▌░░░░░ │ ⏳ 5s │ vX.Y.Z │`
pub(crate) struct StatusBar<'a> {
    pub state: &'a StatusBarState,
}

impl<'a> Widget for StatusBar<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let style_dim = Style::default().fg(Color::DarkGray);
        let style_model = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let style_cost = Style::default().fg(Color::Green);
        let style_streaming = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let style_version = Style::default().fg(Color::DarkGray);

        let width = area.width as usize;
        let mut sections: Vec<Vec<Span>> = Vec::new();

        // P1: Model name (从侧栏移到底栏)
        let model_short = shorten_model_name(&self.state.model);
        sections.push(vec![
            Span::styled("│ ", style_dim),
            Span::styled("🤖 ", style_dim),
            Span::styled(model_short, style_model),
        ]);

        // P1.1: Reasoning effort icon (从侧栏移到底栏，仅非默认时显示)
        if let Some(ref effort) = self.state.reasoning_effort {
            let (icon, effort_style) = match effort.as_str() {
                "low" => (
                    "🧠L",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                "medium" => (
                    "🧠M",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                "high" => (
                    "🧠H",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                _ => ("🧠", style_model),
            };
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled(icon, effort_style),
            ]);
        }

        // P1.2: Poor-mode indicator (从侧栏移到底栏，仅启用时显示)
        if self.state.poor_mode {
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled("🪙", style_cost),
            ]);
        }

        // P1.3: Turn count (从侧栏移到底栏)
        sections.push(vec![
            Span::styled(" │ ", style_dim),
            Span::styled(format!("#{}", self.state.turn_count), style_dim),
        ]);

        // P2: Cwd
        let cwd_short =
            crate::shorten_cwd_for_statusbar(&std::path::PathBuf::from(&self.state.cwd));
        sections.push(vec![
            Span::styled(" │ ", style_dim),
            Span::styled("📁 ", style_dim),
            Span::styled(cwd_short, style_dim),
        ]);

        // P2.5: Git 分支 + 工作区状态（从侧栏移到底栏；非 git 仓库时不显示）
        if !self.state.git_branch.is_empty() {
            let (git_style, dirty_suffix) = match self.state.git_status.as_str() {
                // 工作区干净：分支绿色
                "clean" => (
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                    String::new(),
                ),
                // git_status 为空（非 git 目录 / 未获取）：分支灰色
                "" => (Style::default().fg(Color::DarkGray), String::new()),
                // 有改动：分支黄色，追加摘要（如 ±3）
                summary => (
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    format!(" {summary}"),
                ),
            };
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled("⎇ ", style_dim),
                Span::styled(
                    format!("{}{}", self.state.git_branch, dirty_suffix),
                    git_style,
                ),
            ]);
        }

        // P3: Cost (从侧栏移到底栏)
        // 成本计算与进度条保持一致：累计 + 当前轮 delta。
        // 使用 total_tokens() 确保 streaming 期间也能看到实时成本变化。
        let pricing = runtime::pricing_for_model(&self.state.model);
        let total_usage = TokenUsage {
            input_tokens: self
                .state
                .cumulative_usage
                .input_tokens
                .saturating_add(self.state.turn_usage.input_tokens),
            output_tokens: self
                .state
                .cumulative_usage
                .output_tokens
                .saturating_add(self.state.turn_usage.output_tokens),
            cache_creation_input_tokens: self
                .state
                .cumulative_usage
                .cache_creation_input_tokens
                .saturating_add(self.state.turn_usage.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .state
                .cumulative_usage
                .cache_read_input_tokens
                .saturating_add(self.state.turn_usage.cache_read_input_tokens),
        };
        let cost_usd = pricing.map_or_else(
            || total_usage.estimate_cost_usd().total_cost_usd(),
            |p| {
                total_usage
                    .estimate_cost_usd_with_pricing(p)
                    .total_cost_usd()
            },
        );
        let cost = runtime::format_cost_localized(cost_usd, crate::locale::is_cny_region());
        sections.push(vec![
            Span::styled(" │ ", style_dim),
            Span::styled("💰 ", style_dim),
            Span::styled(cost, style_cost),
        ]);

        // P4: Context usage % + progress bar
        // 使用 context_tokens() 而非 total_tokens()：
        // output tokens 不消耗上下文窗口，不计入进度条。
        let context_tokens = self.state.context_tokens();
        let ctx_window = context_window_for_model(&self.state.model);
        let usage_pct = if ctx_window > 0 {
            ((context_tokens as f64 / ctx_window as f64) * 100.0).min(100.0)
        } else {
            0.0
        };
        let bar = progress_bar_10(usage_pct);
        let pct_style = if usage_pct < 50.0 {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else if usage_pct < 80.0 {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        };
        sections.push(vec![
            Span::styled(" │ ", style_dim),
            Span::styled("ctx:", style_dim),
            Span::styled(format!("{usage_pct:.0}%"), pct_style),
            Span::raw(" "),
            Span::raw(bar),
        ]);

        // P4.5: 缓存命中率（从侧栏移到底栏；无缓存数据时不显示）
        if let Some(rate) = self.state.cache_hit_rate() {
            let cache_style = if rate >= 0.85 {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if rate >= 0.60 {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            };
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled("缓存 ", style_dim),
                Span::styled(format!("{:.0}%", rate * 100.0), cache_style),
            ]);
        }

        // P5: Streaming timer
        if self.state.streaming {
            let elapsed_s = self.state.turn_elapsed_ms / 1000;
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled(format!("⏳ {elapsed_s}s"), style_streaming),
            ]);
        }

        // P6: Version
        sections.push(vec![
            Span::styled(" │ ", style_dim),
            Span::styled(format!("v{}", crate::VERSION), style_version),
        ]);

        // Flatten sections up to available width
        let mut spans: Vec<Span> = Vec::new();
        let mut used: usize = 0;
        for section in &sections {
            let section_width: usize = section
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            if used + section_width > width && !spans.is_empty() {
                break;
            }
            used += section_width;
            spans.extend(section.iter().cloned());
        }

        spans.push(Span::styled(" │", style_dim));

        let line = Line::from(spans);
        Widget::render(line, area, buf);
    }
}

/// 缩短模型名显示：去掉 provider 前缀和版本后缀，只保留核心型号。
fn shorten_model_name(model: &str) -> String {
    // 提取核心模型名: claude-sonnet-4-20250514 → sonnet
    // 或用更短的别名
    let lower = model.to_ascii_lowercase();
    if lower.contains("claude-opus") || lower.contains("opus") {
        "opus".to_string()
    } else if lower.contains("claude-sonnet") || lower.contains("sonnet") {
        "sonnet".to_string()
    } else if lower.contains("claude-haiku") || lower.contains("haiku") {
        "haiku".to_string()
    } else if lower.contains("gpt-5") || lower.contains("gpt5") {
        "gpt-5".to_string()
    } else if lower.contains("gpt-4o-mini") || lower.contains("gpt4o-mini") {
        "gpt-4o-mini".to_string()
    } else if lower.contains("gpt-4o") || lower.contains("gpt4o") {
        "gpt-4o".to_string()
    } else if lower.contains("grok-3") || lower.contains("grok3") {
        "grok-3".to_string()
    } else if lower.contains("grok-2") || lower.contains("grok2") {
        "grok-2".to_string()
    } else if lower.contains("deepseek-reasoner")
        || lower.contains("deepseek-r1")
        || lower.contains("deepseek-v4-pro")
    {
        "ds-v4-pro".to_string()
    } else if lower.contains("deepseek-chat")
        || lower.contains("deepseek-v3")
        || lower.contains("deepseek-v4-flash")
    {
        "ds-v4-flash".to_string()
    } else if lower.contains("qwen-max") {
        "qwen-max".to_string()
    } else if lower.contains("qwen-plus") {
        "qwen-plus".to_string()
    } else if lower.contains("qwen-turbo") {
        "qwen-turbo".to_string()
    } else {
        // 未知模型：截断到40字符
        if model.len() > 40 {
            let mut end = 39;
            while end > 0 && !model.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &model[..end])
        } else {
            model.to_string()
        }
    }
}

/// 根据模型名查询上下文窗口大小（tokens）。
///
/// 使用 `api::model_token_limit()` 获取精确容量，避免硬编码。
/// 注意：此函数与 `compaction_threshold_for_context_window()` 共享同一数据源，
/// 确保显示端和计算端使用相同的 context_window 值，形成闭环。
fn context_window_for_model(model: &str) -> u128 {
    api::model_token_limit(model)
        .map(|limit| limit.context_window_tokens as u128)
        .unwrap_or(200_000) // fallback: 200K for unknown models
}

/// 10格 Unicode 进度条: █████▌░░░░
/// 颜色由调用方设置（根据百分比），此处只返回纯文本字符。
fn progress_bar_10(pct: f64) -> String {
    let filled = (pct / 10.0).clamp(0.0, 10.0);
    let full_blocks = filled.floor() as usize;
    let remainder = filled - filled.floor();
    let partial_char = if remainder >= 0.75 {
        "▊"
    } else if remainder >= 0.5 {
        "▌"
    } else if remainder >= 0.25 {
        "▎"
    } else {
        ""
    };
    let empty = if partial_char.is_empty() {
        10 - full_blocks
    } else {
        9 - full_blocks
    };
    let mut s = "█".repeat(full_blocks);
    s.push_str(partial_char);
    s.push_str(&"░".repeat(empty));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_tokens_falls_back_to_last_ctx_when_turn_starts() {
        // 模拟 turn 刚开始但 API 尚未返回 Usage 事件：
        // turn_in_progress=true 但 turn_usage 全 0，应兜底用 last_ctx。
        let mut state = StatusBarState::default();
        // 先完成一轮：模拟 API 返回了 50000 个 prompt tokens
        state.reset_turn();
        state.turn_usage.input_tokens = 50_000;
        state.finish_turn();
        assert_eq!(state.context_tokens(), 50_000);

        // 新一轮开始（reset_turn 清零 turn_usage），API 尚未响应
        state.reset_turn();
        assert!(state.turn_in_progress);
        assert_eq!(state.turn_usage.context_tokens(), 0);
        // 关键断言：应该用 last_ctx（50_000），而不是 0
        assert_eq!(state.context_tokens(), 50_000);
    }

    #[test]
    fn context_tokens_uses_turn_usage_when_available() {
        // 流式输出收到 API Usage 后，应以最新值为准。
        let mut state = StatusBarState::default();
        state.reset_turn();
        state.last_ctx = 40_000;
        state.turn_usage.input_tokens = 55_000; // API 返回新的 prompt tokens
        assert_eq!(state.context_tokens(), 55_000);
    }

    #[test]
    fn default_state_is_idle() {
        let state = StatusBarState::default();
        assert!(!state.streaming);
        assert_eq!(state.total_tokens(), 0);
    }

    #[test]
    fn reset_turn_marks_streaming() {
        let mut state = StatusBarState::default();
        state.reset_turn();
        assert!(state.streaming);
        assert_eq!(state.turn_usage.total_tokens(), 0);
    }

    #[test]
    fn finish_turn_folds_delta_into_cumulative() {
        let mut state = StatusBarState::default();
        state.reset_turn();
        state.turn_usage.input_tokens = 100;
        state.turn_usage.output_tokens = 50;
        state.finish_turn();
        assert!(!state.streaming);
        assert_eq!(state.cumulative_usage.input_tokens, 100);
        assert_eq!(state.cumulative_usage.output_tokens, 50);
        assert_eq!(state.turn_usage.total_tokens(), 0);
    }

    #[test]
    fn total_tokens_sums_cumulative_and_turn() {
        let mut state = StatusBarState::default();
        state.cumulative_usage.input_tokens = 1000;
        state.turn_usage.input_tokens = 200;
        assert_eq!(state.total_tokens(), 1200);
    }

    #[test]
    fn shorten_model_name_detects_families() {
        assert_eq!(shorten_model_name("claude-opus-4-6-20251014"), "opus");
        assert_eq!(shorten_model_name("claude-sonnet-4-20250514"), "sonnet");
        assert_eq!(shorten_model_name("claude-haiku-4-5-20251001"), "haiku");
        assert_eq!(shorten_model_name("gpt-5-2025-08-07"), "gpt-5");
        assert_eq!(shorten_model_name("gpt-4o-2024-08-06"), "gpt-4o");
        assert_eq!(shorten_model_name("deepseek-chat"), "ds-v4-flash");
        assert_eq!(shorten_model_name("deepseek-reasoner"), "ds-v4-pro");
    }

    #[test]
    fn context_window_returns_correct_size() {
        assert_eq!(context_window_for_model("claude-sonnet-4"), 200_000);
        assert_eq!(context_window_for_model("claude-opus-4-6"), 200_000);
        // DeepSeek V3 (deepseek-chat) has 64K context window
        assert_eq!(context_window_for_model("deepseek-chat"), 64_000);
        // DeepSeek V4 has 1M context window
        assert_eq!(context_window_for_model("deepseek-v4-pro"), 1_000_000);
        assert_eq!(context_window_for_model("deepseek-v4-flash"), 1_000_000);
        assert_eq!(context_window_for_model("unknown-model"), 200_000);
    }

    #[test]
    fn progress_bar_10_correct() {
        assert_eq!(progress_bar_10(0.0), "░░░░░░░░░░");
        assert_eq!(progress_bar_10(100.0), "██████████");
        assert_eq!(progress_bar_10(45.0), "████▌░░░░░");
        assert_eq!(progress_bar_10(72.0), "███████░░░");
        assert_eq!(progress_bar_10(78.0), "███████▊░░");
    }

    #[test]
    fn status_bar_renders_without_panic() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "claude-opus-4-6".to_string(),
            cwd: "~/claw".to_string(),
            turn_count: 12,
            cumulative_usage: TokenUsage {
                input_tokens: 40_000,
                output_tokens: 10_000,
                cache_creation_input_tokens: 5_000,
                cache_read_input_tokens: 45_000,
            },
            ..Default::default()
        };

        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(content.contains("opus"), "should contain model: {content}");
        assert!(content.contains("~/claw"), "should contain cwd: {content}");
        assert!(
            content.contains("#12"),
            "should contain turn count: {content}"
        );
    }

    #[test]
    fn status_bar_shows_streaming_indicator_when_streaming() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "test-model".to_string(),
            streaming: true,
            turn_elapsed_ms: 5000,
            ..Default::default()
        };

        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(content.contains("5s"), "should show elapsed: {content}");
    }

    #[test]
    fn status_bar_shows_git_and_cache_sections() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "deepseek-v4-pro".to_string(),
            cwd: "~/claw".to_string(),
            git_branch: "main".to_string(),
            git_status: "±3".to_string(),
            turn_count: 3,
            cumulative_usage: TokenUsage {
                input_tokens: 1_000,
                output_tokens: 500,
                cache_creation_input_tokens: 1_000, // miss
                cache_read_input_tokens: 9_000,     // hit → 90%
            },
            ..Default::default()
        };
        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(content.contains("main"), "branch: {content}");
        assert!(content.contains("±3"), "git dirty summary: {content}");
        assert!(content.contains("90%"), "cache rate: {content}");
    }

    #[test]
    fn status_bar_hides_git_when_not_in_repo() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "test-model".to_string(),
            cwd: "~".to_string(),
            ..Default::default() // git_branch 为空
        };
        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(!content.contains('⎇'), "no git section: {content}");
        assert!(!content.contains("main"), "no branch: {content}");
    }

    #[test]
    fn status_bar_hides_cache_when_no_cache_usage() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "test-model".to_string(),
            cwd: "~".to_string(),
            cumulative_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            ..Default::default()
        };
        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(!content.contains("缓存"), "no cache section: {content}");
    }

    #[test]
    fn cache_hit_rate_returns_none_without_cache_data() {
        let state = StatusBarState::default();
        assert_eq!(state.cache_hit_rate(), None);
    }

    #[test]
    fn cache_hit_rate_sums_cumulative_and_turn() {
        let mut state = StatusBarState::default();
        state.cumulative_usage.cache_read_input_tokens = 80;
        state.cumulative_usage.cache_creation_input_tokens = 10;
        state.turn_usage.cache_read_input_tokens = 10;
        state.turn_usage.cache_creation_input_tokens = 0;
        // hit = 90, miss = 10 → 0.90
        assert!((state.cache_hit_rate().unwrap() - 0.90).abs() < 1e-9);
    }
}
