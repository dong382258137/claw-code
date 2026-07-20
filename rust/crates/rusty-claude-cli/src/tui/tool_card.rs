#![cfg(feature = "full-tui")]

//! Collapsible tool call card rendering for the TUI output view.
//!
//! When a tool is invoked, a card header is rendered (tool name + input summary).
//! When the tool result arrives, a collapsible body is rendered: if the output
//! exceeds `COLLAPSE_THRESHOLD` lines, only the first few lines + an expand
//! hint are shown; otherwise the full output is displayed.

/// Default threshold: outputs with more than this many lines are collapsed.
const COLLAPSE_THRESHOLD: usize = 15;
/// Number of lines to show when collapsed.
const COLLAPSED_PREVIEW_LINES: usize = 5;

/// Render a tool call start card (header only, result pending).
/// For edit_file tools, includes a colored unified diff preview.
pub(crate) fn render_tool_call_start(name: &str, input: &str) -> String {
    let summary = summarize_tool_input(name, input);
    let mut card = format!("\n┌─ 🔧 {name} {summary}\n");

    // For edit_file, render a colored diff preview
    if name == "edit_file" || name == "Edit" {
        if let Some(diff) = render_edit_diff(input) {
            card.push_str(&diff);
        }
    }

    card.push_str("├─ ⏳ running...\n");
    card
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
/// If `output` has more than `COLLAPSE_THRESHOLD` lines, only the first
/// `COLLAPSED_PREVIEW_LINES` lines are shown followed by an expand hint.
/// Tool results are syntax-highlighted when the tool name implies code output.
pub(crate) fn render_tool_result(name: &str, output: &str, is_error: bool) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let line_count = lines.len();
    let icon = if is_error { "❌" } else { "✅" };

    // Determine if this tool's output should be syntax-highlighted
    let language = detect_language_for_tool(name, output);
    let highlighted_body = |slice: &[&str]| -> String {
        let text = slice.join("\n");
        if let Some(lang) = language {
            let renderer = crate::render::TerminalRenderer::new();
            let highlighted = renderer.highlight_code(&text, &lang);
            // Add card prefix to each line
            highlighted
                .lines()
                .map(|l| format!("│ {l}\n"))
                .collect()
        } else {
            slice
                .iter()
                .map(|l| format!("│ {l}\n"))
                .collect()
        }
    };

    if line_count > COLLAPSE_THRESHOLD {
        // Collapsed view: show preview + expand hint
        let preview: String = highlighted_body(&lines[..COLLAPSED_PREVIEW_LINES.min(line_count)]);
        let hidden = line_count - COLLAPSED_PREVIEW_LINES;
        format!(
            "├─ {icon} {name} ({line_count} lines, +{hidden} hidden)\n{preview}├─ [+] Expand ({hidden} more lines)\n└─\n"
        )
    } else if line_count == 0 {
        format!("├─ {icon} {name} (empty)\n└─\n")
    } else {
        // Full view
        let body = highlighted_body(&lines);
        format!("├─ {icon} {name} ({line_count} lines)\n{body}└─\n")
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
    let tool_word = if count == 1 { "tool" } else { "tools" };
    format!(
        "\n┌─ 🔧 Timeline: {} ({count} {tool_word})\n└─\n",
        parts.join(" | ")
    )
}

/// Summarize tool input to a short one-liner for the card header.
fn summarize_tool_input(name: &str, input: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(input)
        .unwrap_or(serde_json::Value::String(input.to_string()));

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
        let card = render_tool_result("bash", output, false);
        assert!(card.contains("✅ bash"));
        assert!(card.contains("3 lines"));
        assert!(!card.contains("[+] Expand"));
    }

    #[test]
    fn render_tool_result_long_output_collapsed() {
        let output = (1..=20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let card = render_tool_result("bash", &output, false);
        assert!(card.contains("20 lines"));
        assert!(card.contains("[+] Expand"));
        assert!(card.contains("15 more lines"));
        // Preview should contain first 5 lines
        assert!(card.contains("line1"));
        assert!(card.contains("line5"));
        // Should not contain line 6 in preview
        assert!(!card.contains("│ line6"));
    }

    #[test]
    fn render_tool_result_error_shows_x_icon() {
        let card = render_tool_result("bash", "command not found", true);
        assert!(card.contains("❌ bash"));
    }

    #[test]
    fn render_tool_result_empty_output() {
        let card = render_tool_result("bash", "", false);
        assert!(card.contains("empty"));
    }

    #[test]
    fn summarize_bash_input_truncates_long_commands() {
        let long_cmd = "x".repeat(100);
        let input = format!(r#"{{"command":"{long_cmd}"}}"#);
        let summary = summarize_tool_input("bash", &input);
        // Summary is wrapped in backticks: `<truncated>…`
        assert!(summary.contains('…'), "summary should contain ellipsis: {summary}");
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
        let input = r#"{"file_path":"src/main.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#;
        let card = render_tool_call_start("edit_file", input);
        // Should contain red (removed) and green (added) ANSI codes
        assert!(card.contains("\x1b[31m"), "should have red for removed line");
        assert!(card.contains("\x1b[32m"), "should have green for added line");
        assert!(card.contains("let x = 1"));
        assert!(card.contains("let x = 2"));
    }

    #[test]
    fn render_edit_diff_identical_strings_no_diff() {
        let input = r#"{"file_path":"src/main.rs","old_string":"same","new_string":"same"}"#;
        let card = render_tool_call_start("edit_file", input);
        // No diff lines should be rendered
        assert!(!card.contains("\x1b[31m"));
        assert!(!card.contains("\x1b[32m"));
    }

    #[test]
    fn render_edit_diff_multi_line() {
        let input = r#"{"file_path":"test.rs","old_string":"line1\nline2\nline3","new_string":"line1\nmodified\nline3"}"#;
        let card = render_tool_call_start("edit_file", input);
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
        assert!(timeline.contains("1 tool"));
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
        assert!(timeline.contains("3 tools"));
    }

    #[test]
    fn render_timeline_empty_history() {
        let history: Vec<(String, bool)> = vec![];
        let timeline = render_tool_timeline(&history);
        assert!(timeline.is_empty());
    }
}
