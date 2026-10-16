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
//! 2. **黑名单**:不得自动修改 权限/安全/测试判据/升级助手自身。
//! 3. **人工闸门**:v1 默认"改码+测试通过 → 停在待装",不自动装全局。

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
#[must_use]
pub fn test_gate_commands() -> Vec<&'static str> {
    vec!["cargo test --workspace", "cargo fmt --check"]
}

/// 一份被解析的提案(仅取进化所需字段)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProposal {
    pub id: Option<String>,
    pub trigger: Option<String>,
    pub severity: Option<String>,
    pub category: Option<String>,
    /// 提案正文(去掉 front-matter)。
    pub body: String,
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
    ParsedProposal {
        id,
        trigger,
        severity,
        category,
        body: body.trim().to_string(),
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
           改动文件 + 测试证据。**不要**自行触发部署/热升级(那需人工闸门)。\n\
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
}
