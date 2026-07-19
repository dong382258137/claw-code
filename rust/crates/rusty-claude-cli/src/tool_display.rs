//! Tool call visualization: card rendering, diff previews, result formatting, CliToolExecutor.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crate::render::{OutputVerbosity, TerminalRenderer};
use runtime::{ToolError, ToolExecutor};
use tools::GlobalToolRegistry;

use crate::{AllowedToolSet, RuntimeMcpState};

/// 工具卡片左边框（ANSI 245 灰色）。用于卡片体内每行前缀，视觉上把
/// "调用详情"和"工具结果"框在同一个卡片容器里。
pub(crate) const TOOL_CARD_PREFIX: &str = "\x1b[38;5;245m│\x1b[0m ";

/// User 消息卡片前缀（ANSI 111 浅蓝色），与工具卡片的灰色前缀视觉区分。
pub(crate) const USER_CARD_PREFIX: &str = "\x1b[38;5;111m│\x1b[0m ";

pub(crate) const DISPLAY_TRUNCATION_NOTICE: &str =
    "\x1b[2m… output truncated for display; full result preserved in session.\x1b[0m";
pub(crate) const READ_DISPLAY_MAX_LINES: usize = 80;
pub(crate) const READ_DISPLAY_MAX_CHARS: usize = 6_000;
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_LINES: usize = 60;
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_CHARS: usize = 4_000;

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct ToolSearchRequest {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct McpToolRequest {
    #[serde(rename = "qualifiedName")]
    qualified_name: Option<String>,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct ListMcpResourcesRequest {
    server: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct ReadMcpResourceRequest {
    server: String,
    uri: String,
}

pub(crate) struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    pub(crate) output_verbosity: OutputVerbosity,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

impl CliToolExecutor {
    pub(crate) fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            output_verbosity: OutputVerbosity::default(),
            allowed_tools,
            tool_registry,
            mcp_state,
        }
    }

    pub(crate) fn with_verbosity(mut self, verbosity: OutputVerbosity) -> Self {
        self.output_verbosity = verbosity;
        self
    }

    fn execute_search_tool(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        serde_json::to_string_pretty(&self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        ))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute(tool_name, &value)
                .map_err(ToolError::new)
        };
        match result {
            Ok(output) => {
                if self.emit_output && self.output_verbosity.show_tool_results() {
                    // Full 模式：用卡片闭合（带左边框对齐 + 底部边框），
                    // 把工具结果嵌入到 format_tool_call_start 开的卡片容器内。
                    let markdown = format_tool_result_card_close(
                        tool_name,
                        &output,
                        false,
                    );
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                } else if self.emit_output && self.output_verbosity.show_tool_errors() {
                    // Compact / Minimal: show a one-line success marker for key tools
                    let summary = format_tool_result_compact(tool_name);
                    if !summary.is_empty() {
                        self.renderer
                            .stream_markdown(&summary, &mut io::stdout())
                            .map_err(|error| ToolError::new(error.to_string()))?;
                    }
                }
                Ok(output)
            }
            Err(error) => {
                if self.emit_output && self.output_verbosity.show_tool_errors() {
                    // Full 模式：错误结果也嵌入卡片容器（闭合）。
                    // Compact/Minimal 模式：format_tool_result 会输出完整错误（错误不折叠）。
                    let markdown = if self.output_verbosity.show_tool_results() {
                        format_tool_result_card_close(
                            tool_name,
                            &error.to_string(),
                            true,
                        )
                    } else {
                        format_tool_result(
                            tool_name,
                            &error.to_string(),
                            true,
                            self.output_verbosity,
                        )
                    };
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                }
                Err(error)
            }
        }
    }
}

pub(crate) fn short_tool_id(id: &str) -> String {
    let char_count = id.chars().count();
    if char_count <= 12 {
        return id.to_string();
    }
    let prefix: String = id.chars().take(12).collect();
    format!("{prefix}…")
}

