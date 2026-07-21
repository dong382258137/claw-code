//! Right-hand sidebar widget for the TUI split layout.
//!
//! Displays contextual information alongside the main output area:
//! - Session metadata (id, model, provider, branch, cwd)
//! - Current-turn tool call history (name + success/error marker)
//! - Live token usage breakdown (input/output/cache) and estimated cost
//! - Streaming timer
//!
//! All data is read from a shared `StatusBarState` snapshot plus an
//! optional `ToolHistory` (Vec of (tool_name, is_error)) captured by the
//! TUI event loop during the current turn.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget};

use crate::tui::status_bar::StatusBarState;

/// Snapshot of tool calls for the sidebar.
pub(crate) type ToolHistory = Vec<(String, bool)>;

/// Render the sidebar into `area` using `state` + `tool_history`.
pub(crate) fn render_sidebar(
    area: Rect,
    buf: &mut Buffer,
    state: &StatusBarState,
    tool_history: &ToolHistory,
) {
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " 侧栏 ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    block.render(area, buf);

    // Split inner area into three stacked sections: Session, Tools, Usage.
    // Use fixed-ish proportions: usage section gets priority since it has
    // the most variable content.
    let total_h = inner.height;
    // Reserve 9 lines for session (新增"轮次"行), 8 for usage, rest for tools.
    let session_h = total_h.min(9);
    let usage_h = total_h.saturating_sub(session_h).min(8);
    let tools_h = total_h.saturating_sub(session_h + usage_h);

    let mut y = inner.y;
    render_session_section(
        Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: session_h,
        },
        buf,
        state,
    );
    y = y.saturating_add(session_h);

    if tools_h > 0 {
        render_tools_section(
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: tools_h,
            },
            buf,
            tool_history,
        );
        y = y.saturating_add(tools_h);
    }

    if usage_h > 0 {
        render_usage_section(
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: usage_h,
            },
            buf,
            state,
        );
    }
}

