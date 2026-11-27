//! 进化提案归档(inbox)—— 热升级元认知闭环 P1。
//!
//! 把"框架需改进"的观察写成**结构化提案**落盘到
//! `<源码根>/.claw/evolution/inbox/*.md`,作为唤醒进化子智能体(P2/P3)的信号。
//!
//! 设计见 `docs/2026-10-11-metacognitive-evolution-design.md` §4。
//!
//! 落点解析(高→低):
//! 1. 源码根注册表(`source_root::resolve_source_root`)→ `<源码根>/.claw/evolution/inbox`
//! 2. 回退:当前工作区 `.claw/evolution/inbox`(源码根未登记时不丢提案)
//!
//! 关键约束(与 `docs/2026-10-09-metrics-anchoring-design.md` §5 诚实性红线一致):
//! 提案的 `evidence` 必须是**可执行/可复现判据**,不是"感觉"。

use std::path::{Path, PathBuf};

/// inbox 相对源码根的路径片段。
pub const INBOX_REL: &str = ".claw/evolution/inbox";
/// 已处理提案的归档子目录名。
pub const PROCESSED_DIR: &str = "processed";

/// 一条进化提案(与 §4 的 front-matter 契约对应)。
#[derive(Debug, Clone)]
pub struct FrameworkGap {
    /// 触发来源:`punchback` / `loop_detected` / `tool_fail_streak` /
    /// `soft_threshold` / `task_failed` / `agent_reported`。
    pub trigger: String,
    /// 严重度:`low` / `medium` / `high`。
    pub severity: String,
    /// 分类:`bug` / `perf` / `feature` / `dx`。
    pub category: String,
    /// 观察:框架在哪里表现不佳(一段话)。
    pub observation: String,
    /// 证据:可执行/可复现判据(文件:行号 / 命令输出 / 指标数值)。
    pub evidence: Vec<String>,
    /// 建议:可选,初步修改方向。
    pub suggestion: Option<String>,
    /// 验收判据:改完怎么算成功(可执行)。
    pub acceptance: Option<String>,
    /// 触发时的工作目录(任意)。
    pub source_cwd: String,
    /// 会话 id。
    pub source_session: String,
}

/// 把提案渲染为 markdown(含 front-matter)。纯函数,便于单测。
#[must_use]
pub fn render_proposal(gap: &FrameworkGap, id: &str, created_at_ms: i64) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("id: {id}\n"));
    s.push_str(&format!("created_at_ms: {created_at_ms}\n"));
    s.push_str(&format!("source_cwd: {}\n", gap.source_cwd));
    s.push_str(&format!("source_session: {}\n", gap.source_session));
    s.push_str(&format!("trigger: {}\n", gap.trigger));
    s.push_str(&format!("severity: {}\n", gap.severity));
    s.push_str(&format!("category: {}\n", gap.category));
    if gap.evidence.is_empty() {
        s.push_str("evidence: []\n");
    } else {
        s.push_str("evidence:\n");
        for e in &gap.evidence {
            // 单行化,避免破坏 YAML 列表结构。
            s.push_str(&format!("  - \"{}\"\n", e.replace('\n', " ")));
        }
    }
    s.push_str("---\n\n");
    s.push_str("## 观察\n");
    s.push_str(gap.observation.trim());
    s.push_str("\n\n## 证据\n");
    if gap.evidence.is_empty() {
        s.push_str("(未提供 — 提案须补可执行判据)\n");
    } else {
        for e in &gap.evidence {
            s.push_str(&format!("- {e}\n"));
        }
    }
    if let Some(sg) = gap.suggestion.as_ref().filter(|v| !v.trim().is_empty()) {
        s.push_str("\n## 建议\n");
        s.push_str(sg.trim());
        s.push('\n');
    }
    if let Some(ac) = gap.acceptance.as_ref().filter(|v| !v.trim().is_empty()) {
        s.push_str("\n## 验收判据\n");
        s.push_str(ac.trim());
        s.push('\n');
    }
    s
}

/// 解析 inbox 目录(源码根优先,回退工作区)。
#[must_use]
pub fn resolve_inbox_dir(workspace_cwd: &Path) -> PathBuf {
    if let Some(root) = crate::source_root::resolve_source_root() {
        return root.join(INBOX_REL);
    }
    workspace_cwd.join(INBOX_REL)
}

/// 证据包目录（相对源码根）：自动通道只在这里留原始信号，不写提案。
pub const EVIDENCE_DIR: &str = ".claw/evolution/evidence";

