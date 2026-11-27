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
//! 3. `cargo build -p claw-server`(workspace root 执行;输出显示在本
//!    新控制台)。
//! 4. 启动新 exe(`current_dir = user_cwd` → 自动检测 upgrade-request.json
//!    → resume session + restore plan + 继续任务,无感衔接)。
//! 5. 清理 exited 标记,本助手退出。

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::session_mgr::{
    clear_upgrade_exited, clear_upgrade_request, read_upgrade_exited, read_upgrade_request,
    read_upgrade_result, write_upgrade_result, UpgradeRequest, UpgradeResult, UPGRADE_EXITED_REL,
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

/// 保留末尾 N 个字符（编译错误通常在输出末尾，头部是进度/警告）。
fn truncate_tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - max_chars).collect();
    format!("…（前 {} 字符已截断）…\n{tail}", count - max_chars)
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
                log_out(
                    log_file,
                    &format!("[upgrade-helper] env {var}={}", truncate(&value, 300)),
                );
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
        &format!(
            "[upgrade-helper] 升级助手启动: user_cwd={}",
            user_cwd.display()
        ),
    );
    log_key_env(&log_file);

    // P1:Web 模式改为"构建成功才切换" —— helper 在旧进程存活时构建，
    // 构建结果写 `.claw/upgrade-result.json` 回传；失败时旧进程补一条带
    // 编译错误的工具结果并原地续跑，不再"先退出后构建"导致任务死亡。
    let web_request = read_upgrade_request(user_cwd).filter(|req| req.web_port.is_some());
    let outcome = run_helper_flow(user_cwd, web_request.as_ref(), log_file);
    if let Some(request) = web_request.as_ref() {
        let workspace = request_workspace(request, user_cwd);
        if outcome.is_err() && read_upgrade_result(&workspace).is_none() {
            // helper 自身异常(定位源码根失败/备份失败等)也要回传，
            // 否则旧进程会一直等构建结果直到超时。
            let message = outcome
                .as_ref()
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "升级助手异常退出".to_string());
            let _ = write_upgrade_result(
                &request_workspace(request, user_cwd),
                &UpgradeResult::failure(message),
            );
        }
    }
    outcome
}

