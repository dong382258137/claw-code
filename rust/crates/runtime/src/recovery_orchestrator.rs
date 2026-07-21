//! Recovery orchestrator: bridges worker failures to recovery recipes.
//!
//! Encapsulates the lookup + execution + ledger bookkeeping for automatic
//! recovery. Default behavior matches the legacy `attempt_recovery`
//! semantics (simulated step execution). Real step executors can be
//! wired in later by extending this struct or composing with a
//! `RecoveryStepExecutor` trait — the goal of this module is to expose
//! a single `attempt(WorkerFailureKind)` entry point the runtime can
//! call on the failure path.
//!
//! See `docs/harness-engineering-optimization-plan.md` Step 1.2.

use crate::recovery_recipes::{
    attempt_recovery, FailureScenario, RecoveryContext, RecoveryEvent, RecoveryResult,
    RecoveryStepExecutor,
};
use crate::worker_boot::WorkerFailureKind;

/// Outcome of a single recovery attempt. Carries the scenario, the final
/// result, and the structured event log captured during the attempt.
#[derive(Debug, Clone)]
pub struct RecoveryOutcome {
    pub scenario: FailureScenario,
    pub result: RecoveryResult,
    pub events: Vec<RecoveryEvent>,
}

impl RecoveryOutcome {
    /// True when the recovery succeeded completely.
    #[must_use]
    pub fn recovered(&self) -> bool {
        matches!(self.result, RecoveryResult::Recovered { .. })
    }

    /// True when escalation to a human is required.
    #[must_use]
    pub fn escalated(&self) -> bool {
        matches!(self.result, RecoveryResult::EscalationRequired { .. })
    }
}

/// Orchestrates automatic recovery for failures surfaced by the runtime.
///
/// Wraps a [`RecoveryContext`] so callers can ask for recovery by
/// `WorkerFailureKind` without coupling to the recipe lookup logic. Each
/// scenario respects the underlying recipe's `max_attempts` policy (one
/// automatic attempt before escalation by default), preventing infinite
/// retry loops.
///
/// # Example
/// ```
/// use runtime::recovery_orchestrator::RecoveryOrchestrator;
/// use runtime::worker_boot::WorkerFailureKind;
///
/// let mut orchestrator = RecoveryOrchestrator::new();
/// let outcome = orchestrator.attempt(WorkerFailureKind::Provider);
/// assert!(outcome.recovered() || outcome.escalated());
/// ```
#[derive(Debug, Clone, Default)]
pub struct RecoveryOrchestrator {
    ctx: RecoveryContext,
}