/// 把一次自动信号的原始证据落盘（JSON），供 LLM 诊断时读取。
///
/// **为什么不直接写提案**：自动通道只知道「第 N 轮、耗时 M 秒」，无法回答
/// 「在做什么任务、用了哪个工具、具体报什么错」。把它写成提案，只会让进化
/// 子智能体拿数字猜缺陷 —— 实测 inbox 21 条里 20 条正是如此（正文全部是同一
/// 句模板，只有 iterations/elapsed 两个数字），提案质量为零。
///
/// 故此处只保存**可核查的原始事实**，判断与措辞交给 LLM 通过
/// `report_framework_gap` 撰写。
pub fn write_evidence_pack(
    workspace_cwd: &Path,
    trigger: &str,
    evidence: &[String],
    session_id: &str,
    now_ms: i64,
) -> Result<PathBuf, String> {
    let dir = resolve_inbox_dir(workspace_cwd)
        .parent()
        .map(|parent| parent.join("evidence"))
        .unwrap_or_else(|| workspace_cwd.join(EVIDENCE_DIR));
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建证据目录失败: {e}"))?;
    let path = dir.join(format!("{now_ms}-{}.json", slugify(trigger)));
    let body = serde_json::json!({
        "created_at_ms": now_ms,
        "trigger": trigger,
        "session_id": session_id,
        "evidence": evidence,
        "note": "自动信号原始证据；提案须由 LLM 调用 report_framework_gap 撰写",
    });
    let text = serde_json::to_string_pretty(&body).map_err(|e| format!("序列化证据失败: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入证据失败: {e}"))?;
    Ok(path)
}

/// 归档结果:新立案 vs 与**待处理**提案重复。
///
/// 调用方必须据此决定是否拉起进化管道 —— 重复立项再各拉起一次
/// `evolve-source auto`(含 cargo build)是纯浪费,且会把 inbox 灌成
/// 同一发现的 N 份副本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// 新提案已落盘(路径为新文件)。
    Created(PathBuf),
    /// 同一 trigger 已有待处理提案 —— 未写新文件,路径为**已存在**的那份。
    Duplicate(PathBuf),
}

impl ArchiveOutcome {
    /// 相关提案路径(新建的 / 已存在的)。
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Created(p) | Self::Duplicate(p) => p,
        }
    }

    /// 是否为本次新立案。
    #[must_use]
    pub fn is_created(&self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// 归档一条提案,返回 `ArchiveOutcome`。
///
/// 文件名:`<created_at_ms>-<slug>.md`。slug 由 trigger 派生(稳定、可读)。
/// 目录不存在则创建。写入失败返回 `Err`。
///
/// **去重(实测缺陷)**:inbox 曾无任何内容级去重,实测 9 条 `soft-threshold`
/// 提案正文去易变字段(id/created_at/source_session/iterations)后 md5 完全相同
/// (86f5b36d...),来自 6 个不同 session —— 即同一发现被重复立项 9 次,并
/// 各拉起一次完整进化管道。会话内限流(`GAP_REPORT_MIN_INTERVAL_MS`)是
/// 进程内内存态,跨 session 完全失效,挡不住这条路径。
///
/// 判据:inbox 待处理区已有同 `slug`(由 trigger 派生)的提案 → 不再写新文件,
/// 返回 [`ArchiveOutcome::Duplicate`]。只对**待处理**提案去重:一旦被
/// `mark_processed` 移走,同一 trigger 的后续发现可重新立案(否则会永久失声)。
pub fn archive_gap(
    workspace_cwd: &Path,
    gap: &FrameworkGap,
    created_at_ms: i64,
) -> Result<ArchiveOutcome, String> {
    let dir = resolve_inbox_dir(workspace_cwd);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 inbox {} 失败: {e}", dir.display()))?;
    let slug = slugify(&gap.trigger);
    if let Some(existing) = find_pending_with_slug(&dir, &slug) {
        return Ok(ArchiveOutcome::Duplicate(existing));
    }
    let id = format!("{}-{}", gap.source_session, created_at_ms);
    let path = dir.join(format!("{created_at_ms}-{slug}.md"));
    let body = render_proposal(gap, &id, created_at_ms);
    std::fs::write(&path, body).map_err(|e| format!("写 {} 失败: {e}", path.display()))?;
    Ok(ArchiveOutcome::Created(path))
}

/// 在待处理区查找 `*-<slug>.md`。文件名第二段即 trigger slug,无需解析正文。
fn find_pending_with_slug(inbox_dir: &Path, slug: &str) -> Option<PathBuf> {
    let suffix = format!("-{slug}.md");
    list_pending(inbox_dir).into_iter().find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&suffix))
    })
}

