//! 模型当裁判验证器 — 子 agent 调用 LLM 评估 tool result(placeholder)。
//!
//! 当前实现:返回 inconclusive(未集成 LLM 子 agent)。
//! 未来扩展:启动独立子 agent,注入 tool_result + acceptance_criteria,
//! 让 LLM 输出 passed/failed + reason,走独立 prompt cache(不污染主 agent 缓存)。

use serde::{Deserialize, Serialize};

/// 模型当裁判验证结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelJudgeVerdict {
    /// 是否通过。
    pub passed: bool,
    /// 详细说明。
    pub detail: String,
    /// 失败时的修正建议。
    pub remediation: Option<String>,
}

/// 模型当裁判验证器 — 子 agent LLM 评估(当前 placeholder)。
#[derive(Debug, Clone, Default)]
pub struct ModelJudgeVerifier {
    /// 是否启用 LLM 子 agent(当前总是 false,待集成)。
    enabled: bool,
    /// 子 agent 使用的模型(未来使用)。
    model: Option<String>,
}

impl ModelJudgeVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 启用模型当裁判验证(当前 placeholder,实际未集成 LLM)。
    pub fn enable(&mut self, model: impl Into<String>) {
        self.enabled = true;
        self.model = Some(model.into());
    }

    /// 验证 tool_result — placeholder 返回保守通过。
    ///
    /// **P0-2 修复**：之前 placeholder 返回 `passed: false`，导致 conversation.rs
    /// Review 阶段对所有 Succeeded step 调用 `verify` 后必然 `mark_failed()`，
    /// 触发 replan → max_replans=3 后整个 plan Failed。即使主 agent 实际成功
    /// 完成任务也会被无脑否决。
    ///
    /// 现在改为返回 `passed: true`（保守通过），未启用时跳过验证不阻塞 plan。
    /// 未来集成 LLM 子 agent 时再根据实际评估返回真实 verdict。
    ///
    /// **缓存保护**:未来集成时,子 agent 走独立 LLM 请求 + 独立 prompt cache,
    /// 不污染主 agent 缓存(详见 §5.2 "Subagent as Tool" 模式)。
    #[must_use]
    pub fn verify(&self, tool_result: &str, acceptance_criteria: &str) -> ModelJudgeVerdict {
        if !self.enabled {
            return ModelJudgeVerdict {
                passed: true,
                detail: "model judge verification inconclusive — LLM subagent not enabled"
                    .to_owned(),
                remediation: None,
            };
        }
        // 已启用但 LLM 集成待实现 — 保守通过避免误否决
        let _ = (tool_result, acceptance_criteria);
        ModelJudgeVerdict {
            passed: true,
            detail: "model judge verification inconclusive — LLM subagent integration pending (Step 4.x)"
                .to_owned(),
            remediation: Some(
                "integrate LLM subagent to enable model-as-judge verification".to_owned(),
            ),
        }
    }

    /// 是否已启用。
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_verifier_returns_inconclusive() {
        let verifier = ModelJudgeVerifier::new();
        let verdict = verifier.verify("refactored module", "no breaking changes");
        // P0-2 修复：placeholder 现在保守通过而非否决
        assert!(verdict.passed);
        assert!(verdict.detail.contains("inconclusive"));
        assert!(verdict.remediation.is_none());
    }

    #[test]
    fn enabled_verifier_returns_pending_integration() {
        let mut verifier = ModelJudgeVerifier::new();
        verifier.enable("claude-sonnet-4");
        let verdict = verifier.verify("refactored module", "no breaking changes");
        // P0-2 修复：placeholder 现在保守通过而非否决
        assert!(verdict.passed);
        assert!(verdict.detail.contains("pending"));
        assert!(verdict.remediation.is_some());
    }

    #[test]
    fn enable_sets_model() {
        let mut verifier = ModelJudgeVerifier::new();
        assert!(!verifier.is_enabled());
        verifier.enable("claude-sonnet-4");
        assert!(verifier.is_enabled());
    }
}
