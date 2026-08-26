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

pub use rule::{RuleVerdict, RuleVerifier};

use crate::multi_agent::validation::JudgeClient;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
/// v2.0:持有 `RuleVerifier` 执行 `verify_command` 检查 exit_code。
/// v2.1(2026-08-13 P2):新增可选 `ModelJudgeVerifier`(LLM 裁判),当 step
/// 无 `verify_command` 时,用注入的 judge client 语义校验 tool_result 是否
/// 满足 acceptance_criteria,填补「无可执行命令、只有自然语言标准」的验证盲区。
///
/// 缓存保护:ModelJudge 仅通过依赖倒置的 `JudgeClient` trait 调用,
/// 由上层(CLI)注入生产实现;未注入时行为与 v2.0 完全一致(不调 LLM)。
#[derive(Clone, Default)]
pub struct VerifierAgent {
    rule_verifier: RuleVerifier,
    /// 可选的 LLM 裁判 client。None 时 verify 退化为纯规则路径。
    model_judge: Option<Arc<dyn JudgeClient>>,
}

impl std::fmt::Debug for VerifierAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifierAgent")
            .field("rule_verifier", &self.rule_verifier)
            .field("has_model_judge", &self.model_judge.is_some())
            .finish()
    }
}

impl VerifierAgent {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入 LLM 裁判 client。注入后,无 `verify_command` 的 step 会用 LLM
    /// 语义校验(而非直接 skipped)。构造失败由调用方决定是否注入。
    #[must_use]
    pub fn with_model_judge(mut self, client: Arc<dyn JudgeClient>) -> Self {
        self.model_judge = Some(client);
        self
    }

