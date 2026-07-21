//! PlanArtifact 数据结构 — Step 2.1 Plan/Execute/Review 三段循环的核心数据模型。
//!
//! 设计原则(见 `docs/harness-engineering-optimization-plan.md` Step 2.1 与 §5.2):
//! 1. **末尾追加**:PlanArtifact 在 prompt 的"变动区"末尾追加,不污染
//!    "绝对稳定区"(system_prompt + tools_schema)与"半稳定区"(memory/goal/git_context)。
//! 2. **可持久化**:写入 `<workspace>/.claw/plans/<timestamp>.json`,可跨会话恢复。
//! 3. **可校验**:每个 step 含 `acceptance_criteria` + `verify_command`,
//!    与 Stage 3.1 VerifierAgent 对接(v2.0:执行命令检查 exit_code)。
//! 4. **可重规划**:失败 step 触发 `Replan` 重新生成剩余步骤。

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 当前阶段标记 — 便于 PlanArtifact 在执行过程中追踪整体状态机位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanPhase {
    /// Plan 阶段:Planner 正在生成 steps。
    Planning,
    /// Execute 阶段:主 agent 正在执行某个 step。
    Executing,
    /// Review 阶段:PreCompletionChecklistMiddleware 正在校验所有 steps。
    Reviewing,
    /// Done:所有 steps Succeeded。
    Completed,
    /// Failed:至少一个 step Failed 且无法 replan。
    Failed,
}

/// 单个 Plan step 的执行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    /// 尚未开始。
    Pending,
    /// 主 agent 正在执行此 step。
    Executing,
    /// VerifierAgent 已确认满足 `acceptance_criteria`。
    Succeeded,
    /// VerifierAgent 检测到失败,需要 replan 或人工介入。
    Failed,
    /// 因前置 step Failed 而被跳过。
    Skipped,
}

/// Plan 中的一个原子步骤。
///
/// 一个 step 对应主 agent 的一组连续 tool calls,粒度由 planner 决定
/// (建议:一个文件级修改 = 一个 step,跨文件 refactor 拆成多个 step)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// 全局唯一标识(短 uuid 或递增数字字符串)。
    pub id: String,
    /// 人类可读的步骤描述(注入 prompt 让主 agent 知道要做什么)。
    pub description: String,
    /// 完成判定标准(自然语言或结构化),供 VerifierAgent 校验。
    pub acceptance_criteria: String,
    /// v2.0:验证命令(如 `cargo test --no-fail-fast` / `cargo clippy -- -D warnings`)。
    /// `None` → 跳过验证(保守通过,不阻塞 plan)。
    /// `Some(cmd)` → VerifierAgent 执行 cmd,检查 exit_code。
    #[serde(default)]
    pub verify_command: Option<String>,
    /// v2.0:该 step 最近一次执行关联的 tool_use_id(用于精准查找 tool_result)。
    /// `None` 表示尚未执行或主 agent 未关联。
    /// 解决 v1.0 "tool_result 全量拼接,信号被噪音淹没" 问题。
    #[serde(default)]
    pub last_tool_use_id: Option<String>,
    /// 当前状态,见 [`StepStatus`]。
    pub status: StepStatus,
    /// 已尝试次数(含 replan 后的重试)。0 表示未开始,1+ 表示已尝试过。
    pub attempts: u32,
}

impl PlanStep {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        acceptance_criteria: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            acceptance_criteria: acceptance_criteria.into(),
            verify_command: None,
            last_tool_use_id: None,
            status: StepStatus::Pending,
            attempts: 0,
        }
    }

    /// 带验证命令的构造器(v2.0 推荐)。
    #[must_use]
    pub fn with_verify_command(
        id: impl Into<String>,
        description: impl Into<String>,
        acceptance_criteria: impl Into<String>,
        verify_command: impl Into<String>,
    ) -> Self {
        let mut step = Self::new(id, description, acceptance_criteria);
        step.verify_command = Some(verify_command.into());
        step
    }

    /// 标记此 step 开始执行。`attempts` 自增。
    pub fn mark_executing(&mut self) {
        if self.status == StepStatus::Pending || self.status == StepStatus::Failed {
            self.attempts += 1;
        }
        self.status = StepStatus::Executing;
    }

    /// 标记此 step 成功完成。
    pub fn mark_succeeded(&mut self) {
        self.status = StepStatus::Succeeded;
    }

    /// 标记此 step 失败。
    pub fn mark_failed(&mut self) {
        self.status = StepStatus::Failed;
    }

    /// 标记此 step 被跳过(前置依赖 Failed)。
    pub fn mark_skipped(&mut self) {
        self.status = StepStatus::Skipped;
    }

    /// v2.0:关联最近一次 tool_use_id(用于 Review 阶段精准查找 tool_result)。
    pub fn set_last_tool_use_id(&mut self, tool_use_id: impl Into<String>) {
        self.last_tool_use_id = Some(tool_use_id.into());
    }
}

