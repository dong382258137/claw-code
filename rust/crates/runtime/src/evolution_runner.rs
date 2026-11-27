//! 进化运行器(P3)—— 把 inbox 提案转成"进化子智能体"的可执行任务。
//!
//! 设计见 `docs/2026-10-11-metacognitive-evolution-design.md` §3.4。
//!
//! 本模块只做**纯逻辑**:解析提案 + 构造进化任务提示 + 定义测试门命令。
//! 真正 spawn 子智能体(需要 LLM 通道)在 CLI 层(`claw evolve-source run`)完成
//! —— 通过以 `cwd=<源码根>` 启动一个无头 claw 进程实现(复用既有机器)。
//!
//! ## 硬性安全约束(§3.4)
//! 1. **测试门必过才允许提交** —— 循环依赖:用 claw 改 claw,改坏会 brick 全局。
//!    已由代码强制:`run_pipeline` 在部署前执行 `test_gate_commands()`,任一门
//!    失败即中止并计次重试(提示词里的门禁只是请求,不是防线)。
//! 2. **黑名单**:不得自动修改 权限/安全/测试判据/升级助手自身。
//!    已由代码强制:`run_pipeline` 用**真实 diff** 调 `diff_violates_forbidden`,
//!    命中即拒绝部署并隔离提案。
//! 3. **无人工确认(现状)**:不存在"停在待装"的中间态。测试门与黑名单硬校验
//!    通过后,`run_pipeline` 会**自动** `cargo build --release` 并经
//!    `install_binary` 把 release 产物回拷覆盖当前 exe(下次启动生效),
//!    **不等待任何人工确认**。设计文档 §3.4 第 3 条所述的部署确认环节从未接线,
//!    已作废 —— 读旧文档不可据此认为自动部署被人为挡住。
//!    故真实防线只有上面两条(测试门 + 黑名单)。用 claw 改 claw 仍有 brick
//!    风险(测试覆盖不到的路径、构建产物与预期不一致),不要把这条管道当作
//!    "反正有确认环节兜底"来空跑。

use std::path::Path;

/// 进化子智能体**禁止**修改的路径片段(安全边界,§3.4 硬性约束 2)。
pub const FORBIDDEN_PATH_FRAGMENTS: &[&str] = &[
    "permission",
    "approval_token",
    "trust_resolver",
    "upgrade_helper.rs",
    "session_root.rs",
    "reaudit",
];

/// 测试门:cargo 命令(必须全过才允许提交)。
///
/// 三项缺一不可,对应 BLUEPRINT 铁律 I8 的交付基线
/// (`cargo clippy --workspace --all-targets -- -D warnings` 与
/// `scripts/fmt.sh --check` 全绿方可提交):
///
/// - `cargo test --workspace`:行为不回退;
/// - `cargo clippy ... -D warnings`:静态问题不得被自动部署进生产 ——
///   此前门禁只有 test + fmt,自动进化可以带着 clippy 错误上线,而人工
///   提交却必须过 clippy,两条路径的交付标准不一致;
/// - `cargo fmt --check`:格式与仓库一致。
///
/// 注意 `run_test_gate` 按空白切分命令串,故此处不可使用 shell 引号或管道。
#[must_use]
pub fn test_gate_commands() -> Vec<&'static str> {
    vec![
        "cargo test --workspace",
        "cargo clippy --workspace --all-targets -- -D warnings",
        "cargo fmt --check",
    ]
}

/// 自动闸门:校验本次进化改动的文件列表是否触碰黑名单。
///
/// 提示词里的「不得修改」只是**请求**,被审查对象(模型)可能不遵守 ——
/// 自动部署前必须用**真实 diff** 做一次硬校验,否则「自动闸门」等于把
/// 安全边界交给被审查者自己。
///
/// 入参为 `git diff --name-only` 的输出(每行一个路径);返回首个命中的
/// 黑名单片段。零依赖纯函数,便于单测。
#[must_use]
pub fn diff_violates_forbidden(file_list: &str) -> Option<&'static str> {
    for line in file_list.lines() {
        let path = line.trim().to_ascii_lowercase();
        if path.is_empty() {
            continue;
        }
        for &frag in FORBIDDEN_PATH_FRAGMENTS {
            if path.contains(frag) {
                return Some(frag);
            }
        }
    }
    None
}

