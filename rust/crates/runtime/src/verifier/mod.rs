//! Verifier Agent — Step 3.1 V(验证)层。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 3.1
//!
//! ## v2.0 重构(2026-07-21 方向纠正)
//!
//! v1.0 设计存在 4 个结构性缺陷(详见 docs/harness-engineering-optimization-plan.md):
//! 1. Rule Verifier 是"文本启发式",不是"规则验证"(误报率高)
//! 2. Visual / ModelJudge placeholder 返回 `passed: true` = 无效验证
//! 3. `remediation` 字段完全丢失 — 主 agent 盲目重试
//! 4. `tool_result` 全量拼接,信号被噪音淹没
//!
//! v2.0 改动:
//! - **删除** `VisualVerifier` / `ModelJudgeVerifier` 两个 placeholder 模块
//! - **删除** `VerificationMethod` 枚举(只剩 Rule 无意义)
//! - `RuleVerifier` 改为**真正执行命令** + 检查 exit_code + 解析结构化输出
//! - `PlanStep` 新增 `verify_command: Option<String>` + `last_tool_use_id: Option<String>`
//! - `ReviewResult::ReplanTriggered` 携带 `failed_verifications`(remediation 不再丢失)
//! - 主 agent 下次 turn 入口把 remediation 拼入 system prompt
//!
//! ## 缓存保护(详见 §5.2)
//!
//! VerifierAgent 执行命令走子进程,不调 LLM,**不影响主 agent 缓存**。
//! 若未来需要 LLM 裁判,走 `dispatch_subagent`(已实现)而非新建 ModelJudgeVerifier。

pub mod rule;

pub use rule::{RuleVerifier, RuleVerdict};

use serde::{Deserialize, Serialize};

/// VerifierAgent 输出 — 验证单个 step 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationResult {
    /// 是否通过验收标准。
    pub passed: bool,
    /// 详细说明(通过原因或失败原因)。
    pub detail: String,
    /// 失败时的修正建议(注入主 agent prompt 触发 replan)。
    pub remediation: Option<String>,
    /// 验证耗时(毫秒)。
    pub elapsed_ms: u64,
}

impl VerificationResult {
    /// 构造一个通过结果。
    #[must_use]
    pub fn passed(detail: impl Into<String>) -> Self {
        Self {
            passed: true,
            detail: detail.into(),
            remediation: None,
            elapsed_ms: 0,
        }
    }

    /// 构造一个失败结果,带 remediation。
    #[must_use]
    pub fn failed(detail: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self {
            passed: false,
            detail: detail.into(),
            remediation: Some(remediation.into()),
            elapsed_ms: 0,
        }
    }

    /// 跳过验证(无 verify_command 时使用,保守通过)。
    #[must_use]
    pub fn skipped() -> Self {
        Self::passed("verification skipped — no verify_command configured")
    }
}

/// VerifierAgent — 校验 step 是否满足 acceptance_criteria。
///
/// v2.0:只持有 `RuleVerifier`,执行 `verify_command` 检查 exit_code。
/// 不再调 LLM,不影响主 agent 缓存。
#[derive(Debug, Clone, Default)]
pub struct VerifierAgent {
    rule_verifier: RuleVerifier,
}

impl VerifierAgent {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 验证 step 是否满足 acceptance_criteria。
    ///
    /// v2.0 签名:不再需要 `method` 参数(只剩 Rule 路径)。
    ///
    /// # 参数
    /// - `tool_result`:该 step 关联的 tool_result 文本(精准查找,非全量拼接)
    /// - `acceptance_criteria`:自然语言描述的完成标准(用于 remediation 文案)
    /// - `verify_command`:实际执行的验证命令(如 `cargo test --no-fail-fast`)
    ///   - `None` → 返回 `skipped`(保守通过,不阻塞 plan)
    ///   - `Some(cmd)` → 执行 cmd,检查 exit_code,解析输出
    ///
    /// # 返回
    /// [`VerificationResult`] — `passed` / `detail` / `remediation`
    #[must_use]
    pub fn verify(
        &self,
        tool_result: &str,
        acceptance_criteria: &str,
        verify_command: Option<&str>,
    ) -> VerificationResult {
        let start = std::time::Instant::now();
        let verdict = self
            .rule_verifier
            .verify(tool_result, acceptance_criteria, verify_command);
        VerificationResult {
            passed: verdict.passed,
            detail: verdict.detail,
            remediation: verdict.remediation,
            elapsed_ms: start.elapsed().as_millis() as u64,
        }
    }

    /// 获取规则验证器引用(用于配置)。
    #[must_use]
    pub fn rule_verifier(&self) -> &RuleVerifier {
        &self.rule_verifier
    }

    /// 获取可变规则验证器(用于配置)。
    pub fn rule_verifier_mut(&mut self) -> &mut RuleVerifier {
        &mut self.rule_verifier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_without_command_returns_skipped() {
        let agent = VerifierAgent::new();
        let result = agent.verify("some output", "tests pass", None);
        assert!(result.passed);
        assert!(result.detail.contains("skipped"));
    }

    #[test]
    fn verify_with_passing_command_returns_passed() {
        let agent = VerifierAgent::new();
        // cmd /c "exit 0" 在 Windows 上退出码为 0
        let result = agent.verify("output", "tests pass", Some("cmd /c exit 0"));
        assert!(result.passed);
    }

    #[test]
    fn verify_with_failing_command_returns_failed_with_remediation() {
        let agent = VerifierAgent::new();
        let result = agent.verify("output", "tests pass", Some("cmd /c exit 1"));
        assert!(!result.passed);
        assert!(result.remediation.is_some());
    }

    #[test]
    fn verification_result_passed_helper() {
        let r = VerificationResult::passed("all good");
        assert!(r.passed);
        assert_eq!(r.detail, "all good");
        assert!(r.remediation.is_none());
    }

    #[test]
    fn verification_result_failed_helper() {
        let r = VerificationResult::failed("oops", "fix the oops");
        assert!(!r.passed);
        assert_eq!(r.detail, "oops");
        assert_eq!(r.remediation.as_deref(), Some("fix the oops"));
    }

    #[test]
    fn verification_result_serializes_to_json() {
        let result = VerificationResult {
            passed: true,
            detail: "all tests passed".to_owned(),
            remediation: None,
            elapsed_ms: 42,
        };
        let json = serde_json::to_string(&result).expect("should serialize");
        assert!(json.contains("\"passed\":true"));
        assert!(json.contains("\"elapsed_ms\":42"));
    }

    #[test]
    fn verify_records_elapsed_time() {
        let agent = VerifierAgent::new();
        let result = agent.verify("tests passed", "tests pass", Some("cmd /c exit 0"));
        assert!(result.elapsed_ms < 5000, "should complete in under 5 seconds");
    }
}
