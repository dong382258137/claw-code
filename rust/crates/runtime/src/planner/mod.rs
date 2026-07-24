//! Plan/Execute/Review 三段循环 — Step 2.1 主入口。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 2.1
//!
//! 架构:
//! - [`PlanArtifact`]:plan 数据模型(steps + status + replan_count)。
//! - [`PreCompletionChecklistMiddleware`]:Review 阶段中间件,决定 AllPassed/Replan/Failed。
//! - [`Planner`]:复杂任务检测 + PlanArtifact 生成入口。
//! - [`persist_plan_artifact`]:写入 `<workspace>/.claw/plans/<timestamp>.json`。
//!
//! **缓存保护**(详见 §5.2):
//! PlanArtifact 必须末尾追加到 prompt 的"变动区",不污染"绝对稳定区"
//! (system_prompt + tools_schema)与"半稳定区"(memory/goal/git_context)。
//! 预期命中率从 95% 降至 88-92%,通过 `prompt_cache.rs` 已有监控发现。
//!
//! **Feature flag**:
//! 默认不启用,需通过 CLI `--enable-plan-mode` 开启,或 settings.json
//! 配置 `"planMode": true` 启用。

pub mod artifact;
pub mod reviewer;

pub use artifact::{PlanArtifact, PlanPhase, PlanStep, StepRisk, StepStatus};
pub use reviewer::{
    render_remediation_prompt, FailedVerification, PreCompletionChecklistMiddleware, ReviewResult,
    DEFAULT_MAX_REPLANS,
};

use std::fs;
use std::path::{Path, PathBuf};

/// 触发 plan 子调用的用户输入字符数阈值(粗略估算多文件预期)。
pub const COMPLEX_TASK_INPUT_CHARS_THRESHOLD: usize = 200;

/// 触发 plan 的关键词(用户输入包含任一即视为复杂任务)。
pub const COMPLEX_TASK_KEYWORDS: &[&str] = &[
    "multiple files",
    "refactor",
    "across modules",
    "step by step",
    "plan and execute",
    "多文件",
    "分步",
    "重构",
];

/// P2:高风险操作关键词,匹配任一则 step 标记为 High risk。
///
/// 覆盖 6 类高风险操作:
/// - 删除/移除:delete, drop, remove, truncate
/// - 强制操作:force, --force, -f
/// - 生产/部署:production, deploy, release, publish
/// - 安全/凭证:security, auth, password, token, secret, credential
/// - 不可逆:migrate, irreversible
/// - 权限:permission, privilege, chmod, chown
const HIGH_RISK_KEYWORDS: &[&str] = &[
    "delete",
    "drop",
    "remove",
    "truncate",
    "force",
    "--force",
    "production",
    "deploy",
    "release",
    "publish",
    "security",
    "auth",
    "password",
    "token",
    "secret",
    "credential",
    "migrate",
    "irreversible",
    "permission",
    "privilege",
    "chmod",
    "chown",
    "删除",
    "移除",
    "强制",
    "生产环境",
    "部署",
    "发布",
    "安全",
    "密码",
    "令牌",
    "凭证",
    "迁移",
    "权限",
];

/// 复杂任务检测结果 — 用于决定是否触发 planner 子调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComplexityAssessment {
    /// 简单任务,无需 plan,主 agent 直接执行。
    Simple,
    /// 复杂任务,触发 planner 生成 PlanArtifact。
    Complex {
        /// 触发原因(用于诊断日志)。
        reason: String,
    },
}

/// 评估用户输入是否为复杂任务。
///
/// 判定规则(满足任一即视为复杂):
/// 1. 用户输入字符数 > `COMPLEX_TASK_INPUT_CHARS_THRESHOLD`(200)。
/// 2. 包含 `COMPLEX_TASK_KEYWORDS` 中的任一关键词(大小写不敏感)。
#[must_use]
pub fn assess_complexity(user_input: &str) -> ComplexityAssessment {
    let trimmed = user_input.trim();
    if trimmed.chars().count() > COMPLEX_TASK_INPUT_CHARS_THRESHOLD {
        return ComplexityAssessment::Complex {
            reason: format!(
                "input length {} > threshold {}",
                trimmed.chars().count(),
                COMPLEX_TASK_INPUT_CHARS_THRESHOLD
            ),
        };
    }
    let lowered = trimmed.to_ascii_lowercase();
    for keyword in COMPLEX_TASK_KEYWORDS {
        if lowered.contains(keyword) {
            return ComplexityAssessment::Complex {
                reason: format!("matched keyword: {keyword}"),
            };
        }
    }
    ComplexityAssessment::Simple
}

