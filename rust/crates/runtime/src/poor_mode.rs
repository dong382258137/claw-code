//! Tier S #3 穷鬼模式（Poor Mode）。
//!
//! 进程级全局开关，用于跳过消耗 token 但非核心的辅助特性。当前覆盖：
//! - `conversation.rs` turn-end nudge（基于规则的 memory 提取）
//! - 后续可扩展到 prompt_suggestion / auto_dream / extract_memories 等点。
//!
//! 设计原则：
//! - **纯运行时状态**：`AtomicBool` 全局变量，进程级单例，无需 `Send` 句柄。
//! - **启动加载**：`LiveCli` 启动时从 `RuntimeFeatureConfig.poor_mode()` 读取
//!   settings.json 的 `poorMode` 字段，写入全局开关。
//! - **运行时切换**：`/poor` 命令调用 `toggle()` / `set_active()` 立即生效，
//!   无需重启会话。切换不写回 settings.json，下次启动恢复 settings 值。
//! - **零侵入**：跳过点用 `if !is_active()` 包裹，不影响默认行为。
//!
//! 与 CCB（claude-code-best-source）穷鬼模式的差异：
//! - CCB 通过 settings 开关 + 10 处跳过点（extract_memories/auto_dream 等）实现。
//! - claw 当前没有 extract_memories LLM 调用、auto_dream 等特性，因此最小集只需
//!   跳过 nudge 即可；后续新增消耗性特性时在此模块的跳过点列表扩展。

use std::sync::atomic::{AtomicBool, Ordering};

/// 进程级穷鬼模式全局开关。默认关闭。
static POOR_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 返回穷鬼模式是否激活。调用方在跳过点用 `if !is_active()` 守卫。
#[must_use]
pub fn is_active() -> bool {
    POOR_MODE_ACTIVE.load(Ordering::Relaxed)
}

/// 设置穷鬼模式运行时状态。`/poor on|off` 命令调用此函数。
pub fn set_active(active: bool) {
    POOR_MODE_ACTIVE.store(active, Ordering::Relaxed);
}

/// 切换穷鬼模式运行时状态。`/poor` 无参数命令调用此函数，返回切换后的新状态。
pub fn toggle() -> bool {
    let previous = POOR_MODE_ACTIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(!current)
    });
    // fetch_update 返回 Ok(old) 表示成功，Err(old) 也表示当前值（不会发生因为闭包总返回 Some）。
    let old = previous.unwrap_or_else(|old| old);
    !old
}

/// 仅在穷鬼模式**未激活**时执行给定闭包。用于跳过点的简洁表达。
///
/// # 示例
/// ```ignore
/// poor_mode::unless_active(|| {
///     self.turns_since_last_nudge += 1;
///     if should_nudge(...) { ... }
/// });
/// ```
pub fn unless_active<F: FnOnce()>(callback: F) {
    if !is_active() {
        callback();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_inactive() {
        // 注意：全局状态在测试间共享，先重置为已知状态。
        set_active(false);
        assert!(!is_active());
    }

    #[test]
    fn set_and_clear() {
        set_active(true);
        assert!(is_active());
        set_active(false);
        assert!(!is_active());
    }

    #[test]
    fn toggle_flips_state() {
        set_active(false);
        let after = toggle();
        assert!(after);
        assert!(is_active());
        let after = toggle();
        assert!(!after);
        assert!(!is_active());
    }

    #[test]
    fn unless_active_runs_when_inactive() {
        set_active(false);
        let mut called = false;
        unless_active(|| called = true);
        assert!(called);
    }

    #[test]
    fn unless_active_skips_when_active() {
        set_active(true);
        let mut called = false;
        unless_active(|| called = true);
        assert!(!called);
        set_active(false);
    }
}
