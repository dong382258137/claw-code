//! `claw` binary 入口（thin wrapper）。
//!
//! 实际入口逻辑在 `claw_server::main_entry()`；`claw-plus` 兼容入口已移除，
//! 需要一个二进制同时服务命令行与桌面快捷方式。

fn main() {
    claw_server::main_entry();
}
