//! 集成测试公共模块 — 提供环境隔离和辅助函数。
//!
//! 所有集成测试应使用 `isolate()` 防止本地 `~/.claw/` 配置污染测试结果。
//!
//! 用法：
//! ```rust,ignore
//! mod common;
//!
//! #[test]
//! fn my_test() {
//!     let _lock = common::isolate();
//!     // ... 测试逻辑
//! }
//! ```

use plugins::test_isolation::EnvLock;

/// 获取环境隔离锁，将 HOME / XDG 重定向到临时目录。
///
/// 返回的 `EnvLock` 必须保持在测试期间存活（Drop 时自动清理临时目录）。
#[must_use]
pub fn isolate() -> EnvLock {
    EnvLock::lock()
}