    /// 验证 step 是否满足 acceptance_criteria。
    ///
    /// 路径选择:
    /// - `verify_command = Some(cmd)` → 执行命令,检查 exit_code(规则路径)。
    /// - `verify_command = None` 且注入 model_judge 且 tool_result 非空
    ///   → LLM 语义裁判(模型路径)。
    /// - 其余 → `skipped`(保守通过,不阻塞 plan)。
    ///
    /// # 参数
    /// - `tool_result`:该 step 关联的 tool_result 文本(精准查找,非全量拼接)
    /// - `acceptance_criteria`:自然语言描述的完成标准(用于 remediation 文案)
    /// - `verify_command`:实际执行的验证命令(如 `cargo test --no-fail-fast`)
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
        let verdict = if verify_command.is_none() {
            if let Some(client) = &self.model_judge {
                if tool_result.trim().is_empty() {
                    self.rule_verifier
                        .verify(tool_result, acceptance_criteria, None)
                } else {
                    self.model_judge_verify(client, tool_result, acceptance_criteria)
                }
            } else {
                self.rule_verifier
                    .verify(tool_result, acceptance_criteria, None)
            }
        } else {
            self.rule_verifier
                .verify(tool_result, acceptance_criteria, verify_command)
        };
        VerificationResult {
            passed: verdict.passed,
            detail: verdict.detail,
            remediation: verdict.remediation,
            elapsed_ms: start.elapsed().as_millis() as u64,
        }
    }

    /// LLM 语义裁判:判断 tool_result 是否满足 acceptance_criteria。
    ///
    /// 容错策略(保守通过,不阻塞 plan):
    /// - judge 调用失败(网络/API)→ passed,detail 注明失败原因
    /// - judge 输出无法解析 JSON → passed,detail 注明解析失败
    /// - 仅当 judge 明确返回 `passed: false` 时才判定失败并附 remediation。
    fn model_judge_verify(
        &self,
        client: &Arc<dyn JudgeClient>,
        tool_result: &str,
        acceptance_criteria: &str,
    ) -> RuleVerdict {
        let prompt = build_model_judge_prompt(tool_result, acceptance_criteria);
        let response = match client.judge(&prompt) {
            Ok(r) => r,
            Err(e) => {
                return RuleVerdict {
                    passed: true,
                    detail: format!("model judge unavailable — skipping verification: {e}"),
                    remediation: None,
                };
            }
        };
        match parse_model_judge_response(&response) {
            Ok(verdict) if verdict.passed => RuleVerdict {
                passed: true,
                detail: "model judge passed".to_owned(),
                remediation: None,
            },
            Ok(verdict) => RuleVerdict {
                passed: false,
                detail: format!("model judge failed (criteria: {acceptance_criteria})"),
                remediation: verdict
                    .remediation
                    .or_else(|| Some("model judge 判定未通过,请修复后重试。".to_owned())),
            },
            Err(e) => RuleVerdict {
                passed: true,
                detail: format!("model judge response unparseable — skipping: {e}"),
                remediation: None,
            },
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

/// ModelJudge 输出的解析结果。
#[derive(Debug, Deserialize)]
struct ModelJudgeVerdict {
    passed: bool,
    #[serde(default)]
    remediation: Option<String>,
}

/// 构造 LLM 裁判 prompt。
fn build_model_judge_prompt(tool_result: &str, acceptance_criteria: &str) -> String {
    let tool_result = truncate_for_judge(tool_result);
    format!(
        "你是一个验收裁判(verifier judge),判断某个计划步骤的执行结果是否满足验收标准。\n\n\
         ## 验收标准\n{acceptance_criteria}\n\n\
         ## 执行结果(tool_result,可能已截断)\n{tool_result}\n\n\
         请只输出 JSON(不要用 markdown 代码块包裹):\n\
         {{\"passed\": true|false, \"remediation\": \"失败时的具体修正建议(通过时可省略)\"}}"
    )
}

/// 解析 judge 响应,容忍 markdown 代码块包裹。
fn parse_model_judge_response(text: &str) -> Result<ModelJudgeVerdict, String> {
    let trimmed = text.trim();
    let json = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(trimmed)
        .trim();
    serde_json::from_str::<ModelJudgeVerdict>(json)
        .map_err(|e| format!("invalid judge JSON ({e}): {json}"))
}

/// 截断 tool_result,控制 judge prompt 长度(避免超长结果撑爆上下文)。
fn truncate_for_judge(s: &str) -> String {
    const MAX: usize = 8 * 1024;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let start = s
        .char_indices()
        .rev()
        .find(|&(i, _)| i <= s.len() - MAX)
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("...(truncated)...{}", &s[start..])
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
        assert!(
            result.elapsed_ms < 5000,
            "should complete in under 5 seconds"
        );
    }

    /// 本地 mock judge client,用于测试 ModelJudge 路径(不依赖上层 crate)。
    struct MockJudge {
        response: Result<String, String>,
    }

    impl JudgeClient for MockJudge {
        fn judge(&self, _prompt: &str) -> Result<String, String> {
            self.response.clone()
        }
    }

    #[test]
    fn model_judge_passes_when_judge_returns_true() {
        let mock = MockJudge {
            response: Ok(r#"{"passed": true}"#.to_owned()),
        };
        let agent = VerifierAgent::new().with_model_judge(Arc::new(mock));
        let result = agent.verify("tool output", "criteria", None);
        assert!(result.passed);
        assert!(result.detail.contains("model judge"));
    }

    #[test]
    fn model_judge_fails_with_remediation_when_judge_returns_false() {
        let mock = MockJudge {
            response: Ok(r#"{"passed": false, "remediation": "补上错误处理"}"#.to_owned()),
        };
        let agent = VerifierAgent::new().with_model_judge(Arc::new(mock));
        let result = agent.verify("tool output", "criteria", None);
        assert!(!result.passed);
        assert_eq!(result.remediation.as_deref(), Some("补上错误处理"));
    }

    #[test]
    fn model_judge_skips_when_judge_response_unparseable() {
        let mock = MockJudge {
            response: Ok("not json at all".to_owned()),
        };
        let agent = VerifierAgent::new().with_model_judge(Arc::new(mock));
        let result = agent.verify("tool output", "criteria", None);
        assert!(result.passed);
        assert!(result.detail.contains("unparseable"));
    }

    #[test]
    fn model_judge_skips_when_judge_errors() {
        let mock = MockJudge {
            response: Err("api down".to_owned()),
        };
        let agent = VerifierAgent::new().with_model_judge(Arc::new(mock));
        let result = agent.verify("tool output", "criteria", None);
        assert!(result.passed);
        assert!(result.detail.contains("unavailable"));
    }

    #[test]
    fn model_judge_not_used_when_verify_command_present() {
        // 有 verify_command 时始终走规则路径,不调 LLM。
        let mock = MockJudge {
            response: Ok(r#"{"passed": false, "remediation": "x"}"#.to_owned()),
        };
        let agent = VerifierAgent::new().with_model_judge(Arc::new(mock));
        let result = agent.verify("output", "tests pass", Some("cmd /c exit 0"));
        assert!(result.passed);
        assert!(result.detail.contains("exited 0"));
    }

    #[test]
    fn parse_model_judge_response_handles_markdown_wrapper() {
        let parsed =
            parse_model_judge_response("```json\n{\"passed\": true}\n```").expect("should parse");
        assert!(parsed.passed);
    }
}
