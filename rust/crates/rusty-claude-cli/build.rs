use std::env;
use std::process::Command;

fn main() {
    // Get git SHA (short hash)
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());

    println!("cargo:rustc-env=GIT_SHA={git_sha}");

    // TARGET is always set by Cargo during build
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET={target}");

    // Build date from SOURCE_DATE_EPOCH (reproducible builds) or current UTC date.
    // Intentionally ignoring time component to keep output deterministic within a day.
    let build_date = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .map(|_ts| {
            // Use SOURCE_DATE_EPOCH to derive date via chrono if available;
            // for simplicity we just use the env var as a signal and fall back
            // to build-time env. In practice CI sets this via workflow.
            std::env::var("BUILD_DATE").unwrap_or_else(|_| "unknown".to_string())
        })
        .or_else(|| std::env::var("BUILD_DATE").ok())
        .unwrap_or_else(|| {
            // Fall back to current date via `date` command
            Command::new("date")
                .args(["+%Y-%m-%d"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout).ok()
                    } else {
                        None
                    }
                })
                .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string())
        });
    println!("cargo:rustc-env=BUILD_DATE={build_date}");

    // 修复:原本使用 `cargo:rerun-if-changed=.git/HEAD` 和 `.git/refs`,
    // 但这两个路径相对于包目录(`crates/rusty-claude-cli`),而 git 仓库根
    // 在更上层目录(`d:\claw-code-src\.git`),包目录下根本不存在 `.git/`,
    // Cargo 认为"这些文件从未变化" → 永远复用缓存的 build script output
    // → GIT_SHA 一直是首次构建时的值。
    //
    // 修复方案:不输出 rerun-if-changed,让 Cargo 在每次构建时都重新运行
    // build.rs(默认行为)。代价是每次构建多花几十毫秒执行 `git rev-parse`,
    // 但确保 GIT_SHA 始终与当前 HEAD 一致。
}
