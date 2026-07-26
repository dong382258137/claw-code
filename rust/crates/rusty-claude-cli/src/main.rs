//! `claw-plus` binary 入口（thin wrapper）。
//!
//! 实际入口逻辑在 `rusty_claude_cli::main_entry()`，与 `claw` binary
//! (`src/bin/claw.rs`) 共享。保留此 bin 名为 `claw-plus` 仅用于向后兼容；
//! 新用户和新脚本应直接使用 `claw` 命令。

fn main() {
    rusty_claude_cli::main_entry();
}
