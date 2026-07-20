//! PreCompletionChecklistMiddleware — Step 2.1 Plan/Execute/Review 中的 Review 阶段。
//!
//! 在主 agent 试图结束 turn 前拦截,强制跑 verification 流程:
//! 1. 检查所有 `PlanStep` 是否 `Succeeded`。
//! 2. 若有 `Failed`,决定是否触发 `Replan`(`max_replans` 控制)。
//! 3. 若 Replan 上限已达,返回 `Failed` 让主循环退出。
//!
//! 与 Stage 3.1 `VerifierAgent` 的分工:
//! - `VerifierAgent`:校验单个 step 的 `acceptance_criteria`(规则/视觉/模型当裁判)。
//! - `PreCompletionChecklistMiddleware`:全局调度,决定是否 replan / 退出。

use super::artifact::{PlanArtifact, StepStatus};

/// Replan 上限(避免无限重试)。3 次失败后强制 Failed 退出。
pub const DEFAULT_MAX_REPLANS: u32 = 3;

/// Review 阶段的输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewResult {
    /// 所有 step Succeeded,可以结束 turn。
    AllPassed,
    /// 有 step Failed,但已触发 Replan,主 agent 应该继续执行。
    ReplanTriggered {
        /// Replan 后的新计数(1, 2, 3...)。
        new_replan_count: u32,
        /// 重置为 Pending 的 step id 列表。
        reset_step_ids: Vec<String>,
    },
    /// 有 step Failed 且 Replan 上限已达,无法继续。
    Failed {
        /// Failed step id 列表(用于错误消息)。
        failed_step_ids: Vec<String>,
        /// 总 Replan 次数。
        replan_count: u32,
    },
}

/// PreCompletion Checklist 中间件。
///
/// 持有 `max_replans` 配置与 `RecoveryOrchestrator`(Stage 1.2)对接:
/// `Failed` 时通过 `WorkerFailureKind::PlanStep` 触发 RecoveryOrchestrator,
/// 但 RecoveryOrchestrator 的 `max_attempts=1` 硬上限保护不会无限重试。
#[derive(Debug, Clone, Copy)]
pub struct PreCompletionChecklistMiddleware {
    max_replans: u32,
}

impl Default for PreCompletionChecklistMiddleware {
    fn default() -> Self {
        Self {
            max_replans: DEFAULT_MAX_REPLANS,
        }
    }
}

impl PreCompletionChecklistMiddleware {
    #[must_use]
    pub fn new(max_replans: u32) -> Self {
        Self { max_replans }
    }

    #[must_use]
    pub fn max_replans(&self) -> u32 {
        self.max_replans
    }

    /// 执行 Review。
    ///
    /// 调用时机:主 agent 在 `run_turn` 主循环末尾,准备结束 turn 前调用。
    /// 会修改 `artifact`:若触发 replan,Failed steps 重置为 Pending。
    pub fn review(&self, artifact: &mut PlanArtifact) -> ReviewResult {
        artifact.transition_to_reviewing();

        // 1. 所有 step 都 Succeeded → 完成。
        if artifact.all_succeeded() {
            artifact.mark_completed();
            return ReviewResult::AllPassed;
        }

        // 2. 收集 Failed step。
        let failed_ids: Vec<String> = artifact
            .failed_step_ids()
            .into_iter()
            .map(String::from)
            .collect();

        if failed_ids.is_empty() {
            // 没有 Failed,但有 Pending/Executing/Skipped,说明 plan 还没跑完。
            // 回到 Executing 阶段继续。
            artifact.transition_to_executing();
            return ReviewResult::ReplanTriggered {
                new_replan_count: artifact.replan_count,
                reset_step_ids: Vec::new(),
            };
        }

        // 3. 尝试触发 replan。
        let reset_ids: Vec<String> = artifact
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Failed)
            .map(|s| s.id.clone())
            .collect();

        match artifact.trigger_replan(self.max_replans) {
            Some(new_count) => {
                artifact.transition_to_executing();
                ReviewResult::ReplanTriggered {
                    new_replan_count: new_count,
                    reset_step_ids: reset_ids,
                }
            }
            None => {
                artifact.mark_failed();
                ReviewResult::Failed {
                    failed_step_ids: failed_ids,
                    replan_count: artifact.replan_count,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::artifact::{PlanArtifact, PlanPhase, PlanStep, VerificationMethod};

    fn sample_steps() -> Vec<PlanStep> {
        vec![
            PlanStep::new("s1", "step 1", "c1", VerificationMethod::Rule),
            PlanStep::new("s2", "step 2", "c2", VerificationMethod::Rule),
        ]
    }

    #[test]
    fn review_passes_when_all_succeeded() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        for step in &mut artifact.steps {
            step.mark_succeeded();
        }
        let middleware = PreCompletionChecklistMiddleware::default();
        let result = middleware.review(&mut artifact);
        assert_eq!(result, ReviewResult::AllPassed);
        assert_eq!(artifact.phase, PlanPhase::Completed);
    }

    #[test]
    fn review_triggers_replan_on_failure() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        artifact.steps[0].mark_succeeded();
        artifact.steps[1].mark_failed();
        let middleware = PreCompletionChecklistMiddleware::default();
        let result = middleware.review(&mut artifact);
        match result {
            ReviewResult::ReplanTriggered {
                new_replan_count,
                reset_step_ids,
            } => {
                assert_eq!(new_replan_count, 1);
                assert_eq!(reset_step_ids, vec!["s2".to_string()]);
            }
            other => panic!("expected ReplanTriggered, got {other:?}"),
        }
        assert_eq!(artifact.steps[1].status, StepStatus::Pending);
        assert_eq!(artifact.phase, PlanPhase::Executing);
    }

    #[test]
    fn review_fails_when_replan_limit_reached() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        artifact.steps[0].mark_succeeded();
        artifact.steps[1].mark_failed();
        artifact.replan_count = 3; // 已达上限
        let middleware = PreCompletionChecklistMiddleware::default();
        let result = middleware.review(&mut artifact);
        match result {
            ReviewResult::Failed {
                failed_step_ids,
                replan_count,
            } => {
                assert_eq!(failed_step_ids, vec!["s2".to_string()]);
                assert_eq!(replan_count, 3);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(artifact.phase, PlanPhase::Failed);
    }

    #[test]
    fn review_returns_to_executing_when_no_failed_but_not_all_done() {
        // 所有 step 仍 Pending(没 Failed,也没全部 Succeeded)。
        let mut artifact = PlanArtifact::new("test", sample_steps());
        let middleware = PreCompletionChecklistMiddleware::default();
        let result = middleware.review(&mut artifact);
        match result {
            ReviewResult::ReplanTriggered {
                reset_step_ids, ..
            } => {
                assert!(reset_step_ids.is_empty());
            }
            other => panic!("expected ReplanTriggered with empty resets, got {other:?}"),
        }
        assert_eq!(artifact.phase, PlanPhase::Executing);
    }

    #[test]
    fn max_replans_configurable() {
        let middleware = PreCompletionChecklistMiddleware::new(5);
        assert_eq!(middleware.max_replans(), 5);
    }
}