/// 整个 Plan 的状态机。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanArtifact {
    /// 全局唯一 ID(时间戳 + 短随机后缀)。
    pub id: String,
    /// Unix epoch 毫秒。
    pub created_at_ms: u64,
    /// 用户原始任务摘要(注入 prompt 让主 agent 知道大目标)。
    pub task_summary: String,
    /// 步骤列表(顺序敏感)。
    pub steps: Vec<PlanStep>,
    /// 当前整体阶段。
    pub phase: PlanPhase,
    /// 是否触发过 replan(用于诊断 doom loop)。
    pub replan_count: u32,
}

impl PlanArtifact {
    /// 创建新的空 artifact,phase=Planning。
    #[must_use]
    pub fn new(task_summary: impl Into<String>, steps: Vec<PlanStep>) -> Self {
        Self {
            id: generate_plan_id(),
            created_at_ms: now_ms(),
            task_summary: task_summary.into(),
            steps,
            phase: PlanPhase::Planning,
            replan_count: 0,
        }
    }

    /// 当前正在执行的 step(第一个 Executing 状态,或第一个 Pending 状态)。
    #[must_use]
    pub fn current_step(&self) -> Option<&PlanStep> {
        self.steps
            .iter()
            .find(|step| step.status == StepStatus::Executing)
            .or_else(|| {
                self.steps
                    .iter()
                    .find(|step| step.status == StepStatus::Pending)
            })
    }

    /// 当前正在执行的 step 的可变引用。
    pub fn current_step_mut(&mut self) -> Option<&mut PlanStep> {
        // 先找 Executing,找不到再找第一个 Pending(并自动 mark_executing)。
        let has_executing = self
            .steps
            .iter()
            .any(|step| step.status == StepStatus::Executing);
        if has_executing {
            return self
                .steps
                .iter_mut()
                .find(|step| step.status == StepStatus::Executing);
        }
        // 没有 Executing,找第一个 Pending 的索引,mark_executing 后返回引用。
        // (使用索引避免部分 move 借用检查冲突。)
        let pending_idx = self
            .steps
            .iter()
            .position(|step| step.status == StepStatus::Pending)?;
        self.steps[pending_idx].mark_executing();
        Some(&mut self.steps[pending_idx])
    }

    /// 所有 step 是否都已 Succeeded(用于 Review 阶段判断)。
    #[must_use]
    pub fn all_succeeded(&self) -> bool {
        !self.steps.is_empty()
            && self
                .steps
                .iter()
                .all(|step| step.status == StepStatus::Succeeded)
    }

