//! SlopScanner — 内置幻觉/偷懒信号扫描中间件。
//!
//! 设计动机:AI 在执行任务时倾向输出"看似完成"的最短路径,常见偷懒/幻觉信号:
//! - `unimplemented!()` / `todo!()` / `panic!("not implemented")`:Rust 占位实现
//! - `placeholder` / `stub`:标识符或注释标记的未实现部分
//! - `// removed` / `// deleted` / `// ...`:删除标记伪装完成
//! - `TODO` / `FIXME`:遗留待办(轻量警告)
//!
//! 架构(仿照 `LoopDetector` 中间件模式):
//! - `SlopScanner`:无状态扫描器,对工具产物执行文本扫描
//! - `SlopSignal`:命中的信号项(分级 + 证据片段)
//! - `SlopSeverity`:High(占位实现) / Low(待办注释)
//!
//! 集成点:`Conversation::run_post_tool_use_hook`,仅对 `write_file`/`edit_file`
//! 工具的产物扫描,命中时以 warning 追加到 hook messages(不阻断)。
//!
//! 缓存保护:扫描走纯文本匹配,不调 LLM,不修改 system prompt,
//! 输出通过 `HookRunResult::append_message` 回灌到 tool result,
//! 完全不影响 prompt cache 的 static_sections。
//!
//! 配置:通过 `.claw.json` 的 `slopScan` 字段 opt-out(默认开启)。

use serde::{Deserialize, Serialize};

/// 信号严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlopSeverity {
    /// 高置信偷懒/幻觉:占位实现、伪完成标记。必须明确警告。
    High,
    /// 低置信:待办注释、修复标记。轻量提示。
    Low,
}

/// 命中的单个信号项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlopSignal {
    /// 信号级别。
    pub severity: SlopSeverity,
    /// 命中的关键词或模式(如 `unimplemented!`、`TODO`)。
    pub pattern: String,
    /// 证据片段(命中的那一行,已 trim,上限 120 字符)。
    pub evidence: String,
    /// 在新增内容中的行号(1-based)。
    pub line: usize,
}

impl SlopSignal {
    fn new(severity: SlopSeverity, pattern: &str, evidence: &str, line: usize) -> Self {
        let trimmed = evidence.trim();
        let capped: String = trimmed.chars().take(120).collect();
        Self {
            severity,
            pattern: pattern.to_string(),
            evidence: capped,
            line,
        }
    }
}

/// High 置信度信号:占位实现 / 伪完成标记。
///
/// 这些模式几乎总是表示"声称完成但实际未实现"。
const HIGH_PATTERNS: &[&str] = &[
    "unimplemented!",
    "todo!()",
    "unreachable!()",
    "panic!(\"not implemented\"",
    "panic!(\"todo\"",
    "panic!(\"unimplemented\"",
    "not_implemented",
    "placeholder",
    "stub_implementation",
    "// removed",
    "// deleted",
    "/* removed */",
    "/* deleted */",
];

/// Low 置信度信号:待办注释 / 修复标记。
///
/// 这些模式是合法的工程标记,但若声称任务完成却留下这些,值得提示。
const LOW_PATTERNS: &[&str] = &["TODO", "FIXME", "HACK", "XXX"];

/// 无状态扫描器。
///
/// 不持有任何可变状态,可在多线程间共享。每次调用 [`SlopScanner::scan`]
/// 都是对传入文本的独立扫描。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlopScanner;

