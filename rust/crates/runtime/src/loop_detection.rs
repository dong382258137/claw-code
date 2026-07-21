//! LoopDetectionMiddleware — Step 2.2 打断 Doom Loop(同文件 10+ 次编辑)。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 2.2
//!
//! 架构:
//! - [`LoopDetector`][]:跟踪每个文件的编辑次数,在阈值处触发注入上下文或中止。
//! - [`LoopAction`]:中间件输出 — Continue / InjectContext / Abort。
//! - 与 [`RecoveryOrchestrator`](crate::recovery_orchestrator) 对接:Abort
//!   走 `WorkerFailureKind::Protocol` 恢复路径。
//!
//! 阈值:
//! - 5 次同文件编辑 → `InjectContext("consider reconsidering your approach")`
//! - 10 次同文件编辑 → `Abort("doom loop detected")`
//!
//! 经验法则:
//! - MCP Tools ≤ 80, Skills ≤ 15,注册时校验。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 同文件编辑触发警告的阈值(注入上下文,建议重新考虑方法)。
pub const WARN_THRESHOLD: u32 = 5;

/// 同文件编辑触发中止的阈值(Doom Loop 检测)。
pub const ABORT_THRESHOLD: u32 = 10;

/// MCP Tools 注册数量上限(经验法则)。
pub const MCP_TOOLS_MAX: usize = 80;

/// Skills 注册数量上限(经验法则)。
pub const SKILLS_MAX: usize = 15;

/// 中间件输出 — LoopDetector 每次记录编辑后返回的动作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopAction {
    /// 正常继续,无需干预。
    Continue,
    /// 编辑次数达到警告阈值,注入上下文建议重新考虑方法。
    InjectContext(String),
    /// 编辑次数达到中止阈值,检测到 Doom Loop,必须中止当前 turn。
    Abort(String),
}

/// 跟踪每个文件的编辑次数,在阈值处触发干预。
///
/// # Example
/// ```
/// use runtime::loop_detection::{LoopDetector, LoopAction, WARN_THRESHOLD, ABORT_THRESHOLD};
///
/// let mut detector = LoopDetector::new();
/// // 低于 WARN_THRESHOLD → Continue
/// for _ in 0..WARN_THRESHOLD - 1 {
///     assert!(matches!(detector.record_edit("src/main.rs"), LoopAction::Continue));
/// }
/// // 达到 WARN_THRESHOLD → InjectContext
/// assert!(matches!(detector.record_edit("src/main.rs"), LoopAction::InjectContext(_)));
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopDetector {
    /// 每个文件路径的编辑计数。
    edit_counts: HashMap<String, u32>,
    /// 累计总编辑次数(跨所有文件)。
    total_edits: u64,
    /// 是否已经对该文件发出过警告(避免重复注入)。
    warned: HashMap<String, bool>,
}

