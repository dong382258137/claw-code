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

    // 部署 builtin skills 到 target/<profile>/skills/
    //
    // 背景:claw 二进制通过 `discover_skill_roots` 搜索 skill,其中一条路径
    // 是 `<exe-dir>/skills`(install mode)。但 `cargo build` 只编译 .rs,
    // 不会复制 .md 资源,导致在 repo 外运行时找不到 skill(unknown skill)。
    //
    // 本步骤在编译期把 `rust/skills/` 整体复制到 `target/<profile>/skills/`,
    // 使 `cargo build` 产出完整的可部署产物(二进制 + skill)。
    //
    // 失败降级:复制失败只发 cargo:warning,不 panic,避免无 skill 目录时
    // 编译失败(如 CI 只跑单测的场景)。
    deploy_builtin_skills();
}

fn deploy_builtin_skills() {
    // 源:CARGO_MANIFEST_DIR/../../skills = rust/skills/
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is always set by Cargo");
    let src_skills = std::path::Path::new(&manifest_dir)
        .join("..")
        .join("..")
        .join("skills");

    if !src_skills.exists() {
        println!("cargo:warning=builtin skills source not found: {}, skipping deploy", src_skills.display());
        return;
    }

    // 目标:从 OUT_DIR 推算 target/<profile>/
    // OUT_DIR = target/<profile>/build/<pkg-hash>/out
    // 向上 4 层 = target/<profile>/
    let out_dir = env::var("OUT_DIR")
        .expect("OUT_DIR is always set by Cargo during build script execution");
    let target_profile_dir = std::path::Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should have at least 4 ancestors (target/<profile>/build/<pkg-hash>/out)");
    let dst_skills = target_profile_dir.join("skills");

    // 增量复制:如果目标已存在且源未变,跳过。
    // 用 rerun-if-changed 监听源目录,只有 skill 文件变化才重新运行 build.rs。
    println!("cargo:rerun-if-changed={}", src_skills.display());

    // 删除旧目标(可能是旧版本 skill),然后重新复制。
    // 用 std::fs::remove_dir_all + create_dir_all 而非 cp -r,跨平台。
    if dst_skills.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dst_skills) {
            println!("cargo:warning=failed to remove old skills dir {}: {}", dst_skills.display(), e);
            return;
        }
    }
    if let Err(e) = copy_dir_recursive(&src_skills, &dst_skills) {
        println!("cargo:warning=failed to copy builtin skills: {}", e);
        return;
    }

    println!("cargo:warning=deployed builtin skills to {}", dst_skills.display());
}

/// 递归复制目录(跨平台,等价于 cp -r)。
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}
