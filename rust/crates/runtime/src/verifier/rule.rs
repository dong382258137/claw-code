//! 规则反馈验证器 — 执行 `cargo test` / `cargo clippy` / `scripts/fmt.sh --check`。
//!
//! 当前实现:基于 tool_result 文本的启发式规则检测(关键词匹配)。
//! 未来扩展:实际执行命令并捕获 stdout/stderr/exit_code。

use serde::{Deserialize, Serialize};

/// 规则验证结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleVerdict {
    /// 是否通过。
    pub passed: bool,
    /// 详细说明。
    pub detail: String,
    /// 失败时的修正建议。
    pub remediation: Option<String>,
}

/// 规则验证器 — 启发式检测 tool_result 中的失败关键词。
#[derive(Debug, Clone, Default)]
pub struct RuleVerifier {
    /// 自定义失败关键词(默认包含 error/failed/panic)。
    failure_keywords: Vec<String>,
}

/// 默认失败关键词。
pub const DEFAULT_FAILURE_KEYWORDS: &[&str] = &[
    "error",
    "failed",
    "panic",
    "traceback",
    "exception",
    "fatal",
    "compilation failed",
    "test failed",
];

impl RuleVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加自定义失败关键词。
    pub fn add_failure_keyword(&mut self, keyword: impl Into<String>) {
        self.failure_keywords.push(keyword.into());
    }

    /// 获取所有失败关键词(默认 + 自定义)。
    fn all_keywords(&self) -> Vec<&str> {
        let mut all: Vec<&str> = DEFAULT_FAILURE_KEYWORDS.to_vec();
        all.extend(self.failure_keywords.iter().map(String::as_str));
        all
    }

    /// 验证 tool_result — 启发式关键词检测。
    ///
    /// 规则(按优先级):
    /// 1. 若 acceptance_criteria 包含 "test" 且 tool_result 包含 "passed" → 通过
    ///    (显式通过信号优先于失败关键词,避免 "10 tests passed, 0 failed" 误报)
    /// 2. 若 tool_result 包含任何失败关键词(大小写不敏感)且不包含 "passed" → 失败
    /// 3. 否则 → 通过(保守策略,避免误报)
    #[must_use]
    pub fn verify(&self, tool_result: &str, acceptance_criteria: &str) -> RuleVerdict {
        let result_lower = tool_result.to_ascii_lowercase();
        let criteria_lower = acceptance_criteria.to_ascii_lowercase();

        // 优先检测显式通过信号(避免 "10 tests passed, 0 failed" 误报)
        if criteria_lower.contains("test") && result_lower.contains("passed") {
            return RuleVerdict {
                passed: true,
                detail: "tests passed (keyword 'passed' detected)".to_owned(),
                remediation: None,
            };
        }

        // 检测失败关键词
        let matched_keyword = self
            .all_keywords()
            .into_iter()
            .find(|kw| result_lower.contains(*kw));

        if let Some(keyword) = matched_keyword {
            return RuleVerdict {
                passed: false,
                detail: format!("detected failure keyword '{keyword}' in tool result"),
                remediation: Some(format!(
                    "address the '{keyword}' issue and retry; check the tool output for details"
                )),
            };
        }

        // 保守策略:无失败关键词 → 通过
        RuleVerdict {
            passed: true,
            detail: "no failure keywords detected in tool result".to_owned(),
            remediation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_error_keyword_returns_failure() {
        let verifier = RuleVerifier::new();
        let verdict = verifier.verify("error: compilation failed", "must compile");
        assert!(!verdict.passed);
        assert!(verdict.detail.contains("error"));
        assert!(verdict.remediation.is_some());
    }

    #[test]
    fn detect_panic_keyword_returns_failure() {
        let verifier = RuleVerifier::new();
        let verdict = verifier.verify("thread 'main' panicked at src/main.rs:42", "must not panic");
        assert!(!verdict.passed);
    }

    #[test]
    fn no_failure_keywords_returns_pass() {
        let verifier = RuleVerifier::new();
        let verdict = verifier.verify("all checks completed", "must complete");
        assert!(verdict.passed);
    }

    #[test]
    fn test_passed_signal_detected() {
        let verifier = RuleVerifier::new();
        let verdict = verifier.verify("10 tests passed, 0 failed", "all tests must pass");
        assert!(verdict.passed);
        assert!(verdict.detail.contains("passed"));
    }

    #[test]
    fn custom_failure_keyword_added() {
        let mut verifier = RuleVerifier::new();
        verifier.add_failure_keyword("custom_error");
        let verdict = verifier.verify("custom_error: something went wrong", "must work");
        assert!(!verdict.passed);
    }

    #[test]
    fn keyword_matching_is_case_insensitive() {
        let verifier = RuleVerifier::new();
        let verdict = verifier.verify("ERROR: something failed", "must work");
        assert!(!verdict.passed);
    }

    #[test]
    fn empty_tool_result_returns_pass() {
        let verifier = RuleVerifier::new();
        let verdict = verifier.verify("", "must do something");
        assert!(verdict.passed);
    }
}
