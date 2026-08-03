//! P3:完成声明校验 — 四条件严格 gating + 30s 超时。
//!
//! 当 LLM 声称任务"完成"但本轮无工具调用时,自动执行项目验证命令
//! (如 `cargo check`)。验证失败则把 remediation 注入下一 turn,
//! 引导 LLM 修复而非盲目声称完成。
//!
//! ## 四条件 gating(全部满足才触发)
//!
//! 1. **Turn end**:本轮 LLM 响应无 ToolUse 块(pending_tool_uses.is_empty())
//! 2. **No tool calls**:同上,LLM 选择停止调用工具
//! 3. **Contains completion claim**:LLM 文本包含完成信号词(完成/done/implemented 等)
//! 4. **Not already verified**:本 turn 尚未执行过完成声明校验
//!
//! ## 缓存保护(§5.2)
//!
//! - 验证命令走子进程,**不调 LLM,不影响主 agent prompt cache**
//! - remediation 复用现有 `pending_remediation` → `dynamic_sections` 路径(变动区末尾)
//! - 通过则不注入任何内容,零上下文膨胀
//! - 失败时注入 ~200-500 tokens remediation,下一 turn 自动清空
//!
//! ## 验证命令来源
//!
//! 1. 自动探测:根据 workspace 根目录的项目文件(Cargo.toml/package.json 等)
//! 2. 配置覆盖:`settings.completionVerifyCommands` 数组(优先于自动探测)
//! 3. 无命令时跳过(不阻塞,不报错)

use std::path::Path;

use crate::verifier::rule::RuleVerifier;

/// P3 默认超时:30 秒(用户批准的 spec)。
pub const COMPLETION_VERIFY_TIMEOUT_SECS: u64 = 30;

/// 完成信号词 — LLM 声称任务完成的关键词。
///
/// 中文直接子串匹配(中文无词边界问题),英文用短语匹配避免误报
/// (如 "done" 不会匹配 "background")。
const COMPLETION_SIGNALS_ZH: &[&str] = &[
    "完成",
    "已实现",
    "已完成",
    "实现完毕",
    "修改完毕",
    "任务完成",
    "已修复",
    "已处理",
];

const COMPLETION_SIGNALS_EN: &[&str] = &[
    "i'm done",
    "i am done",
    "is done",
    "are done",
    "all done",
    "we're done",
    "we are done",
    "task complete",
    "task completed",
    "implementation complete",
    "is complete",
    "are complete",
    "finished implementing",
    "has been implemented",
    "have been implemented",
    "is now implemented",
    "successfully implemented",
    "i've finished",
    "i have finished",
];

/// 检测到的完成声明信号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionSignal {
    /// 匹配到的信号词。
    pub pattern: String,
    /// 匹配到的原文片段(截断到 80 字符)。
    pub evidence: String,
}

/// 单条验证命令的执行结果。
#[derive(Debug, Clone)]
pub struct CommandVerificationResult {
    /// 执行的命令。
    pub command: String,
    /// 是否通过(exit_code == 0)。
    pub passed: bool,
    /// 详细说明。
    pub detail: String,
    /// 失败时的修正建议。
    pub remediation: Option<String>,
    /// 验证耗时(毫秒)。
    pub elapsed_ms: u64,
}

/// 完成声明校验器 — P3 核心组件。
///
/// 在 LLM 声称完成且无工具调用时,执行项目验证命令。
/// 验证失败则生成 remediation 注入下一 turn system prompt。
///
/// 缓存保护:纯子进程执行,不调 LLM,不污染 prompt cache。
#[derive(Debug, Clone)]
pub struct CompletionVerifier {
    /// 内部复用 RuleVerifier 执行命令(带超时)。
    rule_verifier: RuleVerifier,
}

