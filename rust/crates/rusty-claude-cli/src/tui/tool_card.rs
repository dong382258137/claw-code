#![cfg(feature = "full-tui")]

//! Collapsible tool call card rendering for the TUI output view.
//!
//! When a tool is invoked, a card header is rendered (tool name + input summary).
//! When the tool result arrives, a collapsible body is rendered: if the output
//! exceeds `COLLAPSE_THRESHOLD` lines, only the first few lines + an expand
//! hint are shown; otherwise the full output is displayed.

/// Default threshold: outputs with more than this many lines are collapsed.
/// P1 修复：从 15 降到 5，更激进地折叠工具输出，避免长输出占满输出区。
const COLLAPSE_THRESHOLD: usize = 5;
/// Number of lines to show when collapsed.
const COLLAPSED_PREVIEW_LINES: usize = 3;
/// P1 修复:语法高亮降级阈值。超过此行数的输出不进行语法高亮,
/// 直接显示纯文本。根因:syntect 对 JSON/Rust 等语言高亮会为每个
/// token 生成 ANSI 颜色序列,长输出(如 `cargo test --workspace` 的
/// JSON 结果)会产生数千个 ANSI 序列,经 crossterm 反射为键盘事件
/// 会污染 InputLine。降级为纯文本可从根本上消除密集 ANSI 序列。
const SYNTAX_HIGHLIGHT_MAX_LINES: usize = 50;

/// 展开视图最大行数（P0 修复 2026-08-04：展开+截断，兼顾可见性与空间）。
/// P1 截头部；P0（错误）截尾部——错误摘要/失败列表常在输出末尾。
const MAX_EXPANDED_LINES: usize = 60;

/// Render a tool call start card (header only, result pending).
/// P1 修复：start 卡片只显示一行 header，不显示 diff 和 running 状态。
/// 原因：start 卡片中的 `├─ ⏳ running...\n` 会在 result 到来后仍留在
/// buffer 中无法替换，导致输出区累积大量"running"残留。改为只显示
/// 一行 header，等 result 到来时再显示完整卡片（含 diff/输出）。
/// 对于 edit_file，diff 在 result 卡片中显示。
pub(crate) fn render_tool_call_start(name: &str, input: &str) -> String {
    let summary = summarize_tool_input(name, input);
    format!("\n┌─ 🔧 {name} {summary} ⏳\n")
}

/// Render a colored unified diff for an edit_file tool call.
/// Reads `old_string` and `new_string` from the input JSON.
///
/// P1 升级:从逐行比较改为 Myers 算法(经 `tui_ports::diff_view`)。
/// 原逐行比较无法识别位置偏移的相同行,会把"行顺序变了"误判为全删+全增;
/// Myers 能正确识别上下文行(Equal),diff 更短更易读。
fn render_edit_diff(input: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(input).ok()?;
    let old = parsed
        .get("old_string")
        .or_else(|| parsed.get("oldString"))
        .and_then(|v| v.as_str())?;
    let new = parsed
        .get("new_string")
        .or_else(|| parsed.get("newString"))
        .and_then(|v| v.as_str())?;

    if old == new {
        return None;
    }

    // Myers diff via tui_ports::diff_view(从 grok-build 移植)。
    // start_line=1:tool_card 不显示行号,起始行号无意义,传 1 即可。
    let hunks = crate::tui_ports::diff_view::diff_hunks_from_strings(old, new, 1);
    if hunks.is_empty() {
        return None;
    }
    Some(crate::tui_ports::diff_view::render_hunks_ansi(&hunks))
}

