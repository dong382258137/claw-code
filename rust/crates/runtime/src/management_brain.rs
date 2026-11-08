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
/// 输入本会话的失败记录与附加段(执行脑总结等),输出一份紧凑的 Markdown
/// digest。失败为空时省略失败段(不产生噪音);附加段内容为空时同样省略。
/// 成本注记只进面板/diag,永不写入模型上下文。
pub fn render_digest(
    session_id: &str,
    failures: &[ToolFailure],
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

/// digest 保留上限(按文件数)。超出时删除最旧的 digest。
///
/// 动机:此前无任何清理(doc 注释明确「不做自动清理」),实测累积 18 份且
/// 无上限。digest 是**会话交接载体**,不是永久审计库 —— 审计轨迹由
/// `.claw/history.db`(FTS5 全文)承担,digest 只保留近期窗口。
pub const MAX_DIGEST_FILES: usize = 50;

/// 判断一份 digest 是否含**实质交接内容**。
///
/// 背景:一份 digest 若全文只有「失败模式:本会话无工具失败」,新会话首 turn
/// 花一个槽位读到的是空转内容,而非交接。若最新一份是这种空转 digest 而
/// 更早有一份真交接,按 mtime 取最新会让**空转覆盖有效交接**。
///
/// 判定(保守:不确定为实质,宁可注入也不静默丢信息):
/// - 符合本模块 [`render_digest`] 输出格式(含 `## 失败模式`):有真实失败
///   记录,或含 `## 会话总结` 段 ⇒ 实质;两者皆无 ⇒ 空转;
/// - 不符合本模块格式(外部手写/旧版本):视为实质,不做判断。
#[must_use]
pub fn digest_is_substantive(content: &str) -> bool {
    let Some(pos) = content.find("## 失败模式") else {
        // 非本模块格式:保守放行。
        return true;
    };
    // 失败模式段的边界:下一个 `\n## ` 标题或文末。
    let rest = &content[pos + "## 失败模式".len()..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    let failure_body = &rest[..end];
    // render_digest 在无失败时写入的占位行。
    let no_failures = failure_body.contains("(本会话无工具失败)");
    let has_failures = !no_failures
        && failure_body
            .lines()
            .any(|l| l.trim_start().starts_with("- **"));
    let has_summary = content.contains("## 会话总结");
    has_failures || has_summary
}

/// 落盘会话 digest 到 `.claw/management_brain/digest-<session_id>.md`。
///
/// 目录不存在时自动创建。失败返回 `Err`(调用方应静默忽略,非关键路径)。
/// 写入后按 [`MAX_DIGEST_FILES`] 修剪最旧文件(修剪失败不影响写入结果)。
pub fn write_digest(
    workspace_root: &std::path::Path,
    session_id: &str,
    content: &str,
) -> Result<std::path::PathBuf, std::io::Error> {
    let dir = workspace_root.join(DIGEST_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("digest-{session_id}.md"));
    std::fs::write(&path, content)?;
    prune_digests(&dir, MAX_DIGEST_FILES);
    Ok(path)
}

/// 修剪 digest 目录,只保留最新的 `keep` 份(按 mtime 倒序)。
///
/// 失败静默(非关键路径):目录不可读 / 删除失败都不影响主流程。
fn prune_digests(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(Option<std::time::SystemTime>, std::path::PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("digest-") && n.ends_with(".md"))
        })
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).ok(), p))
        .collect();
    if files.len() <= keep {
        return;
    }
    // 最新在前;超出 keep 的部分删除。
    files.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// 读取**最近一份含实质交接内容的**会话 digest 文本。
