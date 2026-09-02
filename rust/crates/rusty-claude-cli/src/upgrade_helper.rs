//! 升级助手(任务 2:热升级一键化,无感衔接)。
//!
//! 由 `/upgrade` 命令 spawn 的**独立进程**(自带新控制台,编译输出可见)。
//! 自动化"备份 exe → cargo build → 启动新进程"三步,使用户无需手动介入。
//!
//! # 入口
//!
//! `claw-plus --upgrade-helper <user_cwd>` — 内部参数,在 `main_entry` 顶部
//! 检测并分派(不进 REPL/TUI)。
//!
//! # 流程
//!
//! 1. 轮询 `<user_cwd>/.claw/upgrade-exited.json` 出现(旧进程已同意退出、
//!    状态已落盘)。
//! 2. `mv` 当前 exe → `exe.old`(Windows 允许重命名运行中的文件,旧进程
//!    即使尚未完全退出也不冲突)。
//! 3. `cargo build -p rusty-claude-cli`(workspace root 执行;输出显示在本
//!    新控制台)。
//! 4. 启动新 exe(`current_dir = user_cwd` → 自动检测 upgrade-request.json
//!    → resume session + restore plan + 继续任务,无感衔接)。
//! 5. 清理 exited 标记,本助手退出。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::session_mgr::{
    clear_upgrade_exited, read_upgrade_exited, read_upgrade_request, UPGRADE_EXITED_REL,
};

/// 轮询"旧进程已退出"标记的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 等待旧进程退出标记的超时(旧进程需 persist 会话 + shutdown,给足 180s)。
const WAIT_TIMEOUT: Duration = Duration::from_secs(180);

/// 升级助手日志路径(`<user_cwd>/.claw/upgrade-helper.log`)。
///
/// helper 在独立新终端运行,stdout/stderr 随窗口关闭即丢失 —— 失败原因
/// 事后无法定位(2026-09-03 实测)。关键输出(含错误)追加写此文件,
/// 与终端输出同步,供 `Get-Content .claw/upgrade-helper.log` 诊断。
#[must_use]
pub(crate) fn upgrade_log_path(user_cwd: &Path) -> PathBuf {
    user_cwd.join(".claw").join("upgrade-helper.log")
}

/// 打开升级日志(追加模式)。失败静默(日志是诊断辅助,不阻塞升级)。
fn open_upgrade_log(user_cwd: &Path) -> Option<File> {
    let path = upgrade_log_path(user_cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// 同时输出到终端与升级日志。
fn log_out(log_file: &Option<File>, msg: &str) {
    println!("{msg}");
    if let Some(file) = log_file {
        let mut file = file;
        let _ = writeln!(file, "{msg}");
    }
}

/// 截断超长文本(日志可读性)。
fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}\n…(截断)")
    } else {
        head
    }
}

/// 记录关键环境变量到日志(诊断编译失败时的环境差异)。
fn log_key_env(log_file: &Option<File>) {
    for var in [
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
        "MSYSTEM",
        "MSYSTEM_CARCH",
    ] {
        if let Ok(value) = std::env::var(var) {
            if !value.trim().is_empty() {
                log_out(log_file, &format!("[upgrade-helper] env {var}={}", truncate(&value, 300)));
            }
        }
    }
}

#[cfg(windows)]
/// CREATE_NEW_CONSOLE:让升级助手拥有独立控制台,编译输出对用户可见。
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// 由旧进程 `/upgrade` 调用:spawn 自身副本作为升级助手。
///
/// 用 `current_exe`(同一二进制)加 `--upgrade-helper <user_cwd>` 参数启动,
/// Windows 下带 `CREATE_NEW_CONSOLE`(决策点 ③:编译输出显示在新终端)。
/// 助手是独立进程,旧进程退出后继续存活执行迁移。
pub(crate) fn spawn(user_cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--upgrade-helper");
    cmd.arg(user_cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NEW_CONSOLE);
    }
    // spawn 后不 wait:助手独立运行,旧进程随即退出。
    cmd.spawn()?;
    Ok(())
}