impl LoopDetector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次文件编辑,返回应采取的动作。
    ///
    /// - 低于 `WARN_THRESHOLD` → `Continue`
    /// - 等于 `WARN_THRESHOLD` → `InjectContext("consider reconsidering your approach")`
    /// - 大于 `WARN_THRESHOLD` 且低于 `ABORT_THRESHOLD` → `Continue`(已警告)
    /// - 等于或超过 `ABORT_THRESHOLD` → `Abort("doom loop detected: <file> edited <n> times")`
    #[must_use]
    pub fn record_edit(&mut self, file_path: &str) -> LoopAction {
        let count = self.edit_counts.entry(file_path.to_owned()).or_insert(0);
        *count += 1;
        self.total_edits += 1;

        if *count >= ABORT_THRESHOLD {
            LoopAction::Abort(format!(
                "doom loop detected: {file_path} edited {count} times"
            ))
        } else if *count == WARN_THRESHOLD && !self.warned.get(file_path).copied().unwrap_or(false)
        {
            self.warned.insert(file_path.to_owned(), true);
            LoopAction::InjectContext(
                "consider reconsidering your approach — this file has been edited many times"
                    .to_owned(),
            )
        } else {
            LoopAction::Continue
        }
    }

    /// 重置所有跟踪状态(新 turn 开始时调用)。
    pub fn reset(&mut self) {
        self.edit_counts.clear();
        self.warned.clear();
        self.total_edits = 0;
    }

    /// 获取指定文件的当前编辑次数。
    #[must_use]
    pub fn edit_count(&self, file_path: &str) -> u32 {
        self.edit_counts.get(file_path).copied().unwrap_or(0)
    }

    /// 获取累计总编辑次数。
    #[must_use]
    pub fn total_edits(&self) -> u64 {
        self.total_edits
    }

    /// 校验 MCP Tools 注册数量是否在经验法则范围内。
    ///
    /// 返回 `Ok(())` 如果在范围内,否则返回 `Err` 含超量信息。
    pub fn validate_mcp_tools_count(count: usize) -> Result<(), String> {
        if count <= MCP_TOOLS_MAX {
            Ok(())
        } else {
            Err(format!(
                "MCP tools count {count} exceeds recommended maximum {MCP_TOOLS_MAX}"
            ))
        }
    }

    /// 校验 Skills 注册数量是否在经验法则范围内。
    pub fn validate_skills_count(count: usize) -> Result<(), String> {
        if count <= SKILLS_MAX {
            Ok(())
        } else {
            Err(format!(
                "Skills count {count} exceeds recommended maximum {SKILLS_MAX}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_edit_returns_continue_below_warn_threshold() {
        let mut detector = LoopDetector::new();
        for i in 0..WARN_THRESHOLD - 1 {
            let action = detector.record_edit("src/main.rs");
            assert!(
                matches!(action, LoopAction::Continue),
                "expected Continue at edit {i}, got {action:?}"
            );
        }
    }

    #[test]
    fn record_edit_returns_inject_context_at_warn_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..WARN_THRESHOLD - 1 {
            let _ = detector.record_edit("src/main.rs");
        }
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::InjectContext(_)));
        if let LoopAction::InjectContext(msg) = action {
            assert!(msg.contains("reconsidering"));
        }
    }

    #[test]
    fn record_edit_returns_abort_at_abort_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..ABORT_THRESHOLD - 1 {
            let _ = detector.record_edit("src/main.rs");
        }
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::Abort(_)));
        if let LoopAction::Abort(msg) = action {
            assert!(msg.contains("doom loop detected"));
            assert!(msg.contains("src/main.rs"));
        }
    }

    #[test]
    fn different_files_tracked_independently() {
        let mut detector = LoopDetector::new();
        for _ in 0..WARN_THRESHOLD {
            let _ = detector.record_edit("src/a.rs");
        }
        // a.rs 已达 WARN,但 b.rs 仍 Continue
        let action = detector.record_edit("src/b.rs");
        assert!(matches!(action, LoopAction::Continue));
        assert_eq!(detector.edit_count("src/a.rs"), WARN_THRESHOLD);
        assert_eq!(detector.edit_count("src/b.rs"), 1);
    }

    #[test]
    fn reset_clears_counts() {
        let mut detector = LoopDetector::new();
        for _ in 0..WARN_THRESHOLD + 2 {
            let _ = detector.record_edit("src/main.rs");
        }
        detector.reset();
        assert_eq!(detector.edit_count("src/main.rs"), 0);
        assert_eq!(detector.total_edits(), 0);
        // After reset, edits should start fresh
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::Continue));
    }

    #[test]
    fn warn_only_injected_once_per_file() {
        let mut detector = LoopDetector::new();
        // Edit up to WARN_THRESHOLD → InjectContext
        for _ in 0..WARN_THRESHOLD {
            let _ = detector.record_edit("src/main.rs");
        }
        // Next edit (WARN+1) should be Continue, not another InjectContext
        let action = detector.record_edit("src/main.rs");
        assert!(
            matches!(action, LoopAction::Continue),
            "expected Continue after warn already emitted, got {action:?}"
        );
    }

    #[test]
    fn abort_triggers_on_every_edit_above_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..ABORT_THRESHOLD {
            let _ = detector.record_edit("src/main.rs");
        }
        // ABORT_THRESHOLD + 1 should also abort
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::Abort(_)));
    }

    #[test]
    fn total_edits_tracks_across_files() {
        let mut detector = LoopDetector::new();
        let _ = detector.record_edit("a.rs");
        let _ = detector.record_edit("b.rs");
        let _ = detector.record_edit("a.rs");
        assert_eq!(detector.total_edits(), 3);
    }

    #[test]
    fn validate_mcp_tools_count_within_limit() {
        assert!(LoopDetector::validate_mcp_tools_count(80).is_ok());
        assert!(LoopDetector::validate_mcp_tools_count(0).is_ok());
    }

    #[test]
    fn validate_mcp_tools_count_exceeds_limit() {
        assert!(LoopDetector::validate_mcp_tools_count(81).is_err());
    }

    #[test]
    fn validate_skills_count_within_limit() {
        assert!(LoopDetector::validate_skills_count(15).is_ok());
    }

    #[test]
    fn validate_skills_count_exceeds_limit() {
        assert!(LoopDetector::validate_skills_count(16).is_err());
    }
}
