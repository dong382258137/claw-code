//! 双大脑架构 · 管理脑落盘区(设计文档 2026-09-03-dual-brain-architecture.md §5.3)。
//!
//! 架构守护面:会话结束(HookEvent::Stop)后**确定性聚合**本会话的失败模式
//! 与用量成本(零 LLM),落盘 `.claw/management_brain/digest-<session_id>.md`。
//! 新会话首 turn 读取最近 digest 注入,让执行脑感知上一会话遗留(失败教训 /
//! 成本参考),实现"管理脑消化轨迹 → 知识资产"的跨会话闭环。
//!
//! 设计原则:
//! - **零 LLM**:纯确定性聚合(失败签名计数 + 用量成本行),不消耗 token。
//! - **非关键路径**:写入/读取失败静默降级(返回 None / 忽略),不阻断会话。
//! - **审计保留**:digest 按 session_id 命名,天然累积为会话审计轨迹,
//!   不做自动清理(体积小;如需清理由外部维护策略决定)。
//! - **不进 system_prompt 静态段**:digest 经 dynamic_sections 尾部注入,
//!   不破坏前缀缓存(缓存保护)。

/// digest 落盘目录(相对 workspace_root)。
pub const DIGEST_DIR: &str = ".claw/management_brain";

/// 单条工具失败聚合记录:`(tool_name, 首行签名摘要)`。
pub type ToolFailure = (String, String);

/// 会话结束聚合文本(纯函数,便于单测)。
///
/// 输入本会话的失败记录与用量成本行,输出一份紧凑的 Markdown digest:
/// 失败模式计数 + 用量/成本参考。失败为空时省略该段(不产生噪音)。
pub fn render_digest(
    session_id: &str,
    failures: &[ToolFailure],
    usage_summary: &[String],
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

/// 读取**最近**一份会话 digest 的文本(按文件名倒序 = 写入时间倒序)。
///
/// 目录不存在 / 无 digest 文件 / 读取失败均返回 `None`(静默降级)。
pub fn latest_digest_text(workspace_root: &std::path::Path) -> Option<String> {
    let dir = workspace_root.join(DIGEST_DIR);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("digest-") && n.ends_with(".md"))
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
    fn render_digest_includes_usage_lines() {
        let text = render_digest(
            "sess-3",
            &[],
            &["session-total: total_tokens=100 input=80 output=20".to_string()],
        );
        assert!(text.contains("用量与成本"), "text: {text}");
        assert!(text.contains("total_tokens=100"), "text: {text}");
    }

    #[test]
    fn write_and_read_latest_digest_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        write_digest(dir.path(), "old-sess", "digest old").expect("write old");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_digest(dir.path(), "new-sess", "digest new").expect("write new");

        let latest = latest_digest_text(dir.path()).expect("latest exists");
        assert!(latest.contains("digest new"), "latest: {latest}");
        assert!(digest_dir_exists(dir.path()));
    }

    #[test]
    fn latest_digest_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(latest_digest_text(dir.path()).is_none());
    }
}
