#![cfg(feature = "full-tui")]

//! 历史回看：从 session JSONL 流式重放为结构化条目。
//!
//! 数据权威在后端 session JSONL（`.claw/sessions/{id}/session-*.jsonl`），
//! TUI 只在用户滚动到窗口顶部时按需从文件加载更早历史，用与实时输出
//! 相同的渲染管线（markdown → ANSI）重放 —— 与"流式渲染"同一套逻辑。
//!
//! JSONL 行结构（type=message）：
//! ```json
//! {"message":{"blocks":[{"type":"text","text":"..."},
//!                       {"type":"thinking","thinking":"..."},
//!                       {"type":"tool_use","id":"call_x","name":"bash","input":"{...}"},
//!                       {"type":"tool_result","tool_use_id":"call_x","tool_name":"bash","output":"...","is_error":false}]},"type":"message"}
//! ```

use std::path::Path;

use serde_json::Value;

use super::output_view::{OutputBuffer, OutputEntry};

/// 从 session JSONL 的 `[start_line, start_line + count)` 行重放为 OutputEntry。
///
/// - `text` → markdown 一次性渲染（与实时流式的最终渲染结果一致）
/// - `thinking` → Thinking 摘要（与实时 emitter 的摘要格式一致）
/// - `tool_use` + `tool_result` → 在临时 buffer 内按序配对为完成态 ToolCard
///
/// 返回的条目已含完整渲染状态，调用方 `prepend_history` 前置到窗口即可。
pub(crate) fn load_history_entries(
    path: &Path,
    start_line: usize,
    count: usize,
    width: Option<usize>,
) -> Vec<OutputEntry> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    if start_line >= lines.len() {
        return Vec::new();
    }
    let renderer = crate::render::TerminalRenderer::shared();
    // 临时 buffer 用于完成 tool_use/tool_result 配对（complete_tool_card 依赖 buffer 状态）。
    let mut buf = OutputBuffer::default();
    for line in lines.iter().skip(start_line).take(count) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // 非 JSON 行（如压缩摘要）跳过
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("blocks"))
            .and_then(|b| b.as_array())
        else {
            continue;
        };
        for b in blocks {
            let Some(kind) = b.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            match kind {
                "text" => {
                    let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    let rendered = renderer.markdown_to_ansi_with_width(text, width);
                    buf.append(&rendered);
                }
                "thinking" => {
                    let thinking = b.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                    let summary = if thinking.is_empty() {
                        "\n▶ Thinking hidden\n".to_string()
                    } else {
                        format!("\n▶ Thinking ({} chars hidden)\n", thinking.chars().count())
                    };
                    buf.push_entry(OutputEntry::thinking(summary));
                }
                "tool_use" => {
                    let id = b
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = b
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let input = b
                        .get("input")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}")
                        .to_string();
                    buf.push_entry(OutputEntry::tool_card_start(id, name, input));
                }
                "tool_result" => {
                    let tool_use_id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    let tool_name = b
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool");
                    let output = b
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_error = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    if tool_use_id.is_empty() {
                        buf.complete_tool_card_by_name(tool_name, output, is_error);
                    } else {
                        buf.complete_tool_card(tool_use_id, output, is_error);
                    }
                }
                _ => {}
            }
        }
    }
    buf.drain_entries()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp_session(lines: &[&str]) -> std::path::PathBuf {
        // 用 target/tmp（不用 %TEMP%，避免触发 TRAE CN 监听）。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
        std::fs::create_dir_all(&dir).unwrap();
        // 文件名加纳秒时间戳：多个测试并行运行（cargo test 默认线程并行）
        // 时避免共用同一 pid 文件互相覆盖/删除。
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "session_replay_test_{}_{}.jsonl",
            std::process::id(),
            ns
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn replay_text_and_tool_pairing() {
        let path = write_tmp_session(&[
            r#"{"type":"session_meta","session_id":"s"}"#,
            r#"{"message":{"blocks":[{"type":"text","text":"你好，这是历史消息"}]},"type":"message","role":"assistant"}"#,
            r#"{"message":{"blocks":[{"type":"thinking","thinking":"思考过程内容"}]},"type":"message","role":"assistant"}"#,
            r#"{"message":{"blocks":[{"type":"tool_use","id":"call_1","name":"bash","input":"{\"command\":\"ls\"}"}]},"type":"message","role":"assistant"}"#,
            r#"{"message":{"blocks":[{"type":"tool_result","tool_use_id":"call_1","tool_name":"bash","output":"{\"stdout\":\"file.txt\"}","is_error":false}]},"type":"message","role":"tool"}"#,
            r#"{"message":{"blocks":[{"type":"text","text":"**总结**：完成"}]},"type":"message","role":"assistant"}"#,
        ]);
        let entries = load_history_entries(&path, 1, 10, Some(80));
        // text(1) + thinking(1) + tool_use(1) + 尾部 text(1) = 4 条；tool_result 配对进卡片不新增
        assert_eq!(entries.len(), 4, "应为 4 条: {entries:?}");
        // 卡片已配对完成（result=Some）
        let card = entries.iter().find_map(|e| match e {
            OutputEntry::ToolCard { name, result, .. } => Some((name.as_str(), result.is_some())),
            _ => None,
        });
        assert_eq!(card, Some(("bash", true)), "tool_use/tool_result 应配对");
        let rendered: String = entries.iter().map(|e| e.render()).collect();
        assert!(rendered.contains("你好，这是历史消息"));
        assert!(rendered.contains("Thinking"));
        assert!(rendered.contains("总结"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_out_of_range_returns_empty() {
        let path = write_tmp_session(&[r#"{"type":"session_meta"}"#]);
        assert!(load_history_entries(&path, 100, 10, None).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
