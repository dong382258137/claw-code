//! Right-hand sidebar widget for the TUI split layout.
//!
//! Displays contextual information alongside the main output area:
//! - Session metadata (reasoning effort, branch, session id, permissions, goal, turns)
//! - Current-turn tool call history (name + success/error marker)
//! - Live token usage breakdown (input/output/cache hit/cache miss/cache hit rate)
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
///
/// `tools_scroll` 控制工具历史段的滚动：
/// - `None`：跟随底部（显示最新 N 条，N = 可见行数）
/// - `Some(n)`：从底部往上偏移 n 行（手动滚动查看更早的工具调用）
pub(crate) fn render_sidebar(
    area: Rect,
    buf: &mut Buffer,
    state: &StatusBarState,
    tool_history: &ToolHistory,
    tools_scroll: Option<usize>,
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
    // session 删除模型+提供商行(-2), usage 扩展缓存拆3行(+1), 成本移到底栏
    let total_h = inner.height;
    let session_h = total_h.min(7);
    let usage_h = total_h.saturating_sub(session_h).min(9);
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
            tools_scroll,
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
    let effort_label = state.reasoning_effort.as_deref().unwrap_or("默认");
    let effort_color = match effort_label {
        "low" => Color::Blue,
        "medium" => Color::Yellow,
        "high" => Color::Red,
        _ => Color::DarkGray,
    };
    // model/provider 已移到底栏，此处不再显示。
    let mut lines = vec![
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

fn render_tools_section(
    area: Rect,
    buf: &mut Buffer,
    tool_history: &ToolHistory,
    tools_scroll: Option<usize>,
) {
    let total = tool_history.len();
    let scroll_up_hidden = tools_scroll.unwrap_or(0);
    let title = if scroll_up_hidden > 0 {
        format!(" 工具 ({total}) ↑{scroll_up_hidden} ")
    } else {
        format!(" 工具 ({total}) ")
    };
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

    let visible = inner.height as usize;
    let total = tool_history.len();
    let (start, _) = if total <= visible {
        (0, total)
    } else {
        let scroll = tools_scroll.unwrap_or(0).min(total - visible);
        let start = total - visible - scroll;
        (start, visible)
    };

    let items: Vec<ListItem> = tool_history
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
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

    // 缓存统计 (命中= cache_read, 未命中= cache_creation)
    let hit_total = (cum.cache_read_input_tokens as u64) + (turn.cache_read_input_tokens as u64);
    let miss_total =
        (cum.cache_creation_input_tokens as u64) + (turn.cache_creation_input_tokens as u64);
    let cache_sum = hit_total + miss_total;
    let hit_rate = if cache_sum > 0 {
        format!("{:.1}%", (hit_total as f64 / cache_sum as f64) * 100.0)
    } else {
        "—".to_string()
    };
    let hit_rate_color = if cache_sum == 0 {
        Color::DarkGray
    } else if hit_total as f64 / cache_sum as f64 >= 0.85 {
        Color::Green
    } else if hit_total as f64 / cache_sum as f64 >= 0.60 {
        Color::Yellow
    } else {
        Color::Red
    };

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
            Span::styled("命中缓存", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} tokens", hit_total)),
        ]),
        Line::from(vec![
            Span::styled("未命中  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} tokens", miss_total)),
        ]),
        Line::from(vec![
            Span::styled("命中率  ", Style::default().fg(Color::DarkGray)),
            Span::styled(&hit_rate, Style::default().fg(hit_rate_color).add_modifier(Modifier::BOLD)),
        ]),
    ];
    let block = Block::default().borders(Borders::TOP);
    let inner = block.inner(area);
    block.render(area, buf);
    let p = Paragraph::new(lines);
    p.render(inner, buf);
}

fn format_in_out(
    cum: &runtime::TokenUsage,
    turn: &runtime::TokenUsage,
    is_input: bool,
) -> String {
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
            None,
        );
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
            None,
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
            None,
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
            None,
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
}