/// 一份被解析的提案(仅取进化所需字段)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProposal {
    pub id: Option<String>,
    pub trigger: Option<String>,
    pub severity: Option<String>,
    pub category: Option<String>,
    /// 验收判据（正文 `## 验收判据` 段的可执行命令）。
    ///
    /// 这是自动进化的**唯一裁决依据**：管道在部署前跑一次，不通过就不部署。
    /// 缺此字段的提案（旧模板通道产物）不应被部署 —— 没有它无法判断
    /// 「修好了没有」，只能盲改盲部署。
    pub acceptance: Option<String>,
    /// 提案正文(去掉 front-matter)。
    pub body: String,
}

/// 从正文提取 `## 验收判据` 段（到下一个 `## ` 标题或文末）。
///
/// 只认这一种写法，因为 `render_proposal` 与 prompt 都按此格式产出；
/// 宽松解析会让「正文里偶然出现的命令」被误当成判据执行。
#[must_use]
fn extract_acceptance_section(body: &str) -> Option<String> {
    let mut out = String::new();
    let mut inside = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## ") {
            if inside {
                break; // 下一段开始，判据段结束
            }
            if trimmed.contains("验收判据") {
                inside = true;
            }
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    let text = out.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// 解析提案 markdown:提取 front-matter 的简单键值 + 正文。
///
/// 只做**宽松解析**(front-matter 缺失也能用):不引入 YAML 依赖。
#[must_use]
pub fn parse_proposal(text: &str) -> ParsedProposal {
    let mut id = None;
    let mut trigger = None;
    let mut severity = None;
    let mut category = None;
    let mut body = String::new();
    let mut in_fm = false;
    let mut fm_done = false;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if i == 0 && trimmed == "---" {
            in_fm = true;
            continue;
        }
        if in_fm && trimmed == "---" {
            in_fm = false;
            fm_done = true;
            continue;
        }
        if in_fm {
            if let Some((k, v)) = trimmed.split_once(':') {
                let v = v.trim().trim_matches('"').to_string();
                match k.trim() {
                    "id" => id = Some(v),
                    "trigger" => trigger = Some(v),
                    "severity" => severity = Some(v),
                    "category" => category = Some(v),
                    _ => {}
                }
            }
            continue;
        }
        if fm_done || !in_fm {
            body.push_str(line);
            body.push('\n');
        }
    }
    let body = body.trim().to_string();
    // 验收判据从正文 `## 验收判据` 段提取（渲染格式见 evolution_inbox::render_proposal）。
    let acceptance = extract_acceptance_section(&body);
    ParsedProposal {
        id,
        trigger,
        severity,
        category,
        acceptance,
        body,
    }
}

