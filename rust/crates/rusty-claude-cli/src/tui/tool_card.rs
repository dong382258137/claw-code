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

    let mut diff = String::from("\n");
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Simple line-by-line diff (not Myers, but good enough for preview)
    let max_lines = old_lines.len().max(new_lines.len());
    for i in 0..max_lines {
        let old_line = old_lines.get(i).copied().unwrap_or("");
        let new_line = new_lines.get(i).copied().unwrap_or("");
        if old_line == new_line {
            // Context line (unchanged)
            diff.push_str(&format!("│   {old_line}\n"));
        } else {
            if !old_line.is_empty() || i < old_lines.len() {
                // Removed line (red)
                diff.push_str(&format!("\x1b[31m│ - {old_line}\x1b[0m\n"));
            }
            if !new_line.is_empty() || i < new_lines.len() {
                // Added line (green)
                diff.push_str(&format!("\x1b[32m│ + {new_line}\x1b[0m\n"));
            }
        }
    }
    Some(diff)
}

/// Render a tool result card (collapsible).
///
/// 折叠语义（P1 修复）：
/// - `collapsed == true` 且 `line_count > COLLAPSE_THRESHOLD`：
///   折叠预览视图，显示前 `COLLAPSED_PREVIEW_LINES` 行 + `[+] 展开` 提示。
/// - `collapsed == false` 或 `line_count <= COLLAPSE_THRESHOLD`：
///   完整视图，显示全部输出。
///
/// 之前的问题：`complete_tool_card` 把 `collapsed` 设为 `true` 后，
/// `OutputEntry::render()` 走了独立的"只显示摘要"分支，本函数的折叠预览
/// 逻辑（前3行+展开提示）永远不会被执行。现在由 `render()` 统一委托给
/// 本函数，根据 `collapsed` 参数决定折叠/展开。
///
/// 对 edit_file 工具，在 result 卡片中显示 diff（原 start 卡片中的 diff 已移除）。
pub(crate) fn render_tool_result(
    name: &str,
    output: &str,
    is_error: bool,
    input: Option<&str>,
    collapsed: bool,
) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let line_count = lines.len();
    let icon = if is_error { "❌" } else { "✅" };

    // For edit_file, prepend a diff preview before the result body
    let diff_prefix = if (name == "edit_file" || name == "Edit") && !is_error {
        input.and_then(render_edit_diff).unwrap_or_default()
    } else {
        String::new()
    };

    // Determine if this tool's output should be syntax-highlighted
    let language = detect_language_for_tool(name, output);
    let highlighted_body = |slice: &[&str]| -> String {
        let text = slice.join("\n");
        if let Some(lang) = language {
            let renderer = crate::render::TerminalRenderer::new();
            let highlighted = renderer.highlight_code(&text, &lang);
            // Add card prefix to each line
            highlighted.lines().map(|l| format!("│ {l}\n")).collect()
        } else {
            slice.iter().map(|l| format!("│ {l}\n")).collect()
        }
    };

    if collapsed && line_count > COLLAPSE_THRESHOLD {
        // 折叠预览视图：显示前几行 + 展开提示
        let preview: String = highlighted_body(&lines[..COLLAPSED_PREVIEW_LINES.min(line_count)]);
        let hidden = line_count - COLLAPSED_PREVIEW_LINES;
        format!(
            "{diff_prefix}├─ {icon} {name} ({line_count} 行，+{hidden} 行已折叠)\n{preview}├─ [+] 展开（还有 {hidden} 行）\n└─\n"
        )
    } else if line_count == 0 {
        format!("{diff_prefix}├─ {icon} {name} (空)\n└─\n")
    } else {
        // 完整视图
        let body = highlighted_body(&lines);
        format!("{diff_prefix}├─ {icon} {name} ({line_count} 行)\n{body}└─\n")
    }
}

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
pub(crate) fn render_tool_result_public(
    name: &str,
    output: &str,
    is_error: bool,
    input: Option<&str>,
    collapsed: bool,
) -> String {
    render_tool_result(name, output, is_error, input, collapsed)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let card = render_tool_result("bash", output, false, None, false);
        assert!(card.contains("✅ bash"));
        assert!(card.contains("3 行"));
        assert!(!card.contains("[+] 展开"));
    }

    #[test]
    fn render_tool_result_long_output_collapsed() {
        // P1 修复：阈值从 15 降到 5，20 行输出 + collapsed=true 时折叠，只显示前 3 行
        let output = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, false, None, true);
        assert!(card.contains("20 行"));
        assert!(card.contains("[+] 展开"));
        assert!(card.contains("17 行"));
        // Preview should contain first 3 lines (COLLAPSED_PREVIEW_LINES = 3)
        assert!(card.contains("line1"));
        assert!(card.contains("line3"));
        // Should not contain line 4 in preview
        assert!(!card.contains("│ line4"));
    }

    #[test]
    fn render_tool_result_long_output_expanded() {
        // P1 修复：长输出 + collapsed=false 时应显示完整内容，不折叠
        let output = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, false, None, false);
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
        let card = render_tool_result("bash", "command not found", true, None, false);
        assert!(card.contains("❌ bash"));
    }

    #[test]
    fn render_tool_result_empty_output() {
        let card = render_tool_result("bash", "", false, None, false);
        assert!(card.contains("空"));
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
        let card = render_tool_result("edit_file", "ok", false, Some(input), false);
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
        let card = render_tool_result("edit_file", "ok", false, Some(input), false);
        // No diff lines should be rendered
        assert!(!card.contains("\x1b[31m"));
        assert!(!card.contains("\x1b[32m"));
    }

    #[test]
    fn render_edit_diff_multi_line() {
        let input = r#"{"file_path":"test.rs","old_string":"line1\nline2\nline3","new_string":"line1\nmodified\nline3"}"#;
        let card = render_tool_result("edit_file", "ok", false, Some(input), false);
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
}