impl Default for CompletionVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            rule_verifier: RuleVerifier::new().with_timeout(COMPLETION_VERIFY_TIMEOUT_SECS),
        }
    }

    /// 检测 LLM 文本输出中是否包含完成声明信号词。
    ///
    /// 扫描所有 Text 块的拼接文本,命中任一信号词返回 `Some(signal)`。
    /// 中文用子串匹配,英文用大小写不敏感短语匹配。
    #[must_use]
    pub fn detect_completion_claim(text: &str) -> Option<CompletionSignal> {
        if text.is_empty() {
            return None;
        }

        // 中文信号词:直接子串匹配
        for pattern in COMPLETION_SIGNALS_ZH {
            if text.contains(pattern) {
                let evidence = extract_evidence(text, pattern);
                return Some(CompletionSignal {
                    pattern: (*pattern).to_owned(),
                    evidence,
                });
            }
        }

        // 英文信号词:大小写不敏感匹配
        let lowered = text.to_ascii_lowercase();
        for pattern in COMPLETION_SIGNALS_EN {
            if lowered.contains(pattern) {
                let evidence = extract_evidence(text, pattern);
                return Some(CompletionSignal {
                    pattern: (*pattern).to_owned(),
                    evidence,
                });
            }
        }

        None
    }

    /// 自动探测项目验证命令 — 根据 workspace 根目录的项目文件 + CLAUDE.md。
    ///
    /// 改进点 8/9/10:
    /// - 多语言项目收集所有匹配语言的命令(不再命中即 return)
    /// - Node 项目读取 package.json scripts,按 test > lint > build 优先级选择
    /// - 优先合并 CLAUDE.md 中声明的验证命令
    /// 未探测到返回空 Vec(跳过验证)。
    #[must_use]
    pub fn detect_project_commands(workspace_root: &Path) -> Vec<String> {
        let mut commands = Vec::new();

        // 改进点 8:优先解析 CLAUDE.md 中的验证命令(与文件检测合并,去重)
        let claude_commands = parse_claude_md_verify_commands(workspace_root);
        commands.extend(claude_commands);

        // Rust: cargo check(快速编译检查,不跑测试)
        if workspace_root.join("Cargo.toml").exists() {
            commands.push("cargo check".to_owned());
        }

        // Node.js: 读取 package.json scripts,按优先级选择(改进点 9)
        if workspace_root.join("package.json").exists() {
            commands.extend(detect_node_commands(workspace_root));
        }

        // Go: go build ./...
        if workspace_root.join("go.mod").exists() {
            commands.push("go build ./...".to_owned());
        }

        // Python: python -m pytest(如果 pyproject.toml/setup.py 存在)
        if workspace_root.join("pyproject.toml").exists()
            || workspace_root.join("setup.py").exists()
        {
            commands.push("python -m pytest --tb=short -q".to_owned());
        }

        // 改进点 10:多语言/CLAUDE.md 合并后去重,保留首次出现
        dedupe_commands(&mut commands);
        commands
    }

    /// 执行验证命令列表,收集每条命令的结果。
    ///
    /// 每条命令独立执行,互不影响(一条失败不阻断后续)。
    /// 工作目录设为 `workspace_root`,超时 30 秒。
    #[must_use]
    pub fn run_verification(
        &self,
        commands: &[String],
        workspace_root: &Path,
    ) -> Vec<CommandVerificationResult> {
        let verifier = self.rule_verifier.clone().with_working_dir(workspace_root);

        commands
            .iter()
            .map(|cmd| {
                let start = std::time::Instant::now();
                let verdict = verifier.verify("", "completion claim verified", Some(cmd));
                let elapsed_ms = start.elapsed().as_millis() as u64;
                CommandVerificationResult {
                    command: cmd.clone(),
                    passed: verdict.passed,
                    detail: verdict.detail,
                    remediation: verdict.remediation,
                    elapsed_ms,
                }
            })
            .collect()
    }

    /// 将失败的验证结果格式化为 remediation prompt,注入下一 turn。
    ///
    /// 全部通过返回 `None`(不注入任何内容,零上下文膨胀)。
    /// 有失败返回格式化文本(~200-500 tokens),复用 `pending_remediation` 路径。
    #[must_use]
    pub fn render_remediation(results: &[CommandVerificationResult]) -> Option<String> {
        let failures: Vec<&CommandVerificationResult> =
            results.iter().filter(|r| !r.passed).collect();

        if failures.is_empty() {
            return None;
        }

        let mut out = String::with_capacity(256 + failures.len() * 400);
        out.push_str("# ⚠️ Completion Verification Failed — Do Not Claim Done\n\n");
        out.push_str(&format!(
            "你上一轮声称任务完成,但 {} 条验证命令失败。\n",
            failures.len()
        ));
        out.push_str(
            "请针对每条失败命令的 remediation 修复问题,**不要**在未通过验证前再次声称完成。\n\n",
        );

        for (idx, f) in failures.iter().enumerate() {
            out.push_str(&format!("## Failure {}: `{}`\n", idx + 1, f.command));
            out.push_str(&format!("- Detail: {}\n", f.detail));
            if let Some(rem) = &f.remediation {
                // 截断 remediation 避免上下文膨胀(保留前 2KB)
                let truncated = if rem.len() > 2048 {
                    format!("{}...(truncated)", &rem[..2048])
                } else {
                    rem.clone()
                };
                out.push_str(&format!("- Remediation:\n{truncated}\n"));
            }
            out.push('\n');
        }

        out.push_str("修复后重新运行验证命令确认通过,然后再回复完成。\n");

        Some(out)
    }
}