///
/// `exclude_session_id`:排除指定会话的 digest(读侧注入时传当前会话 id,
/// 避免多 turn 会话把"自己上一轮写的 digest"误当成"上一会话遗留")。
///
/// 选取规则(按 mtime 倒序):
/// - 优先返回最新一份**实质**digest(见 [`digest_is_substantive`]);
/// - 若全部为空转(只有用量账单),返回 `None` —— 注入空转 digest 会白占
///   新会话首 turn 的槽位且误导模型「上一会话什么都没做」。
///
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
    // 跳过空转 digest:取最新一份实质内容;全为空转则不注入。
    paths
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok())
        .find(|content| digest_is_substantive(content))
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
        );
        assert!(text.contains("Session Digest"), "text: {text}");
        assert!(text.contains("**edit_file** × 2"), "text: {text}");
        assert!(text.contains("**bash** × 1"), "text: {text}");
        assert!(text.contains("old_string not found"), "text: {text}");
    }

    #[test]
    fn render_digest_omits_failures_when_empty() {
        let text = render_digest("sess-2", &[], &[]);
        assert!(text.contains("无工具失败"), "text: {text}");
    }

    #[test]
    fn render_digest_omits_usage_lines() {
        let text = render_digest("sess-3", &[], &[]);
        assert!(!text.contains("用量与成本"), "text: {text}");
        assert!(!text.contains("total_tokens="), "text: {text}");
    }

    #[test]
    fn render_digest_includes_extra_sections_in_order() {
        let text = render_digest(
            "sess-4",
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
    fn latest_digest_skips_substantive_less_recent_in_favor_of_real_handoff() {
        // 病灶:最新一份 digest 只有「无工具失败」,新会话首 turn
        // 花槽位读到的是空转内容而非交接。修复后应回退到更早的实质 digest。
        let dir = tempfile::tempdir().unwrap();
        let substantive = render_digest(
            "real-handoff",
            &[("bash".to_string(), "command failed".to_string())],
            &[],
        );
        write_digest(dir.path(), "real-handoff", &substantive).expect("write real");
        std::thread::sleep(std::time::Duration::from_millis(20));
        // 更新的一份:无失败、无总结 ⇒ 空转。
        let hollow = render_digest("hollow-newer", &[], &[]);
        write_digest(dir.path(), "hollow-newer", &hollow).expect("write hollow");

        let got = latest_digest_text(dir.path(), None).expect("should fall back to real");
        assert!(
            got.contains("real-handoff"),
            "应跳过空转 digest 回退到实质交接, got: {got}"
        );
    }

    #[test]
    fn latest_digest_none_when_all_are_hollow() {
        // 全为空转时不注入:否则会误导模型「上一会话什么都没做」。
        let dir = tempfile::tempdir().unwrap();
        let hollow = render_digest("only-hollow", &[], &[]);
        write_digest(dir.path(), "only-hollow", &hollow).expect("write");
        assert!(
            latest_digest_text(dir.path(), None).is_none(),
            "全为空转 digest 时不应注入"
        );
    }

    #[test]
    fn digest_is_substantive_distinguishes_cases() {
        // 有失败记录 ⇒ 实质。
        assert!(digest_is_substantive(&render_digest(
            "s1",
            &[("bash".to_string(), "boom".to_string())],
            &[]
        )));
        // 只有会话总结 ⇒ 实质。
        assert!(digest_is_substantive(&render_digest(
            "s2",
            &[],
            &[("会话总结(执行脑)", "- 目标: 修 X".to_string())]
        )));
        // 无失败无总结 ⇒ 空转。
        assert!(!digest_is_substantive(&render_digest("s3", &[], &[])));
        // 非本模块格式(手写)⇒ 保守放行。
        assert!(digest_is_substantive("手写交接: 明天继续"));
    }

    #[test]
    fn write_digest_prunes_to_max_files() {
        // 保留策略:digest 是交接载体不是永久审计库(审计由 history.db 承担)。
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_DIGEST_FILES + 5) {
            write_digest(dir.path(), &format!("sess-{i:03}"), "content").expect("write");
        }
        let count = std::fs::read_dir(dir.path().join(DIGEST_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("digest-") && n.ends_with(".md"))
            })
            .count();
        assert_eq!(count, MAX_DIGEST_FILES, "应修剪到保留上限, got {count}");
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