/// Render a tool result card (collapsible, priority-aware).
///
/// 折叠语义（按 priority 分档，详见 docs/tui-output-intelligence-plan.md §3.2）：
/// - `Priority::P0`：永不折叠，即使 collapsed=true 也强制完整展开（error/关键发现）。
/// - `Priority::P3`：L1 单行摘要，不显示预览（成功确认/interrupted）。
/// - `Priority::P1/P2` + `collapsed`：**单行标题折叠**(P0 改进,原为前3行预览)。
///   工具输出是"噪音",折叠时只占 1 行,把 viewport 空间留给 AI 回复这个"信号"。
/// - `Priority::P1/P2` + `!collapsed`：完整视图。
///
/// 对 edit_file 工具，在 result 卡片中显示 diff（原 start 卡片中的 diff 已移除）。
pub(crate) fn render_tool_result(
    name: &str,
    output: &str,
    is_error: bool,
    input: Option<&str>,
    collapsed: bool,
    priority: crate::tui::output_view::Priority,
) -> String {
    use crate::tui::output_view::Priority;

    let icon = if is_error { "❌" } else { "✅" };

    // For edit_file, prepend a diff preview before the result body
    let diff_prefix = if (name == "edit_file" || name == "Edit") && !is_error {
        input.and_then(render_edit_diff).unwrap_or_default()
    } else {
        String::new()
    };
    // P0 修复(2026-08-04):行数与正文基于提取后的真实内容,而非 pretty JSON 信封。
    // 信封行数会导致"3 行 stdout 显示 38 行 JSON、恒被折叠"。
    let body = extract_tool_output_body(name, output);
    let line_count = body.lines().count();

    // P3：L1 单行摘要（不显示预览，只显示语义摘要）
    if priority == Priority::P3 && collapsed {
        let summary = if let Some(inp) = input {
            summarize_tool_result(name, inp, output, is_error)
        } else {
            format!("📦 {name} · {line_count}行")
        };
        return format!("{diff_prefix}├─ {icon} {name} · {summary}\n└─\n");
    }

    // P0：永不折叠（即使 collapsed=true 也强制完整展开）
    let effective_collapsed = collapsed && priority != Priority::P0;

    // P0 改进(2026-08-01):折叠时只显示单行标题,不再显示 3 行预览。
    // 原折叠预览占 5-6 行(标题+3预览+展开提示+尾行),工具链一长就占满 viewport,
    // 把 AI 回复挤出可见区。改为单行标题(├─ ✅ name (N 行,折叠)) + 尾行,
    // 只占 2 行,用户 Tab 展开看详情。
    // 参考 grok-build 设计:工具输出默认 Collapsed(1 行标题),AgentMessage 永不折叠。
    if effective_collapsed && line_count > COLLAPSE_THRESHOLD {
        return format!(
            "{diff_prefix}├─ {icon} {name} ({line_count} 行，折叠)\n└─\n"
        );
    }

    // Determine if this tool's output should be syntax-highlighted
    let language = detect_language_for_tool(name, &body);
    // P1 修复:超长输出降级为纯文本,避免密集 ANSI 序列反射污染 InputLine。
    let highlight_enabled = line_count <= SYNTAX_HIGHLIGHT_MAX_LINES;
    let highlighted_body = |slice: &[&str]| -> String {
        if !highlight_enabled {
            // 降级:纯文本,不做语法高亮
            return slice.iter().map(|l| format!("│ {l}\n")).collect();
        }
        let text = slice.join("\n");
        if let Some(lang) = language {
            // P0-4 修复:用进程级单例,不再每次 ToolCard 渲染都加载语法集。
            // 原 `TerminalRenderer::new()` 每次都触发 SyntaxSet::load_defaults_newlines()
            // + ThemeSet::load_defaults(),数十 ms/次,是 ToolCard 渲染的核心瓶颈。
            let renderer = crate::render::TerminalRenderer::shared();
            let highlighted = renderer.highlight_code(&text, &lang);
            // Add card prefix to each line
            highlighted.lines().map(|l| format!("│ {l}\n")).collect()
        } else {
            slice.iter().map(|l| format!("│ {l}\n")).collect()
        }
    };

    if effective_collapsed && line_count > COLLAPSE_THRESHOLD {
        // 折叠预览视图：只取前几行,不全量 collect
        let preview_lines: Vec<&str> = output
            .lines()
            .take(COLLAPSED_PREVIEW_LINES.min(line_count))
            .collect();
        let preview: String = highlighted_body(&preview_lines);
        let hidden = line_count - COLLAPSED_PREVIEW_LINES;
        format!(
            "{diff_prefix}├─ {icon} {name} ({line_count} 行，+{hidden} 行已折叠)\n{preview}├─ [+] 展开（还有 {hidden} 行）\n└─\n"
        )
    } else if line_count == 0 {
        format!("{diff_prefix}├─ {icon} {name} (空)\n└─\n")
    } else {
        // 完整视图：基于提取后的真实内容；超 60 行截断（展开+截断）。
        // P0(错误)截尾部——失败摘要常在末尾；其余截头部。
        let lines: Vec<&str> = body.lines().collect();
        let total = lines.len();
        if total > MAX_EXPANDED_LINES {
            let (shown, hidden) = if priority == Priority::P0 {
                (&lines[total - MAX_EXPANDED_LINES..], total - MAX_EXPANDED_LINES)
            } else {
                (&lines[..MAX_EXPANDED_LINES], total - MAX_EXPANDED_LINES)
            };
            let body_str = highlighted_body(shown);
            format!(
                "{diff_prefix}├─ {icon} {name} ({total} 行，截断)\n{body_str}│ …（其余 {hidden} 行省略）\n└─\n"
            )
        } else {
            let body_str = highlighted_body(&lines);
            format!("{diff_prefix}├─ {icon} {name} ({total} 行)\n{body_str}└─\n")
        }
    }
}