/// 从原文中提取信号词周围的上下文作为 evidence(截断到 80 字符)。
fn extract_evidence(text: &str, pattern: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    let pattern_lower = pattern.to_ascii_lowercase();
    if let Some(pos) = lowered.find(&pattern_lower) {
        // 向前后各扩展 30 字符作为上下文
        let start = pos.saturating_sub(30);
        let end = (pos + pattern.len() + 30).min(text.len());
        // 对齐到 char 边界
        let start = text
            .char_indices()
            .find(|&(i, _)| i >= start)
            .map(|(i, _)| i)
            .unwrap_or(start);
        let end = text
            .char_indices()
            .rev()
            .find(|&(i, _)| i <= end)
            .map(|(i, _)| i)
            .unwrap_or(end);
        let snippet = &text[start..end];
        let truncated = if snippet.chars().count() > 80 {
            let chars: Vec<char> = snippet.chars().take(80).collect();
            format!("{}...", chars.iter().collect::<String>())
        } else {
            snippet.to_owned()
        };
        truncated
    } else {
        // fallback: 截断 pattern 本身
        let truncated = if pattern.len() > 80 {
            format!("{}...", &pattern[..80])
        } else {
            pattern.to_owned()
        };
        truncated
    }
}

/// 读取 package.json 的 scripts,按 test > lint > build 优先级选择验证命令(改进点 9)。
///
/// 解析失败、无 scripts、或三者都没有时,回退到 `npm run build`(原行为)。
fn detect_node_commands(workspace_root: &Path) -> Vec<String> {
    let package_json_path = workspace_root.join("package.json");
    let content = match std::fs::read_to_string(&package_json_path) {
        Ok(c) => c,
        Err(_) => return vec!["npm run build".to_owned()],
    };
    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec!["npm run build".to_owned()],
    };
    let scripts = match parsed.get("scripts").and_then(|s| s.as_object()) {
        Some(s) => s,
        None => return vec!["npm run build".to_owned()],
    };
    // 优先级:test > lint > build
    if scripts.contains_key("test") {
        return vec!["npm test".to_owned()];
    }
    if scripts.contains_key("lint") {
        return vec!["npm run lint".to_owned()];
    }
    if scripts.contains_key("build") {
        return vec!["npm run build".to_owned()];
    }
    // 三者都没有:回退到默认行为
    vec!["npm run build".to_owned()]
}

/// 从 CLAUDE.md(含 .claw/CLAUDE.md)中提取验证命令(改进点 8)。
///
/// 扫描所有 ```bash / ```sh 代码块,提取包含验证关键词(cargo test、
/// npm test、pytest 等)的命令行,去重后返回。文件不存在或无匹配返回空 Vec。
fn parse_claude_md_verify_commands(workspace_root: &Path) -> Vec<String> {
    let candidates = [
        workspace_root.join("CLAUDE.md"),
        workspace_root.join(".claw").join("CLAUDE.md"),
    ];
    let mut commands = Vec::new();
    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            extract_verify_commands_from_markdown(&content, &mut commands);
        }
    }
    dedupe_commands(&mut commands);
    commands
}