/// 列出 inbox 中待处理提案(未处理的 `.md`,排除 processed/ 子目录)。
#[must_use]
pub fn list_pending(inbox_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(inbox_dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
        .collect();
    out.sort();
    out
}

/// 标记提案已处理(移入 `processed/`)。幂等:源文件不存在则返回 `Ok(None)`。
pub fn mark_processed(inbox_dir: &Path, proposal: &Path) -> Result<Option<PathBuf>, String> {
    if !proposal.is_file() {
        return Ok(None);
    }
    let dst_dir = inbox_dir.join(PROCESSED_DIR);
    std::fs::create_dir_all(&dst_dir)
        .map_err(|e| format!("创建 {} 失败: {e}", dst_dir.display()))?;
    let name = proposal
        .file_name()
        .ok_or_else(|| format!("无效提案路径: {}", proposal.display()))?;
    let dst = dst_dir.join(name);
    std::fs::rename(proposal, &dst)
        .map_err(|e| format!("移动 {} 失败: {e}", proposal.display()))?;
    Ok(Some(dst))
}

/// 把 trigger 规范化为文件名安全 slug。
///
/// 规则:字母数字→小写保留;其余字符(含 `/`、空格、`_`)→ `-`;
/// 连续 `-` 折叠为一个;首尾 `-` 去除。全空则回退 `"gap"`。
#[must_use]
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "gap".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FrameworkGap {
        FrameworkGap {
            trigger: "punchback".into(),
            severity: "medium".into(),
            category: "bug".into(),
            observation: "用户打回了上一轮答复,疑似框架层缺能力".into(),
            evidence: vec!["cargo test -p runtime 失败: 2 failed".into()],
            suggestion: Some("检查 X".into()),
            acceptance: Some("cargo test -p runtime 全绿".into()),
            source_cwd: "D:\\chanlunV2".into(),
            source_session: "session-abc".into(),
        }
    }

    #[test]
    fn render_has_front_matter_and_sections() {
        let md = render_proposal(&sample(), "id-1", 123);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("trigger: punchback"));
        assert!(md.contains("source_cwd: D:\\chanlunV2"));
        assert!(md.contains("## 观察"));
        assert!(md.contains("## 证据"));
        assert!(md.contains("## 验收判据"));
    }

    #[test]
    fn render_empty_evidence_is_marked() {
        let mut g = sample();
        g.evidence.clear();
        let md = render_proposal(&g, "id-2", 1);
        assert!(md.contains("evidence: []"));
        assert!(md.contains("须补可执行判据"));
    }

    #[test]
    fn slugify_is_filename_safe_and_stable() {
        assert_eq!(slugify("tool_fail_streak"), "tool-fail-streak");
        assert_eq!(slugify("a b/c"), "a-b-c");
        assert_eq!(slugify("///"), "gap");
        assert_eq!(slugify("high"), "high");
    }

    /// 回归(实测缺陷):同一 trigger 的提案会跨 session 重复立案 —— 实测 inbox
    /// 9 条 `soft-threshold` 正文去易变字段后 md5 全同(86f5b36d...),来自 6 个
    /// session;会话内限流是进程内内存态,跨 session 完全失效。
    ///
    /// `archive_gap` 的去重判据 = 待处理区是否已有 `*-<slug>.md`。此处直接验证
    /// 该判据(纯目录操作,不动 `CLAW_SOURCE_ROOT`,避免与其他用例的 env 竞争)。
    #[test]
    fn pending_with_same_trigger_is_deduped() {
        let tmp = std::env::temp_dir().join(format!("claw-inbox-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("mkdir");

        // 空目录 → 无重复。
        assert_eq!(find_pending_with_slug(&tmp, "soft-threshold"), None);

        std::fs::write(tmp.join("1000-soft-threshold.md"), "body").expect("write");

        // 同 trigger(仅时间戳不同)→ 命中已有那份,不再新增。
        let hit = find_pending_with_slug(&tmp, "soft-threshold").expect("应命中已有提案");
        assert!(hit.ends_with("1000-soft-threshold.md"), "hit={hit:?}");

        // 不同 trigger → 不误判为重复。
        assert_eq!(find_pending_with_slug(&tmp, "punchback"), None);

        // 已处理(processed/ 子目录)不算待处理:否则该 trigger 会永久失声。
        std::fs::create_dir_all(tmp.join(PROCESSED_DIR)).expect("mkdir processed");
        std::fs::rename(
            tmp.join("1000-soft-threshold.md"),
            tmp.join(PROCESSED_DIR).join("1000-soft-threshold.md"),
        )
        .expect("rename");
        assert_eq!(find_pending_with_slug(&tmp, "soft-threshold"), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn archive_list_mark_processed_roundtrip() {
        // env 是进程级全局状态：与 source_root / evidence 用例共享同一把锁，
        // 否则并行跑时 A 的恢复会覆盖 B 刚设的值，表现为随机失败。
        let _env_guard = crate::source_root::SOURCE_ROOT_ENV_LOCK
            .lock()
            .expect("source-root env lock");
        let tmp = std::env::temp_dir().join(format!("claw-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // 必须真实创建目录：resolve_source_root 只在 env 指向已存在目录时才采用，
        // 否则会回退到本机注册表（开发机可能已自动登记源码树）。
        std::fs::create_dir_all(&tmp).expect("mkdir");
        // 用 CLAW_SOURCE_ROOT 指向临时目录,使 resolve_inbox_dir 落到 tmp。
        let saved = std::env::var_os(crate::source_root::SOURCE_ROOT_ENV);
        std::env::set_var(crate::source_root::SOURCE_ROOT_ENV, &tmp);

        let outcome = archive_gap(&tmp, &sample(), 424_242).expect("archive");
        assert!(outcome.is_created(), "首次归档应为 Created: {outcome:?}");
        let path = outcome.path().to_path_buf();
        let inbox = resolve_inbox_dir(&tmp);
        assert!(path.starts_with(&inbox), "path={path:?} inbox={inbox:?}");
        let pending = list_pending(&inbox);
        assert_eq!(pending.len(), 1, "pending={pending:?}");

        let moved = mark_processed(&inbox, &pending[0])
            .expect("mark")
            .expect("some");
        assert!(
            moved.ends_with("processed/424242-punchback.md")
                || moved.ends_with("424242-punchback.md")
        );
        assert!(list_pending(&inbox).is_empty(), "处理后 pending 应为空");

        match saved {
            Some(v) => std::env::set_var(crate::source_root::SOURCE_ROOT_ENV, v),
            None => std::env::remove_var(crate::source_root::SOURCE_ROOT_ENV),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 证据包只落 JSON 原始事实，且**不得**写进 inbox。
    ///
    /// 这是本轮修复的核心契约：自动通道降级为纯采集后，若它仍往 inbox 写
    /// 提案，那么「提案由 LLM 撰写」的设计就不成立 —— 队列会被模板垃圾重新
    /// 灌满（实测 21 条里 20 条正是这样来的）。
    #[test]
    fn evidence_pack_lands_in_evidence_dir_not_inbox() {
        // 与 archive/source_root 用例共享同一把 env 锁（进程级全局状态）。
        let _env_guard = crate::source_root::SOURCE_ROOT_ENV_LOCK
            .lock()
            .expect("source-root env lock");
        let tmp = std::env::temp_dir().join(format!("claw-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("mkdir");

        let saved = std::env::var_os(crate::source_root::SOURCE_ROOT_ENV);
        std::env::set_var(crate::source_root::SOURCE_ROOT_ENV, &tmp);

        let evidence = vec!["turn: iterations=64 elapsed=880s trigger=soft_threshold".to_string()];
        let path = write_evidence_pack(&tmp, "soft_threshold", &evidence, "sess-1", 1000)
            .expect("写入证据包");

        assert!(path.is_file(), "证据包应落盘: {path:?}");
        assert!(
            path.to_string_lossy().contains("evidence"),
            "必须落在 evidence/ 目录: {path:?}"
        );
        // 关键：inbox 必须仍然为空 —— 自动通道不再产生提案。
        let inbox = resolve_inbox_dir(&tmp);
        assert!(
            list_pending(&inbox).is_empty(),
            "自动通道不得再往 inbox 写提案"
        );

        // 内容是结构化 JSON，且带 trigger/evidence 两个可核查字段。
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("\"trigger\""), "应含 trigger");
        assert!(text.contains("soft_threshold"), "应含 trigger 值");
        assert!(text.contains("\"evidence\""), "应含 evidence");

        match saved {
            Some(v) => std::env::set_var(crate::source_root::SOURCE_ROOT_ENV, v),
            None => std::env::remove_var(crate::source_root::SOURCE_ROOT_ENV),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