fn render_session_section(area: Rect, buf: &mut Buffer, state: &StatusBarState) {
    // 思考强度显示：None 显示"默认"，Some 显示具体值（low/medium/high）。
    // 用颜色区分：默认=灰色，low=蓝色，medium=黄色，high=红色（强度越高越醒目）。
    let effort_label = state.reasoning_effort.as_deref().unwrap_or("默认");
    let effort_color = match effort_label {
        "low" => Color::Blue,
        "medium" => Color::Yellow,
        "high" => Color::Red,
        _ => Color::DarkGray,
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("模型 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.model),
        ]),
        Line::from(vec![
            Span::styled("提供商 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.provider),
        ]),
        Line::from(vec![
            Span::styled("思考强度 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                effort_label,
                Style::default()
                    .fg(effort_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("分支 ", Style::default().fg(Color::DarkGray)),
            Span::raw(if state.git_branch.is_empty() {
                "（无）"
            } else {
                &state.git_branch
            }),
        ]),
        Line::from(vec![
            Span::styled("会话 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.session_id),
        ]),
        Line::from(vec![
            Span::styled("权限 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.permission_mode),
        ]),
    ];
    // 目标行：仅在设置了 goal 时显示，避免无 goal 时长期显示"（无）"造成噪音。
    if !state.goal_badge.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("目标 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.goal_badge),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("经济模式 ", Style::default().fg(Color::DarkGray)),
        Span::raw(if state.poor_mode { "启用" } else { "关闭" }),
    ]));
    // 新增：累计思考轮次统计（每个 turn +1）
    lines.push(Line::from(vec![
        Span::styled("轮次 ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} 累计", state.turn_count),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    let paragraph = Paragraph::new(lines).alignment(Alignment::Left);
    paragraph.render(area, buf);
}

fn render_tools_section(area: Rect, buf: &mut Buffer, tool_history: &ToolHistory) {
    let title = format!(" 工具 ({}) ", tool_history.len());
    let block = Block::default().borders(Borders::TOP).title(Span::styled(
        title,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    block.render(area, buf);

    if tool_history.is_empty() {
        let p = Paragraph::new(vec![Line::from(Span::styled(
            "  （暂无工具调用）",
            Style::default().fg(Color::DarkGray),
        ))]);
        p.render(inner, buf);
        return;
    }

    // Show most-recent-last (natural reading order); cap to available lines.
    let items: Vec<ListItem> = tool_history
        .iter()
        .enumerate()
        .map(|(i, (name, is_error))| {
            let icon = if *is_error { "x" } else { "v" };
            let color = if *is_error { Color::Red } else { Color::Green };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {icon} "), Style::default().fg(color)),
                Span::raw(format!("{i:>2}. {name}")),
            ]))
        })
        .collect();
    let list = List::new(items);
    list.render(inner, buf);
}

fn render_usage_section(area: Rect, buf: &mut Buffer, state: &StatusBarState) {
    let turn = &state.turn_usage;
    let cum = &state.cumulative_usage;

    // BUG 5 fix: use the authoritative pricing table (runtime::pricing_for_model)
    // instead of hardcoded Sonnet-class rates. Previously the sidebar showed
    // wrong costs for Opus / Haiku / third-party providers. The status bar
    // (status_bar.rs) already did this; the sidebar now matches.
    let pricing = runtime::pricing_for_model(&state.model);
    let cost = runtime::format_usd(
        estimated_cost(cum, pricing.as_ref()) + estimated_cost(turn, pricing.as_ref()),
    );
    let timer = if state.streaming {
        format_elapsed_ms(state.turn_elapsed_ms)
    } else {
        "空闲".to_string()
    };
    let streaming_label = if state.streaming { "流式" } else { "空闲" };

    let lines = vec![
        Line::from(Span::styled(
            " 用量 ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("状态    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                streaming_label,
                Style::default().fg(if state.streaming {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::raw("  "),
            Span::styled(&timer, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("令牌    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} 总计", state.total_tokens())),
        ]),
        Line::from(vec![
            Span::styled("  输入  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_in_out(cum, turn, true)),
        ]),
        Line::from(vec![
            Span::styled("  输出  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_in_out(cum, turn, false)),
        ]),
        Line::from(vec![
            Span::styled("  缓存  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "+{} / 读{} ",
                cum.cache_creation_input_tokens + turn.cache_creation_input_tokens,
                cum.cache_read_input_tokens + turn.cache_read_input_tokens,
            )),
            Span::styled(
                format_cache_hit_rate(cum, turn),
                cache_hit_rate_style(cum, turn),
            ),
        ]),
        Line::from(vec![
            Span::styled("成本    ", Style::default().fg(Color::DarkGray)),
            Span::styled(cost, Style::default().fg(Color::Green)),
        ]),
    ];
    let block = Block::default().borders(Borders::TOP);
    let inner = block.inner(area);
    block.render(area, buf);
    let p = Paragraph::new(lines);
    p.render(inner, buf);
}

fn format_in_out(cum: &runtime::TokenUsage, turn: &runtime::TokenUsage, is_input: bool) -> String {
    let c = if is_input {
        cum.input_tokens
    } else {
        cum.output_tokens
    };
    let t = if is_input {
        turn.input_tokens
    } else {
        turn.output_tokens
    };
    if t > 0 {
        format!("{c} (+{t})")
    } else {
        format!("{c}")
    }
}

/// 计算缓存命中率文本 (含括号),如 "(95.2%)"。
/// 命中率 = hit / (hit + miss) * 100。
/// 当 hit + miss = 0 (尚无缓存数据) 时返回 "(—)"。
fn format_cache_hit_rate(cum: &runtime::TokenUsage, turn: &runtime::TokenUsage) -> String {
    let hit = (cum.cache_read_input_tokens as u64) + (turn.cache_read_input_tokens as u64);
    let miss = (cum.cache_creation_input_tokens as u64) + (turn.cache_creation_input_tokens as u64);
    let total = hit + miss;
    if total == 0 {
        return "(—)".to_string();
    }
    let rate = (hit as f64 / total as f64) * 100.0;
    format!("({rate:.1}%)")
}

/// 命中率颜色:>=85% 绿色,60-85% 黄色,<60% 红色,无数据灰色。
/// DeepSeek 文档建议命中率 >=85% 为良好(对应 input 价格的 1/20 计费)。
fn cache_hit_rate_style(cum: &runtime::TokenUsage, turn: &runtime::TokenUsage) -> Style {
    let hit = (cum.cache_read_input_tokens as u64) + (turn.cache_read_input_tokens as u64);
    let miss = (cum.cache_creation_input_tokens as u64) + (turn.cache_creation_input_tokens as u64);
    let total = hit + miss;
    if total == 0 {
        return Style::default().fg(Color::DarkGray);
    }
    let rate = (hit as f64 / total as f64) * 100.0;
    if rate >= 85.0 {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if rate >= 60.0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Red)
    }
}

/// Cost estimate (USD) for a single usage delta, using the model's pricing
/// table when available. Falls back to `ModelPricing::default_sonnet_tier`
/// when `pricing_for_model` returns None (unknown model).
///
/// BUG 5 fix: previously this function hard-coded $3/M input, $15/M output,
/// $3.75/M cache write, $0.30/M cache read (Sonnet prices), producing wrong
/// costs for Opus / Haiku / third-party providers. It now accepts an optional
/// `ModelPricing` and delegates to `TokenUsage::estimate_cost_usd_with_pricing`
/// — the same path used by `status_bar.rs::estimate_cost` and the JSON output
/// in `run_prompt_json`.
fn estimated_cost(usage: &runtime::TokenUsage, pricing: Option<&runtime::ModelPricing>) -> f64 {
    match pricing {
        Some(p) => usage.estimate_cost_usd_with_pricing(*p),
        None => usage.estimate_cost_usd(),
    }
    .total_cost_usd()
}

fn format_elapsed_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m{s:>02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    #[allow(clippy::field_reassign_with_default)]
    fn make_state() -> StatusBarState {
        let mut s = StatusBarState::default();
        s.model = "claude-sonnet-4-6".to_string();
        s.provider = "Anthropic".to_string();
        s.git_branch = "main".to_string();
        s.session_id = "abc-123".to_string();
        s.permission_mode = "workspace-write".to_string();
        s.cumulative_usage = runtime::TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 200,
            cache_read_input_tokens: 800,
        };
        s.streaming = false;
        s
    }

    #[test]
    fn render_sidebar_does_not_panic_with_empty_history() {
        let state = make_state();
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        });
        let history: ToolHistory = Vec::new();
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 20,
            },
            &mut buf,
            &state,
            &history,
        );
        // Just verifying no panic; content checks would require inspecting buf.
    }

    #[test]
    fn render_sidebar_does_not_panic_with_tools() {
        let state = make_state();
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        });
        let history: ToolHistory = vec![
            ("Read".to_string(), false),
            ("Edit".to_string(), false),
            ("Bash".to_string(), true),
        ];
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 20,
            },
            &mut buf,
            &state,
            &history,
        );
    }

    #[test]
    fn render_sidebar_does_not_panic_when_streaming() {
        let mut state = make_state();
        state.streaming = true;
        state.turn_elapsed_ms = 75_000;
        state.turn_usage = runtime::TokenUsage {
            input_tokens: 200,
            output_tokens: 100,
            ..Default::default()
        };
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        });
        let history: ToolHistory = Vec::new();
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 20,
            },
            &mut buf,
            &state,
            &history,
        );
    }

    #[test]
    fn render_sidebar_handles_tiny_area() {
        let state = make_state();
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        });
        // Should not panic even when there's no room to render all sections.
        let history: ToolHistory = vec![("Edit".to_string(), false)];
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 3,
            },
            &mut buf,
            &state,
            &history,
        );
    }

    #[test]
    fn format_elapsed_ms_formats_correctly() {
        assert_eq!(format_elapsed_ms(0), "0s");
        assert_eq!(format_elapsed_ms(999), "0s");
        assert_eq!(format_elapsed_ms(1000), "1s");
        assert_eq!(format_elapsed_ms(59_999), "59s");
        assert_eq!(format_elapsed_ms(60_000), "1m00s");
        assert_eq!(format_elapsed_ms(125_000), "2m05s");
    }

    #[test]
    fn estimated_cost_scales_with_usage() {
        // BUG 5 fix: estimated_cost now takes an optional ModelPricing.
        // Without pricing (None) it falls back to TokenUsage::estimate_cost_usd
        // which uses the runtime crate's default rates ($15/$75/$18.75/$1.5
        // per M tokens — see runtime/src/usage.rs DEFAULT_*_COST_PER_MILLION).
        let usage = runtime::TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
        };
        let cost = estimated_cost(&usage, None);
        // 15 + 75 + 18.75 + 1.5 = 110.25
        assert!((cost - 110.25).abs() < 0.001);
    }

    #[test]
    fn estimated_cost_uses_provided_pricing() {
        // BUG 5 regression test: when pricing is provided, cost must reflect
        // those rates (not the runtime crate's defaults).
        let custom_pricing = runtime::ModelPricing {
            input_cost_per_million: 3.0,
            output_cost_per_million: 15.0,
            cache_creation_cost_per_million: 3.75,
            cache_read_cost_per_million: 0.30,
        };
        let usage = runtime::TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
        };
        let cost_with_custom = estimated_cost(&usage, Some(&custom_pricing));
        // With custom pricing: 3 + 15 + 3.75 + 0.30 = 22.05
        assert!((cost_with_custom - 22.05).abs() < 0.001);
        // And it must differ from the default-rate cost (110.25).
        let cost_with_none = estimated_cost(&usage, None);
        assert!((cost_with_none - 110.25).abs() < 0.001);
        assert!((cost_with_custom - cost_with_none).abs() > 0.001);
    }

    #[test]
    fn format_in_output_shows_delta_only_when_nonzero() {
        let cum = runtime::TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Default::default()
        };
        let turn_zero = runtime::TokenUsage::default();
        let turn_nonzero = runtime::TokenUsage {
            input_tokens: 200,
            output_tokens: 100,
            ..Default::default()
        };
        assert_eq!(format_in_out(&cum, &turn_zero, true), "1000");
        assert_eq!(format_in_out(&cum, &turn_nonzero, true), "1000 (+200)");
        assert_eq!(format_in_out(&cum, &turn_zero, false), "500");
        assert_eq!(format_in_out(&cum, &turn_nonzero, false), "500 (+100)");
    }

    #[test]
    fn format_cache_hit_rate_handles_zero_total() {
        // 无缓存数据时显示 "—"
        let cum = runtime::TokenUsage::default();
        let turn = runtime::TokenUsage::default();
        assert_eq!(format_cache_hit_rate(&cum, &turn), "(—)");
    }

    #[test]
    fn format_cache_hit_rate_computes_percentage() {
        // DeepSeek 场景:hit=49500, miss=500 → 命中率 99.0%
        let cum = runtime::TokenUsage {
            input_tokens: 0,
            cache_creation_input_tokens: 500, // miss
            cache_read_input_tokens: 49500,   // hit
            ..Default::default()
        };
        let turn = runtime::TokenUsage::default();
        assert_eq!(format_cache_hit_rate(&cum, &turn), "(99.0%)");
    }

    #[test]
    fn format_cache_hit_rate_sums_cumulative_and_turn() {
        // 累计 + 当前 turn 共同计算
        let cum = runtime::TokenUsage {
            cache_creation_input_tokens: 1000, // miss
            cache_read_input_tokens: 9000,     // hit
            ..Default::default()
        };
        let turn = runtime::TokenUsage {
            cache_creation_input_tokens: 500, // miss
            cache_read_input_tokens: 500,     // hit
            ..Default::default()
        };
        // 命中率 = (9000 + 500) / (1000 + 9000 + 500 + 500) = 9500 / 11000 ≈ 86.4%
        assert_eq!(format_cache_hit_rate(&cum, &turn), "(86.4%)");
    }

    #[test]
    fn cache_hit_rate_style_returns_correct_color() {
        use ratatui::style::Color;

        // 无数据 → 灰色
        let empty = runtime::TokenUsage::default();
        let style = cache_hit_rate_style(&empty, &empty);
        assert_eq!(style.fg, Some(Color::DarkGray));

        // 高命中率 (>=85%) → 绿色 + 粗体
        let high_hit = runtime::TokenUsage {
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 900,
            ..Default::default()
        };
        let style = cache_hit_rate_style(&high_hit, &empty);
        assert_eq!(style.fg, Some(Color::Green));

        // 中等命中率 (60-85%) → 黄色
        let mid_hit = runtime::TokenUsage {
            cache_creation_input_tokens: 400,
            cache_read_input_tokens: 600,
            ..Default::default()
        };
        let style = cache_hit_rate_style(&mid_hit, &empty);
        assert_eq!(style.fg, Some(Color::Yellow));

        // 低命中率 (<60%) → 红色
        let low_hit = runtime::TokenUsage {
            cache_creation_input_tokens: 800,
            cache_read_input_tokens: 200,
            ..Default::default()
        };
        let style = cache_hit_rate_style(&low_hit, &empty);
        assert_eq!(style.fg, Some(Color::Red));
    }
}