impl RecoveryOrchestrator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempt automatic recovery for the given failure kind.
    ///
    /// Returns a [`RecoveryOutcome`] carrying the scenario, result, and
    /// structured event log. Subsequent calls for the same scenario return
    /// `EscalationRequired` once `max_attempts` is exceeded (recipe
    /// default = 1), preventing infinite retry loops on the failure path.
    #[must_use]
    pub fn attempt(&mut self, failure_kind: WorkerFailureKind) -> RecoveryOutcome {
        let scenario = FailureScenario::from_worker_failure_kind(failure_kind);
        let result = attempt_recovery(&scenario, &mut self.ctx);
        let events = self.ctx.events().to_vec();
        RecoveryOutcome {
            scenario,
            result,
            events,
        }
    }

    /// Returns the structured event log captured across all attempts so far.
    #[must_use]
    pub fn events(&self) -> &[RecoveryEvent] {
        self.ctx.events()
    }

    /// Returns the number of recovery attempts made for a scenario.
    #[must_use]
    pub fn attempt_count(&self, scenario: &FailureScenario) -> u32 {
        self.ctx.attempt_count(scenario)
    }

    /// Borrow the underlying recovery context (for introspection / tests).
    #[must_use]
    pub fn context(&self) -> &RecoveryContext {
        &self.ctx
    }

    /// Take a mutable borrow of the underlying recovery context. Useful for
    /// configuring simulation knobs (e.g. `with_fail_at_step`) in tests.
    pub fn context_mut(&mut self) -> &mut RecoveryContext {
        &mut self.ctx
    }

    /// Builder-style wrapper around `RecoveryContext::with_fail_at_step` for
    /// configuring simulated step failures in tests. Consumes and returns
    /// `self` so it composes with other builders on
    /// [`ConversationRuntime::with_recovery_orchestrator`].
    #[must_use]
    pub fn with_fail_at_step(mut self, index: usize) -> Self {
        let ctx = std::mem::take(&mut self.ctx);
        self.ctx = ctx.with_fail_at_step(index);
        self
    }

    /// BUG-10:注入 step 执行器,启用真实命令执行(Step 1.2)。
    ///
    /// 注入后,`attempt` 将调用 executor.execute(step, scenario) 执行
    /// 真实命令(如 `git rebase`、`cargo clean`),而非模拟。
    #[must_use]
    pub fn with_step_executor(
        mut self,
        executor: std::sync::Arc<dyn RecoveryStepExecutor>,
    ) -> Self {
        let ctx = std::mem::take(&mut self.ctx);
        self.ctx = ctx.with_step_executor(executor);
        self
    }

    /// `&mut self` 版本的 `with_step_executor`。
    pub fn set_step_executor(&mut self, executor: std::sync::Arc<dyn RecoveryStepExecutor>) {
        self.ctx = std::mem::take(&mut self.ctx).with_step_executor(executor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_recipes::RecoveryResult;

    #[test]
    fn first_attempt_recovers_simulated_steps() {
        let mut orchestrator = RecoveryOrchestrator::new();
        let outcome = orchestrator.attempt(WorkerFailureKind::Provider);
        assert!(outcome.recovered());
        // Provider recipe: single RestartWorker step.
        if let RecoveryResult::Recovered { steps_taken } = outcome.result {
            assert_eq!(steps_taken, 1);
        } else {
            panic!("expected Recovered");
        }
        assert!(!outcome.events.is_empty());
    }

    #[test]
    fn second_attempt_escalates_due_to_max_attempts_policy() {
        let mut orchestrator = RecoveryOrchestrator::new();
        let first = orchestrator.attempt(WorkerFailureKind::TrustGate);
        assert!(first.recovered());
        let second = orchestrator.attempt(WorkerFailureKind::TrustGate);
        assert!(second.escalated());
    }

    #[test]
    fn different_failure_kinds_have_independent_attempt_counters() {
        let mut orchestrator = RecoveryOrchestrator::new();
        let provider_outcome = orchestrator.attempt(WorkerFailureKind::Provider);
        assert!(provider_outcome.recovered());
        let trust_outcome = orchestrator.attempt(WorkerFailureKind::TrustGate);
        assert!(trust_outcome.recovered());
        // Same scenario as Provider -> escalation now.
        let provider_again = orchestrator.attempt(WorkerFailureKind::Provider);
        assert!(provider_again.escalated());
    }

    #[test]
    fn context_mut_exposes_simulation_knobs() {
        let mut orchestrator = RecoveryOrchestrator::new().with_fail_at_step(0);
        let outcome = orchestrator.attempt(WorkerFailureKind::Protocol);
        // Failed at first step -> EscalationRequired.
        assert!(outcome.escalated());
    }

    #[test]
    fn attempt_count_tracks_per_scenario() {
        let mut orchestrator = RecoveryOrchestrator::new();
        let scenario = FailureScenario::from_worker_failure_kind(WorkerFailureKind::Provider);
        assert_eq!(orchestrator.attempt_count(&scenario), 0);
        let _ = orchestrator.attempt(WorkerFailureKind::Provider);
        assert_eq!(orchestrator.attempt_count(&scenario), 1);
        // Second call escalates because max_attempts=1 is already exceeded;
        // escalation path does not increment the attempt counter.
        let _ = orchestrator.attempt(WorkerFailureKind::Provider);
        assert_eq!(orchestrator.attempt_count(&scenario), 1);
    }
}
