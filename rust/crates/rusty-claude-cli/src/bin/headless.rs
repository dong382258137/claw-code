//! `claw-headless` binary:极简 stdio ACP 服务器入口。
//!
//! 与 `claw acp serve` 子命令功能相同,但作为独立 binary 提供,
//! 方便编辑器(Zed 等)直接 spawn。不支持 REPL/TUI/其他 CLI 子命令。
//!
//! # 用法
//! - `claw-headless` — 用默认 model 和 permission_mode 启动
//! - `claw-headless --model <name>` — 指定 model
//! - `claw-headless --permission-mode <mode>` — 指定 permission mode
//! - `claw-headless --model <name> --permission-mode <mode>` — 两者都指定
//!
//! # 设计说明
//! lib + bin 重构(Step A4)后,此 binary 与 `claw` binary 共享 `rusty_claude_cli`
//! lib crate 的全部代码(`run_acp_serve`、`AnthropicRuntimeClient` 等),但入口
//! 极简:只解析 `--model` / `--permission-mode`,直接进入 stdio ACP 服务器循环。
//! 不经过 `CliAction` 分派,不加载 REPL/TUI 相关逻辑。

use rusty_claude_cli::{
    app::run_acp_serve, default_permission_mode, parse_permission_mode_arg, DEFAULT_MODEL,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut model = DEFAULT_MODEL.to_string();
    let mut permission_mode = default_permission_mode();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --model requires a value");
                    std::process::exit(1);
                }
                model = args[i].clone();
            }
            "--permission-mode" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --permission-mode requires a value");
                    std::process::exit(1);
                }
                match parse_permission_mode_arg(&args[i]) {
                    Ok(mode) => permission_mode = mode,
                    Err(e) => {
                        eprintln!("error: invalid permission mode: {e}");
                        std::process::exit(1);
                    }
                }
            }
            "--help" | "-h" => {
                println!("claw-headless — stdio ACP server");
                println!();
                println!("Usage: claw-headless [OPTIONS]");
                println!();
                println!("Options:");
                println!("  --model <name>              Model to use (default: {DEFAULT_MODEL})");
                println!("  --permission-mode <mode>    Permission mode (read-only, workspace-write, danger-full-access)");
                println!("  -h, --help                  Print this help message");
                println!();
                println!("The server speaks newline-delimited JSON-RPC over stdin/stdout.");
                println!("Connect from ACP-compatible editors (Zed, VS Code extensions, etc.)");
                println!("by spawning this binary as the agent process.");
                return;
            }
            other => {
                eprintln!("error: unknown argument: {other}");
                eprintln!("Use --help for usage.");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if let Err(error) = run_acp_serve(model, permission_mode) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