    /// 收集所有 Failed step 的 id(用于 Replan 决策)。
    #[must_use]
    pub fn failed_step_ids(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|step| step.status == StepStatus::Failed)
            .map(|step| step.id.as_str())
            .collect()
    }

    /// 进入 Execute 阶段。
    pub fn transition_to_executing(&mut self) {
        self.phase = PlanPhase::Executing;
    }

    /// 进入 Review 阶段。
    pub fn transition_to_reviewing(&mut self) {
        self.phase = PlanPhase::Reviewing;
    }

    /// 标记整体完成(所有 step Succeeded)。
    pub fn mark_completed(&mut self) {
        self.phase = PlanPhase::Completed;
    }

    /// 标记整体失败(无法 replan)。
    pub fn mark_failed(&mut self) {
        self.phase = PlanPhase::Failed;
    }

    /// 触发 Replan:replan_count +1,Failed steps 重置为 Pending(重新尝试)。
    /// 返回新的 replan_count。若超过最大重试次数(`max_replans`),返回 None。
    pub fn trigger_replan(&mut self, max_replans: u32) -> Option<u32> {
        if self.replan_count >= max_replans {
            return None;
        }
        self.replan_count += 1;
        for step in &mut self.steps {
            if step.status == StepStatus::Failed {
                step.status = StepStatus::Pending;
                // 不重置 attempts — VerifierAgent 可根据 attempts 决定是否升级策略。
            }
        }
        self.phase = PlanPhase::Planning;
        Some(self.replan_count)
    }

    /// 渲染为可注入 prompt 的纯文本(末尾追加到 dynamic_sections)。
    ///
    /// 格式设计原则:
    /// - 用 fence + header 让模型能识别边界。
    /// - 列出所有 step + 当前状态 + 失败次数,让模型理解全局。
    /// - **空 steps 引导**:plan 创建后主 agent 尚未填充 steps 时,
    ///   输出 task_summary + 拆分步骤的引导文本,避免 PlanArtifact
    ///   "创建了但不可见"导致主 agent 无从下手(BUG-1 修复)。
    /// - 不超过 ~1-3K tokens(对应 §5.2 变动区预算)。
    #[must_use]
    pub fn render_for_prompt(&self) -> String {
        let mut out = String::with_capacity(512 + self.steps.len() * 200);
        out.push_str("# Active Plan\n\n");
        out.push_str(&format!("Task: {}\n\n", self.task_summary));
        if self.steps.is_empty() {
            // BUG-1 修复:空 steps 时输出引导文本,让主 agent 知道
            // 当前 plan 已创建但未拆分步骤,需要主 agent 在本轮:
            // 1. 把任务拆分为有序 steps;2. 通过 PlanArtifact 更新接口
            // (或下一轮 turn 入口的 assess_complexity)填充 steps。
            // 否则之前返回空串 → dynamic_sections.push("") → 主 agent
            // 完全看不到 plan,Review 阶段又因 steps 为空跳过,
            // 形成"plan 创建了但永不生效"的死循环。
            out.push_str("Steps: (pending decomposition)\n\n");
            out.push_str(
                "This task was detected as complex but has not yet been decomposed into steps.\n",
            );
            out.push_str(
                "Break it down into ordered steps with explicit acceptance criteria, \
                then execute them one by one. Each step should map to a coherent unit of work \
                (e.g., one file-level change or one cross-module refactor).\n",
            );
            out.push_str("\nFocus on decomposing and executing the task. Do not skip ahead.");
            return out;
        }
        out.push_str("Steps:\n");
        for (idx, step) in self.steps.iter().enumerate() {
            let status_label = match step.status {
                StepStatus::Pending => "⏳ pending",
                StepStatus::Executing => "▶ executing",
                StepStatus::Succeeded => "✓ done",
                StepStatus::Failed => "✗ failed",
                StepStatus::Skipped => "⊘ skipped",
            };
            out.push_str(&format!(
                "{}. [{}] {}\n   acceptance: {}\n   verify: {} (attempts: {})\n",
                idx + 1,
                status_label,
                step.description,
                step.acceptance_criteria,
                step.verify_command.as_deref().unwrap_or("(skip)"),
                step.attempts,
            ));
        }
        out.push_str("\nFocus on the current step. Do not skip ahead.");
        out
    }
}

