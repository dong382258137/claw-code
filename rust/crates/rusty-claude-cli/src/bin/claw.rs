//! `claw` binary 入口（thin wrapper）。
//!
//! 与 `claw-plus` binary (`src/main.rs`) 共享 `rusty_claude_cli::main_entry()`。
//! 这是推荐的入口名；`claw-plus` 仅为向后兼容保留。

fn main() {
    rusty_claude_cli::main_entry();
}
