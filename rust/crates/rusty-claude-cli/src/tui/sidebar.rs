//! Right-hand sidebar widget for the TUI split layout.
//!
//! Displays contextual information alongside the main output area:
//! - Session metadata (branch, session id, permissions, goal, git status)
//! - Current-turn skill invocations
//! - Current-turn tool call history (name + success/error marker)
//! - Session statistics (message count, success rate, duration)
//! - Live token usage breakdown (input/output/cache hit/cache miss/cache hit rate)
//! - Streaming timer
//!
//! All data is read from a shared `StatusBarState` snapshot plus
//! `ToolHistory` and `SkillHistory` captured by the TUI event loop.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget};

use crate::tui::status_bar::StatusBarState;

/// Snapshot of tool calls for the sidebar.
pub(crate) type ToolHistory = Vec<(String, bool)>;

/// Snapshot of skill invocations for the sidebar (skill_name, is_error).
pub(crate) type SkillHistory = Vec<(String, bool)>;

// ---- layout helpers ----

/// Section allocation: carve an area of `height` rows from `inner`, returning
/// the sub-area and updating `y`. Returns None when height == 0.
fn take_section(y: &mut u16, inner: Rect, height: u16) -> Option<Rect> {
    if height == 0 {
        return None;
    }
    let h = height.min(inner.height.saturating_sub(y.saturating_sub(inner.y)));
    if h == 0 {
        return None;
    }
    let area = Rect {
        x: inner.x,
        y: *y,
        width: inner.width,
        height: h,
    };
    *y = y.saturating_add(h);
    Some(area)
}

// ---- main render entry ----

/// Render the sidebar into `area` using `state` + `tool_history` + `skill_history`.
///
/// `tools_scroll` 控制工具历史段的滚动：
/// - `None`：跟随底部（显示最新 N 条，N = 可见行数）
/// - `Some(n)`：从底部往上偏移 n 行（手动滚动查看更早的工具调用）
pub(crate) fn render_sidebar(
    area: Rect,
    buf: &mut Buffer,
    state: &StatusBarState,
    tool_history: &ToolHistory,
    skill_history: &SkillHistory,
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

    let total_h = inner.height;
    let mut y = inner.y;

    // Section layout (top→bottom):
    //  Session: 5-6 lines (branch, session, permissions, goal?, git)
    //  Skills:  dynamic (if any skill invocations)
    //  Tools:   dynamic
    //  Stats+Usage: remaining → 2 stat lines + usage details

    let session_h = total_h.min(6);
    if let Some(a) = take_section(&mut y, inner, session_h) {
        render_session_section(a, buf, state)
    }

    // Skills section: show only when there are skill invocations
    if !skill_history.is_empty() {
        let skills_visible = skill_history.len().min(6) as u16 + 2; // +2 for border
        let remaining = inner.height.saturating_sub(y.saturating_sub(inner.y));
        if remaining >= 4 {
            // Need at least 4 rows (1 header + 1 item + 2 borders)
            let skills_h = skills_visible.min(remaining);
            if let Some(a) = take_section(&mut y, inner, skills_h) {
                render_skills_section(a, buf, skill_history)
            }
        }
    }

    // Tools section: carve remaining space, leaving at least 9 rows for stats+usage
    let remaining = inner.height.saturating_sub(y.saturating_sub(inner.y));
    let reserve_for_bottom = 12u16; // 10 usage lines + 1 top border + 1 margin
    let tools_h = remaining.saturating_sub(reserve_for_bottom);
    if tools_h > 0 {
        if let Some(a) = take_section(&mut y, inner, tools_h) {
            render_tools_section(a, buf, tool_history, tools_scroll)
        }
    }

    // Stats + Usage section: remaining space
    let remaining = inner.height.saturating_sub(y.saturating_sub(inner.y));
    if remaining > 0 {
        if let Some(a) = take_section(&mut y, inner, remaining) {
            render_usage_section(a, buf, state)
        }
    }
}

// ---- session section ----

