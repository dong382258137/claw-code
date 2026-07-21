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

pub use artifact::{PlanArtifact, PlanPhase, PlanStep, StepStatus};
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
pub fn load_plan_artifact(
    path: &Path,
) -> Result<PlanArtifact, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let artifact: PlanArtifact = serde_json::from_str(&contents)?;
    Ok(artifact)
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

        let artifact = PlanArtifact::new(
            "test task",
            vec![PlanStep::new(
                "s1",
                "step 1",
                "criteria",
            )],
        );
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

        let mut artifact = PlanArtifact::new(
            "task",
            vec![PlanStep::new("s1", "step", "c")],
        );
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
}