/// 升级助手主流程(Web / CLI-TUI 共用构建与部署步骤)。
fn run_helper_flow(
    user_cwd: &Path,
    web_request: Option<&UpgradeRequest>,
    log_file: Option<File>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. CLI/TUI：等旧进程写 exited 标记后再迁移；Web(P1)：旧进程保持运行，
    //    构建成功后由结果文件/commit 请求触发退出。
    //    注意:不能在启动时无条件 clear exited —— 那会与旧进程的
    //    write_upgrade_exited 竞态:旧进程若已写好标记(如 shutdown 很快),
    //    clear 会误删,本助手随后轮询永远等不到 → 卡满 180s 超时
    //    (2026-09-02 实测复现)。防残留改由 old_pid 校验完成:
    //    上次升级失败残留的 exited 携带旧 old_pid,与本次 request.old_pid
    //    不匹配,判定为残留并清除,不会误触发本次迁移。
    let expected_old_pid = read_upgrade_request(user_cwd).map(|req| req.old_pid);
    if web_request.is_none() {
        wait_for_exited(user_cwd, expected_old_pid, &log_file)?;
        // 同 Web 路径：标记 ≠ 进程已死。CLI 路径虽无端口可抢，但旧进程若
        // 仍持有 exe 与工作区状态，迁移后会出现两个进程并存。这里一并确认。
        ensure_old_process_gone(expected_old_pid, None, &log_file);
        log_out(&log_file, "[upgrade-helper] 旧进程已退出,开始迁移");
    } else {
        log_out(
            &log_file,
            "[upgrade-helper] Web 构建先行：旧进程保持运行，构建成功后切换",
        );
    }

    // 2. 备份当前 exe → exe.old（**复制**而不是改名）。
    //    历史事故：部署版从 claw-web.exe 启动时，旧实现先把入口改名成 .old，
    //    随后构建产物名对不上（cargo 只产出 claw.exe）而失败，桌面快捷方式
    //    的目标直接消失。复制备份保证构建失败时入口仍然可用（旧版本）。
    //    旧进程在等待 exited 标记后已退出，文件不再被占用。
    let exe = std::env::current_exe()?;
    let old_exe = backup_path(&exe);
    if old_exe.exists() {
        // 上次升级残留的 .old:先删除旧备份(它已不是本次运行的二进制)。
        std::fs::remove_file(&old_exe)?;
    }
    copy_with_retry(&exe, &old_exe, Duration::from_secs(10))?;
    log_out(
        &log_file,
        &format!(
            "[upgrade-helper] 备份: {} → {}",
            exe.display(),
            old_exe.display()
        ),
    );

    // 3. cargo build。源码根定位优先级(P0 注册表):
    //    ① 源码根注册表(`~/.claw/source-root` / env),使**任意目录**下运行的
    //       热升级也能找到源码树 —— 这是元认知进化闭环的地基(见
    //       docs/2026-10-11-metacognitive-evolution-design.md §3.1);
    //    ② user_cwd 上溯(在源码目录内运行时的旧路径);
    //    ③ exe 上溯(源码 target 布局)。
    //    历史教训:部署版 exe 位于 `.cargo/bin`,从 exe 上溯到用户主目录不含
    //    Cargo.toml,推导必失败(2026-09-02 实测);注册表正是为此补的缺口。
    let workspace_root = runtime::source_root::resolve_source_root()
        .filter(|p| is_claw_server_workspace(p))
        .map(Ok)
        .unwrap_or_else(|| {
            workspace_root_from_user_cwd(user_cwd).or_else(|_| workspace_root_from_exe(&exe))
        })
        .map_err(|err| {
            log_out(
                &log_file,
                &format!("[upgrade-helper] workspace 定位失败: {err}"),
            );
            err
        })?;
    // 定位成功即自动登记(P0):源码树内首次热升级后,后续任意目录都能定位。
    let _ = runtime::source_root::register_source_root(&workspace_root);
    // 2b. 前端源码比 web/dist 新时先构建前端（dist 会在 cargo build 时内联进二进制）。
    ensure_frontend_built(&workspace_root, &log_file)?;

    // 3. cargo build。产物路径必须与"当前 exe 运行场景"对齐:
    //    - exe 在 `target/debug`(源码 debug 布局) → 默认 build(debug),产物已在原位
    //    - exe 在 `target/release`(源码 release 布局) → `--release`,产物已在原位
    //    - 其余位置(`~/.cargo/bin` 部署版) → `--release`,产物在 target/release,
    //      需回拷到 exe 原路径(原文件已被 mv 成 .old,不复制则启动必然失败)。
    let in_target_debug = exe.starts_with(workspace_root.join("target").join("debug"));
    let in_target_release = exe.starts_with(workspace_root.join("target").join("release"));
    let target_layout = in_target_debug || in_target_release;
    // Web 构建先行：旧进程仍在运行，必须先改名释放 exe 路径（Windows 允许
    // 重命名运行中的镜像），否则 cargo 无法把新产物写回原位。
    let release_first = web_request.is_some();
    if target_layout || release_first {
        // target 布局下 cargo 会把新产物写到 exe 原路径；helper 自身正运行于
        // 该路径，Windows 不允许覆盖运行中的镜像。先把自身改名为 .old
        // （Windows 允许 rename 运行中的 exe），构建失败时再改回来。
        let _ = std::fs::remove_file(&old_exe);
        std::fs::rename(&exe, &old_exe)?;
        log_out(
            &log_file,
            &format!(
                "[upgrade-helper] 释放 exe 路径: {} → {}",
                exe.display(),
                old_exe.display()
            ),
        );
    }
    let build_started = Instant::now();
    let use_release = !in_target_debug;
    let profile_dir = if use_release { "release" } else { "debug" };
    log_out(
        &log_file,
        &format!(
            "[upgrade-helper] 编译: cargo build {}{} @ {}",
            if use_release { "--release " } else { "" },
            "-p claw-server",
            workspace_root.display()
        ),
    );
    let mut args = vec!["build".to_string()];
    if use_release {
        args.push("--release".to_string());
    }
    args.extend(["-p".to_string(), "claw-server".to_string()]);
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
            // 已释放 exe 路径时必须还原，否则入口消失。
            if target_layout || release_first {
                let _ = std::fs::rename(&old_exe, &exe);
            }
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
        if target_layout || release_first {
            // 构建失败：把旧镜像改回原路径，保证入口仍可启动旧版本。
            let _ = std::fs::rename(&old_exe, &exe);
        }
        if let Some(request) = web_request {
            // P1:把编译错误回传旧进程，由它补一条 HotUpgrade 错误结果并
            // 原地续跑（模型当场修代码重试），不再需要人工重启。
            let detail = if stderr.trim().is_empty() {
                stdout.to_string()
            } else {
                stderr.to_string()
            };
            let _ = write_upgrade_result(
                &request_workspace(request, user_cwd),
                &UpgradeResult {
                    ok: false,
                    elapsed_ms: Some(build_started.elapsed().as_millis() as u64),
                    stderr_tail: Some(truncate_tail(&detail, 6000)),
                    message: None,
                },
            );
            log_out(
                &log_file,
                "[upgrade-helper] 已回传编译错误，旧进程继续当前任务",
            );
        }
        return Err("cargo build failed".into());
    }

    // 3b. 部署版(非源码 target 布局):把构建产物回拷到 exe 原路径。
    if !target_layout {
        let exe_name = exe
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("无法获取 exe 文件名")?;
        // 入口名不一定是 cargo 产物名：桌面快捷方式用 claw-web.exe，但
        // `cargo build -p claw-server` 只产出 claw.exe。按产物名解析，
        // 避免再次出现“构建成功却找不到产物、入口已被改名”的事故。
        let built = match resolve_built_product(&workspace_root, profile_dir, exe_name) {
            Some(built) => built,
            None => {
                // 路径已释放时先还原，避免入口消失。
                if release_first {
                    let _ = std::fs::rename(&old_exe, &exe);
                }
                return Err(format!(
                    "构建产物缺失: {}/{}（入口={exe_name}, profile={profile_dir}）",
                    workspace_root.join("target").join(profile_dir).display(),
                    "claw.exe"
                )
                .into());
            }
        };
        // helper 自身就运行在 exe 路径上，不能直接覆盖；先把自身改名成
        // .old 释放路径（备份副本已存在，这里覆盖它），再落新产物。
        // Web 构建先行时路径已在构建前释放，无需再次改名。
        if !release_first {
            let _ = std::fs::remove_file(&old_exe);
            std::fs::rename(&exe, &old_exe)?;
        }
        if let Err(error) = copy_with_retry(&built, &exe, Duration::from_secs(15)) {
            // 失败回滚：把旧镜像改回原路径，快捷方式仍然可用。
            let _ = std::fs::rename(&old_exe, &exe);
            return Err(error.into());
        }
        log_out(
            &log_file,
            &format!(
                "[upgrade-helper] 已部署: {} ← {}",
                exe.display(),
                built.display()
            ),
        );
        // 部署目录里通常同时存在 claw.exe 与 claw-web.exe（硬链接快捷入口）。
        // 替换其中一个后另一个仍指向旧内容，必须一起同步，否则双击快捷方式还是旧版本。
        sync_launcher_siblings(&exe, &built, &log_file);
    }

    // 3c. Web(P1)：构建成功才切换。写结果 → 通知旧进程确认切换 → 等它退出。
    if let Some(request) = web_request {
        let workspace = request_workspace(request, user_cwd);
        let _ = write_upgrade_result(
            &workspace,
            &UpgradeResult {
                ok: true,
                elapsed_ms: Some(build_started.elapsed().as_millis() as u64),
                stderr_tail: None,
                message: None,
            },
        );
        let port = request.web_port.expect("filtered web_port");
        if matches!(
            notify_upgrade_commit(port, request.web_token.as_deref(), &workspace),
            CommitOutcome::Cancelled
        ) {
            log_out(&log_file, "[upgrade-helper] 旧进程已取消升级，不启动新进程");
            return Ok(());
        }
        // 旧进程读到结果（或收到 commit）后写 exited 标记并退出。
        wait_for_exited(user_cwd, Some(request.old_pid), &log_file)?;
        // 标记 ≠ 进程已死。旧进程可能写了标记却未真正退出（卡在析构、
        // 被挂起的会话线程、或子进程继承了监听 socket），此时若直接启动
        // 新进程，旧进程会变成孤儿继续占着端口，新进程 bind 失败退回随机
        // 端口，已打开的页面失联。这里强制确认它真的消失了。
        ensure_old_process_gone(Some(request.old_pid), Some(port), &log_file);
    }

    // 4. 启动新 exe(user_cwd 为工作目录 → 自动检测标记 → resume 会话)。
    log_out(
        &log_file,
        &format!(
            "[upgrade-helper] 编译成功,准备启动新进程: {}",
            exe.display()
        ),
    );
    let mut cmd = Command::new(&exe);
    cmd.current_dir(user_cwd);

    // Web 模式：旧进程是 `claw serve`。启动新进程继续跑 serve（同端口、同 token），
    // 并脱离 helper 的控制台，避免用户关掉升级窗口时把 Web 服务一起带走。
    if let Some(request) = web_request {
        let port = request.web_port.expect("filtered web_port");
        std::thread::sleep(Duration::from_millis(1200));
        let mut web_cmd = Command::new(&exe);
        web_cmd.current_dir(user_cwd);
        for arg in web_restart_args(port, user_cwd) {
            web_cmd.arg(arg);
        }
        if let Some(token) = request.web_token.as_deref() {
            web_cmd.env("CLAW_WEB_TOKEN", token);
        }
        web_cmd.stdin(Stdio::null());
        match open_web_server_log(user_cwd) {
            Some(file) => {
                if let Ok(clone) = file.try_clone() {
                    web_cmd.stdout(Stdio::from(file));
                    web_cmd.stderr(Stdio::from(clone));
                }
            }
            None => {
                web_cmd.stdout(Stdio::null());
                web_cmd.stderr(Stdio::null());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            web_cmd.creation_flags(DETACHED_PROCESS);
        }
        let mut child = web_cmd.spawn()?;
        log_out(
            &log_file,
            &format!(
                "[upgrade-helper] Web 服务已重启: pid={} port={} (日志 .claw/web-server.log)",
                child.id(),
                port
            ),
        );
        // 健康检查：新版本起不来时自动回滚旧版本，避免服务彻底不可用。
        let healthy = match request.web_token.as_deref() {
            Some(token) => wait_web_healthy(port, token, Duration::from_secs(90)),
            None => true,
        };
        if !healthy {
            log_out(&log_file, "[upgrade-helper] 新版本健康检查失败，回滚旧版本");
            let _ = child.kill();
            let workspace = request_workspace(request, user_cwd);
            let _ = write_upgrade_result(
                &workspace,
                &UpgradeResult::failure("新版本启动失败，已回滚旧版本"),
            );
            let restored =
                std::fs::remove_file(&exe).is_ok() && std::fs::rename(&old_exe, &exe).is_ok();
            if restored {
                let mut rollback_cmd = Command::new(&exe);
                rollback_cmd.current_dir(user_cwd);
                for arg in web_restart_args(port, user_cwd) {
                    rollback_cmd.arg(arg);
                }
                if let Some(token) = request.web_token.as_deref() {
                    rollback_cmd.env("CLAW_WEB_TOKEN", token);
                }
                rollback_cmd.stdin(Stdio::null());
                match open_web_server_log(user_cwd) {
                    Some(file) => {
                        if let Ok(clone) = file.try_clone() {
                            rollback_cmd.stdout(Stdio::from(file));
                            rollback_cmd.stderr(Stdio::from(clone));
                        }
                    }
                    None => {
                        rollback_cmd.stdout(Stdio::null());
                        rollback_cmd.stderr(Stdio::null());
                    }
                }
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    const DETACHED_PROCESS: u32 = 0x0000_0008;
                    rollback_cmd.creation_flags(DETACHED_PROCESS);
                }
                match rollback_cmd.spawn() {
                    Ok(old_child) => log_out(
                        &log_file,
                        &format!(
                            "[upgrade-helper] 已回滚旧版本: pid={} port={}",
                            old_child.id(),
                            port
                        ),
                    ),
                    Err(error) => log_out(
                        &log_file,
                        &format!("[upgrade-helper] 回滚旧版本启动失败: {error}"),
                    ),
                }
            } else {
                log_out(
                    &log_file,
                    &format!(
                        "[upgrade-helper] 回滚失败，请手动用 {} 恢复",
                        old_exe.display()
                    ),
                );
            }
            clear_upgrade_exited(user_cwd);
            return Ok(());
        }
        clear_upgrade_exited(user_cwd);
        // Web 断点续跑(P0):请求标记由新 serve 进程消费(恢复会话后清理),
        // 此处不能清 —— 否则新进程启动时读不到挂起现场。
        return Ok(());
    }

    // CLI/TUI 模式：把新进程 stdio 指向 helper 自己的控制台(不依赖继承):
    // 用户 TUI 的 stdio 可能是管道(从 IDE/启动器启动),旧进程退出后管道读端
    // 关闭 → 新进程初始化时 println! 触发 os error 232 panic。实测 CREATE_NEW_CONSOLE
    // 子进程在父 stdio 是管道时默认 GetStdHandle 仍无效(errno 22),但**能显式打开
    // CONIN$/CONOUT$**(conout-ok;conin-ok)——helper 是 CREATE_NEW_CONSOLE,有控制台。
    // 打开失败(环境无有效控制台)时不 spawn 新进程,避免继承无效句柄崩溃:
    // 改为提示用户手动运行 claw 接管(同样自动 resume 会话)。
    #[cfg(windows)]
    let con_available = {
        use std::os::windows::io::{FromRawHandle, IntoRawHandle};
        use std::process::Stdio;
        let conin = std::fs::OpenOptions::new().read(true).open("CONIN$");
        let conout = std::fs::OpenOptions::new().write(true).open("CONOUT$");
        match (conin, conout) {
            (Ok(conin), Ok(conout)) => {
                log_out(
                    &log_file,
                    "[upgrade-helper] 控制台句柄就绪(CONIN$/CONOUT$),将显式设置新进程 stdio",
                );
                let conout2 = conout.try_clone().expect("CONOUT$ 句柄可克隆");
                cmd.stdin(unsafe { Stdio::from_raw_handle(conin.into_raw_handle()) });
                cmd.stdout(unsafe { Stdio::from_raw_handle(conout2.into_raw_handle()) });
                cmd.stderr(unsafe { Stdio::from_raw_handle(conout.into_raw_handle()) });
                true
            }
            (Err(_), _) => {
                log_out(
                    &log_file,
                    "[upgrade-helper] CONIN$ 打开失败(环境无有效控制台)",
                );
                false
            }
            (_, Err(_)) => {
                log_out(
                    &log_file,
                    "[upgrade-helper] CONOUT$ 打开失败(环境无有效控制台)",
                );
                false
            }
        }
    };
    #[cfg(not(windows))]
    let con_available = true;

    if con_available {
        let mut child = cmd.spawn()?;

        // 5. 清理 exited 标记(本次升级完成)。
        clear_upgrade_exited(user_cwd);
        log_out(
            &log_file,
            "[upgrade-helper] 新进程已启动(无感衔接),助手驻留保持终端。",
        );

        // 6. 驻留监护:helper 不能立即退出 —— 若退出,共用控制台随之关闭;wait 保持
        //    终端存活到新进程(REPL)退出。
        let _ = child.wait();
    } else {
        // 无可用控制台:不 spawn(避免新进程继承无效 stdio 崩溃),提示手动接管。
        clear_upgrade_exited(user_cwd);
        log_out(
            &log_file,
            "[upgrade-helper] 本环境无可用控制台,未自动启动新进程。\
             请在任意终端手动运行 `claw`(会自动检测升级标记,resume 会话并继续任务)。\
             旧二进制已备份为 exe.old,可随时手动恢复。",
        );
    }
    Ok(())
}