/// 验证命令关键词 — 用于在 CLAUDE.md 代码块中识别验证命令行。
const VERIFY_KEYWORDS: &[&str] = &[
    "cargo test",
    "cargo clippy",
    "cargo check",
    "npm test",
    "npm run lint",
    "npm run build",
    "pytest",
    "ruff",
    "mypy",
    "make test",
    "make check",
    "make lint",
    "go test",
    "go vet",
    "pyright",
    "flake8",
];

/// 从 Markdown 文本中提取 ```bash / ```sh 代码块内的验证命令。
fn extract_verify_commands_from_markdown(content: &str, out: &mut Vec<String>) {
    let mut in_code_block = false;
    for line in content.lines() {
        let trimmed = line.trim();
        // 检测代码块围栏
        if trimmed.starts_with("```") {
            if in_code_block {
                // 关闭围栏
                in_code_block = false;
            } else {
                // 开启围栏:仅 bash / sh 语言块
                let lang = trimmed.trim_start_matches("```").trim();
                if lang == "bash" || lang == "sh" {
                    in_code_block = true;
                }
            }
            continue;
        }
        if in_code_block {
            // 跳过空行和注释行
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // 去掉常见的 shell prompt 前缀 $
            let cmd = trimmed.trim_start_matches('$').trim();
            for kw in VERIFY_KEYWORDS {
                if cmd.contains(kw) {
                    out.push(cmd.to_owned());
                    break;
                }
            }
        }
    }
}

