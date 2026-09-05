//! 双大脑架构 · 管理脑落盘区(设计文档 2026-09-03-dual-brain-architecture.md §5.3)。
//!
//! 架构守护面:会话收尾(HookEvent::Stop)后**确定性聚合**本会话的失败模式
//! 与用量成本(零 LLM)落盘 `.claw/management_brain/digest-<session_id>.md`;
//! 若执行脑在最终答复中输出了 `[SESSION_SUMMARY]` 自写总结(见
//! [`extract_session_summary`]),一并收入 digest"会话总结"段。
//! 新会话首 turn 读取最近 digest 注入,让执行脑感知上一会话遗留(失败教训 /
//! 成本参考 / 结论待办),实现"管理脑消化轨迹 → 知识资产"的跨会话闭环。
//!
//! # 执行脑自写总结(2026-09-05 试点)
//!
//! 会话结论/待办的采集不再走"独立 LLM 提炼通道"(该通道只有尾部截断转录,
//! 早期结论召回差、双轨冗余),改为:请求构造注入一句轻提示 → 执行脑在
//! **最终答复**末尾用完整上下文输出 `[SESSION_SUMMARY]` 标记 → turn 收尾
//! 运行时解析落盘。代价是检测与执行两个软点(执行脑可能不输出标记),试点
//! 观察执行率后决定是否保留。
//!
//! 设计原则:
//! - **非关键路径**:写入/读取/解析失败静默降级(返回 None / 忽略),不阻断会话。
//! - **审计保留**:digest 按 session_id 命名,天然累积为会话审计轨迹,
//!   不做自动清理(体积小;如需清理由外部维护策略决定)。
//! - **不进 system_prompt 静态段**:digest 经动态槽位尾部注入,
//!   不破坏前缀缓存(缓存保护)。

/// digest 落盘目录(相对 workspace_root)。
pub const DIGEST_DIR: &str = ".claw/management_brain";

/// 执行脑自写总结的文本标记(注入提示 + 解析共用,保证字面一致)。
pub const SESSION_SUMMARY_MARKER: &str = "[SESSION_SUMMARY]";

/// 执行脑自写总结的单条上限(字符)。防止模型输出失控撑大 digest 注入体积。
pub const SESSION_SUMMARY_MAX_CHARS: usize = 1200;

/// 注入给执行脑的轻量总结提示(请求构造时随槽位注入,turn 首轮请求一次)。
///
/// 措辞刻意轻:只要求"若已完整处理"才输出标记;中间过程 / 工具循环轮
/// 模型应省略,避免每轮都制造归档噪音。
pub const SESSION_SUMMARY_PROMPT: &str = "[runtime] 若你判断当前请求已完整处理、无需更多工具调用，\
    请在最终答复末尾另起一段输出 [SESSION_SUMMARY]，下面用 ≤5 条 '- ' 列表写明：\
    本会话正在处理的目标 / 已确认的结论 / 未完成的待办(含等待用户确认的项)。\
    若当前只是中间过程、或本次无需跨会话归档，可省略该标记。";

/// 从 assistant 最终答复文本中提取执行脑自写总结(标记之后的内容)。
///
/// 容错规则:
/// - 大小写不敏感匹配(模型可能写成 `[session_summary]` / `[Session Summary]`);
/// - 若标记独占一行,跳过该行取后续内容;标记同行有内容则一并纳入;
/// - 空内容返回 None;超 [`SESSION_SUMMARY_MAX_CHARS`] 截断并加省略号。
pub fn extract_session_summary(final_text: &str) -> Option<String> {
    let marker_lower = SESSION_SUMMARY_MARKER.to_lowercase(); // "[session_summary]"
    let lower = final_text.to_lowercase();
    let idx = lower.find(&marker_lower)?;
    let mut rest = &final_text[idx + SESSION_SUMMARY_MARKER.len()..];
    // 标记行尾若只剩空白(标记独占一行),跳到下一行。
    if let Some(nl) = rest.find('\n') {
        let same_line = rest[..nl].trim();
        if same_line.is_empty() {
            rest = &rest[nl + 1..];
        }
    }
    let content = rest.trim();
    if content.is_empty() {
        return None;
    }
    let truncated: String = content.chars().take(SESSION_SUMMARY_MAX_CHARS).collect();
    if truncated.chars().count() < content.chars().count() {
        Some(format!("{truncated}…"))
    } else {
        Some(content.to_string())
    }
}

/// 单条工具失败聚合记录:`(tool_name, 首行签名摘要)`。
pub type ToolFailure = (String, String);