/// 前端源码(`web/src`)比已构建的 `web/dist/index.html` 新时先执行
/// `npm run build`，避免 Rust 修复生效但前端仍是旧产物。
///
/// - 找不到 npm：记录日志并沿用现有 dist（不阻断升级）；
/// - npm 执行失败：中止升级并把构建错误回传，让模型修复后重试。
fn ensure_frontend_built(
    workspace_root: &Path,
    log_file: &Option<File>,
) -> Result<(), Box<dyn std::error::Error>> {
    let web_dir = workspace_root
        .join("crates")
        .join("claw-server")
        .join("web");
    if !web_dir.join("package.json").is_file() {
        return Ok(());
    }
    let Some(newest_src) = newest_mtime(&web_dir.join("src")) else {
        return Ok(());
    };
    let dist_mtime = std::fs::metadata(web_dir.join("dist").join("index.html"))
        .ok()
        .and_then(|meta| meta.modified().ok());
    if let Some(dist_mtime) = dist_mtime {
        if newest_src <= dist_mtime {
            return Ok(());
        }
    }

    log_out(
        log_file,
        "[upgrade-helper] 检测到前端源码更新，执行 npm run build",
    );
    match npm_build(&web_dir) {
        Ok(output) if output.status.success() => {
            log_out(log_file, "[upgrade-helper] 前端构建完成");
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = if stderr.trim().is_empty() {
                stdout.to_string()
            } else {
                stderr.to_string()
            };
            log_out(
                log_file,
                &format!(
                    "[upgrade-helper] 前端构建失败(exit={:?}): {}",
                    output.status.code(),
                    truncate(&detail, 2000)
                ),
            );
            Err(format!(
                "前端构建失败(npm run build):\n{}",
                truncate_tail(&detail, 4000)
            )
            .into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            log_out(
                log_file,
                "[upgrade-helper] 未找到 npm，跳过前端构建(沿用现有 dist)",
            );
            Ok(())
        }
        Err(error) => {
            log_out(
                log_file,
                &format!("[upgrade-helper] 启动 npm 失败: {error}，跳过前端构建"),
            );
            Ok(())
        }
    }
}

fn npm_build(web_dir: &Path) -> std::io::Result<std::process::Output> {
    #[cfg(windows)]
    {
        // Windows 下 npm 是 .cmd，必须经由 cmd /C 启动。
        Command::new("cmd")
            .args(["/C", "npm", "run", "build"])
            .current_dir(web_dir)
            .output()
    }
    #[cfg(not(windows))]
    {
        Command::new("npm")
            .args(["run", "build"])
            .current_dir(web_dir)
            .output()
    }
}

/// 目录树里最新文件修改时间（不存在/为空返回 None）。
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                newest = Some(match newest {
                    Some(previous) if previous >= modified => previous,
                    _ => modified,
                });
            }
        }
    }
    newest
}