/// 持久化 PlanArtifact 到 `<workspace>/.claw/plans/<id>.json`。
///
/// 文件路径用 plan id 命名(包含时间戳),同一 plan 多次 replan 不会产生多个文件,
/// 而是覆写同一文件(因为 replan_count 在 artifact 内部,文件本身只反映最新状态)。
///
/// 失败时返回 `Err`,调用方决定是否继续(通常记日志不阻断主流程)。
pub fn persist_plan_artifact(
    artifact: &PlanArtifact,
    workspace_root: &Path,
) -> Result<PathBuf, std::io::Error> {
    let plans_dir = workspace_root.join(".claw").join("plans");
    fs::create_dir_all(&plans_dir)?;
    let file_path = plans_dir.join(format!("{}.json", artifact.id));
    let json = serde_json::to_string_pretty(artifact)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(&file_path, json)?;
    Ok(file_path)
}

/// 从文件加载 PlanArtifact(用于跨会话恢复 plan 状态)。
pub fn load_plan_artifact(path: &Path) -> Result<PlanArtifact, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let artifact: PlanArtifact = serde_json::from_str(&contents)?;
    Ok(artifact)
}

/// P2:评估单个 step 的风险级别。
///
/// 检查 step 描述是否包含高风险关键词(删除/强制/生产/安全/不可逆/权限)。
/// 命中任一返回 `High`,否则 `Low`。用于驱动 Pre-commitment protocol 注入。
#[must_use]
pub fn assess_step_risk(description: &str) -> StepRisk {
    let lowered = description.to_ascii_lowercase();
    if HIGH_RISK_KEYWORDS.iter().any(|kw| lowered.contains(kw)) {
        StepRisk::High
    } else {
        StepRisk::Low
    }
}

/// Heuristic task decomposition — converts a complex user request into
/// concrete `PlanStep`s without calling an LLM sub-agent.
///
/// Returns at least 1 step even when no patterns match.
#[must_use]
pub fn decompose_task(user_input: &str) -> Vec<PlanStep> {
    let mut steps: Vec<PlanStep> = Vec::new();
    let mut step_id = 0u32;

    // 1. Check for multi-file operations — one step per detected file path.
    for file_path in extract_file_paths(user_input) {
        step_id += 1;
        steps.push(PlanStep::new(
            format!("step_{step_id}"),
            format!("Modify `{file_path}`"),
            format!("Verify {file_path} compiles and passes tests"),
        ));
    }

    // 2. Check for sequential markers.
    let sequential_markers = ["first", "then", "after that", "next", "finally"];
    let input_lower = user_input.to_lowercase();
    let has_markers = sequential_markers.iter().any(|m| input_lower.contains(m));

    // 3. Sentence-level decomposition for long or sequential input.
    if steps.is_empty() && (has_markers || user_input.len() > 300) {
        for sentence in split_into_sentences(user_input) {
            let trimmed = sentence.trim();
            if trimmed.is_empty() || trimmed.len() < 10 {
                continue;
            }
            step_id += 1;
            let short = if trimmed.len() > 80 {
                format!("{}…", &trimmed[..80])
            } else {
                trimmed.to_string()
            };
            steps.push(PlanStep::new(
                format!("step_{step_id}"),
                short,
                "Verify the step completed correctly".to_string(),
            ));
        }
        steps.truncate(10);
    }

    // 4. Fallback: at minimum one step.
    if steps.is_empty() {
        step_id += 1;
        let summary = if user_input.len() > 120 {
            format!("{}…", &user_input[..120])
        } else {
            user_input.to_string()
        };
        steps.push(PlanStep::new(
            format!("step_{step_id}"),
            format!("Execute: {summary}"),
            "Task completed and verified".to_string(),
        ));
    }

    // P2:对每个 step 评估风险级别,High risk step 在 render 时注入 Pre-commitment。
    // 检查 step description + 原始 user_input(兜底:泛化 description 时从任务上下文捕获风险)。
    let input_lower = user_input.to_ascii_lowercase();
    let input_is_high_risk = HIGH_RISK_KEYWORDS.iter().any(|kw| input_lower.contains(kw));
    let is_fallback_single_step = steps.len() == 1;
    for step in &mut steps {
        step.risk_level = assess_step_risk(&step.description);
        // 若 step description 未命中但整体任务命中,且 step 是兜底单步,继承 high-risk。
        if step.risk_level == StepRisk::Low && input_is_high_risk && is_fallback_single_step {
            step.risk_level = StepRisk::High;
        }
    }

    steps
}

