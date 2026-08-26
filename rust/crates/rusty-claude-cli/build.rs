use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // 声明 build script 输入(rerun-if-changed)。必须在其它指令之前输出且
    // 集合保持稳定:Cargo 一旦收到 rerun-if-changed 就认为 build script 的
    // 输入只有这些路径,集合遗漏任何影响输出的输入都会导致对应输出停留在
    // 旧构建(见 emit_rerun_if_changed 的修复背景)。
    emit_rerun_if_changed();

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
            // 跨平台当前日期:chrono(Windows 下 `date +%Y-%m-%d` 是 GNU 语法,
            // cmd.exe 的 date 不识别,导致 build_date 恒为 unknown)。
            // SOURCE_DATE_EPOCH 存在时上游 env 已处理,此处仅兜底。
            chrono::Local::now()
                .format("%Y-%m-%d")
                .to_string()
        });
    println!("cargo:rustc-env=BUILD_DATE={build_date}");

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

/// 声明 build script 的输入路径,使 git HEAD/分支变化与 skills 源变化都能
/// 触发 build.rs 重跑,从而 GIT_SHA 与 skills 部署始终与最新状态一致。
///
/// 修复背景:此前不输出 rerun-if-changed,依赖 Cargo"无指令则每次重跑"的
/// 默认行为——该假设仅对 clean build 成立;增量构建 / `cargo install` 复用
/// fingerprint 时(git commit 不改变源码 mtime)build.rs 不会重跑,GIT_SHA
/// 停留在首次构建时的值(曾导致部署的二进制标注旧 SHA)。改为显式声明输入:
///
/// - <gitdir>/HEAD: detached HEAD 时内容即 SHA,commit 更新其 mtime;
/// - <commondir>/refs/: loose ref(如 refs/heads/main)每次 commit 更新
///   mtime,Cargo 对目录递归跟踪;不硬编码分支名,以免分支切换/改名导致集合失效;
/// - <commondir>/packed-refs: 分支被打包(gc/浅克隆)时 commit 更新该文件;
///   文件不存在时按 Cargo 语义视为"始终变化"→ 每次重跑,是安全的兜底
///   (代价仅几十毫秒);
/// - <skills>/: 源 skill 变化时重新部署到 target/<profile>/skills。
///
/// 找不到 git 目录(crates.io 打包源码)时不输出 git 相关指令,Cargo 回退到
/// 每次重跑,行为等同修复前,安全。
fn emit_rerun_if_changed() {
    if let Some(git_dir) = find_git_dir() {
        let common_dir = find_common_dir(&git_dir);
        println!("cargo::rerun-if-changed={}", git_dir.join("HEAD").display());
        println!(
            "cargo::rerun-if-changed={}",
            common_dir.join("refs").display()
        );
        println!(
            "cargo::rerun-if-changed={}",
            common_dir.join("packed-refs").display()
        );
    }
    let skills_dir = skills_source_dir();
    if skills_dir.exists() {
        println!("cargo::rerun-if-changed={}", skills_dir.display());
    }
}

/// 从 CARGO_MANIFEST_DIR 向上定位 git 仓库根的 .git 条目。
/// 普通仓库是目录;worktree/submodule 是文件,内容为 `gitdir: <path>`。
/// 只接受含 HEAD 文件的有效 gitdir:残留的空 `.git` 目录(误操作 git init
/// 遗留,如 crates/rusty-claude-cli/.git)会被跳过,继续向上找真正的仓库根。
fn find_git_dir() -> Option<PathBuf> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").ok()?;
    let mut dir = PathBuf::from(manifest_dir);
    loop {
        let git_entry = dir.join(".git");
        if let Some(gitdir) = resolve_git_entry(&git_entry, &dir) {
            return Some(gitdir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// 解析单个 .git 条目为有效 gitdir(含 HEAD 文件):
/// - 目录形态:须含 HEAD(空残留 .git 目录视为无效,返回 None);
/// - 文件形态(worktree/submodule):内容 `gitdir: <path>`,目标须含 HEAD。
fn resolve_git_entry(git_entry: &Path, parent: &Path) -> Option<PathBuf> {
    if git_entry.is_dir() {
        return git_entry
            .join("HEAD")
            .is_file()
            .then_some(git_entry.to_path_buf());
    }
    if git_entry.is_file() {
        let content = std::fs::read_to_string(git_entry).ok()?;
        let line = content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))?;
        let p = PathBuf::from(line);
        let gitdir = if p.is_absolute() { p } else { parent.join(p) };
        return gitdir.join("HEAD").is_file().then_some(gitdir);
    }
    None
}

/// worktree 场景:gitdir 下 commondir 文件指向公共 gitdir(refs/ 的权威位置);
/// 普通仓库无 commondir,gitdir 即公共 gitdir。
fn find_common_dir(git_dir: &Path) -> PathBuf {
    let commondir = git_dir.join("commondir");
    if let Ok(content) = std::fs::read_to_string(&commondir) {
        let line = content.trim();
        if !line.is_empty() {
            let p = PathBuf::from(line);
            return if p.is_absolute() { p } else { git_dir.join(p) };
        }
    }
    git_dir.to_path_buf()
}

/// rust/skills/ 源目录(与 deploy_builtin_skills 的部署源一致)。
fn skills_source_dir() -> PathBuf {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by Cargo");
    PathBuf::from(manifest_dir)
        .join("..")
        .join("..")
        .join("skills")
}

fn deploy_builtin_skills() {
    let src_skills = skills_source_dir();

    if !src_skills.exists() {
        println!(
            "cargo:warning=builtin skills source not found: {}, skipping deploy",
            src_skills.display()
        );
        return;
    }

    // 目标:从 OUT_DIR 推算 target/<profile>/
    // OUT_DIR = target/<profile>/build/<pkg-hash>/out
    // 向上 4 层 = target/<profile>/
    let out_dir =
        env::var("OUT_DIR").expect("OUT_DIR is always set by Cargo during build script execution");
    let target_profile_dir = std::path::Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should have at least 4 ancestors (target/<profile>/build/<pkg-hash>/out)");
    let dst_skills = target_profile_dir.join("skills");
    // skills 源目录的变化已通过 emit_rerun_if_changed() 中的
    // `rerun-if-changed=<skills>/` 声明触发 build.rs 重跑,这里不再重复输出。

    // 删除旧目标(可能是旧版本 skill),然后重新复制。
    // 用 std::fs::remove_dir_all + create_dir_all 而非 cp -r,跨平台。
    if dst_skills.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dst_skills) {
            println!(
                "cargo:warning=failed to remove old skills dir {}: {}",
                dst_skills.display(),
                e
            );
            return;
        }
    }
    if let Err(e) = copy_dir_recursive(&src_skills, &dst_skills) {
        println!("cargo:warning=failed to copy builtin skills: {}", e);
        return;
    }

    println!(
        "cargo:warning=deployed builtin skills to {}",
        dst_skills.display()
    );
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
