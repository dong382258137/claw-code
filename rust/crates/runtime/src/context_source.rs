//! ContextSource — priority enum for context assembly ordering.
//!
//! Each variant represents a distinct category of context content with a
//! deterministic priority ordering (lower numeric value = higher priority).
//! System prompts always come first; user input always comes last.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ContextSource — priority enum
// ---------------------------------------------------------------------------

/// Represents a context source with a fixed priority ordering.
///
/// Lower numeric value = higher importance. The ordering is:
/// ```text
/// System(0) > Tools(1) > Memory(2) > Goal(3) > GitContext(4) > History(5) > User(6)
/// ```
///
/// System prompts and tool definitions are considered "stable" (rarely change),
/// while history and user input are considered "volatile" (change frequently).
/// This distinction is used by the cache break point computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// Core system prompt (highest priority, always included).
    System = 0,
    /// Tool definitions and schemas.
    Tools = 1,
    /// Semantic memory / long-term context.
    Memory = 2,
    /// Current goal / objective.
    Goal = 3,
    /// Git context (branch, diff, staged files).
    GitContext = 4,
    /// Conversation history (volatile).
    History = 5,
    /// Latest user input (lowest priority, truncated first).
    User = 6,
}

impl ContextSource {
    /// Returns all variants in priority order (ascending priority value).
    pub fn all_sorted() -> Vec<ContextSource> {
        vec![
            ContextSource::System,
            ContextSource::Tools,
            ContextSource::Memory,
            ContextSource::Goal,
            ContextSource::GitContext,
            ContextSource::History,
            ContextSource::User,
        ]
    }

    /// Returns the numeric priority (lower = higher importance).
    pub fn priority(&self) -> u8 {
        *self as u8
    }

    /// Returns `true` if this source is considered **stable** (rarely changes
    /// between turns). Stable sources precede the cache break point.
    ///
    /// Stable: System, Tools, Memory, Goal, GitContext
    /// Volatile: History, User
    pub fn is_stable(&self) -> bool {
        matches!(
            self,
            ContextSource::System
                | ContextSource::Tools
                | ContextSource::Memory
                | ContextSource::Goal
                | ContextSource::GitContext
        )
    }

    /// Returns `true` if this source is considered **volatile** (changes
    /// frequently between turns). Volatile sources follow the cache break point.
    ///
    /// Volatile: History, User
    pub fn is_volatile(&self) -> bool {
        !self.is_stable()
    }

    /// Returns a human-readable label for the source.
    pub fn label(&self) -> &'static str {
        match self {
            ContextSource::System => "system",
            ContextSource::Tools => "tools",
            ContextSource::Memory => "memory",
            ContextSource::Goal => "goal",
            ContextSource::GitContext => "git_context",
            ContextSource::History => "history",
            ContextSource::User => "user",
        }
    }
}

impl std::fmt::Display for ContextSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_ordering() {
        assert!(ContextSource::System.priority() < ContextSource::Tools.priority());
        assert!(ContextSource::Tools.priority() < ContextSource::Memory.priority());
        assert!(ContextSource::Memory.priority() < ContextSource::Goal.priority());
        assert!(ContextSource::Goal.priority() < ContextSource::GitContext.priority());
        assert!(
            ContextSource::GitContext.priority() < ContextSource::History.priority(),
            "GitContext should have higher priority than History"
        );
        assert!(
            ContextSource::History.priority() < ContextSource::User.priority(),
            "History should have higher priority than User"
        );
    }

    #[test]
    fn test_all_sorted_returns_seven_variants() {
        let all = ContextSource::all_sorted();
        assert_eq!(all.len(), 7);
    }

    #[test]
    fn test_stable_sources() {
        assert!(ContextSource::System.is_stable());
        assert!(ContextSource::Tools.is_stable());
        assert!(ContextSource::Memory.is_stable());
        assert!(ContextSource::Goal.is_stable());
        assert!(ContextSource::GitContext.is_stable());
        assert!(!ContextSource::History.is_stable());
        assert!(!ContextSource::User.is_stable());
    }

    #[test]
    fn test_volatile_sources() {
        assert!(!ContextSource::System.is_volatile());
        assert!(ContextSource::History.is_volatile());
        assert!(ContextSource::User.is_volatile());
    }

    #[test]
    fn test_label() {
        assert_eq!(ContextSource::System.label(), "system");
        assert_eq!(ContextSource::GitContext.label(), "git_context");
        assert_eq!(ContextSource::User.label(), "user");
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", ContextSource::System), "system");
        assert_eq!(format!("{}", ContextSource::GitContext), "git_context");
    }

    #[test]
    fn test_serde_roundtrip() {
        for source in ContextSource::all_sorted() {
            let json = serde_json::to_string(&source).unwrap();
            let decoded: ContextSource = serde_json::from_str(&json).unwrap();
            assert_eq!(source, decoded, "serde roundtrip failed for {:?}", source);
        }
    }
}