/// 构造进化子智能体的任务提示(纯函数,便于单测)。
///
/// **严格约束**必须写进提示 —— 子智能体是自改源码的高风险操作。
#[must_use]
pub fn evolution_prompt(proposal_path: &Path, proposal: &ParsedProposal) -> String {
    let forbidden = FORBIDDEN_PATH_FRAGMENTS.join(", ");
    let gates = test_gate_commands()
        .iter()
        .map(|c| format!("  - `{c}`"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "你是 CLAW 框架的**进化子智能体**。你的任务:依据一份框架缺陷提案,修改 claw 源码。\n\
         \n\
         ## 提案\n\
         来源文件: {path}\n\
         trigger={trigger} severity={severity} category={category}\n\
         \n\
         {body}\n\
         \n\
         ## 硬性约束(违反即失败)\n\
         1. **只改源码,不碰判据**:不得修改任何测试来让它们变绿。不得放宽断言。\n\
         2. **黑名单**:不得修改涉及以下内容的文件/逻辑 —— {forbidden}。\n\
         3. **证据优先**:先复现提案中的证据(命令/测试),确认缺陷真实存在;\n\
            若无法复现,直接输出结论『无法复现,放弃』并停止,不要臆改。\n\
         4. **共因优先**:若提案描述的是框架层缺陷,优先修根因而非打补丁。\n\
         \n\
         ## 测试门(必须全部通过才算完成)\n\
         {gates}\n\
         \n\
         ## 交付\n\
         - 若修复成功且测试门全过:用 git 提交(不要 push),并在最终答复里列出\n\
           改动文件 + 测试证据。**不要**自行构建/拷贝二进制(部署由流水线自动完成)。\n\
         - 若无法修复或测试门不过:如实报告失败原因,不做部分提交。",
        path = proposal_path.display(),
        trigger = proposal.trigger.as_deref().unwrap_or("(unknown)"),
        severity = proposal.severity.as_deref().unwrap_or("(unknown)"),
        category = proposal.category.as_deref().unwrap_or("(unknown)"),
        body = proposal.body,
        forbidden = forbidden,
        gates = gates,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_acceptance_section() {
        // 与 render_proposal 产出格式一致：`## 验收判据` 段到下一个 `## ` 标题。
        let text = "---\ntrigger: agent_reported\n---\n\n## 观察\n框架慢了\n\n## 证据\n- x\n\n## 验收判据\ncargo test -p runtime --lib convergence\n\n## 备注\n不应被当作判据\n";
        let p = parse_proposal(text);
        assert_eq!(
            p.acceptance.as_deref(),
            Some("cargo test -p runtime --lib convergence"),
            "应只提取验收判据段，不含后续段落"
        );
    }

    /// 缺 `## 验收判据` 段 → `None`。
    ///
    /// 这是旧模板通道（只写 iterations/elapsed）产物的特征，管道据此拒绝部署。
    #[test]
    fn parse_without_acceptance_section_yields_none() {
        let text = "---\ntrigger: soft_threshold\n---\n\n## 观察\nturn 收尾命中廉价信号\n\n## 证据\n- iterations=64\n";
        let p = parse_proposal(text);
        assert!(
            p.acceptance.is_none(),
            "无验收判据段时必须为 None，否则管道会误判可验证"
        );
    }

    /// 空段（只有标题没有内容）→ `None`，不能返回空串让管道当成"有判据"。
    #[test]
    fn parse_empty_acceptance_section_yields_none() {
        let text = "## 观察\nx\n\n## 验收判据\n\n## 其它\ny\n";
        let p = parse_proposal(text);
        assert!(p.acceptance.is_none(), "空判据段应视为缺失");
    }

    #[test]
    fn parse_extracts_front_matter_and_body() {
        let text = "---\nid: s-1\ntrigger: task_failed\nseverity: high\ncategory: bug\nevidence:\n  - \"x\"\n---\n\n## 观察\n框架慢了\n";
        let p = parse_proposal(text);
        assert_eq!(p.id.as_deref(), Some("s-1"));
        assert_eq!(p.trigger.as_deref(), Some("task_failed"));
        assert_eq!(p.severity.as_deref(), Some("high"));
        assert_eq!(p.category.as_deref(), Some("bug"));
        assert!(p.body.contains("框架慢了"));
        assert!(!p.body.contains("---"), "body 不应含 front-matter 分隔符");
    }

    #[test]
    fn parse_tolerates_missing_front_matter() {
        let p = parse_proposal("just a body\nline 2");
        assert!(p.id.is_none());
        assert_eq!(p.trigger, None);
        assert!(p.body.contains("just a body"));
    }

    #[test]
    fn prompt_contains_hard_constraints() {
        let p = parse_proposal("---\ntrigger: punchback\n---\nbody");
        let prompt = evolution_prompt(Path::new("/x/inbox/1.md"), &p);
        assert!(prompt.contains("不得修改任何测试"), "应含测试约束");
        assert!(prompt.contains("黑名单"), "应含黑名单约束");
        assert!(prompt.contains("cargo test --workspace"), "应含测试门");
        assert!(prompt.contains("upgrade_helper.rs"), "黑名单应含升级助手");
        assert!(
            prompt.contains("不要") && prompt.contains("部署"),
            "应含人工闸门说明"
        );
    }

    /// 自动闸门的安全核心:黑名单必须由**真实 diff** 硬校验, 不能只靠提示词。
    ///
    /// 被审查对象是模型本身 —— 提示词里的"不得修改"只是请求。若部署前不核对
    /// 实际改动文件, 自动闸门等于把安全边界交给被审查者自己。
    #[test]
    fn diff_violates_forbidden_detects_blacklisted_paths() {
        // 正常改动放行
        assert_eq!(
            diff_violates_forbidden("rust/crates/runtime/src/convergence.rs\n"),
            None
        );
        // 空 diff 放行
        assert_eq!(diff_violates_forbidden(""), None);
        assert_eq!(diff_violates_forbidden("\n \n"), None);
        // 黑名单命中(大小写不敏感)
        assert_eq!(
            diff_violates_forbidden("rust/crates/claw-server/src/upgrade_helper.rs"),
            Some("upgrade_helper.rs")
        );
        assert_eq!(
            diff_violates_forbidden("src/Foo/PERMISSION_gate.rs"),
            Some("permission")
        );
        assert_eq!(
            diff_violates_forbidden("a.rs\nb/trust_resolver.rs\nc.rs"),
            Some("trust_resolver")
        );
    }
}