/// 把多行字符串的每一行都加上 `TOOL_CARD_PREFIX` 前缀。
/// 空行也保留前缀，保持卡片左边框连续。
pub(crate) fn indent_with_card_prefix(content: &str) -> String {
    content
        .lines()
        .map(|line| format!("{TOOL_CARD_PREFIX}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 渲染工具卡片的开头部分（顶部边框 + 标题行 + 详情行）。
/// 不含底部闭合边框 `╰─╯`——底部边框由 `format_tool_result_card_close` 补上，
/// 这样调用详情和工具结果在同一卡片内。
pub(crate) fn format_tool_call_start(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));

    let detail = match name {
        "bash" | "Bash" => format_bash_call(&parsed),
        "read_file" | "Read" => {
            let path = extract_tool_path(&parsed);
            format!("\x1b[2m📄 Reading {path}…\x1b[0m")
        }
        "write_file" | "Write" => {
            let path = extract_tool_path(&parsed);
            let lines = parsed
                .get("content")
                .and_then(|value| value.as_str())
                .map_or(0, |content| content.lines().count());
            format!("\x1b[1;32m✏️ Writing {path}\x1b[0m \x1b[2m({lines} lines)\x1b[0m")
        }
        "edit_file" | "Edit" => {
            let path = extract_tool_path(&parsed);
            let old_value = parsed
                .get("old_string")
                .or_else(|| parsed.get("oldString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let new_value = parsed
                .get("new_string")
                .or_else(|| parsed.get("newString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            format!(
                "\x1b[1;33m📝 Editing {path}\x1b[0m{}",
                format_patch_preview(old_value, new_value)
                    .map(|preview| format!("\n{preview}"))
                    .unwrap_or_default()
            )
        }
        "glob_search" | "Glob" => format_search_start("🔎 Glob", &parsed),
        "grep_search" | "Grep" => format_search_start("🔎 Grep", &parsed),
        "PowerShell" | "powershell" => {
            let command = parsed
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if command.is_empty() {
                summarize_tool_payload(input)
            } else {
                format!("\x1b[1;34m🖥️ PowerShell\x1b[0m \x1b[2m{command}\x1b[0m")
            }
        }
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("?")
            .to_string(),
        _ => summarize_tool_payload(input),
    };

    // 顶部边框宽度对齐 name + 4 (两侧空格) + 4 (装饰 ╭─ ─╮)
    let border = "─".repeat(name.len() + 8);
    let indented_detail = indent_with_card_prefix(&detail);
    format!(
        "\x1b[38;5;245m╭─ \x1b[1;36m{name}\x1b[0;38;5;245m ─╮\x1b[0m\n{indented_detail}"
    )
}

/// 渲染工具卡片的闭合部分：工具结果（带左边框对齐）+ 底部边框。
/// 在 Full 模式下被调用，把工具结果嵌入到与调用详情同一个卡片容器内。
/// 内层复用 `format_tool_result` 的解析逻辑，外层加卡片左边框对齐 + 底部闭合边框。
pub(crate) fn format_tool_result_card_close(name: &str, output: &str, is_error: bool) -> String {
    let inner = format_tool_result(name, output, is_error, OutputVerbosity::Full);
    let border = "─".repeat(name.len() + 8);
    // 在详情和结果之间加一个空行（带左边框），视觉分隔调用与结果
    format!(
        "\n{TOOL_CARD_PREFIX}\n{}\n\x1b[38;5;245m╰{border}╯\x1b[0m",
        indent_with_card_prefix(&inner)
    )
}

/// 渲染 user 消息卡片：顶部边框 + 用户输入（每行加 `│` 前缀）+ 底部边框。
/// 颜色用浅蓝（38;5;111）与工具卡片灰色（38;5;245）区分。
/// 调用方需在打印卡片前清除 rustyline 的 echo 行（单行输入时），否则会重复显示。
pub(crate) fn format_user_message_card(input: &str) -> String {
    let label = "you";
    let border = "─".repeat(label.len() + 8);
    let indented: String = input
        .lines()
        .map(|line| format!("{USER_CARD_PREFIX}{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "\x1b[38;5;111m╭─ \x1b[1;37m{label}\x1b[0;38;5;111m ─╮\x1b[0m\n{indented}\n\x1b[38;5;111m╰{border}╯\x1b[0m"
    )
}

/// 打印 user 消息卡片。
/// 单行输入时先清除 rustyline 的 echo 行（避免重复显示）。
/// 清除行数根据终端宽度和输入显示宽度计算，覆盖长输入换行的情况。
/// 多行输入时不清除 echo（多行清除复杂且易出错），保留 echo + 卡片视觉冗余。
/// P3 多行粘贴折叠实现后，多行粘贴会被折叠为单行占位符，统一走单行清除路径。
pub(crate) fn print_user_card(input: &str) {
    let line_count = input.lines().count();
    if line_count <= 1 {
        clear_rustyline_echo(input);
    }
    println!("{}", format_user_message_card(input.trim()));
}

/// 清除 rustyline 的回显行。
/// rustyline prompt "> " 占 2 列，长输入在窄终端会换行多行。
/// 根据终端宽度和输入显示宽度计算换行数，逐行上移并清除整行。
/// 注意：调用此函数前，光标必须紧跟在 rustyline 回显之后（无其他输出插入），
/// 否则清除会错位。CJK 字符按 2 列估算（粗略近似，不引入 unicode-width 依赖）。
pub(crate) fn clear_rustyline_echo(input: &str) {
    let cols = match crossterm::terminal::size() {
        Ok((c, _)) if c > 0 => c as usize,
        _ => 80,
    };
    let prompt_width = 2; // "> "
    let input_width = estimate_display_width(input);
    let total_width = prompt_width + input_width;
    let wrapped_lines = ((total_width + cols - 1) / cols).max(1);
    let mut stdout = io::stdout();
    for _ in 0..wrapped_lines {
        // \x1b[A = 光标上移一行, \x1b[2K = 清除整行
        let _ = write!(stdout, "\x1b[A\x1b[2K");
    }
    let _ = stdout.flush();
}

/// 估算字符串在终端的显示宽度（列数）。
/// ASCII 及窄字符算 1 列，CJK、全角、emoji 等宽字符算 2 列。
/// 粗略近似，不处理零宽字符、组合字符和 emoji ZWJ 序列。
pub(crate) fn estimate_display_width(s: &str) -> usize {
    s.chars()
        .map(|c| if is_wide_char(c as u32) { 2 } else { 1 })
        .sum()
}

/// 判断码点是否属于宽字符区间（CJK、全角、emoji 等）。
/// 参考 East Asian Width 的粗略近似，覆盖常见 CJK 和 emoji 区间。
pub(crate) fn is_wide_char(code: u32) -> bool {
    matches!(
        code,
        0x1100..=0x115F     // Hangul Jamo
        | 0x2E80..=0x303E   // CJK Radicals / Kangxi
        | 0x3040..=0x33BF   // Hiragana / Katakana / CJK Compat
        | 0x3400..=0x4DBF   // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0xA000..=0xA4CF   // Yi Syllables / Radicals
        | 0xAC00..=0xD7AF   // Hangul Syllables
        | 0xF900..=0xFAFF   // CJK Compatibility Ideographs
        | 0xFE30..=0xFE4F   // CJK Compatibility Forms
        | 0xFF00..=0xFF60   // Fullwidth Forms
        | 0xFFE0..=0xFFE6   // Fullwidth signs
        | 0x1F300..=0x1FAFF // Emoji & pictographs
        | 0x20000..=0x3FFFD // CJK Extensions B-F
    )
}

pub(crate) fn format_tool_result(
    name: &str,
    output: &str,
    is_error: bool,
    verbosity: OutputVerbosity,
) -> String {
    // 成功路径仅在 Full 模式被调用（见 CliToolExecutor::execute 的门控），
    // 错误路径在 Full/Compact/Minimal 模式都被调用（Silent 由 show_tool_errors() 抑制）。
    // verbosity 参数保留供未来按级别裁剪错误详情，当前错误一律走完整渲染。
    let _ = verbosity;
    let icon = if is_error {
        "\x1b[1;31m✗\x1b[0m"
    } else {
        "\x1b[1;32m✓\x1b[0m"
    };
    if is_error {
        let summary = truncate_for_summary(output.trim(), 160);
        return if summary.is_empty() {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
        } else {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n\x1b[38;5;203m{summary}\x1b[0m")
        };
    }

    let parsed: serde_json::Value =
        serde_json::from_str(output).unwrap_or(serde_json::Value::String(output.to_string()));
    match name {
        "bash" | "Bash" => format_bash_result(icon, &parsed),
        "read_file" | "Read" => format_read_result(icon, &parsed),
        "write_file" | "Write" => format_write_result(icon, &parsed),
        "edit_file" | "Edit" => format_edit_result(icon, &parsed),
        "glob_search" | "Glob" => format_glob_result(icon, &parsed),
        "grep_search" | "Grep" => format_grep_result(icon, &parsed),
        _ => format_generic_tool_result(icon, name, &parsed),
    }
}

/// Compact/Minimal 模式下的工具成功结果摘要。返回空字符串表示该工具不显示摘要
/// （Minimal 模式下非关键工具静默）；返回非空字符串则由调用方 stream_markdown 打印。
pub(crate) fn format_tool_result_compact(name: &str) -> String {
    // Minimal: 仅关键工具显示摘要，其他工具完全静默。
    if !matches!(
        name,
        "read_file" | "write_file" | "edit_file" | "bash" | "bash_command"
            | "glob_search" | "grep_search"
    ) {
        return String::new();
    }
    format!("\x1b[2m○ {name} ok\x1b[0m")
}

pub(crate) fn extract_tool_path(parsed: &serde_json::Value) -> String {
    parsed
        .get("file_path")
        .or_else(|| parsed.get("filePath"))
        .or_else(|| parsed.get("path"))
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string()
}

pub(crate) fn format_search_start(label: &str, parsed: &serde_json::Value) -> String {
    let pattern = parsed
        .get("pattern")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let scope = parsed
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or(".");
    format!("{label} {pattern}\n\x1b[2min {scope}\x1b[0m")
}

pub(crate) fn format_patch_preview(old_value: &str, new_value: &str) -> Option<String> {
    if old_value.is_empty() && new_value.is_empty() {
        return None;
    }
    Some(format!(
        "\x1b[38;5;203m- {}\x1b[0m\n\x1b[38;5;70m+ {}\x1b[0m",
        truncate_for_summary(first_visible_line(old_value), 72),
        truncate_for_summary(first_visible_line(new_value), 72)
    ))
}

pub(crate) fn format_bash_call(parsed: &serde_json::Value) -> String {
    let command = parsed
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if command.is_empty() {
        String::new()
    } else {
        format!(
            "\x1b[48;5;236;38;5;255m $ {} \x1b[0m",
            truncate_for_summary(command, 160)
        )
    }
}

pub(crate) fn first_visible_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
}

pub(crate) fn format_bash_result(icon: &str, parsed: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    let mut lines = vec![format!("{icon} \x1b[38;5;245mbash\x1b[0m")];
    if let Some(task_id) = parsed
        .get("backgroundTaskId")
        .and_then(|value| value.as_str())
    {
        write!(&mut lines[0], " backgrounded ({task_id})").expect("write to string");
    } else if let Some(status) = parsed
        .get("returnCodeInterpretation")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
    {
        write!(&mut lines[0], " {status}").expect("write to string");
    }

    if let Some(stdout) = parsed.get("stdout").and_then(|value| value.as_str()) {
        if !stdout.trim().is_empty() {
            lines.push(truncate_output_for_display(
                stdout,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            ));
        }
    }
    if let Some(stderr) = parsed.get("stderr").and_then(|value| value.as_str()) {
        if !stderr.trim().is_empty() {
            lines.push(format!(
                "\x1b[38;5;203m{}\x1b[0m",
                truncate_output_for_display(
                    stderr,
                    TOOL_OUTPUT_DISPLAY_MAX_LINES,
                    TOOL_OUTPUT_DISPLAY_MAX_CHARS,
                )
            ));
        }
    }

    lines.join("\n\n")
}

pub(crate) fn format_read_result(icon: &str, parsed: &serde_json::Value) -> String {
    let file = parsed.get("file").unwrap_or(parsed);
    let path = extract_tool_path(file);
    let start_line = file
        .get("startLine")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let num_lines = file
        .get("numLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_lines = file
        .get("totalLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(num_lines);
    let content = file
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let end_line = start_line.saturating_add(num_lines.saturating_sub(1));

    format!(
        "{icon} \x1b[2m📄 Read {path} (lines {}-{} of {})\x1b[0m\n{}",
        start_line,
        end_line.max(start_line),
        total_lines,
        truncate_output_for_display(content, READ_DISPLAY_MAX_LINES, READ_DISPLAY_MAX_CHARS)
    )
}

pub(crate) fn format_write_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let kind = parsed
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("write");
    let line_count = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .map_or(0, |content| content.lines().count());
    format!(
        "{icon} \x1b[1;32m✏️ {} {path}\x1b[0m \x1b[2m({line_count} lines)\x1b[0m",
        if kind == "create" { "Wrote" } else { "Updated" },
    )
}

pub(crate) fn format_structured_patch_preview(parsed: &serde_json::Value) -> Option<String> {
    let hunks = parsed.get("structuredPatch")?.as_array()?;
    let mut preview = Vec::new();
    for hunk in hunks.iter().take(2) {
        let lines = hunk.get("lines")?.as_array()?;
        for line in lines.iter().filter_map(|value| value.as_str()).take(6) {
            match line.chars().next() {
                Some('+') => preview.push(format!("\x1b[38;5;70m{line}\x1b[0m")),
                Some('-') => preview.push(format!("\x1b[38;5;203m{line}\x1b[0m")),
                _ => preview.push(line.to_string()),
            }
        }
    }
    if preview.is_empty() {
        None
    } else {
        Some(preview.join("\n"))
    }
}

pub(crate) fn format_edit_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let suffix = if parsed
        .get("replaceAll")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        " (replace all)"
    } else {
        ""
    };
    let preview = format_structured_patch_preview(parsed).or_else(|| {
        let old_value = parsed
            .get("oldString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let new_value = parsed
            .get("newString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        format_patch_preview(old_value, new_value)
    });

    match preview {
        Some(preview) => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m\n{preview}"),
        None => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m"),
    }
}

pub(crate) fn format_glob_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if filenames.is_empty() {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files")
    } else {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files\n{filenames}")
    }
}

pub(crate) fn format_grep_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_matches = parsed
        .get("numMatches")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let content = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let summary = format!(
        "{icon} \x1b[38;5;245mgrep_search\x1b[0m {num_matches} matches across {num_files} files"
    );
    if !content.trim().is_empty() {
        format!(
            "{summary}\n{}",
            truncate_output_for_display(
                content,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            )
        )
    } else if !filenames.is_empty() {
        format!("{summary}\n{filenames}")
    } else {
        summary
    }
}

pub(crate) fn format_generic_tool_result(icon: &str, name: &str, parsed: &serde_json::Value) -> String {
    let rendered_output = match parsed {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string_pretty(parsed).unwrap_or_else(|_| parsed.to_string())
        }
        _ => parsed.to_string(),
    };
    let preview = truncate_output_for_display(
        &rendered_output,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );

    if preview.is_empty() {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
    } else if preview.contains('\n') {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n{preview}")
    } else {
        format!("{icon} \x1b[38;5;245m{name}:\x1b[0m {preview}")
    }
}

pub(crate) fn summarize_tool_payload(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.trim().to_string(),
    };
    truncate_for_summary(&compact, 96)
}

pub(crate) fn truncate_for_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

pub(crate) fn truncate_output_for_display(content: &str, max_lines: usize, max_chars: usize) -> String {
    let original = content.trim_end_matches('\n');
    if original.is_empty() {
        return String::new();
    }

    let mut preview_lines = Vec::new();
    let mut used_chars = 0usize;
    let mut truncated = false;

    for (index, line) in original.lines().enumerate() {
        if index >= max_lines {
            truncated = true;
            break;
        }

        let newline_cost = usize::from(!preview_lines.is_empty());
        let available = max_chars.saturating_sub(used_chars + newline_cost);
        if available == 0 {
            truncated = true;
            break;
        }

        let line_chars = line.chars().count();
        if line_chars > available {
            preview_lines.push(line.chars().take(available).collect::<String>());
            truncated = true;
            break;
        }

        preview_lines.push(line.to_string());
        used_chars += newline_cost + line_chars;
    }

    let mut preview = preview_lines.join("\n");
    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(DISPLAY_TRUNCATION_NOTICE);
    }
    preview
}