impl SlopScanner {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 扫描文本,返回所有命中的信号(按行号排序)。
    ///
    /// 每行最多产生一个信号(优先 High),避免单行重复匹配噪音。
    pub fn scan(&self, content: &str) -> Vec<SlopSignal> {
        let mut signals = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            if let Some(pattern) = HIGH_PATTERNS.iter().find(|p| line.contains(*p)) {
                signals.push(SlopSignal::new(SlopSeverity::High, pattern, line, line_no));
                continue;
            }
            if let Some(pattern) = LOW_PATTERNS.iter().find(|p| line.contains(*p)) {
                signals.push(SlopSignal::new(SlopSeverity::Low, pattern, line, line_no));
            }
        }
        signals
    }

    /// 将信号列表渲染为 warning 消息文本(供 hook message 回灌)。
    ///
    /// 设计:
    /// - 无信号 → 返回 `None`(调用方据此跳过 append)
    /// - 有信号 → 单条消息,包含 High/Low 计数 + 每条证据
    /// - 上限 5 条证据,超出则聚合"还有 N 条"
    #[must_use]
    pub fn render_warning(&self, signals: &[SlopSignal]) -> Option<String> {
        if signals.is_empty() {
            return None;
        }
        let high_count = signals
            .iter()
            .filter(|s| s.severity == SlopSeverity::High)
            .count();
        let low_count = signals.len() - high_count;

        let mut msg = String::from("⚠ Slop scan (built-in hallucination/laziness guard):\n");
        if high_count > 0 {
            msg.push_str(&format!(
                "  High-confidence placeholder/stub signals: {high_count}\n"
            ));
        }
        if low_count > 0 {
            msg.push_str(&format!(
                "  Low-confidence TODO/FIXME markers: {low_count}\n"
            ));
        }
        msg.push_str("  Evidence (up to 5):");
        for sig in signals.iter().take(5) {
            let tag = match sig.severity {
                SlopSeverity::High => "HIGH",
                SlopSeverity::Low => "low",
            };
            msg.push_str(&format!(
                "\n    [{tag}] L{} {}: {}",
                sig.line, sig.pattern, sig.evidence
            ));
        }
        if signals.len() > 5 {
            msg.push_str(&format!("\n    ...and {} more", signals.len() - 5));
        }
        msg.push_str(
            "\n  If this is intentional (e.g. TDD red phase, scaffold), ignore. \
             Otherwise, verify the task is actually complete.",
        );
        Some(msg)
    }
}

/// 从 write_file/edit_file 工具的 JSON 输出中提取需扫描的新增文本。
///
/// write_file 输出含 `content` 字段(完整写入内容);
/// edit_file 输出含 `newString` 字段(替换后的文本)。
/// 两者都可能含 `structuredPatch.lines`(diff 行,以 `+`/`-`/` ` 开头)。
///
/// 优先级:直接字段 > structuredPatch 的新增行。返回 `None` 表示无法提取
/// (如 JSON 解析失败、非文件工具输出),调用方据此跳过扫描。
pub fn extract_scan_target(tool_output: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(tool_output).ok()?;
    let obj = value.as_object()?;

    // 优先:edit_file 的 newString(替换后的内容,最精确)
    if let Some(new_str) = obj.get("newString").and_then(serde_json::Value::as_str) {
        if !new_str.trim().is_empty() {
            return Some(new_str.to_string());
        }
    }
    // 次选:write_file 的 content(完整写入内容)
    if let Some(content) = obj.get("content").and_then(serde_json::Value::as_str) {
        if !content.trim().is_empty() {
            return Some(content.to_string());
        }
    }
    // 兜底:structuredPatch.lines 中以 '+' 开头的行(新增行)
    if let Some(patch) = obj
        .get("structuredPatch")
        .and_then(serde_json::Value::as_array)
    {
        let mut added = String::new();
        for hunk in patch {
            if let Some(lines) = hunk.get("lines").and_then(serde_json::Value::as_array) {
                for line in lines {
                    if let Some(s) = line.as_str() {
                        if let Some(rest) = s.strip_prefix('+') {
                            added.push_str(rest);
                            added.push('\n');
                        }
                    }
                }
            }
        }
        if !added.trim().is_empty() {
            return Some(added);
        }
    }
    None
}