/// 去重:保留首次出现,移除后续重复命令(保持顺序)。
fn dedupe_commands(commands: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    commands.retain(|c| seen.insert(c.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_completion_claim ──

    #[test]
    fn detect_chinese_completion_signal() {
        let signal = CompletionVerifier::detect_completion_claim("任务已完成,所有修改通过测试。");
        assert!(signal.is_some());
        let s = signal.unwrap();
        assert!(s.pattern.contains("完成"));
        assert!(!s.evidence.is_empty());
    }

    #[test]
    fn detect_english_completion_signal() {
        let signal =
            CompletionVerifier::detect_completion_claim("I'm done with the implementation.");
        assert!(signal.is_some());
        let s = signal.unwrap();
        assert_eq!(s.pattern, "i'm done");
    }

    #[test]
    fn detect_implemented_signal() {
        let signal = CompletionVerifier::detect_completion_claim(
            "The feature has been implemented successfully.",
        );
        assert!(signal.is_some());
    }

    #[test]
    fn no_signal_for_regular_text() {
        let signal = CompletionVerifier::detect_completion_claim("Let me check the file first.");
        assert!(signal.is_none());
    }

    #[test]
    fn no_false_positive_for_background() {
        // "done" in "background" should NOT match
        let signal = CompletionVerifier::detect_completion_claim("running in background mode");
        assert!(signal.is_none());
    }

    #[test]
    fn no_signal_for_empty_text() {
        assert!(CompletionVerifier::detect_completion_claim("").is_none());
    }

    #[test]
    fn detect_all_done_signal() {
        let signal = CompletionVerifier::detect_completion_claim("All done!");
        assert!(signal.is_some());
        assert_eq!(signal.unwrap().pattern, "all done");
    }

    #[test]
    fn detect_chinese_yixiancheng_signal() {
        let signal = CompletionVerifier::detect_completion_claim("功能已实现,请验收。");
        assert!(signal.is_some());
        assert!(signal.unwrap().pattern.contains("已实现"));
    }

    // ── detect_project_commands ──

    #[test]
    fn detect_rust_project() {
        let temp = std::env::temp_dir();
        let rust_dir = temp.join("test_completion_rust");
        let _ = std::fs::create_dir_all(&rust_dir);
        let _ = std::fs::write(
            rust_dir.join("Cargo.toml"),
            "[package]\nname = \"test\"\nversion = \"0.1\"\n",
        );

        let commands = CompletionVerifier::detect_project_commands(&rust_dir);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], "cargo check");

        let _ = std::fs::remove_dir_all(&rust_dir);
    }

    #[test]
    fn detect_no_project_returns_empty() {
        let temp = std::env::temp_dir();
        let empty_dir = temp.join("test_completion_empty");
        let _ = std::fs::create_dir_all(&empty_dir);

        let commands = CompletionVerifier::detect_project_commands(&empty_dir);
        assert!(commands.is_empty());

        let _ = std::fs::remove_dir_all(&empty_dir);
    }

    #[test]
    fn detect_multi_language_project() {
        // 改进点 10:Rust + Python 混合项目应返回两条命令(不再命中即 return)
        let temp = std::env::temp_dir();
        let mixed_dir = temp.join("test_completion_mixed");
        let _ = std::fs::remove_dir_all(&mixed_dir);
        let _ = std::fs::create_dir_all(&mixed_dir);
        let _ = std::fs::write(
            mixed_dir.join("Cargo.toml"),
            "[package]\nname = \"test\"\nversion = \"0.1\"\n",
        );
        let _ = std::fs::write(
            mixed_dir.join("pyproject.toml"),
            "[project]\nname = \"test\"\n",
        );

        let commands = CompletionVerifier::detect_project_commands(&mixed_dir);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0], "cargo check");
        assert_eq!(commands[1], "python -m pytest --tb=short -q");

        let _ = std::fs::remove_dir_all(&mixed_dir);
    }

    #[test]
    fn detect_node_project_with_test_script() {
        // 改进点 9:有 test 脚本时优先用 npm test
        let temp = std::env::temp_dir();
        let node_dir = temp.join("test_completion_node_test");
        let _ = std::fs::remove_dir_all(&node_dir);
        let _ = std::fs::create_dir_all(&node_dir);
        let _ = std::fs::write(
            node_dir.join("package.json"),
            r#"{"scripts": {"test": "jest", "lint": "eslint .", "build": "tsc"}}"#,
        );

        let commands = CompletionVerifier::detect_project_commands(&node_dir);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], "npm test");

        let _ = std::fs::remove_dir_all(&node_dir);
    }

    #[test]
    fn detect_node_project_with_lint_script() {
        // 改进点 9:无 test、有 lint → npm run lint
        let temp = std::env::temp_dir();
        let node_dir = temp.join("test_completion_node_lint");
        let _ = std::fs::remove_dir_all(&node_dir);
        let _ = std::fs::create_dir_all(&node_dir);
        let _ = std::fs::write(
            node_dir.join("package.json"),
            r#"{"scripts": {"lint": "eslint .", "build": "tsc"}}"#,
        );

        let commands = CompletionVerifier::detect_project_commands(&node_dir);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], "npm run lint");

        let _ = std::fs::remove_dir_all(&node_dir);
    }

    #[test]
    fn detect_node_project_with_only_build_script() {
        // 改进点 9:只有 build → npm run build
        let temp = std::env::temp_dir();
        let node_dir = temp.join("test_completion_node_build");
        let _ = std::fs::remove_dir_all(&node_dir);
        let _ = std::fs::create_dir_all(&node_dir);
        let _ = std::fs::write(node_dir.join("package.json"), r#"{"scripts": {"build": "tsc"}}"#);

        let commands = CompletionVerifier::detect_project_commands(&node_dir);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], "npm run build");

        let _ = std::fs::remove_dir_all(&node_dir);
    }

    #[test]
    fn detect_node_project_invalid_json_falls_back() {
        // 改进点 9:JSON 解析失败 → 回退到 npm run build
        let temp = std::env::temp_dir();
        let node_dir = temp.join("test_completion_node_invalid");
        let _ = std::fs::remove_dir_all(&node_dir);
        let _ = std::fs::create_dir_all(&node_dir);
        let _ = std::fs::write(node_dir.join("package.json"), "{ not valid json");

        let commands = CompletionVerifier::detect_project_commands(&node_dir);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], "npm run build");

        let _ = std::fs::remove_dir_all(&node_dir);
    }

    #[test]
    fn parse_claude_md_verify_commands_extracts_bash_and_sh() {
        // 改进点 8:从 ```bash / ```sh 块提取验证命令,忽略其他语言块
        let temp = std::env::temp_dir();
        let dir = temp.join("test_completion_claude_md");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("CLAUDE.md"),
            "# Project\n\n## Verify\n\n```bash\ncargo test\ncargo clippy\n```\n\n```sh\nnpm run lint\n```\n\n```python\nprint('hi')\n```\n",
        );

        let commands = parse_claude_md_verify_commands(&dir);
        assert!(commands.contains(&"cargo test".to_owned()));
        assert!(commands.contains(&"cargo clippy".to_owned()));
        assert!(commands.contains(&"npm run lint".to_owned()));
        // python 代码块不应被解析
        assert!(!commands.contains(&"print('hi')".to_owned()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_claude_md_verify_commands_no_file_returns_empty() {
        let temp = std::env::temp_dir();
        let dir = temp.join("test_completion_no_claude_md");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let commands = parse_claude_md_verify_commands(&dir);
        assert!(commands.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_project_commands_merges_claude_md_and_file_detection() {
        // 改进点 8:CLAUDE.md 命令与文件检测命令合并去重
        let temp = std::env::temp_dir();
        let dir = temp.join("test_completion_merge");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"test\"\nversion = \"0.1\"\n",
        );
        let _ = std::fs::write(dir.join("CLAUDE.md"), "```bash\ncargo test\n```\n");

        let commands = CompletionVerifier::detect_project_commands(&dir);
        // CLAUDE.md 的 cargo test + 文件检测的 cargo check
        assert!(commands.contains(&"cargo test".to_owned()));
        assert!(commands.contains(&"cargo check".to_owned()));
        // 去重:不应有重复条目
        let check_count = commands.iter().filter(|c| *c == "cargo check").count();
        assert_eq!(check_count, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_claude_md_strips_dollar_prompt() {
        // 验证 $ prompt 前缀被去除
        let temp = std::env::temp_dir();
        let dir = temp.join("test_completion_dollar");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("CLAUDE.md"),
            "```bash\n$ cargo test\n$ make check\n```\n",
        );

        let commands = parse_claude_md_verify_commands(&dir);
        assert!(commands.contains(&"cargo test".to_owned()));
        assert!(commands.contains(&"make check".to_owned()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── render_remediation ──

    #[test]
    fn render_remediation_none_when_all_pass() {
        let results = vec![CommandVerificationResult {
            command: "cargo check".to_owned(),
            passed: true,
            detail: "exited 0".to_owned(),
            remediation: None,
            elapsed_ms: 100,
        }];
        assert!(CompletionVerifier::render_remediation(&results).is_none());
    }

    #[test]
    fn render_remediation_some_when_failure() {
        let results = vec![CommandVerificationResult {
            command: "cargo check".to_owned(),
            passed: false,
            detail: "exited 1".to_owned(),
            remediation: Some("compile error".to_owned()),
            elapsed_ms: 200,
        }];
        let remediation = CompletionVerifier::render_remediation(&results);
        assert!(remediation.is_some());
        let r = remediation.unwrap();
        assert!(r.contains("Completion Verification Failed"));
        assert!(r.contains("cargo check"));
        assert!(r.contains("compile error"));
    }

    #[test]
    fn render_remediation_empty_results_returns_none() {
        let results: Vec<CommandVerificationResult> = vec![];
        assert!(CompletionVerifier::render_remediation(&results).is_none());
    }

    // ── extract_evidence ──

    #[test]
    fn extract_evidence_includes_context() {
        let text = "I have checked the file and I'm done with the changes.";
        let evidence = extract_evidence(text, "I'm done");
        assert!(evidence.contains("I'm done"));
        assert!(evidence.len() < 120);
    }

    #[test]
    fn extract_evidence_truncates_long_snippets() {
        // 用足够长的 pattern 使 snippet > 80 chars(30 ctx + pattern + 30 ctx > 80)
        let pattern = "i have finished implementing everything here"; // 43 chars
        let long_text = format!("{}{}{}", "a".repeat(100), pattern, "b".repeat(100));
        let evidence = extract_evidence(&long_text, pattern);
        assert!(evidence.contains("finished"));
        // snippet = 30 + 43 + 30 = 103 chars > 80 → 触发截断
        assert!(evidence.ends_with("..."));
    }
}