/// 会话收尾聚合文本(纯函数,便于单测)。
///
/// 输入本会话的失败记录、用量成本行与附加段(执行脑总结等),
/// 输出一份紧凑的 Markdown digest。失败为空时省略失败段(不产生噪音);
/// 附加段内容为空时同样省略。
pub fn render_digest(
    session_id: &str,
    failures: &[ToolFailure],
    usage_summary: &[String],
    extra_sections: &[(&str, String)],
) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!("# Session Digest · {session_id}\n"));

    // 失败模式聚合:按 tool_name 计数,附首行签名样例(最多列 10 条)。
    if !failures.is_empty() {
        use std::collections::BTreeMap;
        let mut by_tool: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (tool, sig) in failures {
            by_tool.entry(tool.as_str()).or_default().push(sig.as_str());
        }
        out.push_str("\n## 失败模式(自动捕获,零 LLM)\n");
        for (tool, sigs) in &by_tool {
            out.push_str(&format!("- **{tool}** × {}\n", sigs.len()));
            for sig in sigs.iter().take(2) {
                let s: String = sig.chars().take(160).collect();
                out.push_str(&format!("  - `{s}`\n"));
            }
        }
    } else {
        out.push_str("\n## 失败模式\n- (本会话无工具失败)\n");
    }

    if !usage_summary.is_empty() {
        out.push_str("\n## 用量与成本\n");
        for line in usage_summary {
            out.push_str(line);
            out.push('\n');
        }
    }

    // 附加段(会话结论 / LLM 提炼等):按出现顺序渲染,空内容跳过。
    for (title, content) in extra_sections {
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {title}\n"));
        out.push_str(content);
        out.push('\n');
    }

    out.push_str("\n---\n管理脑 digest · 供新会话首 turn 感知上一会话遗留。\n");
    out
}

/// 落盘会话 digest 到 `.claw/management_brain/digest-<session_id>.md`。
///
/// 目录不存在时自动创建。失败返回 `Err`(调用方应静默忽略,非关键路径)。
pub fn write_digest(
    workspace_root: &std::path::Path,
    session_id: &str,
    content: &str,
) -> Result<std::path::PathBuf, std::io::Error> {
    let dir = workspace_root.join(DIGEST_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("digest-{session_id}.md"));
    std::fs::write(&path, content)?;
    Ok(path)
}

/// 读取**最近**一份会话 digest 的文本(按文件修改时间倒序取最新)。
///
/// `exclude_session_id`:排除指定会话的 digest(读侧注入时传当前会话 id,
/// 避免多 turn 会话把"自己上一轮写的 digest"误当成"上一会话遗留")。
/// 目录不存在 / 无 digest 文件 / 读取失败均返回 `None`(静默降级)。
pub fn latest_digest_text(
    workspace_root: &std::path::Path,
    exclude_session_id: Option<&str>,
) -> Option<String> {
    let dir = workspace_root.join(DIGEST_DIR);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    if !(n.starts_with("digest-") && n.ends_with(".md")) {
                        return false;
                    }
                    // 排除当前会话自己的 digest(文件名含 session_id)。
                    if let Some(ex) = exclude_session_id {
                        if n.contains(ex) {
                            return false;
                        }
                    }
                    true
                })
                .unwrap_or(false)
        })
        .collect();
    if paths.is_empty() {
        return None;
    }
    // 文件名含 session_id + 时间特征;按文件修改时间倒序取最新。
    paths.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    paths.reverse();
    std::fs::read_to_string(&paths[0]).ok()
}