/// 从工具结果 JSON 信封中提取面向用户的真实内容。
///
/// 背景（P0 修复 2026-08-04）：所有工具结果经 `serde_json::to_string_pretty`
/// 序列化为 pretty JSON 信封（bash 17 字段 + sandboxStatus，3 行 stdout 会膨胀
/// 成 38 行 JSON）。折叠判定与渲染此前直接统计/展示信封，导致"输出内容被当成
/// 工具输出折叠"。本函数按工具结构提取真实内容；未知工具/非 JSON 回退原始输出。
///
/// 字段路径与 `src/tool_display.rs` 的 format_*_result 保持一致（同一份 schema）。
fn extract_tool_output_body(name: &str, output: &str) -> String {
    use serde_json::Value;
    match name {
        // bash：stdout 是主体；stderr 非空时追加（错误常在 stderr）
        "bash" | "Bash" => match serde_json::from_str::<Value>(output) {
            Ok(v) => {
                let stdout = v.get("stdout").and_then(|s| s.as_str()).unwrap_or("");
                let stderr = v.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
                if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    format!("{stdout}\n[stderr]\n{stderr}")
                }
            }
            Err(_) => output.to_string(),
        },
        // read_file：内容是答案
        "read_file" | "Read" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("file")
                .and_then(|f| f.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => output.to_string(),
        },
        // grep_search：content 是预格式化匹配文本
        "grep_search" | "Grep" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => output.to_string(),
        },
        // glob_search：文件名列表（分类器给 P3，这里提取以备不时之需）
        "glob_search" | "Glob" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("filenames")
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            Err(_) => output.to_string(),
        },
        // WebFetch：result 是正文
        "WebFetch" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => output.to_string(),
        },
        // write/edit：纯确认 JSON → 空；若尾部带 cargo check 输出（JSON 解析
        // 失败），提取 cargo check 文本——编译错误是信号，不能折叠掉。
        "write_file" | "Write" | "edit_file" | "Edit" => {
            match serde_json::from_str::<Value>(output) {
                Ok(_) => String::new(),
                Err(_) => output
                    .split_once("--- cargo check ---")
                    .map(|(_, tail)| tail.trim_start().to_string())
                    .unwrap_or_default(),
            }
        }
        // WebSearch/未知工具：结构异构，回退原始输出
        _ => output.to_string(),
    }
}

/// 公开接口：供 `output_view.rs` 的 compute_priority 调用（沿用 P1 重构的
pub(crate) fn extract_tool_output_body_public(name: &str, output: &str) -> String {
    extract_tool_output_body(name, output)
}

