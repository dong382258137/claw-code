//! 视觉反馈验证器 — Playwright 截图对比(placeholder)。
//!
//! 当前实现:返回 skipped(未集成 Playwright)。
//! 未来扩展:启动 Playwright headless 浏览器,截图,与 baseline 图像做像素差异对比。

use serde::{Deserialize, Serialize};

/// 视觉验证结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualVerdict {
    /// 是否通过。
    pub passed: bool,
    /// 详细说明。
    pub detail: String,
    /// 失败时的修正建议。
    pub remediation: Option<String>,
}

/// 视觉验证器 — Playwright 截图对比(当前 placeholder)。
#[derive(Debug, Clone, Default)]
pub struct VisualVerifier {
    /// 是否启用 Playwright(当前总是 false,待集成)。
    enabled: bool,
    /// baseline 截图目录(未来使用)。
    baseline_dir: Option<std::path::PathBuf>,
}

impl VisualVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 启用 Playwright 验证(当前 placeholder,实际未集成)。
    pub fn enable(&mut self, baseline_dir: impl Into<std::path::PathBuf>) {
        self.enabled = true;
        self.baseline_dir = Some(baseline_dir.into());
    }

    /// 验证 tool_result — 当前返回 skipped。
    ///
    /// Placeholder 行为:
    /// - 若未启用 → 返回 skipped(passed=false, detail 说明未集成)
    /// - 若已启用但无 Playwright → 返回 skipped(待集成)
    #[must_use]
    pub fn verify(&self, tool_result: &str, acceptance_criteria: &str) -> VisualVerdict {
        if !self.enabled {
            return VisualVerdict {
                passed: false,
                detail: "visual verification skipped — Playwright not enabled".to_owned(),
                remediation: None,
            };
        }
        // 已启用但 Playwright 集成待实现
        let _ = (tool_result, acceptance_criteria);
        VisualVerdict {
            passed: false,
            detail: "visual verification skipped — Playwright integration pending (Step 4.x)"
                .to_owned(),
            remediation: Some(
                "integrate Playwright headless browser to enable screenshot comparison".to_owned(),
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
    fn disabled_verifier_returns_skipped() {
        let verifier = VisualVerifier::new();
        let verdict = verifier.verify("screenshot.png", "UI matches baseline");
        assert!(!verdict.passed);
        assert!(verdict.detail.contains("skipped"));
        assert!(verdict.remediation.is_none());
    }

    #[test]
    fn enabled_verifier_returns_pending_integration() {
        let mut verifier = VisualVerifier::new();
        verifier.enable("/tmp/baselines");
        let verdict = verifier.verify("screenshot.png", "UI matches baseline");
        assert!(!verdict.passed);
        assert!(verdict.detail.contains("pending"));
        assert!(verdict.remediation.is_some());
    }

    #[test]
    fn enable_sets_baseline_dir() {
        let mut verifier = VisualVerifier::new();
        assert!(!verifier.is_enabled());
        verifier.enable("/tmp/baselines");
        assert!(verifier.is_enabled());
    }
}