/// 生成形如 `plan-1777000000000-a1b2` 的 ID(时间戳 + 4 位随机后缀)。
fn generate_plan_id() -> String {
    let ts = now_ms();
    // 简单 4 位 hex 随机(SystemTime 提供的纳秒取模)。
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = format!("{nanos:04x}");
    let suffix_len = suffix.len();
    let suffix = if suffix_len >= 4 {
        suffix[suffix_len - 4..].to_string()
    } else {
        suffix
    };
    format!("plan-{ts}-{suffix}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_steps() -> Vec<PlanStep> {
        vec![
            PlanStep::new("s1", "Read file", "file read"),
            PlanStep::new("s2", "Edit file", "edit applied"),
        ]
    }

    #[test]
    fn new_artifact_starts_in_planning_phase() {
        let artifact = PlanArtifact::new("test task", sample_steps());
        assert_eq!(artifact.phase, PlanPhase::Planning);
        assert_eq!(artifact.replan_count, 0);
        assert!(!artifact.steps.is_empty());
    }

    #[test]
    fn current_step_picks_first_pending_when_no_executing() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        // 第一次调用:无 Executing → 标记第一个 Pending 为 Executing 并返回。
        let step = artifact.current_step_mut().expect("should find step");
        assert_eq!(step.id, "s1");
        assert_eq!(step.status, StepStatus::Executing);
        assert_eq!(step.attempts, 1);
    }

    #[test]
    fn current_step_prefers_existing_executing() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        // 手动把 s2 标记为 Executing,验证不会重复标记 s1。
        artifact.steps[1].mark_executing();
        let step = artifact.current_step_mut().expect("should find step");
        assert_eq!(step.id, "s2");
        assert_eq!(artifact.steps[0].status, StepStatus::Pending);
        // attempts 不变(原本已经是 Executing)。
        assert_eq!(artifact.steps[1].attempts, 1);
    }

    #[test]
    fn all_succeeded_returns_false_with_pending() {
        let artifact = PlanArtifact::new("test", sample_steps());
        assert!(!artifact.all_succeeded());
    }

    #[test]
    fn all_succeeded_returns_true_when_all_done() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        for step in &mut artifact.steps {
            step.mark_succeeded();
        }
        assert!(artifact.all_succeeded());
    }

    #[test]
    fn failed_step_ids_collects_only_failed() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        artifact.steps[0].mark_failed();
        artifact.steps[1].mark_succeeded();
        let failed = artifact.failed_step_ids();
        assert_eq!(failed, vec!["s1"]);
    }

    #[test]
    fn trigger_replan_resets_failed_to_pending() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        // mark_executing 先自增 attempts,再 mark_failed 标记状态。
        artifact.steps[0].mark_executing();
        artifact.steps[0].mark_failed();
        let new_count = artifact.trigger_replan(3).expect("should allow replan");
        assert_eq!(new_count, 1);
        assert_eq!(artifact.steps[0].status, StepStatus::Pending);
        // attempts 不重置(让 VerifierAgent 看到 escalating attempts)。
        assert_eq!(artifact.steps[0].attempts, 1);
        assert_eq!(artifact.phase, PlanPhase::Planning);
    }

    #[test]
    fn trigger_replan_returns_none_when_exceeding_max() {
        let mut artifact = PlanArtifact::new("test", sample_steps());
        artifact.replan_count = 3;
        let result = artifact.trigger_replan(3);
        assert!(result.is_none());
    }

    #[test]
    fn render_for_prompt_contains_all_steps() {
        let artifact = PlanArtifact::new("test task", sample_steps());
        let rendered = artifact.render_for_prompt();
        assert!(rendered.contains("Active Plan"));
        assert!(rendered.contains("Read file"));
        assert!(rendered.contains("Edit file"));
        assert!(rendered.contains("Focus on the current step"));
    }

    #[test]
    fn render_for_prompt_returns_decomposition_guide_when_no_steps() {
        // BUG-1 修复:空 steps 不再返回空串,而是返回引导文本,
        // 让主 agent 看到 plan 并知道需要拆分步骤。
        let artifact = PlanArtifact::new("test", Vec::new());
        let rendered = artifact.render_for_prompt();
        assert!(!rendered.is_empty(), "空 steps 必须返回引导文本,不能为空串");
        assert!(rendered.contains("Active Plan"));
        assert!(rendered.contains("pending decomposition"));
        assert!(rendered.contains("Break it down"));
    }

    #[test]
    fn mark_executing_increments_attempts_only_on_state_change() {
        let mut step = PlanStep::new("s1", "d", "c");
        step.mark_executing();
        assert_eq!(step.attempts, 1);
        // 第二次调用(已经是 Executing),attempts 不变。
        step.mark_executing();
        assert_eq!(step.attempts, 1);
        // 失败后再次 mark_executing,attempts 应该 +1。
        step.mark_failed();
        step.mark_executing();
        assert_eq!(step.attempts, 2);
    }
}