/// 校验 digest 目录是否存在(供测试断言)。
#[allow(dead_code)]
pub(crate) fn digest_dir_exists(workspace_root: &std::path::Path) -> bool {
    workspace_root.join(DIGEST_DIR).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_digest_includes_failure_modes() {
        let text = render_digest(
            "sess-1",
            &[
                ("edit_file".to_string(), "old_string not found".to_string()),
                (
                    "edit_file".to_string(),
                    "old_string not found again".to_string(),
                ),
                ("bash".to_string(), "command not found".to_string()),
            ],
            &[],
            &[],
        );
        assert!(text.contains("Session Digest"), "text: {text}");
        assert!(text.contains("**edit_file** × 2"), "text: {text}");
        assert!(text.contains("**bash** × 1"), "text: {text}");
        assert!(text.contains("old_string not found"), "text: {text}");
    }

    #[test]
    fn render_digest_omits_failures_when_empty() {
        let text = render_digest("sess-2", &[], &[], &[]);
        assert!(text.contains("无工具失败"), "text: {text}");
    }

    #[test]
    fn render_digest_includes_usage_lines() {
        let text = render_digest(
            "sess-3",
            &[],
            &["session-total: total_tokens=100 input=80 output=20".to_string()],
            &[],
        );
        assert!(text.contains("用量与成本"), "text: {text}");
        assert!(text.contains("total_tokens=100"), "text: {text}");
    }

    #[test]
    fn render_digest_includes_extra_sections_in_order() {
        let text = render_digest(
            "sess-4",
            &[],
            &[],
            &[
                ("会话目标与结论(规则提取)", "目标: 修 bug".to_string()),
                ("会话提炼(LLM 摘要)", String::new()), // 空内容应跳过
                ("会话提炼2", "已确认: 修复生效".to_string()),
            ],
        );
        assert!(text.contains("## 会话目标与结论(规则提取)"), "text: {text}");
        assert!(text.contains("目标: 修 bug"), "text: {text}");
        assert!(
            !text.contains("## 会话提炼(LLM 摘要)"),
            "空段应跳过: {text}"
        );
        assert!(text.contains("已确认: 修复生效"), "text: {text}");
        // 附加段位于尾部注释之前(用量段为空时不渲染,故不作相对比较)。
        let extra_pos = text.find("## 会话目标与结论").unwrap();
        let tail_pos = text.find("管理脑 digest").unwrap();
        assert!(extra_pos < tail_pos, "text: {text}");
    }

    #[test]
    fn write_and_read_latest_digest_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        write_digest(dir.path(), "old-sess", "digest old").expect("write old");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_digest(dir.path(), "new-sess", "digest new").expect("write new");

        let latest = latest_digest_text(dir.path(), None).expect("latest exists");
        assert!(latest.contains("digest new"), "latest: {latest}");
        assert!(digest_dir_exists(dir.path()));
    }

    #[test]
    fn latest_digest_excludes_current_session() {
        let dir = tempfile::tempdir().unwrap();
        // 模拟多 turn 会话:本会话(旧 turn)先写,随后另一会话写 → 最新是他人。
        write_digest(dir.path(), "cur-sess-turn1", "cur own").expect("write cur");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_digest(dir.path(), "other-sess", "other newer").expect("write other");
        std::thread::sleep(std::time::Duration::from_millis(20));
        // 本会话再写(覆盖式) → mtime 最新的是自己;排除后应回退到 other。
        write_digest(dir.path(), "cur-sess-turn2", "cur own latest").expect("write cur again");

        let excluded =
            latest_digest_text(dir.path(), Some("cur-sess")).expect("other digest exists");
        assert!(excluded.contains("other newer"), "excluded: {excluded}");

        // 不排除时仍取最新(兼容原语义)。
        let raw = latest_digest_text(dir.path(), None).expect("latest exists");
        assert!(raw.contains("cur own latest"), "raw: {raw}");
    }

    #[test]
    fn latest_digest_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(latest_digest_text(dir.path(), None).is_none());
        assert!(latest_digest_text(dir.path(), Some("any-sess")).is_none());
    }

    #[test]
    fn latest_digest_none_when_only_current_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_digest(dir.path(), "cur-sess", "own").expect("write");
        // 只剩自己的 digest 时,排除后无可注入内容(读侧自我注入修复)。
        assert!(latest_digest_text(dir.path(), Some("cur-sess")).is_none());
    }

    #[test]
    fn extract_session_summary_captures_block_after_marker() {
        let text = "诊断完成,结论如上。\n\n[SESSION_SUMMARY]\n- 目标: 修复热升级文案\n- 结论: 二进制已含全部修复\n- 待办: 确认具体现象";
        let got = extract_session_summary(text).expect("summary");
        assert!(got.contains("目标: 修复热升级文案"), "got: {got}");
        assert!(got.contains("结论: 二进制已含全部修复"), "got: {got}");
        assert!(!got.contains("诊断完成"), "应只取标记后内容: {got}");
    }

    #[test]
    fn extract_session_summary_is_case_insensitive() {
        let text = "正文\n[session_summary] 目标: 简化机制";
        let got = extract_session_summary(text).expect("summary");
        assert!(got.starts_with("目标: 简化机制"), "got: {got}");
    }

    #[test]
    fn extract_session_summary_none_without_marker() {
        assert!(extract_session_summary("没有任何标记的答复").is_none());
        assert!(extract_session_summary("").is_none());
    }

    #[test]
    fn extract_session_summary_none_when_marker_empty() {
        assert!(extract_session_summary("[SESSION_SUMMARY]\n\n").is_none());
        assert!(extract_session_summary("[SESSION_SUMMARY]").is_none());
    }

    #[test]
    fn extract_session_summary_truncates_overflow() {
        let body = "x".repeat(SESSION_SUMMARY_MAX_CHARS + 500);
        let text = format!("[SESSION_SUMMARY]\n{body}");
        let got = extract_session_summary(&text).expect("summary");
        assert!(
            got.chars().count() <= SESSION_SUMMARY_MAX_CHARS + 1,
            "应截断: len={}",
            got.chars().count()
        );
        assert!(got.ends_with('…'), "截断应带省略号: {got:?}");
    }

    #[test]
    fn session_summary_marker_consistent_between_prompt_and_parser() {
        assert!(SESSION_SUMMARY_PROMPT.contains(SESSION_SUMMARY_MARKER));
    }
}