/// 轮询旧进程"已退出"标记（校验 old_pid 归属；上次失败残留会被清除）。
fn wait_for_exited(
    user_cwd: &Path,
    expected_old_pid: Option<u32>,
    log_file: &Option<File>,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    loop {
        if let Some(exited) = read_upgrade_exited(user_cwd) {
            if exited_belongs(&exited, expected_old_pid) {
                return Ok(());
            }
            // 上次升级失败残留:清除并继续等本次标记。
            clear_upgrade_exited(user_cwd);
        }
        if started.elapsed() > WAIT_TIMEOUT {
            let message = format!(
                "等待旧进程退出标记超时({}s): 未出现 {}",
                WAIT_TIMEOUT.as_secs(),
                user_cwd.join(UPGRADE_EXITED_REL).display()
            );
            log_out(log_file, &format!("[upgrade-helper] {message}"));
            return Err(message.into());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 确认旧进程**真的**消失了；仍存活则强制终止，再确认端口释放。
///
/// **为什么必须有**:`wait_for_exited` 只信任旧进程写下的文件标记,从不验证
/// 该进程是否真的消失。一旦旧进程写了标记却因故未退出(卡在析构、被挂起的
/// 会话线程、或子进程继承了监听 socket),helper 仍会照常启动新进程,于是:
///
/// - 旧进程成为**孤儿**,继续占着端口与内存;
/// - 新进程 bind 失败 → 退回随机端口,已打开的页面失联;
/// - 最坏情况两个进程同时活着,对同一工作区各写一份状态。
///
/// 实测症状:4317 上残留一个 3.5 小时前启动的孤儿 serve,而当时的新进程被
/// 挤到 58014,日志里却只有"Web 服务已重启: port=4317"的成功记录 —— 因为
/// helper 根本没检查过旧进程是否还在。
///
/// 这里补上"标记 ≠ 进程已死"这一课:先给一个短暂宽限期让正常退出完成,
/// 之后仍存活就强制终止进程树,确保新进程能拿到原端口。
fn ensure_old_process_gone(old_pid: Option<u32>, port: Option<u16>, log_file: &Option<File>) {
    let Some(pid) = old_pid.filter(|p| *p != 0) else {
        return;
    };
    // 宽限期:正常退出路径在写完标记后约 300ms 内 exit(0),
    // 给 2s 避免把"正在优雅退出"误判为"卡死"而强杀。
    let grace = Instant::now();
    while grace.elapsed() < Duration::from_secs(2) {
        if !process_is_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // ⚠ PID 复用防护：旧进程可能早已退出，而系统把同一个 PID 分配给了
    // 无关进程。此时只凭"PID 存活"就去 kill，会误杀无辜进程。
    // 必须确认该 PID 当前的映像名仍是 claw，才允许强制终止。
    if !process_is_claw(pid) {
        log_out(
            log_file,
            &format!("[upgrade-helper] pid={pid} 存活但已不是 claw（PID 复用），跳过强制终止"),
        );
        return;
    }
    log_out(
        log_file,
        &format!(
            "[upgrade-helper] 旧进程 pid={pid} 已写退出标记但仍在运行，强制终止（否则新进程无法接管端口）"
        ),
    );
    kill_process_tree(pid);
    // 终止后等它真正消失,再等端口释放,避免新进程 bind 抢跑。
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(port) = port {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !port_is_listening(port) {
                log_out(
                    log_file,
                    &format!("[upgrade-helper] 端口 {port} 已释放，可安全接管"),
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        log_out(
            log_file,
            &format!("[upgrade-helper] 端口 {port} 仍被占用，新进程可能退回随机端口"),
        );
    }
}

/// 进程是否仍存活。
///
/// 用 `tasklist`/`kill -0` 而非 Win32 FFI:`claw-server` 未声明 `windows-sys`
/// 依赖,为一次存活检查引入平台 FFI 不划算,且子进程方式跨平台一致。
fn process_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        let output = runtime::sandbox::quiet_command("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                // 无匹配时 tasklist 输出固定提示 "INFO: No tasks..."(本地化会变),
                // 因此改判"是否出现该 PID 的 CSV 行"更稳:CSV 首字段是映像名,
                // 第二字段是 PID。
                text.lines().any(|line| {
                    line.split(',')
                        .nth(1)
                        .map(|field| field.trim_matches('"') == pid.to_string())
                        .unwrap_or(false)
                })
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// 该 PID 当前的映像名是否仍是 claw。
///
/// **为什么必须有**：`ensure_old_process_gone` 会强制终止 `old_pid`。但旧进程
/// 可能早已退出，而系统会**复用 PID** —— 此时同一 PID 属于某个无关进程，
/// 仅凭"PID 存活"就 kill 会误杀用户正在用的程序。强制终止前必须确认它还是
/// 本产品（`claw.exe` / `claw-web.exe` / `claw-plus.exe` 均为本产品映像）。
///
/// 查询失败（进程刚好退出、tasklist/ps 不可用）时返回 `false`：
/// 宁可少杀一次（新进程退回随机端口，用户手动切 URL），也不误杀无关进程。
fn process_is_claw(pid: u32) -> bool {
    let Some(image) = process_image_name(pid) else {
        return false;
    };
    let image = image.trim().to_ascii_lowercase();
    // 取文件名主干再比较，避免路径里恰好含 "claw" 的无关进程被误判。
    let stem = image
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(image.as_str())
        .trim_end_matches(".exe");
    stem.starts_with("claw")
}

/// 查询 PID 的映像名。
///
/// Windows 用 `tasklist /FO CSV /NH`（首字段为映像名、次字段为 PID）；
/// Unix 用 `ps -o comm=`。两者都不依赖额外 crate —— `claw-server` 未声明
/// `windows-sys`，为一次存活检查引入平台 FFI 不划算。
fn process_image_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let output = runtime::sandbox::quiet_command("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        // 无匹配时 tasklist 打印本地化的 "INFO: No tasks..." 提示，
        // 因此不判提示文本，而是找"第二字段等于该 PID"的 CSV 行。
        text.lines().find_map(|line| {
            let mut fields = line.split(',');
            let image = fields.next()?.trim_matches('"').trim();
            let got_pid = fields.next()?.trim_matches('"').trim();
            (got_pid == pid.to_string() && !image.is_empty()).then(|| image.to_string())
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!name.is_empty()).then_some(name)
    }
}

/// 本机端口是否仍处于监听状态。
fn port_is_listening(port: u16) -> bool {
    use std::net::{SocketAddr, TcpListener};
    // 能 bind 成功 = 已释放;bind 失败 = 仍被占用。
    // 不用 connect 探测:旧进程可能监听但暂时不 accept。
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpListener::bind(addr) {
        Ok(listener) => {
            drop(listener);
            false
        }
        Err(_) => true,
    }
}

/// 终止进程树(尽力而为,失败不影响后续启动尝试)。
fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = runtime::sandbox::quiet_command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 升级请求所属工作区（缺省退回 user_cwd）。
fn request_workspace(request: &UpgradeRequest, user_cwd: &Path) -> PathBuf {
    request
        .workspace
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| user_cwd.to_path_buf())
}

/// helper 通知旧进程"构建成功，可以切换"的结果。
enum CommitOutcome {
    /// 旧进程确认切换（写 exited 后退出）。
    Accepted,
    /// 旧进程已取消升级（409），helper 不应再启动新进程。
    Cancelled,
    /// 连不上旧进程（通常它已按结果文件自行退出），继续等 exited 即可。
    Unreachable,
}

fn notify_upgrade_commit(port: u16, token: Option<&str>, workspace: &Path) -> CommitOutcome {
    let Some(token) = token else {
        return CommitOutcome::Unreachable;
    };
    let path = format!(
        "/api/upgrade/commit?t={token}&workspace={}",
        percent_encode(&workspace.display().to_string())
    );
    match http_status(port, "POST", &path, Duration::from_secs(5)) {
        Some(409) => CommitOutcome::Cancelled,
        Some(_) => CommitOutcome::Accepted,
        None => CommitOutcome::Unreachable,
    }
}

/// 健康检查：轮询 `/api/meta`，确认新进程已就绪。
fn wait_web_healthy(port: u16, token: &str, timeout: Duration) -> bool {
    let started = Instant::now();
    let path = format!("/api/meta?t={token}");
    while started.elapsed() < timeout {
        if http_status(port, "GET", &path, Duration::from_secs(2)) == Some(200) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// 极简本地 HTTP 请求，只读取状态码（与同端口 serve 通信，避免 HTTP 客户端依赖）。
fn http_status(port: u16, method: &str, path: &str, timeout: Duration) -> Option<u16> {
    let address = ("127.0.0.1", port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&address, timeout).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut buffer = [0u8; 256];
    let read = stream.read(&mut buffer).ok()?;
    let head = String::from_utf8_lossy(&buffer[..read]);
    let mut parts = head.split_whitespace();
    let _version = parts.next()?;
    parts.next()?.parse::<u16>().ok()
}

/// 查询参数最小百分号编码（工作区路径可能含中文/空格/反斜杠）。
fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Web 热升级：新 serve 的启动参数（纯函数便于测试）。
fn web_restart_args(port: u16, user_cwd: &Path) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--cwd".to_string(),
        user_cwd.display().to_string(),
        "--no-open".to_string(),
    ]
}

/// Web 热升级：新 serve 的日志（stdout/stderr 追加）。
fn open_web_server_log(user_cwd: &Path) -> Option<File> {
    let path = user_cwd.join(".claw").join("web-server.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// 部署目录里同时存在 `claw.exe` / `claw-web.exe` 时，把构建产物同步到另一个入口。
fn sync_launcher_siblings(exe: &Path, built: &Path, log_file: &Option<File>) {
    let Some(parent) = exe.parent() else {
        return;
    };
    let Some(current) = exe.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    for name in ["claw.exe", "claw-web.exe"] {
        if name == current {
            continue;
        }
        let sibling = parent.join(name);
        if sibling == built || !sibling.exists() {
            continue;
        }
        if std::fs::remove_file(&sibling).is_ok() {
            match std::fs::copy(built, &sibling) {
                Ok(_) => log_out(
                    log_file,
                    &format!(
                        "[upgrade-helper] 同步入口: {} ← {}",
                        sibling.display(),
                        built.display()
                    ),
                ),
                Err(error) => log_out(
                    log_file,
                    &format!(
                        "[upgrade-helper] 同步入口失败 {}: {error}",
                        sibling.display()
                    ),
                ),
            }
        }
    }
}

/// 拷贝文件并重试：旧进程在写 exited 标记后还要几十/几百毫秒才真正释放
/// exe（Windows 会以 os error 32 拒绝覆盖正在运行的镜像）。固定 sleep
/// 不可靠，这里按小步长重试直到超时。
fn copy_with_retry(from: &Path, to: &Path, timeout: Duration) -> std::io::Result<()> {
    let started = Instant::now();
    loop {
        match std::fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(error) => {
                if started.elapsed() >= timeout {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

/// 入口文件名 → cargo 产物文件名。`claw-web.exe` 只是 claw.exe 的硬链接入口。
fn build_product_name(exe_name: &str) -> String {
    if exe_name.eq_ignore_ascii_case("claw-web.exe") || exe_name.eq_ignore_ascii_case("claw-web") {
        if exe_name.to_ascii_lowercase().ends_with(".exe") {
            "claw.exe".to_string()
        } else {
            "claw".to_string()
        }
    } else {
        exe_name.to_string()
    }
}

/// 在 target/<profile> 里定位构建产物：优先入口映射名，再回退到 claw(.exe)。
fn resolve_built_product(
    workspace_root: &Path,
    profile_dir: &str,
    exe_name: &str,
) -> Option<PathBuf> {
    let dir = workspace_root.join("target").join(profile_dir);
    let primary = dir.join(build_product_name(exe_name));
    if primary.is_file() {
        return Some(primary);
    }
    for fallback in ["claw.exe", "claw"] {
        let candidate = dir.join(fallback);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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

/// 判断目录是否为 `claw-server` 的 cargo workspace。
///
/// 不能只用"含 Cargo.toml"判定 —— 仓库里可能有多个含 Cargo.toml 的目录
/// (如本项目 `D:\claw-code-src` 下有 `rust/` 与 `demo-monorepo/`),
/// `read_dir` 遍历顺序不定,误命中非目标目录会导致
/// `cargo build -p claw-server` 报 "package not found"。
/// 可靠判定:该目录是 cargo workspace 且含 `crates/claw-server` 包。
fn is_claw_server_workspace(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file()
        && dir
            .join("crates")
            .join("claw-server")
            .join("Cargo.toml")
            .is_file()
}

/// 从用户工作目录推导 cargo workspace root。
///
/// 部署版 exe 位于 `~/.cargo/bin`,`workspace_root_from_exe` 对其必然失败
/// (上溯到用户主目录无 Cargo.toml)。因此 `/upgrade` 的自编译升级必须基于
/// **用户当前工作目录**定位源码仓库:
///
/// 1. 从 `user_cwd` 向上逐级找 `claw-server` 的 workspace(在源码目录内运行)。
/// 2. 回退:探测 `user_cwd` 的直接子目录(覆盖"cwd=仓库根、workspace 在
///    `rust/` 子目录"的布局,如本项目 `D:\claw-code-src` → `D:\claw-code-src\rust`)。
/// 3. 仍失败则报错,提示需在源码仓库内运行 `/upgrade`。
fn workspace_root_from_user_cwd(user_cwd: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut cursor = Some(user_cwd);
    while let Some(dir) = cursor {
        if is_claw_server_workspace(dir) {
            return Ok(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    if let Ok(entries) = std::fs::read_dir(user_cwd) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && is_claw_server_workspace(&path) {
                return Ok(path);
            }
        }
    }
    Err(format!(
        "无法从工作目录 {} 定位 claw-server 的 cargo workspace(未找到含 crates/claw-server 的目录);请在源码仓库内运行 /upgrade",
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
        assert_eq!(
            backup_path(exe),
            PathBuf::from(r"C:\rust\target\debug\claw-plus.exe.old")
        );
    }

    #[test]
    fn backup_path_preserves_extensionless_names() {
        let exe = Path::new("/usr/local/bin/claw-plus");
        assert_eq!(
            backup_path(exe),
            PathBuf::from("/usr/local/bin/claw-plus.old")
        );
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
        let exe = dir
            .path()
            .join("target")
            .join("debug")
            .join("claw-plus.exe");
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
        assert!(
            err.to_string().contains("无法从 exe 路径推导"),
            "got: {err}"
        );
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

    /// 构造一个含 `claw-server` 包的 workspace 目录结构。
    fn make_rusty_workspace(base: &Path) {
        std::fs::write(base.join("Cargo.toml"), "[workspace]\n").unwrap();
        let cli = base.join("crates").join("claw-server");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(cli.join("Cargo.toml"), "[package]\n").unwrap();
    }

    /// user_cwd 本身就是 workspace(cwd 含 Cargo.toml + claw-server)→ 直接命中。
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
    /// 必须跳过非 claw-server 的候选(如 demo-monorepo),命中真正的 workspace。
    #[test]
    fn workspace_root_from_user_cwd_skips_non_rusty_workspace() {
        let dir = tempfile::tempdir().unwrap();
        // 干扰候选:含 Cargo.toml 但非 claw-server workspace(demo-monorepo 场景)。
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

    /// 完全找不到 claw-server workspace → 明确报错。
    #[test]
    fn workspace_root_from_user_cwd_errors_when_no_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            workspace_root_from_user_cwd(dir.path()).expect_err("should reject non-workspace dir");
        assert!(err.to_string().contains("无法从工作目录"), "got: {err}");
    }

    #[test]
    fn truncate_tail_keeps_the_end_of_build_output() {
        assert_eq!(truncate_tail("short", 10), "short");
        let text = "aaaaaaaaaabbbbbbbbbbcccccccccc";
        let truncated = truncate_tail(text, 12);
        assert!(truncated.ends_with("cccccccccc"), "got: {truncated}");
        assert!(!truncated.contains("aaaaaaaaaa"), "head should be dropped");
    }

    #[test]
    fn newest_mtime_tracks_deepest_file() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("old.txt"), "old").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(nested.join("new.txt"), "new").unwrap();

        let newest = newest_mtime(dir.path()).expect("mtime");
        let new_mtime = std::fs::metadata(nested.join("new.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(newest, new_mtime);
    }

    #[test]
    fn newest_mtime_missing_dir_is_none() {
        assert!(newest_mtime(Path::new("__definitely_missing__")).is_none());
    }

    #[test]
    fn percent_encode_escapes_windows_path_and_cjk() {
        assert_eq!(
            percent_encode("D:\\work dir\\子"),
            "D%3A%5Cwork%20dir%5C%E5%AD%90"
        );
    }

    #[test]
    fn web_restart_args_keep_port_cwd_and_disable_browser() {
        let args = web_restart_args(4399, Path::new(r"D:\work\repo"));
        assert_eq!(
            args,
            vec![
                "serve",
                "--port",
                "4399",
                "--cwd",
                r"D:\work\repo",
                "--no-open"
            ]
        );
    }

    #[test]
    fn sync_launcher_siblings_replaces_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("claw.exe");
        let sibling = dir.path().join("claw-web.exe");
        let built = dir.path().join("built.exe");
        std::fs::write(&exe, "new").unwrap();
        std::fs::write(&sibling, "old").unwrap();
        std::fs::write(&built, "new").unwrap();
        sync_launcher_siblings(&exe, &built, &None);
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "new");
    }

    #[test]
    fn sync_launcher_siblings_skips_missing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("claw.exe");
        let built = dir.path().join("built.exe");
        std::fs::write(&exe, "new").unwrap();
        std::fs::write(&built, "new").unwrap();
        sync_launcher_siblings(&exe, &built, &None);
        assert!(!dir.path().join("claw-web.exe").exists());
    }

    #[test]
    fn copy_with_retry_copies_file() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.exe");
        let to = dir.path().join("b.exe");
        std::fs::write(&from, "bin").unwrap();
        copy_with_retry(&from, &to, Duration::from_secs(1)).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "bin");
    }

    #[test]
    fn build_product_name_maps_web_launcher_to_claw() {
        assert_eq!(build_product_name("claw-web.exe"), "claw.exe");
        assert_eq!(build_product_name("claw.exe"), "claw.exe");
        assert_eq!(build_product_name("claw-plus.exe"), "claw-plus.exe");
    }

    #[test]
    fn resolve_built_product_falls_back_to_claw() {
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join("target").join("release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("claw.exe"), "bin").unwrap();
        let built = resolve_built_product(dir.path(), "release", "claw-web.exe")
            .expect("应回退到 claw.exe");
        assert_eq!(built, release.join("claw.exe"));
    }

    // ---- 2026-11-27 旧进程未退出导致端口交接失败 ----

    /// 当前进程必然存活 —— 存活检测不能把活进程判死（否则会误杀自己）。
    #[test]
    fn process_is_alive_detects_self() {
        assert!(
            process_is_alive(std::process::id()),
            "自身 PID 必须判为存活"
        );
    }

    /// 不存在的 PID 必须判为已死 —— 这是"旧进程是否真的退出"的判据。
    /// 用 `u32::MAX` 而非随机小 PID：Windows 的 PID 是 4 的倍数且远小于
    /// 该值，不会误命中真实进程。
    #[test]
    fn process_is_alive_rejects_impossible_pid() {
        assert!(!process_is_alive(u32::MAX), "不存在的 PID 必须判为已死");
    }

    /// 端口占用探测：自己绑上就是"被占用"，释放后就是"已释放"。
    /// 不依赖固定端口号，由系统分配，避免与真实服务冲突。
    #[test]
    fn port_is_listening_tracks_bind_state() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_is_listening(port), "自己绑定的端口应判为被占用");
        drop(listener);
        // 关闭后可能短暂处于 TIME_WAIT，但 TcpListener::bind 用 SO_REUSEADDR
        // 语义在 Windows 上会立刻成功，故无需重试循环。
        assert!(!port_is_listening(port), "已释放的端口应判为可用");
    }

    /// `old_pid` 缺失/为 0 时必须直接返回，不做任何探测。
    /// 旧标记（升级前写入、无 old_pid 字段）会让这里为 None，若误判成
    /// "进程存活"就会去 kill PID 0（Windows 上等价于终止空闲进程组）。
    #[test]
    fn ensure_old_process_gone_is_noop_without_pid() {
        // 无日志文件句柄：仅验证不 panic、不阻塞。
        ensure_old_process_gone(None, Some(1), &None);
        ensure_old_process_gone(Some(0), Some(1), &None);
    }
}