/// 判断工具名是否为文件修改类(需扫描)。
///
/// dispatch 链使用小写加下划线的工具名(`write_file`/`edit_file`),
/// 部分历史路径使用 PascalCase(`Write`/`Edit`/`MultiEdit`/`NotebookEdit`)。
/// 两者都接受。
pub fn is_file_modifying_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "write_file" | "edit_file" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_detects_unimplemented_macro() {
        let scanner = SlopScanner::new();
        let signals = scanner.scan("fn foo() -> i32 {\n    unimplemented!()\n}\n");
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].severity, SlopSeverity::High);
        assert_eq!(signals[0].pattern, "unimplemented!");
        assert_eq!(signals[0].line, 2);
    }

    #[test]
    fn scan_detects_todo_macro_and_comment_distinctly() {
        let scanner = SlopScanner::new();
        // todo!() 是 High,TODO 注释是 Low,两行各产一个信号
        let signals = scanner.scan("todo!()\n// TODO: refactor\n");
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].severity, SlopSeverity::High);
        assert_eq!(signals[0].pattern, "todo!()");
        assert_eq!(signals[1].severity, SlopSeverity::Low);
        assert_eq!(signals[1].pattern, "TODO");
    }

    #[test]
    fn scan_detects_removed_markers() {
        let scanner = SlopScanner::new();
        let signals = scanner.scan("let x = 1; // removed old logic\n");
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].severity, SlopSeverity::High);
        assert_eq!(signals[0].pattern, "// removed");
    }

    #[test]
    fn scan_clean_code_produces_no_signals() {
        let scanner = SlopScanner::new();
        let signals = scanner.scan("fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
        assert!(signals.is_empty());
    }

    #[test]
    fn scan_high_takes_precedence_on_same_line() {
        // 同一行既有 unimplemented! 又有 TODO,只产 High 信号(每行最多一个)
        let scanner = SlopScanner::new();
        let signals = scanner.scan("unimplemented!() // TODO fix\n");
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].severity, SlopSeverity::High);
    }

    #[test]
    fn render_warning_returns_none_when_empty() {
        let scanner = SlopScanner::new();
        assert!(scanner.render_warning(&[]).is_none());
    }

    #[test]
    fn render_warning_includes_counts_and_evidence() {
        let scanner = SlopScanner::new();
        let signals = scanner.scan("unimplemented!()\n// TODO: x\nFIXME: y\n");
        let warning = scanner.render_warning(&signals).unwrap();
        assert!(warning.contains("High-confidence placeholder/stub signals: 1"));
        assert!(warning.contains("Low-confidence TODO/FIXME markers: 2"));
        assert!(warning.contains("unimplemented!"));
        assert!(warning.contains("TODO"));
        assert!(warning.contains("FIXME"));
    }

    #[test]
    fn render_warning_caps_at_five_evidence() {
        let scanner = SlopScanner::new();
        let content = (0..10)
            .map(|i| format!("unimplemented!() // case {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let signals = scanner.scan(&content);
        let warning = scanner.render_warning(&signals).unwrap();
        assert!(warning.contains("...and 5 more"));
    }

    #[test]
    fn extract_scan_target_prefers_newstring() {
        let output = r#"{"newString":"todo!()","content":"unimplemented!()"}"#;
        let target = extract_scan_target(output).unwrap();
        assert!(target.contains("todo!()"));
        assert!(!target.contains("unimplemented!"));
    }

    #[test]
    fn extract_scan_target_falls_back_to_content() {
        let output = r#"{"content":"unimplemented!()"}"#;
        let target = extract_scan_target(output).unwrap();
        assert!(target.contains("unimplemented!"));
    }

    #[test]
    fn extract_scan_target_falls_back_to_structured_patch() {
        let output = r#"{"structuredPatch":[{"lines":["+todo!()","-old"," context"]}]}"#;
        let target = extract_scan_target(output).unwrap();
        assert!(target.contains("todo!()"));
        assert!(!target.contains("old"));
    }

    #[test]
    fn extract_scan_target_returns_none_for_non_json() {
        assert!(extract_scan_target("not json").is_none());
    }

    #[test]
    fn extract_scan_target_returns_none_for_empty_content() {
        let output = r#"{"content":"   "}"#;
        assert!(extract_scan_target(output).is_none());
    }

    #[test]
    fn is_file_modifying_tool_accepts_both_naming_conventions() {
        assert!(is_file_modifying_tool("write_file"));
        assert!(is_file_modifying_tool("edit_file"));
        assert!(is_file_modifying_tool("Write"));
        assert!(is_file_modifying_tool("Edit"));
        assert!(is_file_modifying_tool("MultiEdit"));
        assert!(!is_file_modifying_tool("read_file"));
        assert!(!is_file_modifying_tool("bash"));
    }

    #[test]
    fn evidence_is_capped_at_120_chars() {
        let scanner = SlopScanner::new();
        let long_line = format!("unimplemented!() // {}", "x".repeat(200));
        let signals = scanner.scan(&long_line);
        assert_eq!(signals.len(), 1);
        assert!(signals[0].evidence.chars().count() <= 120);
    }
}