/// Update an existing [`PlanArtifact`] with new or modified steps (G8.9).
///
/// Parses a structured or natural-language update description and applies
/// it to the plan. Supports:
/// - Adding new steps: `"add: Verify auth module compiles"`
/// - Marking steps done: `"done: step_1"`
/// - Marking steps failed: `"fail: step_2, reason: compilation error"`
/// - Replanning: resets Failed steps to Pending
///
/// Returns the number of changes applied.
pub fn update_plan(artifact: &mut PlanArtifact, update: &str) -> usize {
    let mut changes = 0usize;
    let trimmed = update.trim();

    // ── Pattern: "add: <description>" ──
    if let Some(desc) = trimmed
        .strip_prefix("add:")
        .or_else(|| trimmed.strip_prefix("Add:"))
        .or_else(|| trimmed.strip_prefix("ADD:"))
    {
        let desc = desc.trim();
        if !desc.is_empty() {
            let next_id = format!("step_{}", artifact.steps.len() + 1);
            artifact.steps.push(PlanStep::new(
                next_id,
                desc,
                "Verify step completed correctly".to_string(),
            ));
            changes += 1;
            return changes;
        }
    }

    // ── Pattern: "done: <step_id>" ──
    if let Some(step_id) = trimmed
        .strip_prefix("done:")
        .or_else(|| trimmed.strip_prefix("Done:"))
        .or_else(|| trimmed.strip_prefix("DONE:"))
    {
        let step_id = step_id.trim();
        if let Some(step) = artifact.steps.iter_mut().find(|s| s.id == step_id) {
            step.mark_succeeded();
            changes += 1;
        }
        return changes;
    }

    // ── Pattern: "fail: <step_id>[, reason: <text>]" ──
    if let Some(rest) = trimmed
        .strip_prefix("fail:")
        .or_else(|| trimmed.strip_prefix("Fail:"))
        .or_else(|| trimmed.strip_prefix("FAIL:"))
    {
        let rest = rest.trim();
        let step_id = rest.split(',').next().map(str::trim).unwrap_or(rest);
        if let Some(step) = artifact.steps.iter_mut().find(|s| s.id == step_id) {
            step.mark_failed();
            changes += 1;
        }
        return changes;
    }

    // ── Pattern: "replan" ──
    if trimmed.eq_ignore_ascii_case("replan") {
        if artifact.trigger_replan(3).is_some() {
            changes += 1;
        }
        return changes;
    }

    // ── Fallback: sentence-level decomposition appended as new steps ──
    for sentence in split_into_sentences(trimmed) {
        let s = sentence.trim();
        if s.is_empty() || s.len() < 10 {
            continue;
        }
        let next_id = format!("step_{}", artifact.steps.len() + 1);
        let short = if s.len() > 80 {
            format!("{}…", &s[..80])
        } else {
            s.to_string()
        };
        artifact.steps.push(PlanStep::new(
            next_id,
            short,
            "Verify step completed correctly".to_string(),
        ));
        changes += 1;
    }

    changes
}

fn extract_file_paths(text: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for word in text.split_whitespace() {
        let clean =
            word.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == ',' || c == '.');
        if is_likely_path(clean) && seen.insert(clean.to_string()) {
            paths.push(clean.to_string());
        }
    }
    paths
}

fn is_likely_path(s: &str) -> bool {
    let has_sep = s.contains('/') || s.contains('\\');
    let has_ext = s.ends_with(".rs")
        || s.ends_with(".toml")
        || s.ends_with(".md")
        || s.ends_with(".json")
        || s.ends_with(".ts")
        || s.ends_with(".py")
        || s.ends_with(".js");
    has_sep && has_ext && s.len() >= 5 && s.len() <= 120
}

