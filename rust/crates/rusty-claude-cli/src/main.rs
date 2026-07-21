//! `claw` binary 入口:解析 CLI 参数,分派到对应的 CliAction。
//!
//! lib + bin 重构后(Step A4),所有共享代码(模块声明、根级定义、run() 等)
//! 移至 `src/lib.rs`。此文件只保留 `fn main()` 入口,通过 `rusty_claude_cli::run()`
//! 调用 lib crate 的逻辑。`claw-headless` binary(`src/bin/headless.rs`)以同样
//! 方式复用 lib crate,但走 ACP serve 路径而非完整 CLI 分派。

use rusty_claude_cli::{classify_error_kind, run, split_error_hint};

fn main() {
    // 诊断：注册 panic hook，落盘到 ~/.claw/claw-crash.log
    // 双击运行时 stderr 不可见，panic hook 是唯一能确认"是否 panic"的可靠信号。
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic>".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let claw_dir = home.join(".claw");
        let _ = std::fs::create_dir_all(&claw_dir);
        let crash_path = claw_dir.join("claw-crash.log");
        let _ = std::fs::write(
            &crash_path,
            format!(
                "PANIC at {location}\nMessage: {msg}\nTimestamp: {}\n",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ),
        );
        eprintln!("thread panicked at {location}: {msg}");
        eprintln!("Crash log: {}", crash_path.display());
    }));

    if let Err(error) = run() {
        let message = error.to_string();
        // When --output-format json is active, emit errors as JSON so downstream
        // tools can parse failures the same way they parse successes (ROADMAP #42).
        let argv: Vec<String> = std::env::args().collect();
        let json_output = argv
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "json")
            || argv.iter().any(|a| a == "--output-format=json");
        if json_output {
            // #77: classify error by prefix so downstream claws can route without
            // regex-scraping the prose. Split short-reason from hint-runbook.
            let kind = classify_error_kind(&message);
            let (short_reason, hint) = split_error_hint(&message);
            eprintln!(
                "{}",
                serde_json::json!({
                    "type": "error",
                    "error": short_reason,
                    "kind": kind,
                    "hint": hint,
                    "exit_code": 1,
                })
            );
        } else {
            // #156: Add machine-readable error kind to text output so stderr observers
            // don't need to regex-scrape the prose.
            let kind = classify_error_kind(&message);
            if message.contains("`claw --help`") {
                eprintln!(
                    "[error-kind: {kind}]
error: {message}"
                );
            } else {
                eprintln!(
                    "[error-kind: {kind}]
error: {message}

Run `claw --help` for usage."
                );
            }
        }
        std::process::exit(1);
    }
}