fn render_session_section(area: Rect, buf: &mut Buffer, state: &StatusBarState) {
    // 思考强度、经济模式、轮次 已移到底栏，此处不再显示。
    let mut lines = vec![
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
    // Git 工作区状态
    let (git_label, git_color) = if state.git_status.is_empty() {
        ("…".to_string(), Color::DarkGray)
    } else if state.git_status == "clean" {
        ("clean".to_string(), Color::Green)
    } else {
        (state.git_status.clone(), Color::Yellow)
    };
    lines.push(Line::from(vec![
        Span::styled("Git  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            git_label,
            Style::default().fg(git_color).add_modifier(Modifier::BOLD),
        ),
    ]));
    let paragraph = Paragraph::new(lines).alignment(Alignment::Left);
    paragraph.render(area, buf);
}

// ---- skills section (new) ----

fn render_skills_section(area: Rect, buf: &mut Buffer, skill_history: &SkillHistory) {
    let total = skill_history.len();
    let block = Block::default().borders(Borders::TOP).title(Span::styled(
        format!(" 技能 ({total}) "),
        Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    block.render(area, buf);

    let visible = inner.height as usize;
    if visible == 0 {
        return;
    }

    let start = total.saturating_sub(visible);
    let take = total.saturating_sub(start).min(visible);

    let items: Vec<ListItem> = skill_history
        .iter()
        .enumerate()
        .skip(start)
        .take(take)
        .map(|(_i, (name, is_error))| {
            let icon = if *is_error { "✗" } else { "⚡" };
            let color = if *is_error {
                Color::Red
            } else {
                Color::LightBlue
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {icon} "), Style::default().fg(color)),
                Span::raw(name.clone()),
            ]))
        })
        .collect();

    let list = List::new(items);
    list.render(inner, buf);
}

// ---- tools section ----

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

// ---- usage + stats section ----

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

    // 会话时长
    let session_duration = if state.session_start_ms > 0 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let elapsed_ms = now_ms.saturating_sub(state.session_start_ms);
        format_elapsed_ms(elapsed_ms)
    } else {
        "—".to_string()
    };

    // 工具总数与成功率
    let tool_total = state.tool_success_count + state.tool_error_count;
    let success_rate = if tool_total > 0 {
        let rate = (state.tool_success_count as f64 / tool_total as f64) * 100.0;
        format!("{rate:.0}%")
    } else {
        "—".to_string()
    };
    let rate_color = if tool_total == 0 {
        Color::DarkGray
    } else if state.tool_error_count == 0 {
        Color::Green
    } else if state.tool_success_count as f64 / tool_total as f64 >= 0.75 {
        Color::Yellow
    } else {
        Color::Red
    };

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
        // 消息数 + 会话时长（紧凑一行）
        Line::from(vec![
            Span::styled("消息    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "{}轮  时长 {}",
                state.message_count, session_duration
            )),
        ]),
        // 工具统计 + 成功率（紧凑一行）
        Line::from(vec![
            Span::styled("工具    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}次  成功率 ", tool_total)),
            Span::styled(
                &success_rate,
                Style::default().fg(rate_color).add_modifier(Modifier::BOLD),
            ),
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
            Span::styled(
                &hit_rate,
                Style::default()
                    .fg(hit_rate_color)
                    .add_modifier(Modifier::BOLD),
            ),
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
        s.model = "deepseek-v4-pro".to_string();
        s.provider = "deepseek".to_string();
        s.git_branch = "main".to_string();
        s.session_id = "abc-123".to_string();
        s.permission_mode = "workspace-write".to_string();
        s.git_status = "clean".to_string();
        s.session_start_ms = 0; // will show "—" for duration
        s.message_count = 8;
        s.tool_success_count = 10;
        s.tool_error_count = 2;
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
            height: 30,
        });
        let history: ToolHistory = Vec::new();
        let skills: SkillHistory = Vec::new();
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 30,
            },
            &mut buf,
            &state,
            &history,
            &skills,
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
            height: 30,
        });
        let history: ToolHistory = vec![
            ("Read".to_string(), false),
            ("Edit".to_string(), false),
            ("Bash".to_string(), true),
        ];
        let skills: SkillHistory = Vec::new();
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 30,
            },
            &mut buf,
            &state,
            &history,
            &skills,
            None,
        );
    }

    #[test]
    fn render_sidebar_does_not_panic_with_skills() {
        let state = make_state();
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 30,
        });
        let history: ToolHistory = Vec::new();
        let skills: SkillHistory = vec![
            ("plan-mode".to_string(), false),
            ("refactor".to_string(), false),
        ];
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 30,
            },
            &mut buf,
            &state,
            &history,
            &skills,
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
            height: 30,
        });
        let history: ToolHistory = Vec::new();
        let skills: SkillHistory = Vec::new();
        render_sidebar(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 30,
            },
            &mut buf,
            &state,
            &history,
            &skills,
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
        let skills: SkillHistory = vec![("test-skill".to_string(), false)];
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
            &skills,
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

    #[test]
    fn usage_section_shows_stats_lines() {
        let state = make_state();
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 16,
        });
        let usage_area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 16,
        };
        render_usage_section(usage_area, &mut buf, &state);
        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("8轮"),
            "should show message count: {content}"
        );
        assert!(
            content.contains("12次"),
            "should show tool total: {content}"
        );
        assert!(
            content.contains("83%"),
            "should show success rate: {content}"
        );
    }
}