/// Detect the syntax highlighting language for a tool's output.
/// Detect the syntax highlighting language for a tool's output.
/// Returns None if no highlighting should be applied.
fn detect_language_for_tool(tool_name: &str, output: &str) -> Option<String> {
    match tool_name {
        "bash" | "Bash" => {
            // Bash output: try to detect if it looks like a stack trace or JSON
            if output.trim_start().starts_with('{') || output.trim_start().starts_with('[') {
                Some("json".to_string())
            } else {
                None // Plain shell output, no highlighting
            }
        }
        "read_file" | "Read" => {
            // File content: detect by shebang or common patterns
            if output.starts_with("#!") {
                Some("bash".to_string())
            } else if output.contains("fn ") && output.contains("pub ") {
                Some("rust".to_string())
            } else if output.contains("def ") && output.contains("import ") {
                Some("python".to_string())
            } else if output.contains("function") && output.contains("var ") {
                Some("javascript".to_string())
            } else {
                None
            }
        }
        "edit_file" | "Edit" | "write_file" | "Write" => {
            // Diff or file content
            if output.starts_with("---") || output.starts_with("@@") {
                Some("diff".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Render a tool call timeline summary.
/// Format: `🔧 bash → ✓ | read_file → ✓ | edit_file → ✓ (3 tools)`
pub(crate) fn render_tool_timeline(history: &[(String, bool)]) -> String {
    if history.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for (name, is_error) in history {
        let icon = if *is_error { "✗" } else { "✓" };
        let colored = if *is_error {
            format!("\x1b[31m{name} → {icon}\x1b[0m")
        } else {
            format!("\x1b[32m{name} → {icon}\x1b[0m")
        };
        parts.push(colored);
    }
    let count = history.len();
    let tool_word = "个工具";
    format!(
        "\n┌─ 🔧 时间线：{}（{count} {tool_word}）\n└─\n",
        parts.join(" | ")
    )
}

/// Summarize tool input to a short one-liner for the card header.
/// P1 重构：公开接口供 OutputView::render() 调用。
pub(crate) fn summarize_tool_input_public(name: &str, input: &str) -> String {
    summarize_tool_input(name, input)
}

/// Render a tool result card (collapsible).
/// P1 重构：公开接口供 OutputView::render() 调用。
/// `collapsed` 参数控制折叠预览/完整展开两种视图。
/// `priority` 参数决定 P0 永不折叠 / P3 单行摘要（详见方案 §3.2）。
pub(crate) fn render_tool_result_public(
    name: &str,
    output: &str,
    is_error: bool,
    input: Option<&str>,
    collapsed: bool,
    priority: crate::tui::output_view::Priority,
) -> String {
    render_tool_result(name, output, is_error, input, collapsed, priority)
}

/// Summarize tool input to a short one-liner for the card header.
fn summarize_tool_input(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));

    match name {
        "bash" | "Bash" => {
            let cmd = parsed
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let cmd_short = truncate_str(cmd, 60);
            format!("`{cmd_short}`")
        }
        "read_file" | "Read" => {
            let path = parsed
                .get("file_path")
                .or_else(|| parsed.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("📄 {path}")
        }
        "write_file" | "Write" => {
            let path = parsed
                .get("file_path")
                .or_else(|| parsed.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let lines = parsed
                .get("content")
                .and_then(|v| v.as_str())
                .map_or(0, |c| c.lines().count());
            format!("✏️ {path} ({lines} lines)")
        }
        "edit_file" | "Edit" => {
            let path = parsed
                .get("file_path")
                .or_else(|| parsed.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("✏️ {path}")
        }
        "grep" | "Grep" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("🔎 `{pattern}`")
        }
        "glob" | "Glob" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("🌐 `{pattern}`")
        }
        _ => {
            // Generic: show first key-value pair
            if let Some(obj) = parsed.as_object() {
                if let Some((k, v)) = obj.iter().next() {
                    let v_str = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}={}", truncate_str(&v_str, 40))
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        }
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let truncated: String = chars.iter().take(max).collect();
        format!("{truncated}…")
    }
}

/// 生成工具结果的 L1 摘要（单行，用于 P3 折叠视图）。
///
/// 按工具类型提取关键信息：
/// - bash: 解析 returnCodeInterpretation + stdout 末行（结论在末尾）
/// - edit/write: 路径 + 修改处数
/// - read_file: 路径 + 行数
/// - grep/glob: pattern + 匹配数
///
/// 详见 docs/tui-output-intelligence-plan.md §3.3
pub(crate) fn summarize_tool_result(
    name: &str,
    input: &str,
    result: &str,
    is_error: bool,
) -> String {
    let parsed_input: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let parsed_result: serde_json::Value =
        serde_json::from_str(result).unwrap_or(serde_json::Value::Null);

    match name {
        "bash" | "Bash" => {
            let rc = parsed_result
                .get("returnCodeInterpretation")
                .and_then(|v| v.as_str());
            let stdout = parsed_result
                .get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let line_count = stdout.lines().count();
            let last_line = stdout.lines().last().unwrap_or("");
            match rc {
                Some("interrupted") => "⏹ · 已取消".to_string(),
                Some(r) if r.starts_with("exit_code:") && r != "exit_code:0" => {
                    format!("❌ · {r} · {}行 · {}", line_count, truncate_str(last_line, 60))
                }
                Some("idle.timeout") | Some("timeout") | Some("test.hung") => {
                    format!("⏱ · {rc:?} · {}行", line_count)
                }
                _ if is_error => format!("❌ · {line_count}行"),
                _ => format!("✅ · {line_count}行 · {}", truncate_str(last_line, 60)),
            }
        }
        "edit_file" | "Edit" | "write_file" | "Write" => {
            let path = parsed_input
                .get("file_path")
                .or_else(|| parsed_input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let hunks = parsed_result
                .get("structured_patch")
                .and_then(|v| v.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            if hunks > 0 {
                format!("✏️ {path} · {hunks}处修改")
            } else {
                format!("✏️ {path}")
            }
        }
        "read_file" | "Read" => {
            let path = parsed_input
                .get("file_path")
                .or_else(|| parsed_input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let num_lines = parsed_result
                .get("file")
                .and_then(|f| f.get("numLines"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            format!("📄 {path} · {num_lines}行")
        }
        "grep" | "Grep" | "grep_search" => {
            let pattern = parsed_input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let num_matches = parsed_result
                .get("num_matches")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let num_files = parsed_result
                .get("num_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            format!("🔎 `{pattern}` · {num_matches}处 / {num_files}文件")
        }
        "glob" | "Glob" | "glob_search" => {
            let pattern = parsed_input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let num_files = parsed_result
                .get("num_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            format!("🌐 `{pattern}` · {num_files}文件")
        }
        "WebFetch" | "WebSearch" => {
            let line_count = result.lines().count();
            format!("🌐 · {line_count}行")
        }
        _ => {
            let line_count = result.lines().count();
            format!("📦 {name} · {line_count}行")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::output_view::Priority;

    #[test]
    fn render_tool_call_start_has_tool_name() {
        let card = render_tool_call_start("bash", r#"{"command":"ls -la"}"#);
        assert!(card.contains("🔧 bash"));
        assert!(card.contains("`ls -la`"));
    }

    #[test]
    fn render_tool_call_start_for_read_file() {
        let card = render_tool_call_start("read_file", r#"{"file_path":"/tmp/test.rs"}"#);
        assert!(card.contains("📄 /tmp/test.rs"));
    }

    #[test]
    fn render_tool_result_short_output_full_view() {
        let output = "line1\nline2\nline3";
        // 短输出：无论 collapsed 参数都显示完整内容
        let card = render_tool_result("bash", output, false, None, false, Priority::P1);
        assert!(card.contains("✅ bash"));
        assert!(card.contains("3 行"));
        assert!(!card.contains("[+] 展开"));
    }

    #[test]
    fn render_tool_result_long_output_collapsed() {
        // P0 改进(2026-08-01):折叠时只显示单行标题,不再显示 3 行预览。
        // 原行为:标题 + 3 行预览 + 展开提示 + 尾行 = 6 行
        // 新行为:标题(含行数+折叠标记) + 尾行 = 2 行,把 viewport 空间留给 AI 回复
        let output = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, false, None, true, Priority::P2);
        assert!(card.contains("20 行"), "应显示总行数: {card}");
        assert!(card.contains("折叠"), "应显示折叠标记: {card}");
        // 不应显示预览内容或展开提示(单行折叠)
        assert!(!card.contains("[+] 展开"), "不应显示展开提示: {card}");
        assert!(!card.contains("│ line1"), "不应显示预览行: {card}");
        assert!(!card.contains("│ line3"), "不应显示预览行: {card}");
    }

    #[test]
    fn render_tool_result_long_output_expanded() {
        // P1 修复：长输出 + collapsed=false 时应显示完整内容，不折叠
        let output = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, false, None, false, Priority::P1);
        assert!(card.contains("✅ bash"));
        assert!(card.contains("20 行"));
        // 展开状态不应有折叠提示
        assert!(!card.contains("[+] 展开"));
        // 应包含所有行
        assert!(card.contains("line1"));
        assert!(card.contains("line20"));
    }

    #[test]
    fn render_tool_result_error_shows_x_icon() {
        let card = render_tool_result("bash", "command not found", true, None, false, Priority::P0);
        assert!(card.contains("❌ bash"));
    }

    #[test]
    fn render_tool_result_empty_output() {
        let card = render_tool_result("bash", "", false, None, false, Priority::P1);
        assert!(card.contains("空"));
    }

    /// P3 折叠时显示 L1 单行摘要（不显示预览行）
    #[test]
    fn render_tool_result_p3_shows_l1_summary() {
        let input = r#"{"command":"ls"}"#;
        let result = r#"{"stdout":"file1\nfile2\nfile3","returnCodeInterpretation":"exit_code:0"}"#;
        // P3 + collapsed → L1 摘要，不显示预览行
        let card = render_tool_result("bash", result, false, Some(input), true, Priority::P3);
        assert!(card.contains("✅"), "P3 应显示 ✅ 图标: {card}");
        assert!(!card.contains("[+] 展开"), "P3 不应显示展开提示: {card}");
        assert!(!card.contains("│ file1"), "P3 不应显示预览行: {card}");
    }

    /// P0 即使 collapsed=true 也强制完整展开
    #[test]
    fn render_tool_result_p0_never_collapsed() {
        let output = (1..=50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        // P0 + collapsed=true → 仍完整展开
        let card = render_tool_result("bash", &output, true, None, true, Priority::P0);
        assert!(card.contains("❌"), "P0 错误应显示 ❌: {card}");
        assert!(!card.contains("[+] 展开"), "P0 不应折叠: {card}");
        assert!(card.contains("line50"), "P0 应显示最后一行: {card}");
    }

    /// summarize_tool_result: bash interrupted → ⏹ 已取消
    #[test]
    fn summarize_tool_result_bash_interrupted() {
        let input = r#"{"command":"sleep 100"}"#;
        let result = r#"{"returnCodeInterpretation":"interrupted"}"#;
        let s = summarize_tool_result("bash", input, result, false);
        assert!(s.contains("已取消"), "interrupted 应显示已取消: {s}");
    }

    /// summarize_tool_result: bash ok → ✅ 末行
    #[test]
    fn summarize_tool_result_bash_ok_last_line() {
        let input = r#"{"command":"ls"}"#;
        let result = r#"{"stdout":"file1\nfile2\nfinal.txt","returnCodeInterpretation":"exit_code:0"}"#;
        let s = summarize_tool_result("bash", input, result, false);
        assert!(s.contains("✅"), "应显示 ✅: {s}");
        assert!(s.contains("final.txt"), "应显示末行: {s}");
    }

    #[test]
    fn summarize_bash_input_truncates_long_commands() {
        let long_cmd = "x".repeat(100);
        let input = format!(r#"{{"command":"{long_cmd}"}}"#);
        let summary = summarize_tool_input("bash", &input);
        // Summary is wrapped in backticks: `<truncated>…`
        assert!(
            summary.contains('…'),
            "summary should contain ellipsis: {summary}"
        );
        assert!(summary.len() < 70);
    }

    #[test]
    fn summarize_edit_file_shows_path() {
        let input = r#"{"file_path":"src/main.rs","old_string":"a","new_string":"b"}"#;
        let summary = summarize_tool_input("edit_file", input);
        assert!(summary.contains("src/main.rs"));
    }

    #[test]
    fn summarize_generic_tool_shows_first_key() {
        let input = r#"{"key1":"value1","key2":"value2"}"#;
        let summary = summarize_tool_input("custom_tool", input);
        assert!(summary.contains("key1"));
        assert!(summary.contains("value1"));
    }

    #[test]
    fn render_edit_diff_shows_red_and_green_lines() {
        // P1 修复：diff 已从 start 卡片移到 result 卡片
        let input =
            r#"{"file_path":"src/main.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#;
        let card = render_tool_result("edit_file", "ok", false, Some(input), false, Priority::P1);
        // Should contain red (removed) and green (added) ANSI codes
        assert!(
            card.contains("\x1b[31m"),
            "should have red for removed line: {card}"
        );
        assert!(
            card.contains("\x1b[32m"),
            "should have green for added line: {card}"
        );
        assert!(card.contains("let x = 1"));
        assert!(card.contains("let x = 2"));
    }

    #[test]
    fn render_edit_diff_identical_strings_no_diff() {
        let input = r#"{"file_path":"src/main.rs","old_string":"same","new_string":"same"}"#;
        let card = render_tool_result("edit_file", "ok", false, Some(input), false, Priority::P1);
        // No diff lines should be rendered
        assert!(!card.contains("\x1b[31m"));
        assert!(!card.contains("\x1b[32m"));
    }

    #[test]
    fn render_edit_diff_multi_line() {
        let input = r#"{"file_path":"test.rs","old_string":"line1\nline2\nline3","new_string":"line1\nmodified\nline3"}"#;
        let card = render_tool_result("edit_file", "ok", false, Some(input), false, Priority::P1);
        // line1 and line3 are context, line2 is removed, modified is added
        assert!(card.contains("line2"));
        assert!(card.contains("modified"));
        // Context line should not have color codes
        assert!(card.contains("│   line1"));
    }

    #[test]
    fn render_timeline_single_tool() {
        let history = vec![("bash".to_string(), false)];
        let timeline = render_tool_timeline(&history);
        assert!(timeline.contains("bash → ✓"));
        assert!(timeline.contains("1 个工具"));
    }

    #[test]
    fn render_timeline_multiple_tools() {
        let history = vec![
            ("bash".to_string(), false),
            ("read_file".to_string(), false),
            ("edit_file".to_string(), true),
        ];
        let timeline = render_tool_timeline(&history);
        assert!(timeline.contains("bash → ✓"));
        assert!(timeline.contains("read_file → ✓"));
        assert!(timeline.contains("edit_file → ✗"));
        assert!(timeline.contains("3 个工具"));
    }
    #[test]
    fn render_timeline_empty_history() {
        let history: Vec<(String, bool)> = vec![];
        let timeline = render_tool_timeline(&history);
        assert!(timeline.is_empty());
    }

    // ---------- 提取层测试（P0 修复 2026-08-04） ----------

    /// 提取：bash pretty JSON 信封 → stdout（stderr 为空时不追加）
    #[test]
    fn extract_tool_output_body_bash_extracts_stdout() {
        let output = r#"{
  "stdout": "hello\nworld",
  "stderr": "",
  "returnCodeInterpretation": "exit_code:0",
  "sandboxStatus": {}
}"#;
        let body = extract_tool_output_body("bash", output);
        assert_eq!(body, "hello\nworld");
    }

    /// 提取：bash stderr 非空时追加（错误常在 stderr，不能丢）
    #[test]
    fn extract_tool_output_body_bash_appends_stderr() {
        let output = r#"{
  "stdout": "partial output",
  "stderr": "boom\nproblem",
  "returnCodeInterpretation": "exit_code:1"
}"#;
        let body = extract_tool_output_body("bash", output);
        assert_eq!(body, "partial output\n[stderr]\nboom\nproblem");
    }

    /// 提取：read_file → file.content
    #[test]
    fn extract_tool_output_body_read_file_extracts_content() {
        let output = r#"{
  "type": "file",
  "file": {
    "filePath": "src/main.rs",
    "content": "fn main() {}\n",
    "numLines": 1,
    "startLine": 1,
    "totalLines": 1
  }
}"#;
        let body = extract_tool_output_body("read_file", output);
        assert_eq!(body, "fn main() {}\n");
    }

    /// 提取：write_file 带 cargo check 尾部 → 提取 cargo check 错误，剔除确认 JSON
    #[test]
    fn extract_tool_output_body_write_file_with_cargo_check() {
        let output = format!(
            "{}\n\n--- cargo check ---\nerror[E0308]: mismatched types\n --> src/main.rs:2:23",
            r#"{
  "type": "write",
  "filePath": "src/main.rs",
  "content": "fn main() {}",
  "structuredPatch": [],
  "originalFile": null,
  "gitDiff": null
}"#
        );
        let body = extract_tool_output_body("write_file", &output);
        assert!(body.contains("error[E0308]"), "应提取 cargo check 错误: {body}");
        assert!(!body.contains("filePath"), "不应包含确认 JSON: {body}");
    }

    /// 提取：非 JSON 结果回退原始输出（不崩溃）
    #[test]
    fn extract_tool_output_body_non_json_falls_back() {
        let body = extract_tool_output_body("bash", "plain text output");
        assert_eq!(body, "plain text output");
    }

    /// 提取：未知工具回退原始输出
    #[test]
    fn extract_tool_output_body_unknown_tool_falls_back() {
        let body = extract_tool_output_body("WebSearch", r#"{"query":"x"}"#);
        assert_eq!(body, r#"{"query":"x"}"#);
    }

    // ---------- 渲染接线测试（P0 修复 2026-08-04） ----------

    /// 核心回归：展开卡片渲染真实 stdout，而非 JSON 信封（无 sandboxStatus/键名）
    #[test]
    fn render_tool_result_renders_body_not_json_envelope() {
        let output = r#"{
  "stdout": "hello",
  "stderr": "",
  "sandboxStatus": { "enabled": true }
}"#;
        let card = render_tool_result("bash", output, false, None, false, Priority::P1);
        assert!(card.contains("hello"), "应渲染 stdout 内容: {card}");
        assert!(!card.contains("sandboxStatus"), "不应渲染 JSON 信封: {card}");
        assert!(!card.contains("\"stdout\""), "不应渲染 JSON 键: {card}");
    }

    /// 展开 + 截断：100 行 body → 显示前 60 行 + 省略标记（P1 截头部）
    #[test]
    fn render_tool_result_expanded_truncates_head_at_60() {
        let output = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, false, None, false, Priority::P1);
        assert!(card.contains("line1"), "应显示前 60 行: {card}");
        assert!(card.contains("line60"), "应显示到第 60 行: {card}");
        assert!(!card.contains("line61"), "不应显示第 61 行: {card}");
        assert!(card.contains("其余 40 行省略"), "应显示省略标记: {card}");
    }

    /// 错误展开截尾部：P0 显示最后 60 行（失败摘要/错误列表常在输出末尾）
    #[test]
    fn render_tool_result_error_truncates_tail_at_60() {
        let output = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, true, None, true, Priority::P0);
        assert!(card.contains("line100"), "P0 应显示最后一行: {card}");
        assert!(card.contains("line41"), "P0 应显示尾部内容: {card}");
        assert!(!card.contains("│ line1\n"), "P0 不应显示开头: {card}");
        assert!(!card.contains("│ line2\n"), "P0 不应显示开头行: {card}");
        assert!(card.contains("其余 40 行省略"), "应显示省略标记: {card}");
    }
}
