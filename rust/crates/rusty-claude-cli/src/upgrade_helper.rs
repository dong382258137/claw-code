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
    println!(
        "[upgrade-helper] 升级助手启动: user_cwd={}",
        user_cwd.display()
    );

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
    println!("[upgrade-helper] 旧进程已退出,开始迁移");

    // 2. mv 当前 exe → exe.old(备份;Windows 允许重命名运行中的文件)。
    let exe = std::env::current_exe()?;
    let old_exe = backup_path(&exe);
    if old_exe.exists() {
        // 上次升级残留的 .old:先删除旧备份(它已不是本次运行的二进制)。
        std::fs::remove_file(&old_exe)?;
    }
    std::fs::rename(&exe, &old_exe)?;
    println!(
        "[upgrade-helper] 备份: {} → {}",
        exe.display(),
        old_exe.display()
    );

    // 3. cargo build(workspace root = exe 上溯:debug → target → rust/)。
    let workspace_root = workspace_root_from_exe(&exe)?;
    println!(
        "[upgrade-helper] 编译: cargo build -p rusty-claude-cli @ {}",
        workspace_root.display()
    );
    let status = Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg("rusty-claude-cli")
        .current_dir(&workspace_root)
        .status()?;
    if !status.success() {
        eprintln!(
            "[upgrade-helper] 编译失败(exit={:?})。旧二进制已备份为 {},可手动恢复。",
            status.code(),
            old_exe.display()
        );
        return Err("cargo build failed".into());
    }

    // 4. 启动新 exe(user_cwd 为工作目录 → 自动检测标记 → resume 会话)。
    println!("[upgrade-helper] 编译成功,启动新进程: {}", exe.display());
    let mut cmd = Command::new(&exe);
    cmd.current_dir(user_cwd);
    cmd.spawn()?;

    // 5. 清理 exited 标记(本次升级完成),助手退出。
    clear_upgrade_exited(user_cwd);
    println!("[upgrade-helper] 完成,新进程已启动(无感衔接)。");
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
}