fn split_into_sentences(text: &str) -> Vec<String> {
    text.split_inclusive(&['.', '!', '?', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assess_complexity_returns_simple_for_short_input() {
        let result = assess_complexity("hello world");
        assert_eq!(result, ComplexityAssessment::Simple);
    }

    #[test]
    fn assess_complexity_returns_complex_for_long_input() {
        let long_input = "a".repeat(COMPLEX_TASK_INPUT_CHARS_THRESHOLD + 1);
        let result = assess_complexity(&long_input);
        assert!(matches!(result, ComplexityAssessment::Complex { .. }));
    }

    #[test]
    fn assess_complexity_returns_complex_for_keyword_match() {
        let result = assess_complexity("refactor the auth module");
        match result {
            ComplexityAssessment::Complex { reason } => {
                assert!(reason.contains("refactor"));
            }
            other => panic!("expected Complex, got {other:?}"),
        }
    }

    #[test]
    fn assess_complexity_returns_complex_for_chinese_keyword() {
        let result = assess_complexity("多文件重构");
        match result {
            ComplexityAssessment::Complex { reason } => {
                assert!(reason.contains("多文件") || reason.contains("重构"));
            }
            other => panic!("expected Complex, got {other:?}"),
        }
    }

    #[test]
    fn assess_complexity_is_case_insensitive() {
        let result = assess_complexity("REFACTOR everything");
        assert!(matches!(result, ComplexityAssessment::Complex { .. }));
    }

    #[test]
    fn assess_complexity_ignores_leading_whitespace() {
        let result = assess_complexity("    refactor    ");
        assert!(matches!(result, ComplexityAssessment::Complex { .. }));
    }

    #[test]
    fn persist_and_load_plan_artifact_round_trip() {
        let temp = std::env::temp_dir().join(format!(
            "planner-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp).unwrap();

        let artifact =
            PlanArtifact::new("test task", vec![PlanStep::new("s1", "step 1", "criteria")]);
        let path = persist_plan_artifact(&artifact, &temp).expect("persist should succeed");
        assert!(path.exists());

        let loaded = load_plan_artifact(&path).expect("load should succeed");
        assert_eq!(loaded.id, artifact.id);
        assert_eq!(loaded.task_summary, "test task");
        assert_eq!(loaded.steps.len(), 1);
        assert_eq!(loaded.steps[0].id, "s1");

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn persist_creates_claw_plans_directory() {
        let temp = std::env::temp_dir().join(format!(
            "planner-mkdir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // 确保 temp 存在但 .claw/plans 不存在。
        fs::create_dir_all(&temp).unwrap();
        let plans_dir = temp.join(".claw").join("plans");
        assert!(!plans_dir.exists());

        let artifact = PlanArtifact::new("t", Vec::new());
        let _ = persist_plan_artifact(&artifact, &temp).expect("should succeed");
        assert!(plans_dir.exists());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn persist_overwrites_same_id() {
        // 同一 plan 的 replan 应该覆写同一文件,不产生多个文件。
        let temp = std::env::temp_dir().join(format!(
            "planner-overwrite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp).unwrap();

        let mut artifact = PlanArtifact::new("task", vec![PlanStep::new("s1", "step", "c")]);
        let path = persist_plan_artifact(&artifact, &temp).unwrap();

        // 模拟 replan:同一 id,但 step 状态改变。
        artifact.steps[0].mark_failed();
        let _ = artifact.trigger_replan(3);
        let path2 = persist_plan_artifact(&artifact, &temp).unwrap();

        assert_eq!(path, path2);
        let files: Vec<_> = fs::read_dir(temp.join(".claw").join("plans"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1, "should overwrite, not create new file");

        let _ = fs::remove_dir_all(&temp);
    }

    // ── P2:Pre-commitment risk 评估测试 ──

    #[test]
    fn assess_step_risk_returns_high_for_delete() {
        assert_eq!(assess_step_risk("Delete the user table"), StepRisk::High);
    }

    #[test]
    fn assess_step_risk_returns_high_for_production_deploy() {
        assert_eq!(
            assess_step_risk("Deploy to production environment"),
            StepRisk::High
        );
    }

    #[test]
    fn assess_step_risk_returns_high_for_security_keywords() {
        assert_eq!(assess_step_risk("Update auth token"), StepRisk::High);
        assert_eq!(assess_step_risk("Rotate password"), StepRisk::High);
        assert_eq!(
            assess_step_risk("Fix security vulnerability"),
            StepRisk::High
        );
    }

    #[test]
    fn assess_step_risk_returns_high_for_chinese_keywords() {
        assert_eq!(assess_step_risk("删除用户数据"), StepRisk::High);
        assert_eq!(assess_step_risk("部署到生产环境"), StepRisk::High);
        assert_eq!(assess_step_risk("修改权限配置"), StepRisk::High);
    }

    #[test]
    fn assess_step_risk_returns_low_for_safe_operations() {
        assert_eq!(assess_step_risk("Read configuration file"), StepRisk::Low);
        assert_eq!(assess_step_risk("Add unit tests"), StepRisk::Low);
        assert_eq!(assess_step_risk("Update documentation"), StepRisk::Low);
    }

    #[test]
    fn assess_step_risk_is_case_insensitive() {
        assert_eq!(assess_step_risk("DELETE all rows"), StepRisk::High);
        assert_eq!(assess_step_risk("Force Push"), StepRisk::High);
    }

    #[test]
    fn decompose_task_marks_high_risk_steps() {
        // 包含 "delete" 关键词的输入,分解后的 step 应标记 High risk
        let steps = decompose_task("delete the migration files and refactor auth");
        assert!(steps.iter().any(|s| s.risk_level == StepRisk::High));
    }

    #[test]
    fn decompose_task_keeps_low_risk_for_safe_input() {
        let steps = decompose_task("read the config and update the docs");
        assert!(steps.iter().all(|s| s.risk_level == StepRisk::Low));
    }

    #[test]
    fn decompose_task_fallback_inherits_high_risk_from_input() {
        // 兜底单步(无法拆分)时,若整体任务命中高风险,继承 High
        let steps = decompose_task("force deploy");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].risk_level, StepRisk::High);
    }
}