/// 升级助手主入口(由 `main_entry` 在检测到 `--upgrade-helper` 时调用)。
pub(crate) fn run_helper(user_cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let log_file = open_upgrade_log(user_cwd);
    log_out(
        &log_file,
        &format!("[upgrade-helper] 升级助手启动: user_cwd={}", user_cwd.display()),
    );
    log_key_env(&log_file);

    // 1. 等旧进程写 exited 标记(校验 old_pid 归属)。
    //    注意:不能在启动时无条件 clear exited —— 那会与旧进程的
    //    write_upgrade_exited 竞态:旧进程若已写好标记(如 shutdown 很快),
    //    clear 会误删,本助手随后轮询永远等不到 → 卡满 180s 超时
    //    (2026-09-02 实测复现)。防残留改由 old_pid 校验完成:
    //    上次升级失败残留的 exited 携带旧 old_pid,与本次 request.old_pid
    //    不匹配,判定为残留并清除,不会误触发本次迁移。
    let expected_old_pid = read_upgrade_request(user_cwd).map(|req| req.old_pid);
    let started = Instant::now();
    loop {
        if let Some(exited) = read_upgrade_exited(user_cwd) {
            if exited_belongs(&exited, expected_old_pid) {
                break;
            }
            // 上次升级失败残留:清除并继续等本次标记。
            clear_upgrade_exited(user_cwd);
        }
        if started.elapsed() > WAIT_TIMEOUT {
            return Err(format!(
                "等待旧进程退出标记超时({}s): 未出现 {}",
                WAIT_TIMEOUT.as_secs(),
                user_cwd.join(UPGRADE_EXITED_REL).display()
            )
            .into());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    log_out(&log_file, "[upgrade-helper] 旧进程已退出,开始迁移");

    // 2. mv 当前 exe → exe.old(备份;Windows 允许重命名运行中的文件)。
    let exe = std::env::current_exe()?;
    let old_exe = backup_path(&exe);
    if old_exe.exists() {
        // 上次升级残留的 .old:先删除旧备份(它已不是本次运行的二进制)。
        std::fs::remove_file(&old_exe)?;
    }
    std::fs::rename(&exe, &old_exe)?;
    log_out(
        &log_file,
        &format!(
            "[upgrade-helper] 备份: {} → {}",
            exe.display(),
            old_exe.display()
        ),
    );

    // 3. cargo build。源码根优先从 user_cwd 推导 —— 部署版 exe 位于
    //    `.cargo/bin`,从 exe 上溯到用户主目录不含 Cargo.toml,推导必失败
    //    (2026-09-02 实测)。user_cwd 推导失败时回退 exe 推导(源码 target 布局)。
    let workspace_root = workspace_root_from_user_cwd(user_cwd)
        .or_else(|_| workspace_root_from_exe(&exe))
        .map_err(|err| {
            log_out(&log_file, &format!("[upgrade-helper] workspace 定位失败: {err}"));
            err
        })?;

    // 3. cargo build。产物路径必须与"当前 exe 运行场景"对齐:
    //    - exe 在 `target/debug`(源码 debug 布局) → 默认 build(debug),产物已在原位
    //    - exe 在 `target/release`(源码 release 布局) → `--release`,产物已在原位
    //    - 其余位置(`~/.cargo/bin` 部署版) → `--release`,产物在 target/release,
    //      需回拷到 exe 原路径(原文件已被 mv 成 .old,不复制则启动必然失败)。
    let in_target_debug = exe.starts_with(&workspace_root.join("target").join("debug"));
    let in_target_release = exe.starts_with(&workspace_root.join("target").join("release"));
    let use_release = !in_target_debug;
    let profile_dir = if use_release { "release" } else { "debug" };
    log_out(
        &log_file,
        &format!(
            "[upgrade-helper] 编译: cargo build {}{} @ {}",
            if use_release { "--release " } else { "" },
            "-p rusty-claude-cli",
            workspace_root.display()
        ),
    );
    let mut args = vec!["build".to_string()];
    if use_release {
        args.push("--release".to_string());
    }
    args.extend(["-p".to_string(), "rusty-claude-cli".to_string()]);
    // 用 .output() 而非 .status():捕获 cargo 的 stdout/stderr 并写入日志,
    // 否则编译错误只出现在独立新终端,窗口关闭即丢失,无法定位
    // (2026-09-03 实测:仅剩 exit=101,原因不可见)。
    let output = Command::new("cargo")
        .args(&args)
        .current_dir(&workspace_root)
        .output()
        .map_err(|err| {
            log_out(
                &log_file,
                &format!("[upgrade-helper] 启动 cargo 失败: {err}(cargo 是否在 PATH?)"),
            );
            err
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = format!(
            "[upgrade-helper] 编译失败(exit={:?})。旧二进制已备份为 {},可手动恢复。\n\
             --- cargo stderr ---\n{}\n--- cargo stdout ---\n{}",
            output.status.code(),
            old_exe.display(),
            truncate(&stderr, 4000),
            truncate(&stdout, 2000)
        );
        log_out(&log_file, &msg);
        eprintln!("{msg}");
        return Err("cargo build failed".into());
    }

    // 3b. 部署版(非源码 target 布局):把构建产物回拷到 exe 原路径。
    if !in_target_debug && !in_target_release {
        let exe_name = exe
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("无法获取 exe 文件名")?;
        let built = workspace_root.join("target").join(profile_dir).join(exe_name);
        if !built.is_file() {
            return Err(format!(
                "构建产物缺失: {}(预期 profile={})",
                built.display(),
                profile_dir
            )
            .into());
        }
        std::fs::copy(&built, &exe)?;
        log_out(
            &log_file,
            &format!(
                "[upgrade-helper] 已部署: {} ← {}",
                exe.display(),
                built.display()
            ),
        );
    }

    // 4. 启动新 exe(user_cwd 为工作目录 → 自动检测标记 → resume 会话)。
    log_out(
        &log_file,
        &format!("[upgrade-helper] 编译成功,启动新进程: {}", exe.display()),
    );
    let mut cmd = Command::new(&exe);
    cmd.current_dir(user_cwd);
    cmd.spawn()?;

    // 5. 清理 exited 标记(本次升级完成),助手退出。
    clear_upgrade_exited(user_cwd);
    log_out(&log_file, "[upgrade-helper] 完成,新进程已启动(无感衔接)。");
    Ok(())
}

/// 备份路径:`claw-plus.exe` → `claw-plus.exe.old`。
fn backup_path(exe: &Path) -> PathBuf {
    let mut os = exe.as_os_str().to_os_string();
    os.push(".old");
    PathBuf::from(os)
}

/// 判断 exited 标记是否属于本次升级(与 request 的 old_pid 匹配)。
///
/// - `expected_old_pid = None`(异常,无 request) → 保守信任 exited(返回 true)
/// - exited 无 old_pid 且期望有 → 不归属(返回 false,防上次失败残留误触发)
fn exited_belongs(exited: &serde_json::Value, expected_old_pid: Option<u32>) -> bool {
    let exited_pid = exited
        .get("old_pid")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    match expected_old_pid {
        Some(expected) => exited_pid == Some(expected),
        None => true,
    }
}

/// 从 exe 路径推导 workspace root(源码仓库根,含 Cargo.toml)。
///
/// 开发布局:`rust/target/debug/claw-plus.exe` → 上溯 3 层 = `rust/`。
/// 推导失败(target 布局非预期)时返回错误。
fn workspace_root_from_exe(exe: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = exe
        .parent() // debug
        .and_then(Path::parent) // target
        .and_then(Path::parent) // rust/ (workspace root)
        .ok_or_else(|| "无法从 exe 路径推导 workspace root".to_string())?;
    if !root.join("Cargo.toml").exists() {
        return Err(format!(
            "workspace root {} 不含 Cargo.toml(非源码布局?),无法自编译升级",
            root.display()
        )
        .into());
    }
    Ok(root.to_path_buf())
}

/// 判断目录是否为 `rusty-claude-cli` 的 cargo workspace。
///
/// 不能只用"含 Cargo.toml"判定 —— 仓库里可能有多个含 Cargo.toml 的目录
/// (如本项目 `D:\claw-code-src` 下有 `rust/` 与 `demo-monorepo/`),
/// `read_dir` 遍历顺序不定,误命中非目标目录会导致
/// `cargo build -p rusty-claude-cli` 报 "package not found"。
/// 可靠判定:该目录是 cargo workspace 且含 `crates/rusty-claude-cli` 包。
fn is_rusty_claude_workspace(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file()
        && dir
            .join("crates")
            .join("rusty-claude-cli")
            .join("Cargo.toml")
            .is_file()
}

/// 从用户工作目录推导 cargo workspace root。
///
/// 部署版 exe 位于 `~/.cargo/bin`,`workspace_root_from_exe` 对其必然失败
/// (上溯到用户主目录无 Cargo.toml)。因此 `/upgrade` 的自编译升级必须基于
/// **用户当前工作目录**定位源码仓库:
///
/// 1. 从 `user_cwd` 向上逐级找 `rusty-claude-cli` 的 workspace(在源码目录内运行)。
/// 2. 回退:探测 `user_cwd` 的直接子目录(覆盖"cwd=仓库根、workspace 在
///    `rust/` 子目录"的布局,如本项目 `D:\claw-code-src` → `D:\claw-code-src\rust`)。
/// 3. 仍失败则报错,提示需在源码仓库内运行 `/upgrade`。
fn workspace_root_from_user_cwd(
    user_cwd: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut cursor = Some(user_cwd);
    while let Some(dir) = cursor {
        if is_rusty_claude_workspace(dir) {
            return Ok(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    if let Ok(entries) = std::fs::read_dir(user_cwd) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && is_rusty_claude_workspace(&path) {
                return Ok(path);
            }
        }
    }
    Err(format!(
        "无法从工作目录 {} 定位 rusty-claude-cli 的 cargo workspace(未找到含 crates/rusty-claude-cli 的目录);请在源码仓库内运行 /upgrade",
        user_cwd.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_appends_old() {
        let exe = Path::new(r"C:\rust\target\debug\claw-plus.exe");
        assert_eq!(backup_path(exe), PathBuf::from(r"C:\rust\target\debug\claw-plus.exe.old"));
    }

    #[test]
    fn backup_path_preserves_extensionless_names() {
        let exe = Path::new("/usr/local/bin/claw-plus");
        assert_eq!(backup_path(exe), PathBuf::from("/usr/local/bin/claw-plus.old"));
    }

    #[test]
    fn workspace_root_from_exe_dev_layout() {
        // 模拟 rust/target/debug/claw-plus.exe,上溯 3 层 = rust/(含 Cargo.toml)。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path(); // 模拟 rust/
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let exe = root.join("target").join("debug").join("claw-plus.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"fake exe").unwrap();

        let derived = workspace_root_from_exe(&exe).expect("derive");
        assert_eq!(derived, root);
    }

    #[test]
    fn workspace_root_from_exe_rejects_missing_cargo_toml() {
        // target/debug/exe 上溯 3 层后无 Cargo.toml(如安装到 Program Files)→ 报错。
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("target").join("debug").join("claw-plus.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"fake exe").unwrap();

        let err = workspace_root_from_exe(&exe).expect_err("should reject non-source layout");
        assert!(err.to_string().contains("Cargo.toml"), "got: {err}");
    }

    #[test]
    fn workspace_root_from_exe_rejects_too_shallow() {
        // parent 链快速耗尽:裸文件名 parent()=Some(""),"" 的 parent()=None
        // → 第 2 次 and_then 即得 None → 报"无法推导"。
        // 注意:tempdir(Windows ≥3 层)与 a/b/exe(第 3 层得 Some(""))都不会
        // 在第 3 次 and_then 前耗尽,会落入 Cargo.toml 检查分支。
        let exe = Path::new("claw-plus.exe");
        let err = workspace_root_from_exe(exe).expect_err("should reject shallow path");
        assert!(err.to_string().contains("无法从 exe 路径推导"), "got: {err}");
    }

    // ---- 2026-09-02 竞态修复:exited 归属校验 ----

    #[test]
    fn exited_belongs_matching_pid() {
        let exited = serde_json::json!({"old_pid": 123, "exited_at_ms": 1});
        assert!(exited_belongs(&exited, Some(123)));
    }

    #[test]
    fn exited_belongs_rejects_other_pid() {
        // 上次升级失败残留的 exited(old_pid 不同)必须判为不归属,防止误触发。
        let exited = serde_json::json!({"old_pid": 999, "exited_at_ms": 1});
        assert!(!exited_belongs(&exited, Some(123)));
    }

    #[test]
    fn exited_belongs_trusts_when_no_request() {
        // 无 request(异常场景)时保守信任 exited。
        let exited = serde_json::json!({"old_pid": 123, "exited_at_ms": 1});
        assert!(exited_belongs(&exited, None));
    }

    #[test]
    fn exited_belongs_missing_pid_is_not_belongs() {
        // exited 缺 old_pid 且期望有 → 不归属(防异常标记)。
        let exited = serde_json::json!({"exited_at_ms": 1});
        assert!(!exited_belongs(&exited, Some(123)));
    }

    // ---- 2026-09-02 部署版源码根推导修复 ----

    /// 构造一个含 `rusty-claude-cli` 包的 workspace 目录结构。
    fn make_rusty_workspace(base: &Path) {
        std::fs::write(base.join("Cargo.toml"), "[workspace]\n").unwrap();
        let cli = base.join("crates").join("rusty-claude-cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(cli.join("Cargo.toml"), "[package]\n").unwrap();
    }

    /// user_cwd 本身就是 workspace(cwd 含 Cargo.toml + rusty-claude-cli)→ 直接命中。
    #[test]
    fn workspace_root_from_user_cwd_hits_cwd_itself() {
        let dir = tempfile::tempdir().unwrap();
        make_rusty_workspace(dir.path());
        let root = workspace_root_from_user_cwd(dir.path()).expect("derive");
        assert_eq!(root, dir.path());
    }

    /// user_cwd 是仓库根、workspace 在子目录(本项目 rust/)→ 向下探测命中。
    #[test]
    fn workspace_root_from_user_cwd_hits_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("rust");
        std::fs::create_dir_all(&sub).unwrap();
        make_rusty_workspace(&sub);
        let root = workspace_root_from_user_cwd(dir.path()).expect("derive");
        assert_eq!(root, sub);
    }

    /// 向上找:在 workspace 子目录内运行 /upgrade(cwd=rust/crates/app)→ 命中 workspace。
    #[test]
    fn workspace_root_from_user_cwd_hits_ancestor_when_nested() {
        let dir = tempfile::tempdir().unwrap();
        make_rusty_workspace(dir.path());
        let nested = dir.path().join("crates").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        let root = workspace_root_from_user_cwd(&nested).expect("derive");
        assert_eq!(root, dir.path());
    }

    /// 回归(2026-09-02):仓库根下存在多个含 Cargo.toml 的子目录
    /// (本项目 `rust/` 与 `demo-monorepo/`),read_dir 顺序不定。
    /// 必须跳过非 rusty-claude-cli 的候选(如 demo-monorepo),命中真正的 workspace。
    #[test]
    fn workspace_root_from_user_cwd_skips_non_rusty_workspace() {
        let dir = tempfile::tempdir().unwrap();
        // 干扰候选:含 Cargo.toml 但非 rusty-claude-cli workspace(demo-monorepo 场景)。
        let decoy = dir.path().join("demo-monorepo");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::write(decoy.join("Cargo.toml"), "[package]\n").unwrap();
        // 真正目标:rust/。
        let target = dir.path().join("rust");
        std::fs::create_dir_all(&target).unwrap();
        make_rusty_workspace(&target);
        let root = workspace_root_from_user_cwd(dir.path()).expect("derive");
        assert_eq!(root, target, "必须跳过 demo-monorepo 命中 rust");
    }

    /// 完全找不到 rusty-claude-cli workspace → 明确报错。
    #[test]
    fn workspace_root_from_user_cwd_errors_when_no_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = workspace_root_from_user_cwd(dir.path())
            .expect_err("should reject non-workspace dir");
        assert!(
            err.to_string().contains("无法从工作目录"),
            "got: {err}"
        );
    }
}
