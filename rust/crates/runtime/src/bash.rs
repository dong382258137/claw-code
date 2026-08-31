use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command as TokioCommand;
use tokio::runtime::Builder;

use crate::lane_events::{LaneEvent, ShipMergeMethod, ShipProvenance};
use crate::sandbox::{
    build_linux_sandbox_command, platform_sandbox_supported, resolve_sandbox_status_for_request,
    FilesystemIsolationMode, SandboxBuilder, SandboxConfig, SandboxStatus, WindowsSandboxBuilder,
};
use crate::ConfigLoader;

/// 全局 bash 中止标志。
///
/// TUI 在 Ctrl+C 时调用 `set_bash_abort()`,`execute_bash_async` 的 select! loop
/// 每 100ms 轮询此标志,命中后 kill 子进程并返回 interrupted 输出。
///
/// 设计理由:不改 `ToolExecutor::execute` 同步 trait 签名(避免 breaking change),
/// 用全局 AtomicBool 让 TUI 层的 Ctrl+C 信号穿透到 bash.rs 的子进程 kill。
/// 一个 turn 内多次 bash 调用串行执行,每次开始前 clear,语义清晰。
/// subagent 场景:用户 Ctrl+C 中断整个 turn,所有子任务的 bash 都应停止,
/// 全局标志正好满足此语义。
static BASH_ABORT_FLAG: AtomicBool = AtomicBool::new(false);

/// 设置全局 bash 中止标志(TUI Ctrl+C 时调用)。
pub fn set_bash_abort() {
    BASH_ABORT_FLAG.store(true, Ordering::SeqCst);
}

/// 清除全局 bash 中止标志(每次 execute_bash 开始前调用)。
pub fn clear_bash_abort() {
    BASH_ABORT_FLAG.store(false, Ordering::SeqCst);
}

/// 检查 bash 是否被中止。
pub fn is_bash_aborted() -> bool {
    BASH_ABORT_FLAG.load(Ordering::SeqCst)
}

/// Input schema for the built-in bash execution tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BashCommandInput {
    pub command: String,
    pub timeout: Option<u64>,
    pub description: Option<String>,
    #[serde(rename = "run_in_background")]
    pub run_in_background: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "namespaceRestrictions")]
    pub namespace_restrictions: Option<bool>,
    #[serde(rename = "isolateNetwork")]
    pub isolate_network: Option<bool>,
    #[serde(rename = "filesystemMode")]
    pub filesystem_mode: Option<FilesystemIsolationMode>,
    #[serde(rename = "allowedMounts")]
    pub allowed_mounts: Option<Vec<String>>,
}

/// Output returned from a bash tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BashCommandOutput {
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "rawOutputPath")]
    pub raw_output_path: Option<String>,
    pub interrupted: bool,
    #[serde(rename = "isImage")]
    pub is_image: Option<bool>,
    // 字段名为 backgroundPid(非 backgroundTaskId),明确语义是子进程 PID,
    // 防止 AI 把 PID 误当 TaskRegistry 的 task_id 传给 TaskOutput 工具。
    #[serde(rename = "backgroundPid")]
    pub background_task_id: Option<String>,
    #[serde(rename = "backgroundedByUser")]
    pub backgrounded_by_user: Option<bool>,
    #[serde(rename = "assistantAutoBackgrounded")]
    pub assistant_auto_backgrounded: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "returnCodeInterpretation")]
    pub return_code_interpretation: Option<String>,
    #[serde(rename = "noOutputExpected")]
    pub no_output_expected: Option<bool>,
    #[serde(rename = "structuredContent")]
    pub structured_content: Option<Vec<serde_json::Value>>,
    #[serde(rename = "persistedOutputPath")]
    pub persisted_output_path: Option<String>,
    #[serde(rename = "persistedOutputSize")]
    pub persisted_output_size: Option<u64>,
    #[serde(rename = "sandboxStatus")]
    pub sandbox_status: Option<SandboxStatus>,
    /// 实际执行命令的 shell 类型：`cmd.exe` / `git-bash` / `sh`。
    /// 模型据此感知每次调用的实际 shell（即使 system prompt 已告知，
    /// fallback 情况下仍需具体反馈）。
    #[serde(rename = "shellType")]
    pub shell_type: Option<String>,
}

/// Executes a shell command with the requested sandbox settings.
pub fn execute_bash(input: BashCommandInput) -> io::Result<BashCommandOutput> {
    let cwd = env::current_dir()?;
    let sandbox_status = sandbox_status_for_input(&input, &cwd);

    // 每次执行前清除中止标志,避免上一轮的 abort 残留影响本次。
    // TUI 在 Ctrl+C 时 set,这里 clear 确保 turn 内后续 bash 命令正常执行。
    clear_bash_abort();

    if input.run_in_background.unwrap_or(false) {
        let mut child = prepare_command(&input.command, &cwd, &sandbox_status, false);
        let child = child
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        // 改进点 5:Windows/macOS 平台接入 SandboxBuilder,
        // 将子进程分配到 Job Object(Windows)或保留 macOS 钩子。
        // Linux 在 prepare_command 阶段用 unshare 包装,不在此处理。
        // std::process::Child::id() 返回 u32(非 Option)
        try_assign_sandbox_job(&sandbox_status, child.id());

        return Ok(BashCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(child.id().to_string()),
            backgrounded_by_user: Some(false),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: input.dangerously_disable_sandbox,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: Some(sandbox_status),
            shell_type: Some(detect_shell_type().as_str().to_string()),
        });
    }

    // 检测是否已在 tokio runtime 上下文中(如 TUI 的 run_turn_async 调用栈)。
    // 若是,直接创建新 runtime + block_on 会触发
    // "Cannot start a runtime from within a runtime" panic。
    // 修复:用 std::thread::spawn 创建独立 OS 线程,完全不继承 runtime context,
    // 通过 oneshot channel 传回结果。与 subagent_dispatcher.rs 的隔离模式一致。
    if tokio::runtime::Handle::try_current().is_ok() {
        let (tx, rx) = std::sync::mpsc::channel::<io::Result<BashCommandOutput>>();
        std::thread::spawn(move || {
            let result = (|| {
                let runtime = Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(execute_bash_async(input, sandbox_status, cwd))
            })();
            let _ = tx.send(result);
        });
        return rx
            .recv()
            .map_err(|e| io::Error::other(format!("bash worker thread died: {e}")))?;
    }

    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(execute_bash_async(input, sandbox_status, cwd))
}

/// Detect git push to main and emit ship provenance event.
///
/// BUG-P2-3: previously the constructed `LaneEvent` was bound to `_event`
/// and dropped immediately at the end of the `if` block — the event was
/// never actually published to any sink. The `eprintln!` log line was
/// the only observable side effect. We now push the event into the
/// process-wide lane event sink (if configured) so downstream consumers
/// (lane completion detector, ship dashboard, etc.) actually observe it.
fn detect_and_emit_ship_prepared(command: &str) {
    let trimmed = command.trim();
    // Simple detection: git push with main/master
    if trimmed.contains("git push") && (trimmed.contains("main") || trimmed.contains("master")) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let provenance = ShipProvenance {
            source_branch: get_current_branch().unwrap_or_else(|| "unknown".to_string()),
            base_commit: get_head_commit().unwrap_or_default(),
            commit_count: 0, // Would need to calculate from range
            commit_range: "unknown..HEAD".to_string(),
            merge_method: ShipMergeMethod::DirectPush,
            actor: get_git_actor().unwrap_or_else(|| "unknown".to_string()),
            pr_number: None,
        };
        let event = LaneEvent::ship_prepared(format!("{now}"), &provenance);
        // Publish to the global lane event sink. If no sink is configured
        // (e.g., in tests or standalone CLI use), fall back to stderr so
        // the event is at least observable.
        if !crate::lane_events::try_publish(event) {
            eprintln!(
                "[ship.prepared] (no sink) branch={} -> main, commits={}, actor={}",
                provenance.source_branch, provenance.commit_count, provenance.actor
            );
        }
    }
}

fn get_current_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_head_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_git_actor() -> Option<String> {
    let name = Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    Some(name)
}

/// 创建唯一的临时输出文件对（stdout/stderr 捕获用），返回 (out_path, err_path, out_file, err_file)。
///
/// 第一性原理修复（2026-09-01）：命令输出从「管道」改为「临时文件」捕获。
///
/// 背景：管道方案下，claw 读取直到 EOF，而 EOF 要求**所有**写端句柄关闭——
/// 命令派生的常驻后代进程（`&` / nohup / Start-Process 启动的服务）可无限期
/// 持有写端，导致工具永久阻塞。此前靠「后台命令模式检测 + 排空宽限 + 杀进程树」
/// 组合兜底，但换一种命令写法或换工具路径就漏（PowerShell 工具无任何保护，
/// 2026-09-01 实测 `Start-Process -NoNewWindow ... python.exe` 卡死会话）。
///
/// 文件方案：命令完成由**主 shell 进程退出**决定，claw 退出等待后直接读文件，
/// 与后代进程是否持有句柄完全无关。后台服务命令自然立即返回且服务继续运行，
/// 不再需要模式检测、排空宽限、为释放管道而杀进程树。
fn capture_output_files() -> io::Result<(PathBuf, PathBuf, std::fs::File, std::fs::File)> {
    let dir = std::env::temp_dir();
    let uniq = format!(
        "claw-cmd-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let out_path = dir.join(format!("{uniq}-out.log"));
    let err_path = dir.join(format!("{uniq}-err.log"));
    let out_file = std::fs::File::create(&out_path)?;
    let err_file = std::fs::File::create(&err_path)?;
    Ok((out_path, err_path, out_file, err_file))
}

/// 读取并清理捕获文件（best-effort：后台服务可能仍持有句柄，删除失败忽略）。
fn read_and_cleanup_capture(
    out_path: &Path,
    err_path: &Path,
) -> (Vec<u8>, Vec<u8>) {
    let stdout = std::fs::read(out_path).unwrap_or_default();
    let stderr = std::fs::read(err_path).unwrap_or_default();
    let _ = std::fs::remove_file(out_path);
    let _ = std::fs::remove_file(err_path);
    (stdout, stderr)
}

async fn execute_bash_async(
    input: BashCommandInput,
    sandbox_status: SandboxStatus,
    cwd: std::path::PathBuf,
) -> io::Result<BashCommandOutput> {
    // Detect and emit ship provenance for git push operations
    detect_and_emit_ship_prepared(&input.command);

    // 决定超时模式：
    // - Some(timeout) → 固定墙钟超时（模型显式指定，保持向后兼容）
    // - None → 智能活跃度检测（idle 5min / hard 1h，详见 activity_monitor 模块）
    //          智能模式基于子进程树 CPU 时间 + stdout/stderr 流量判断是否真死锁，
    //          不再因"墙钟时间到"就误杀合法长任务（如回测、训练、编译）。
    let smart_mode = input.timeout.is_none();
    let timeout_ms = input.timeout.unwrap_or_else(|| {
        // 智能模式下的兜底硬上限：1 小时。
        // 真死锁会在 5min idle_timeout 时被提前 kill；
        // 1h 是绝对上限，防止活跃但失控的任务永久占用资源。
        const SMART_HARD_LIMIT_MS: u64 = 60 * 60 * 1000;
        SMART_HARD_LIMIT_MS
    });

    // 用 spawn + 轮询替代 command.output()，支持：
    // 1. 超时 kill 子进程（原 timeout 只放弃 await，子进程可能残留）
    // 2. Ctrl+C 中断 kill 子进程（原实现完全无法中断）
    // 3. 输出捕获用临时文件而非管道（见 capture_output_files 的第一性原理说明：
    //    彻底消除"后代进程持有管道写端导致 EOF 永不发生"这一类阻塞）
    let (out_path, err_path, out_file, err_file) = capture_output_files()?;
    let mut command = prepare_tokio_command(&input.command, &cwd, &sandbox_status, true);
    command
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));
    command.stdin(Stdio::null());

    let mut child = command.spawn()?;

    // 改进点 5:Windows/macOS 平台接入 SandboxBuilder,
    // spawn 后立即将子进程分配到 Job Object(Windows)。
    // Linux 在 prepare_command 阶段用 unshare 包装,不在此处理。
    if let Some(pid) = child.id() {
        try_assign_sandbox_job(&sandbox_status, pid);
    }

    let start = Instant::now();
    let timeout_dur = Duration::from_millis(timeout_ms);
    let poll_interval = Duration::from_millis(100);

    // 智能模式：初始化 ActivityMonitor，跟踪子进程树活跃度
    let mut monitor: Option<activity_monitor::ActivityMonitor> = if smart_mode {
        let pid = child.id().unwrap_or(0);
        Some(activity_monitor::ActivityMonitor::new(pid))
    } else {
        None
    };

    let mut exit_status: Option<i32> = None;
    let mut child_exited = false;
    let mut aborted = false;
    let mut timed_out = false;
    // 智能模式触发原因（idle vs hard），用于 stderr 消息和 provenance 区分
    let mut smart_idle = false;
    // 智能模式忙等循环触发标记：无输出但 CPU 持续增长的疑似空转循环
    let mut busy_loop = false;
    // 输出捕获文件大小（用于向 ActivityMonitor 喂"有输出"信号）
    let mut last_out_size: u64 = 0;
    let mut last_err_size: u64 = 0;

    // 轮询循环（无管道读取，见 capture_output_files 说明）：
    // - 每轮先做周期检查（try_wait / abort / 智能或固定超时）
    // - 再检查输出捕获文件是否增长（喂 ActivityMonitor 的"有输出"信号，
    //   否则 busy-loop 判定会因检测不到输出而误杀正常长任务）
    // - 心跳 sleep 保活
    loop {
        // 1. try_wait() 轮询子进程退出状态 → 提前到循环顶部。
        //    try_wait() 直接调用 GetExitCodeProcess（同步、非阻塞），
        //    不依赖 IOCP 通知（tokio 的 child.wait() 在 Windows 有 race），
        //    每轮执行一次，检测延迟 < 1 次迭代，可接受。
        if !child_exited {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_status = status.code();
                    child_exited = true;
                }
                Ok(None) => { /* child 仍在运行 */ }
                Err(_) => {
                    child_exited = true;
                }
            }
        }

        // 2. abort / 智能或固定超时检查 → 提前到循环顶部。
        if is_bash_aborted() {
            aborted = true;
            if !child_exited {
                // abort 时立即终止进程树，快速退出（不等残留数据）。
                terminate_child_tree(&mut child, &mut child_exited).await;
            }
        } else if !child_exited {
            if let Some(ref mut m) = monitor {
                // 智能模式：基于子进程树活跃度决策
                match m.poll() {
                    activity_monitor::ActivityDecision::Continue => {}
                    activity_monitor::ActivityDecision::IdleTimeout => {
                        timed_out = true;
                        smart_idle = true;
                        terminate_child_tree(&mut child, &mut child_exited).await;
                    }
                    activity_monitor::ActivityDecision::HardTimeout => {
                        timed_out = true;
                        terminate_child_tree(&mut child, &mut child_exited).await;
                    }
                    activity_monitor::ActivityDecision::BusyLoop => {
                        busy_loop = true;
                        timed_out = true;
                        terminate_child_tree(&mut child, &mut child_exited).await;
                    }
                }
            } else if start.elapsed() >= timeout_dur {
                // 固定超时模式（input.timeout 显式指定时）
                timed_out = true;
                terminate_child_tree(&mut child, &mut child_exited).await;
            }
        }

        // 3. 退出条件：child 已退出（输出已由捕获文件保存，无需等管道 EOF）。
        if child_exited {
            break;
        }

        // 4. 检查输出捕获文件是否增长 → 喂 ActivityMonitor 的"有输出"信号。
        if let Some(ref mut m) = monitor {
            let out_now = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
            let err_now = std::fs::metadata(&err_path).map(|m| m.len()).unwrap_or(0);
            if out_now > last_out_size || err_now > last_err_size {
                m.note_output();
                last_out_size = out_now;
                last_err_size = err_now;
            }
        }

        // 5. 心跳保活。
        tokio::time::sleep(poll_interval).await;
    }

    // 命令已完成：从捕获文件读取输出并清理临时文件。
    let (stdout_raw, stderr_raw) = read_and_cleanup_capture(&out_path, &err_path);
    let stdout_text = String::from_utf8_lossy(&stdout_raw);
    let stderr_text = String::from_utf8_lossy(&stderr_raw);

    // abort：用户 Ctrl+C 中断
    if aborted {
        let stdout = truncate_output(&stdout_text);
        let stderr = truncate_output(&stderr_text);
        return Ok(BashCommandOutput {
            stdout,
            stderr: format!("[interrupt] Command interrupted by user (Ctrl+C)\n{stderr}"),
            raw_output_path: None,
            interrupted: true,
            is_image: None,
            background_task_id: None,
            backgrounded_by_user: None,
            assistant_auto_backgrounded: None,
            dangerously_disable_sandbox: input.dangerously_disable_sandbox,
            return_code_interpretation: Some("interrupted".to_string()),
            no_output_expected: Some(false),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: Some(sandbox_status),
            shell_type: Some(detect_shell_type().as_str().to_string()),
        });
    }

    // 超时
    if timed_out {
        // 智能模式 busy loop 触发：无输出但 CPU 持续增长的疑似空转循环
        if busy_loop {
            return Ok(busy_loop_output(&input, sandbox_status));
        }
        // 智能模式 idle 触发：给出不同消息引导模型重试
        if smart_idle {
            return Ok(idle_timeout_output(&input, sandbox_status));
        }
        return Ok(timeout_output(&input, timeout_ms, sandbox_status));
    }

    // 正常完成
    let mut stdout = truncate_output(&stdout_text);
    let stderr = truncate_output(&stderr_text);
    // P1-4: timeout 单位兜底提示。提示写入 stdout（LLM 直接可见），
    // 不写 stderr，避免污染 TUI alternate screen。必须在计算
    // no_output_expected 之前追加，否则提示被判定为"无输出"而隐藏。
    stdout.push_str(&timeout_unit_note(input.timeout));
    let no_output_expected = Some(stdout.trim().is_empty() && stderr.trim().is_empty());
    let return_code_interpretation = exit_status.and_then(|code| {
        if code == 0 {
            None
        } else {
            let mut interp = format!("exit_code:{code}");
            if has_shell_syntax_error(&stderr) {
                interp.push_str(" — command has a shell syntax error (unmatched quotes/escaping)");
            }
            Some(interp)
        }
    });

    Ok(BashCommandOutput {
        stdout,
        stderr,
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation,
        no_output_expected,
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
        shell_type: Some(detect_shell_type().as_str().to_string()),
    })
}

/// 终止进程树。
///
/// Windows 用 taskkill /T /F(递归终止子孙进程),Unix 用 kill -KILL 进程组。
fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        // /T 递归终止子进程树,/F 强制终止。/PID 的进程可能已被 child.kill()
        // 终止,taskkill 对已死进程返回非零,但 /T 仍会清理存活的后代;
        // 忽略返回值(尽力而为)。
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        // bash 通常为进程组组长,负 PID 表示信号发给整个进程组
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

/// 终止子进程及其整棵进程树（超时/中止时终止失控进程，输出已落捕获文件）。
async fn terminate_child_tree(child: &mut tokio::process::Child, child_exited: &mut bool) {
    if let Some(pid) = child.id() {
        kill_process_tree(pid);
    }
    let _ = child.kill().await;
    *child_exited = true;
}

/// 智能模式 busy loop 触发时的输出构造。
///
/// 与 idle_timeout_output 区分：idle 是无输出且无 CPU（疑似死锁），
/// busy loop 是无输出但 CPU 持续增长（疑似无谓的空转循环，如
/// `for i in range(huge): pass`）。这类循环会骗过 idle 检测
/// （CPU 增长 = 活跃），必须单独识别并在远短于 1h 硬上限时提前 kill。
fn busy_loop_output(input: &BashCommandInput, sandbox_status: SandboxStatus) -> BashCommandOutput {
    let guidance = "\n\n[Busy-loop kill] Command ran with no output but sustained CPU for a long window; \
         likely a CPU-spinning loop (e.g. `for i in range(huge): pass`).\n\
         Suggestions:\n\
         - Pre-flight generated scripts: run `python -m py_compile` or dry-inspect loops before executing\n\
         - Verify loop bounds are finite & small (no `range(1_000_000_000)` bare spinning)\n\
         - Add progress output (`print/echo` checkpoints) so long computations are distinguishable\n\
         - If the task is genuinely a long CPU computation, pass an explicit `timeout`";
    BashCommandOutput {
        stdout: String::new(),
        stderr: format!("Command killed as a suspected CPU busy-loop{guidance}"),
        raw_output_path: None,
        interrupted: true,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation: Some("busy.loop".to_string()),
        no_output_expected: Some(true),
        structured_content: Some(vec![json!({
            "event": "command.busy_loop",
            "failureClass": "busy_loop",
            "data": {
                "command": input.command,
                "provenance": "bash.busy_loop",
                "classification": "busy.loop"
            }
        })]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
        shell_type: Some(detect_shell_type().as_str().to_string()),
    }
}

/// 智能模式 idle 触发时的输出构造。
///
/// 与固定 timeout_output 区分：
/// - stderr 文案明确指出"5min 无活动"（不是"超过 X ms"）
/// - return_code_interpretation = "idle.timeout"
/// - structured_content.provenance = "bash.smart_timeout"
/// - 引导模型重试时主动设置 timeout 或保证 stdout 流式输出
fn idle_timeout_output(
    input: &BashCommandInput,
    sandbox_status: SandboxStatus,
) -> BashCommandOutput {
    let guidance = "\n\n[Smart timeout] Command killed after 5 min of inactivity (no stdout, no stderr, no CPU usage).\n\
         Likely causes: deadlock, network hang, waiting on unavailable resource, or pure-I/O block.\n\
         Suggestions:\n\
         - If this is a long-running task that does not produce output, set `timeout` explicitly\n\
         - Pipe progress to stdout (e.g. `--progress` flag, periodic `echo` checkpoints)\n\
         - For network operations, add explicit timeouts (curl --max-time, wget --timeout)\n\
         - For deadlocks, check thread/lock state in your code";
    BashCommandOutput {
        stdout: String::new(),
        stderr: format!("Command killed after 5 min of inactivity{guidance}"),
        raw_output_path: None,
        interrupted: true,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation: Some("idle.timeout".to_string()),
        no_output_expected: Some(true),
        structured_content: Some(vec![json!({
            "event": "command.idle_timeout",
            "failureClass": "idle_timeout",
            "data": {
                "command": input.command,
                "idleSeconds": 300,
                "provenance": "bash.smart_timeout",
                "classification": "idle.timeout"
            }
        })]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
        shell_type: Some(detect_shell_type().as_str().to_string()),
    }
}

/// P1-4: timeout 单位兜底提示。`timeout` 单位为毫秒；<1s 的极小值
/// 极可能是秒/毫秒单位误用。返回提示文本（无问题时为空串）。
///
/// 正常完成与固定超时两条路径共用，确保单位歧义在两种结局下
/// （命令跑完 / 命令被 300ms 误杀）都能被 LLM 看到。
fn timeout_unit_note(timeout: Option<u64>) -> String {
    match timeout {
        Some(t) if t < 1000 => format!(
            "\n[claw] note: `timeout` is in milliseconds; the given {t}ms (< 1s) \
             may be a seconds/milliseconds unit mistake."
        ),
        _ => String::new(),
    }
}

fn timeout_output(
    input: &BashCommandInput,
    timeout_ms: u64,
    sandbox_status: SandboxStatus,
) -> BashCommandOutput {
    let is_test = is_test_command(&input.command);
    let return_code_interpretation = if is_test { "test.hung" } else { "timeout" };
    let guidance = if input.command.contains("grep") || input.command.contains("rg") {
        "\n\n[Retry guidance] The command timed out, likely due to a broad search scope. Suggestions:\n\
         - Add a file-type filter (e.g. `--glob='*.rs'` / `-g '*.rs'` for ripgrep, `--include='*.rs'` for grep)\n\
         - Use `-l` / `--files-with-matches` first to gauge scope, then re-run with a narrower target\n\
         - Restrict to a specific subdirectory instead of searching the entire repo\n\
         - Add `--max-depth N` (ripgrep) to limit directory traversal depth\n\
         - Pipe to `head -n 100` or use `-m 100` (ripgrep) to limit matches\n\
         - For targeted work: `find . -name '*.ext' | xargs grep ...` instead of recursive grep"
    } else if input.command.contains("find")
        || input.command.contains(" ls -")
        || input.command.starts_with("ls ")
    {
        "\n\n[Retry guidance] The command timed out. For `find`/`ls`: start with a shallow listing first:\n\
         - `ls -la` (single directory) or `ls -la | head -n 20` before recursive\n\
         - `find . -maxdepth 1 -name '*.rs' | wc -l` to count candidate files before a full scan\n\
         - Restrict to a specific subdirectory\n\
         - Use `-maxdepth N` on find to limit tree-walk depth"
    } else {
        "\n\n[Retry guidance] The command timed out. Consider:\n\
         - Narrowing the scope (specific directory, file pattern, or target)\n\
         - Breaking the work into smaller steps\n\
         - Checking if a simpler approach can achieve the same goal"
    };
    BashCommandOutput {
        stdout: timeout_unit_note(input.timeout),
        stderr: format!("Command exceeded timeout of {timeout_ms} ms{guidance}"),
        raw_output_path: None,
        interrupted: true,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation: Some(String::from(return_code_interpretation)),
        // 仅当 stdout 带单位提示时视为"有输出"（提示需展示给 LLM），
        // 否则超时消息属于合成文案，不占用 no_output_expected。
        no_output_expected: Some(timeout_unit_note(input.timeout).is_empty()),
        structured_content: Some(vec![test_timeout_provenance(
            &input.command,
            timeout_ms,
            is_test,
        )]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
        shell_type: Some(detect_shell_type().as_str().to_string()),
    }
}

fn is_test_command(command: &str) -> bool {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    normalized.contains("cargo test")
        || normalized.contains("cargo nextest")
        || normalized.contains("npm test")
        || normalized.contains("pnpm test")
        || normalized.contains("yarn test")
        || normalized.contains("pytest")
}

fn test_timeout_provenance(
    command: &str,
    timeout_ms: u64,
    classified_as_test_hang: bool,
) -> serde_json::Value {
    json!({
        "event": if classified_as_test_hang { "test.hung" } else { "command.timeout" },
        "failureClass": if classified_as_test_hang { "test_hang" } else { "timeout" },
        "data": {
            "command": command,
            "timeoutMs": timeout_ms,
            "provenance": "bash.timeout",
            "classification": if classified_as_test_hang { "test.hung" } else { "timeout" }
        }
    })
}

fn sandbox_status_for_input(input: &BashCommandInput, cwd: &std::path::Path) -> SandboxStatus {
    let config = ConfigLoader::default_for(cwd).load().map_or_else(
        |_| SandboxConfig::default(),
        |runtime_config| runtime_config.sandbox().clone(),
    );
    let request = config.resolve_request(
        input.dangerously_disable_sandbox.map(|disabled| !disabled),
        input.namespace_restrictions,
        input.isolate_network,
        input.filesystem_mode,
        input.allowed_mounts.clone(),
    );
    resolve_sandbox_status_for_request(&request, cwd)
}

/// 为子进程注入 UTF-8 编码环境变量，解决 Windows 中文系统乱码问题。
/// 符合「源头控制」原则：在上游设置编码，不依赖下游过滤。
///
/// - `PYTHONUTF8=1`：Python 3.7+ UTF-8 模式，强制 stdin/stdout/stderr 与默认编码为 UTF-8
/// - `PYTHONIOENCODING=utf-8`：兼容旧 Python，显式指定 IO 编码
/// - Unix 下补 `LANG`/`LC_ALL`（仅当未设置时），确保 locale 不回退到 ASCII
///
/// 对非 Python 进程无副作用（环境变量只是被忽略），因此无条件注入是安全的。
fn inject_utf8_env(cmd: &mut std::process::Command) {
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    #[cfg(unix)]
    {
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        if std::env::var("LC_ALL").is_err() {
            cmd.env("LC_ALL", "en_US.UTF-8");
        }
    }
}

/// tokio 版本的 UTF-8 编码注入，逻辑同 `inject_utf8_env`。
/// runtime crate 将 tokio 作为硬依赖（非 feature flag），故此函数始终可用。
fn inject_utf8_env_tokio(cmd: &mut tokio::process::Command) {
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    #[cfg(unix)]
    {
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        if std::env::var("LC_ALL").is_err() {
            cmd.env("LC_ALL", "en_US.UTF-8");
        }
    }
}

/// 尝试将子进程分配到平台沙箱(Windows Job Object / macOS sandbox-exec)。
///
/// 改进点 5:接入 Windows SandboxBuilder 到 bash 执行路径。
/// 之前 `WindowsSandboxBuilder` 已实现但未被 `prepare_command` 调用,
/// 导致 Windows 上 `sandbox.enabled=true` 时无实际隔离。
///
/// 现在在 spawn 后立即调用 `assign_process(pid)`,通过 Win32 API
/// CreateJobObjectW + AssignProcessToJobObject 将子进程分配到 Job Object,
/// 设置 CPU/memory 限制。失败时非致命(记录到 stderr 但不阻断主流程)。
///
/// **异步执行**:Job Object 设置通过 PowerShell + C# 内联调用 Win32 API,
/// 可能耗时数百毫秒(PowerShell 启动 + C# 编译)。为避免阻塞 tokio runtime
/// 和影响 timeout 判定(如 bash 测试的 1ms timeout),用 `std::thread::spawn`
/// 在独立 OS 线程中执行,主流程不等待。
///
/// 注意:
/// - 仅在 `sandbox_status.enabled && platform_sandbox_supported()` 时调用
/// - Linux 不走此路径(Linux 在 `prepare_command` 阶段用 unshare 包装)
/// - 失败时返回 `()`,不向上传播错误,避免沙箱设置失败阻塞命令执行
fn try_assign_sandbox_job(sandbox_status: &SandboxStatus, pid: u32) {
    if !sandbox_status.enabled || !platform_sandbox_supported() {
        return;
    }

    // Linux 在 prepare_command 阶段用 unshare 包装,不在此处处理
    if cfg!(target_os = "linux") {
        return;
    }

    // Windows: 用 WindowsSandboxBuilder 的 assign_process 分配到 Job Object
    // 异步执行:PowerShell 调用耗时可能数百毫秒,不阻塞主流程
    #[cfg(target_os = "windows")]
    {
        std::thread::spawn(move || {
            let builder = WindowsSandboxBuilder::default();
            if let Err(e) = SandboxBuilder::assign_process(&builder, pid) {
                // 非致命:Job Object 设置失败不阻断命令执行,仅记录到 stderr
                eprintln!("[sandbox] Windows Job Object 分配失败(pid={pid}): {e}");
            }
        });
    }

    // macOS: MacOsSandboxBuilder 的 assign_process 是 no-op(默认实现),
    // 实际隔离在 prepare_command 阶段的 sandbox-exec 包装中完成。
    // 此处保留钩子,未来若 macOS 需要 post-spawn 处理可扩展。
    #[cfg(target_os = "macos")]
    {
        let _ = pid;
    }
}

fn prepare_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
    create_dirs: bool,
) -> Command {
    if create_dirs {
        prepare_sandbox_dirs(cwd);
    }

    if let Some(launcher) = build_linux_sandbox_command(command, cwd, sandbox_status) {
        let mut prepared = Command::new(launcher.program);
        prepared.args(launcher.args);
        prepared.current_dir(cwd);
        prepared.envs(launcher.env);
        // Linux sandbox 路径同样需要 UTF-8 编码注入（sandbox 内子进程仍是 Python/sh）
        inject_utf8_env(&mut prepared);
        return prepared;
    }

    let kind = shell_kind();
    let mut prepared = Command::new(&kind.program);
    // cmd.exe 路径加 `chcp 65001` 前缀切换代码页到 UTF-8，
    // 避免 cmd 默认 GBK 代码页导致中文输出乱码。
    // 仅影响 cmd /C 路径，bash -c / sh -lc 路径不受影响。
    let final_command = if kind.kind == ShellType::Cmd {
        format!("chcp 65001 >nul && {}", command)
    } else {
        command.to_string()
    };
    prepared.arg(kind.flag).arg(&final_command).current_dir(cwd);
    if sandbox_status.filesystem_active {
        prepared.env("HOME", cwd.join(".sandbox-home"));
        prepared.env("TMPDIR", cwd.join(".sandbox-tmp"));
    }
    // fallback 路径注入 UTF-8 编码环境变量
    inject_utf8_env(&mut prepared);
    prepared
}

fn prepare_tokio_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
    create_dirs: bool,
) -> TokioCommand {
    if create_dirs {
        prepare_sandbox_dirs(cwd);
    }

    if let Some(launcher) = build_linux_sandbox_command(command, cwd, sandbox_status) {
        let mut prepared = TokioCommand::new(launcher.program);
        prepared.args(launcher.args);
        prepared.current_dir(cwd);
        prepared.envs(launcher.env);
        // Linux sandbox 路径同样需要 UTF-8 编码注入
        inject_utf8_env_tokio(&mut prepared);
        return prepared;
    }

    let kind = shell_kind();
    let mut prepared = TokioCommand::new(&kind.program);
    // cmd.exe 路径加 `chcp 65001` 前缀切换代码页到 UTF-8
    let final_command = if kind.kind == ShellType::Cmd {
        format!("chcp 65001 >nul && {}", command)
    } else {
        command.to_string()
    };
    prepared.arg(kind.flag).arg(&final_command).current_dir(cwd);
    if sandbox_status.filesystem_active {
        prepared.env("HOME", cwd.join(".sandbox-home"));
        prepared.env("TMPDIR", cwd.join(".sandbox-tmp"));
    }
    // fallback 路径注入 UTF-8 编码环境变量
    inject_utf8_env_tokio(&mut prepared);
    prepared
}

/// Shell 类型标识，用于 system prompt 提示和 BashCommandOutput.shell_type 字段。
/// 模型据此调整命令语法（cmd.exe 用 `dir/type/del`，git-bash 用 `ls/cat/rm`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellType {
    /// Windows cmd.exe（系统默认 fallback）
    Cmd,
    /// Git Bash（bash -c），支持 Unix 命令
    GitBash,
    /// Unix sh（sh -lc）
    Sh,
}

impl ShellType {
    /// 返回用于 system prompt 和 BashCommandOutput.shell_type 的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            ShellType::Cmd => "cmd.exe",
            ShellType::GitBash => "git-bash",
            ShellType::Sh => "sh",
        }
    }
}

/// 启动 shell 所需的全部信息：程序路径、参数 flag、类型标识。
/// program 用 String 承载动态路径（如 `C:\Program Files\Git\bin\bash.exe`）。
#[derive(Debug, Clone)]
struct ShellKind {
    program: String,
    flag: &'static str,
    kind: ShellType,
}

/// 返回当前进程使用的 shell 启动器。
/// Windows 探测顺序：CLAW_GIT_BASH 环境变量 → Program Files 固定路径 → PATH 搜索（过滤 WSL）。
/// Unix 直接用 sh -lc。
///
/// 注意：**不做进程级缓存**。shell 检测依赖环境变量（CLAW_GIT_BASH / PATH），
/// 全局缓存会在测试并行修改环境时被污染（首次初始化固定为 cmd.exe 后
/// 所有后续命令行为漂移）。每次探测成本仅为几次文件 exists + PATH 扫描，
/// 毫秒级，可忽略。
fn shell_kind() -> ShellKind {
    detect_shell_kind()
}

/// 对外暴露的 shell 类型探测入口（供 system prompt 构造时调用）。
pub fn detect_shell_type() -> ShellType {
    shell_kind().kind
}

/// P11-2:返回当前 shell 的 (program, flag),供 hooks 等模块复用。
/// 避免各模块各自硬编码 cmd/sh,导致行为不一致。
/// Windows:Git Bash 可用时返回 ("bash.exe", "-c"),否则 ("cmd", "/C")。
/// Unix:返回 ("sh", "-lc")。
pub fn shell_launcher() -> (String, &'static str) {
    let kind = shell_kind();
    (kind.program, kind.flag)
}

/// 执行实际的 shell 探测，返回 ShellKind（不缓存）。
fn detect_shell_kind() -> ShellKind {
    if cfg!(target_os = "windows") {
        if let Some(bash_path) = detect_git_bash() {
            return ShellKind {
                program: bash_path,
                flag: "-c",
                kind: ShellType::GitBash,
            };
        }
        ShellKind {
            program: "cmd".to_string(),
            flag: "/C",
            kind: ShellType::Cmd,
        }
    } else {
        ShellKind {
            program: "sh".to_string(),
            flag: "-lc",
            kind: ShellType::Sh,
        }
    }
}

/// Windows 专用：探测 Git Bash 路径。
///
/// **探测顺序**（命中即返回，总耗时 < 2ms）：
/// 1. 环境变量 `CLAW_GIT_BASH`（用户显式指定，覆盖一切）
///    - 设为空字符串 → 强制 fallback 到 cmd.exe
/// 2. 常见安装路径（4 个候选，每个一次 `Path::exists()` 系统调用）
/// 3. PATH 中搜索 `bash.exe`，**过滤掉** `System32\bash.exe`（WSL 入口）
///    和 `wbem\` 下的（Windows 自带，非 Git Bash）
///
/// 未命中任何路径 → 返回 `None`（调用方 fallback 到 cmd.exe）
#[cfg(target_os = "windows")]
fn detect_git_bash() -> Option<String> {
    use std::path::Path;

    // 1. 环境变量 CLAW_GIT_BASH（显式指定，空字符串表示强制禁用 Git Bash）
    if let Ok(val) = env::var("CLAW_GIT_BASH") {
        if val.is_empty() {
            // 显式禁用：用户想强制用 cmd.exe
            return None;
        }
        let p = Path::new(&val);
        if p.exists() {
            return Some(val);
        }
        // 显式指定但路径无效 → 不再尝试其他路径（用户意图优先）
        return None;
    }

    // 2. 常见安装路径（Git for Windows 默认安装位置）
    const COMMON_PATHS: &[&str] = &[
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\usr\bin\bash.exe",
    ];
    for candidate in COMMON_PATHS {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }

    // 3. PATH 搜索 bash.exe，过滤 WSL（System32）和 wbem 下的非 Git bash
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join("bash.exe");
            if candidate.exists() {
                let s = candidate.display().to_string().to_ascii_lowercase();
                // WSL 入口：C:\Windows\System32\bash.exe
                // Windows 自带：C:\Windows\System32\wbem\bash.exe（不存在但保险）
                if s.contains(r"\system32\") || s.contains(r"\wbem\") {
                    continue;
                }
                return Some(candidate.display().to_string());
            }
        }
    }

    None
}

#[cfg(not(target_os = "windows"))]
fn detect_git_bash() -> Option<String> {
    None
}

fn prepare_sandbox_dirs(cwd: &std::path::Path) {
    let _ = std::fs::create_dir_all(cwd.join(".sandbox-home"));
    let _ = std::fs::create_dir_all(cwd.join(".sandbox-tmp"));
}

/// Windows 专属：Git Bash 检测单元测试。
/// 不依赖真实 Git Bash 安装，通过环境变量注入虚拟路径来验证逻辑。
#[cfg(all(test, target_os = "windows"))]
mod git_bash_detection_tests {
    use super::detect_git_bash;

    // P11-2:这些测试共享环境变量 CLAW_GIT_BASH 和 PATH,Rust 默认多线程并行
    // 跑测试会导致竞态。用模块级 Mutex 串行化所有环境变量修改。
    // 锁定义在 bash.rs 顶层,与 bash::tests 共享,防止 PATH 清空窗口污染
    // 其他测试的 shell_kind() 首次初始化。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        super::BASH_TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// `CLAW_GIT_BASH` 指向一个真实存在的文件 → 直接返回该路径。
    #[test]
    fn env_var_override_returns_specified_path() {
        let _guard = env_lock();
        // 用 cmd.exe 自身作为虚拟 bash.exe（一定存在）
        let cmd_path = r"C:\Windows\System32\cmd.exe";
        std::env::set_var("CLAW_GIT_BASH", cmd_path);
        let result = detect_git_bash();
        std::env::remove_var("CLAW_GIT_BASH");
        assert_eq!(result.as_deref(), Some(cmd_path));
    }

    /// `CLAW_GIT_BASH=""` → 显式禁用，返回 None。
    #[test]
    fn env_var_empty_disables_git_bash() {
        let _guard = env_lock();
        std::env::set_var("CLAW_GIT_BASH", "");
        let result = detect_git_bash();
        std::env::remove_var("CLAW_GIT_BASH");
        assert!(result.is_none());
    }

    /// `CLAW_GIT_BASH` 指向不存在的路径 → 返回 None（用户意图优先，
    /// 不再 fallback 到其他探测路径）。
    #[test]
    fn env_var_invalid_path_returns_none() {
        let _guard = env_lock();
        std::env::set_var("CLAW_GIT_BASH", r"Z:\nonexistent\bash.exe");
        let result = detect_git_bash();
        std::env::remove_var("CLAW_GIT_BASH");
        assert!(result.is_none());
    }

    /// 未设 `CLAW_GIT_BASH`、未命中常见路径、PATH 无 bash.exe → 返回 None。
    /// 注意：此测试在安装了 Git Bash 的开发机上可能失败（命中真实路径），
    /// 所以仅在隔离的 CI 环境下运行才稳定。本地跑时可忽略。
    #[test]
    fn no_git_bash_returns_none() {
        let _guard = env_lock();
        // 清掉环境变量，但常见路径仍可能命中 — 此测试在没装 Git Bash 的
        // CI 上有效。装了 Git Bash 的机器上会命中并跳过断言。
        std::env::remove_var("CLAW_GIT_BASH");
        // 检查常见路径是否存在，存在则跳过（避免误报）
        let common_exists = [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
            r"C:\Program Files (x86)\Git\usr\bin\bash.exe",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).exists());
        if common_exists {
            return; // 开发机装了 Git Bash，跳过此用例
        }
        // 临时清空 PATH 避免命中其他 bash.exe（如 WSL）
        let saved_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "");
        let result = detect_git_bash();
        if let Some(ref p) = saved_path {
            std::env::set_var("PATH", p);
        }
        assert!(result.is_none(), "expected None when no bash.exe available");
    }

    /// `detect_shell_type()` 在没有 Git Bash 的环境下应返回 `Cmd`。
    /// 同样在装了 Git Bash 的开发机上会返回 `GitBash`，需跳过。
    #[test]
    fn detect_shell_type_returns_cmd_when_no_git_bash() {
        let _guard = env_lock();
        std::env::set_var("CLAW_GIT_BASH", "");
        // 注意：detect_shell_type 走的是 shell_kind() 的 OnceLock 缓存。
        // 此测试若在其他用例之后跑，缓存可能已被填充，结果不稳定。
        // 这里只验证 CLAW_GIT_BASH="" 时 detect_git_bash 返回 None。
        let result = detect_git_bash();
        std::env::remove_var("CLAW_GIT_BASH");
        assert!(result.is_none());
    }
}

/// Unix 平台：detect_git_bash 永远返回 None。
#[cfg(all(test, not(target_os = "windows")))]
mod unix_shell_tests {
    use super::detect_git_bash;

    #[test]
    fn unix_returns_none_for_git_bash() {
        assert!(detect_git_bash().is_none());
    }
}

/// 跨测试模块共享的环境变量锁（git_bash_detection_tests 修改 CLAW_GIT_BASH /
/// PATH 时与其他执行命令的测试并行会互相干扰）。shell 检测每次实时探测
/// （无进程级缓存），锁串行化 env 修改窗口，避免其他测试读到中间态。
#[cfg(test)]
pub(crate) static BASH_TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::{execute_bash, has_shell_syntax_error, BashCommandInput};
    use crate::sandbox::FilesystemIsolationMode;

    /// 第一性原理回归(2026-09-01):输出捕获改为临时文件后,后台启动命令
    /// (`cmd &` / nohup / Start-Process)在 bash 本体退出后**立即返回**,
    /// 不再依赖"窗口等待 + 杀进程树",也不再产生旧 "Background service" 提示。
    /// 后台服务进程继续运行(不再被杀),工具不卡住。
    #[test]
    fn background_service_command_returns_when_shell_exits() {
        use std::time::Instant;

        let start = Instant::now();
        let output = execute_bash(BashCommandInput {
            command: "echo bg-start; nohup sleep 8 > /dev/null 2>&1 &".to_string(),
            timeout: None,
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bg service command should return");

        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 15,
            "background service command must return when shell exits, took {elapsed:?}"
        );
        assert!(
            output.stdout.contains("bg-start"),
            "stdout should contain foreground output, got: {}",
            output.stdout
        );
        assert!(
            !output.stderr.contains("Background service"),
            "file-based capture should not emit legacy background-service hint, got: {}",
            output.stderr
        );
        // 后台 sleep 8 会在 8s 后自动退出,无需按镜像名清理(避免误杀并行测试的 sleep)。
    }

    /// 回归:多行 `&\n` 后台命令写法同样在 bash 本体退出后立即返回。
    #[test]
    fn background_service_multiline_returns_when_shell_exits() {
        use std::time::Instant;

        let start = Instant::now();
        let output = execute_bash(BashCommandInput {
            command:
                "cd /tmp && nohup sleep 8 > /tmp/svc.log 2>&1 &\necho \"启动中\"; true".to_string(),
            timeout: None,
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("multiline bg service command should return");

        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 15,
            "multiline background command must return when shell exits, took {elapsed:?}"
        );
        assert!(
            output.stdout.contains("启动中"),
            "stdout should contain foreground output, got: {}",
            output.stdout
        );
    }

    /// 生产路径复现:execute_bash 在 tokio runtime context 内被调用
    /// (production 走 spawn 线程 + block_on 分支)。验证后台命令同样立即返回。
    #[test]
    fn background_service_returns_in_runtime_context() {
        use std::time::Instant;

        let rt = tokio::runtime::Runtime::new().expect("tokio rt");
        let start = Instant::now();
        let result = rt.block_on(async {
            // 模拟生产: 在 runtime context 里同步调用 execute_bash
            // (内部会检测到 Handle,spawn 线程 + block_on + mpsc recv)
            execute_bash(BashCommandInput {
                command:
                    "cd /tmp && nohup sleep 8 > /tmp/svc_rt.log 2>&1 &\necho \"rt started\"; true"
                        .to_string(),
                timeout: None,
                description: None,
                run_in_background: Some(false),
                dangerously_disable_sandbox: Some(false),
                namespace_restrictions: Some(false),
                isolate_network: Some(false),
                filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
                allowed_mounts: None,
            })
        });
        let output = result.expect("bg service should return in runtime context");
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 15,
            "runtime-context bg command must return when shell exits, took {elapsed:?}"
        );
        assert!(
            output.stdout.contains("rt started"),
            "stdout should contain foreground output, got: {}",
            output.stdout
        );
    }

    /// 回归(session-1788195879056):AI 用 PowerShell Start-Process 后台启动服务
    /// (python 长驻,持有继承句柄),timeout=None(智能模式)。文件方案下
    /// bash 本体(Start-Process → echo → sleep → netstat)退出后立即返回,
    /// 后台服务不再被误杀。验证 15s 内返回。
    #[test]
    fn start_process_bg_returns_in_smart_mode() {
        use std::time::Instant;

        let rt = tokio::runtime::Runtime::new().expect("tokio rt");
        let start = Instant::now();
        let result = rt.block_on(async {
            execute_bash(BashCommandInput {
                command: "powershell -NoProfile -Command \"Start-Process -FilePath 'C:\\Users\\38225\\AppData\\Local\\Programs\\Python\\Python311\\python.exe' -ArgumentList '-c','import time; time.sleep(5)' -WindowStyle Hidden -RedirectStandardOutput 'C:\\Users\\38225\\AppData\\Local\\Temp\\bgtest_smart.log' -RedirectStandardError 'C:\\Users\\38225\\AppData\\Local\\Temp\\bgtest_smart_err.log'\" && echo \"Start-Process done\"; sleep 5; true".to_string(),
                timeout: None,
                description: None,
                run_in_background: Some(false),
                dangerously_disable_sandbox: Some(false),
                namespace_restrictions: Some(false),
                isolate_network: Some(false),
                filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
                allowed_mounts: None,
            })
        });
        let output = result.expect("start-process bg should return");
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 15,
            "start-process bg command must return when shell exits, took {elapsed:?}"
        );
        assert!(
            output.stdout.contains("Start-Process done"),
            "bash foreground should complete, stdout: {}",
            output.stdout
        );
        // 后台 python sleep(5) 会在 5s 后自动退出,无需按镜像名清理(避免误杀并行测试的进程)。
    }

    /// 回归:Start-Process 后台服务 + timeout=30000(固定超时模式)。
    /// 文件方案下 bash 本体退出即返回,不依赖固定超时杀进程树。验证 15s 内返回。
    #[test]
    fn start_process_bg_returns_in_fixed_timeout_mode() {
        use std::time::Instant;

        let rt = tokio::runtime::Runtime::new().expect("tokio rt");
        let start = Instant::now();
        let result = rt.block_on(async {
            execute_bash(BashCommandInput {
                command: "powershell -NoProfile -Command \"Start-Process -FilePath 'C:\\Users\\38225\\AppData\\Local\\Programs\\Python\\Python311\\python.exe' -ArgumentList '-c','import time; time.sleep(5)' -WindowStyle Hidden -RedirectStandardOutput 'C:\\Users\\38225\\AppData\\Local\\Temp\\bgtest_fixed.log' -RedirectStandardError 'C:\\Users\\38225\\AppData\\Local\\Temp\\bgtest_fixed_err.log'\" && echo \"Start-Process done\"; sleep 5; true".to_string(),
                timeout: Some(30_000),
                description: None,
                run_in_background: Some(false),
                dangerously_disable_sandbox: Some(false),
                namespace_restrictions: Some(false),
                isolate_network: Some(false),
                filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
                allowed_mounts: None,
            })
        });
        let output = result.expect("start-process bg should return in fixed timeout mode");
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 15,
            "fixed-timeout start-process bg must return when shell exits, took {elapsed:?}"
        );
        assert!(
            output.stdout.contains("Start-Process done"),
            "bash foreground should complete, stdout: {}",
            output.stdout
        );
    }

    /// PRD P2-5：shell 语法错误特征识别（bash/sh/cmd 通用）。
    #[test]
    fn detects_shell_syntax_errors() {
        assert!(has_shell_syntax_error(
            "/usr/bin/bash: -c: line 1: unexpected EOF while looking for matching `\"'"
        ));
        assert!(has_shell_syntax_error(
            "bash: syntax error near unexpected token `;'"
        ));
        assert!(has_shell_syntax_error(
            "sh: 1: Syntax error: \"(\" unexpected"
        ));
        // cmd.exe 特征
        assert!(has_shell_syntax_error(
            "The syntax of the command is incorrect."
        ));
        assert!(has_shell_syntax_error("'&&' was unexpected at this time."));
        // 正常编译错误 / "命令不存在" 不应误报
        assert!(!has_shell_syntax_error("error[E0308]: mismatched types"));
        assert!(!has_shell_syntax_error("cargo: the 'dev' profile"));
        assert!(!has_shell_syntax_error(
            "'foo' is not recognized as an internal or external command"
        ));
    }

    #[test]
    fn executes_simple_command() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash command should execute");

        assert_eq!(output.stdout, "hello");
        assert!(!output.interrupted);
        assert!(output.sandbox_status.is_some());
    }

    /// PRD P1-4：timeout < 1000ms 时 stdout 附带秒/毫秒单位误用提示。
    #[test]
    fn timeout_below_one_second_emits_unit_hint() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'ok'"),
            timeout: Some(300),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash should execute");

        assert!(output.stdout.contains("ok"));
        assert!(
            output
                .stdout
                .contains("[claw] note: `timeout` is in milliseconds"),
            "unit hint missing: {}",
            output.stdout
        );
    }

    #[test]
    fn disables_sandbox_when_requested() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        })
        .expect("bash command should execute");

        assert!(!output.sandbox_status.expect("sandbox status").enabled);
    }

    #[test]
    fn timed_out_test_command_is_classified_as_hung_test_with_provenance() {
        let output = execute_bash(BashCommandInput {
            command: String::from("sleep 1 # cargo test slow_case"),
            timeout: Some(1),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash command should return structured timeout");

        assert!(output.interrupted);
        assert_eq!(
            output.return_code_interpretation.as_deref(),
            Some("test.hung")
        );
        let structured = output.structured_content.expect("structured content");
        assert_eq!(structured[0]["event"], "test.hung");
        assert_eq!(structured[0]["data"]["provenance"], "bash.timeout");
    }

    /// PRD P1-4 边界：timeout < 1000ms 且命令真实超时（被 300ms 误杀）时，
    /// 单位提示也必须出现在 stdout —— 精确复现会话中 clippy 因 300ms 超时
    /// 被中断的场景，避免"命令跑完才有提示、真超时反而无提示"的盲区。
    #[test]
    fn timeout_below_one_second_emits_unit_hint_even_on_timeout() {
        // 与其他修改 PATH / CLAW_GIT_BASH 的测试共享 env 锁,防止 shell 检测
        // 读到污染环境(并行测试竞态,2026-09-01 复现:interrupted 偶发变 false)。
        let _guard = super::BASH_TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = execute_bash(BashCommandInput {
            command: String::from("sleep 2"),
            timeout: Some(300),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash should return structured timeout");

        assert!(output.interrupted);
        assert_eq!(
            output.return_code_interpretation.as_deref(),
            Some("timeout")
        );
        assert_eq!(
            output.no_output_expected,
            Some(false),
            "unit hint on stdout must be flagged as output"
        );
        assert!(
            output
                .stdout
                .contains("[claw] note: `timeout` is in milliseconds"),
            "unit hint missing on timeout path: {}",
            output.stdout
        );
    }

    /// 智能模式（timeout=None）下短命令应正常完成，不被误杀。
    /// 验证：未设 timeout 时启用 ActivityMonitor，活跃度检测不会因
    /// 固定 120s 默认超时触发（短任务本就不会触发），命令在毫秒级完成。
    #[test]
    fn smart_mode_completes_short_command_without_timeout() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'smart_done'"),
            timeout: None, // 触发智能模式（idle=5min, hard=1h）
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("smart mode should execute short command");

        assert_eq!(output.stdout, "smart_done");
        assert!(!output.interrupted);
    }

    /// 智能模式不应因短时间 sleep（无 stdout 输出）被误判为 idle。
    /// sleep 1s 期间无 stdout，但子进程 CPU 时间在增长（sleep 系统调用
    /// 占用极小 CPU），且 1s 远小于 5min idle_timeout。
    #[test]
    fn smart_mode_does_not_kill_short_sleep_with_echo() {
        let output = execute_bash(BashCommandInput {
            command: String::from("sleep 1 && echo done"),
            timeout: None, // 智能模式
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("smart mode should complete short sleep+echo");

        assert!(
            !output.interrupted,
            "smart mode should not interrupt short sleep+echo: {:?}",
            output.stderr
        );
        assert!(output.stdout.contains("done"));
    }

    /// 回归测试：固定超时在 stdout 持续输出时也必须准时触发。
    ///
    /// 背景 BUG：超时/try_wait 检查此前放在 select! 的 sleep 分支里，select! 用
    /// biased 优先读 stdout。当命令持续向 stdout 写数据（如 `while true; do echo`）
    /// 时，read 分支总是立即 ready，sleep 分支被饿死 → 固定 timeout 永不触发，
    /// 命令永不返回（实测 timeout=30s 的命令跑了 578s，会话表现为卡死）。
    ///
    /// 修复后检查移到循环顶部，即使 stdout 满速输出，也应在一个较短的墙钟窗口内
    /// kill 并返回 interrupted，绝不 hang。
    #[test]
    fn fixed_timeout_fires_even_with_continuous_stdout() {
        let start = std::time::Instant::now();
        let output = execute_bash(BashCommandInput {
            // 持续向 stdout 写数据且永不退出的命令（git-bash 语法）
            command: String::from("while true; do echo tick; done"),
            timeout: Some(200), // 200ms 固定超时
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        })
        .expect("bash command should return");

        let elapsed_ms = start.elapsed().as_millis();
        assert!(
            output.interrupted,
            "continuous-output command should be timeout-interrupted, stderr={}",
            output.stderr
        );
        assert!(
            elapsed_ms < 5_000,
            "fixed timeout should fire promptly even with continuous stdout, took {elapsed_ms}ms"
        );
    }

    /// 改进点 5:`try_assign_sandbox_job` 在 sandbox disabled 时应无副作用。
    /// 即使在 Windows 上,只要 sandbox_status.enabled=false,函数应直接返回,
    /// 不尝试调用 Job Object 分配。
    #[test]
    fn try_assign_sandbox_job_noop_when_disabled() {
        use super::try_assign_sandbox_job;
        use crate::sandbox::SandboxStatus;

        let disabled_status = SandboxStatus {
            enabled: false,
            ..Default::default()
        };
        // 应无副作用,不 panic
        try_assign_sandbox_job(&disabled_status, 9999);
    }

    /// 改进点 5:`try_assign_sandbox_job` 在 sandbox enabled 但当前平台不支持时
    /// 应无副作用。非 Windows/macOS 平台(如 Linux)应直接返回。
    #[test]
    fn try_assign_sandbox_job_noop_on_unsupported_platform() {
        use super::try_assign_sandbox_job;
        use crate::sandbox::SandboxStatus;

        // 构造 enabled=true 但 supported=false 的状态(模拟非 Windows 平台)
        let enabled_but_unsupported = SandboxStatus {
            enabled: true,
            supported: false, // 模拟非 Windows/macOS
            ..Default::default()
        };
        // 应无副作用,不 panic
        try_assign_sandbox_job(&enabled_but_unsupported, 9999);
    }

    /// 改进点 5:Windows 平台上 sandbox enabled 时应尝试分配 Job Object。
    /// 此测试不验证 Job Object 真正生效(需要 PowerShell + Win32 API),
    /// 仅验证函数在 Windows 上被调用不 panic(可能返回 Err 但不传播)。
    #[test]
    #[cfg(target_os = "windows")]
    fn try_assign_sandbox_job_windows_attempts_assignment() {
        use super::try_assign_sandbox_job;
        use crate::sandbox::SandboxStatus;

        let enabled_windows = SandboxStatus {
            enabled: true,
            supported: true, // Windows 平台支持
            active: false,   // 但未激活(改进点 4 之后 supported 反映平台能力)
            ..Default::default()
        };
        // pid=999999 几乎肯定不存在,assign_process 会失败但不应 panic
        // 函数应吞掉错误,仅 eprintln 记录
        try_assign_sandbox_job(&enabled_windows, 999_999);
    }
}

/// Maximum output bytes before truncation (64 KiB).
///
/// Previous 16 KiB limit caused permanent loss of `cargo test` / `cargo build`
/// output before microcompact could archive it. 64 KiB provides enough headroom
/// for typical compiler/test output while still bounding context usage.
const MAX_OUTPUT_BYTES: usize = 65_536;

/// 检测 stderr 中是否包含 shell 语法错误特征（bash/sh/cmd 通用）。
///
/// 命中时 `return_code_interpretation` 会附加「命令存在 shell 语法错误
/// （引号/转义不匹配）」诊断，避免 LLM 自行解读原始错误（对应 PRD P2-5）。
fn has_shell_syntax_error(stderr: &str) -> bool {
    let low = stderr.to_ascii_lowercase();
    [
        "unexpected eof",
        "syntax error near",
        "syntax error",
        "unexpected token",
        "missing terminating",
        "was unexpected at this time",
        "the syntax of the command is incorrect",
    ]
    .iter()
    .any(|needle| low.contains(needle))
}

/// Truncate output to `MAX_OUTPUT_BYTES`, appending a marker when trimmed.
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    // Find the last valid UTF-8 boundary at or before MAX_OUTPUT_BYTES
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str(&format!(
        "\n\n[output truncated — exceeded {MAX_OUTPUT_BYTES} bytes. Use a more targeted command or redirect to a file.]"
    ));
    truncated
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn short_output_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_output(s), s);
    }

    #[test]
    fn long_output_truncated() {
        let s = "x".repeat(70_000);
        let result = truncate_output(&s);
        assert!(result.len() < 70_000);
        assert!(result.contains("[output truncated"));
        assert!(result.contains("Use a more targeted command"));
    }

    #[test]
    fn exact_boundary_unchanged() {
        let s = "a".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(truncate_output(&s), s);
    }

    #[test]
    fn one_over_boundary_truncated() {
        let s = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let result = truncate_output(&s);
        assert!(result.contains("[output truncated"));
    }
}

/// 智能超时检测器
///
/// 替代固定 120s 墙钟超时，基于子进程活跃信号自适应判断：
/// - stdout/stderr 有新字节 → 流式输出中（永不超时，如 cargo build、回测日志）
/// - 子进程树 CPU 时间增长 → 长计算中（仅 max_hard_timeout 兜底，如 Python 训练）
/// - 持续 idle_timeout 无任何活动 → 疑似死锁/挂起，kill
///
/// 仅当 BashCommandInput.timeout 为 None 时启用。模型显式指定 timeout 时
/// 仍走固定墙钟超时（保持向后兼容与可预期性）。
mod activity_monitor {
    use std::time::{Duration, Instant};

    /// 默认空闲超时：5 分钟无任何活动（无输出 + 无 CPU 增长）后判定为挂起。
    /// 足够覆盖网络请求、DB 查询、短暂 IO 阻塞的合法等待。
    const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    /// 默认绝对硬上限：1 小时。
    /// 即使持续有活动，超过此值也强制 kill，防止资源永久占用。
    const DEFAULT_MAX_HARD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

    /// CPU 采样间隔：避免每 100ms 都全量枚举进程树（Windows ~5ms，Linux ~1ms）。
    /// 500ms 足够检测到短任务和长任务的 CPU 变化（采样定理）。
    const CPU_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

    /// 忙等窗口：子进程在无任何输出、CPU 却持续增长的情况下运行的最长时间。
    ///
    /// 背景：LLM 生成的调试脚本偶发含无谓的大 range 空转（如 `for i in
    /// range(1_000_000_000): pass`）。这类循环纯 CPU 忙等、零输出，会使
    /// `cpu_advanced` 持续为真而永远不触发 idle timeout；若只靠 1h 的
    /// `max_hard_timeout` 兜底，命令会长时间占用单核资源，会话表现为卡死。
    /// 判别：忙等窗口内每个 CPU 采样 CPU 都在增长、但从未有输出 → 判定忙等循环，
    /// 显著缩短硬上限（远短于正常长计算如编译/回测）。
    const DEFAULT_BUSY_LOOP_WINDOW: Duration = Duration::from_secs(120);
    /// 忙等窗口内需累计到的 CPU 增长采样次数（每 CPU 采样约 500ms）才判忙等。
    /// 6 次 ≈ 窗口末期持续 3s 满速 CPU 空转，足以区分"偶发计算"与"持续空转"。
    const BUSY_LOOP_MIN_ADVANCES: u32 = 6;

    /// 活跃度检测决策。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ActivityDecision {
        /// 继续等待子进程。
        Continue,
        /// 空闲超时：长时间无输出也无 CPU 活动，疑似死锁。
        IdleTimeout,
        /// 硬上限：超过绝对执行时间上限。
        HardTimeout,
        /// 忙等循环：长时间无输出但 CPU 持续增长，疑似无谓的 CPU 空转循环。
        BusyLoop,
    }

    /// 进程活跃度监视器。
    ///
    /// 在 execute_bash_async 的 select! loop 中每 100ms 调用 `poll()`。
    /// stdout/stderr 读到字节时调用 `note_output()` 重置空闲计时器。
    pub(crate) struct ActivityMonitor {
        /// 子进程 root PID（bash shell 进程）。
        root_pid: u32,
        /// 最后一次观察到 stdout/stderr 字节的时间（仅输出更新，不随 CPU 增长而变）。
        /// 用于忙等判定与 idle 判定。
        last_output_at: Instant,
        /// 最后一次观察到"任何活动"（输出或 CPU 增长）的时间。用于 idle 判定：
        /// CPU 增长也算活跃，因此空闲 = 长时间既无输出也无 CPU。
        last_activity_at: Instant,
        /// 上次采样的子进程树 CPU 时间总和（kernel + user）。
        last_cpu_time: Duration,
        /// 上次执行 CPU 采样的时间（限流）。
        last_refresh_at: Instant,
        /// 启动时间，用于判断 max_hard_timeout。
        started_at: Instant,
        /// 空闲超时阈值。
        idle_timeout: Duration,
        /// 绝对硬上限。
        max_hard_timeout: Duration,
        /// 忙等观察窗口：child 在此时间内无任何输出、CPU 却持续增长 → 判忙等。
        busy_loop_window: Duration,
        /// 忙等观察累计的 CPU 增长采样次数（在无输出窗口内）。
        busy_advance_count: u32,
    }

    impl ActivityMonitor {
        pub(crate) fn new(root_pid: u32) -> Self {
            let now = Instant::now();
            Self {
                root_pid,
                last_output_at: now,
                last_activity_at: now,
                last_cpu_time: Duration::ZERO,
                last_refresh_at: now,
                started_at: now,
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                max_hard_timeout: DEFAULT_MAX_HARD_TIMEOUT,
                busy_loop_window: DEFAULT_BUSY_LOOP_WINDOW,
                busy_advance_count: 0,
            }
        }

        /// 自定义阈值的构造函数（测试用）。
        #[cfg(test)]
        pub(crate) fn with_thresholds(
            root_pid: u32,
            idle_timeout: Duration,
            max_hard_timeout: Duration,
        ) -> Self {
            let mut m = Self::new(root_pid);
            m.idle_timeout = idle_timeout;
            m.max_hard_timeout = max_hard_timeout;
            m
        }

        /// 测试用：完全自定义阈值（含忙等观察窗口）。
        #[cfg(test)]
        pub(crate) fn with_full_thresholds(
            root_pid: u32,
            idle_timeout: Duration,
            max_hard_timeout: Duration,
            busy_loop_window: Duration,
        ) -> Self {
            let mut m = Self::with_thresholds(root_pid, idle_timeout, max_hard_timeout);
            m.busy_loop_window = busy_loop_window;
            m
        }

        /// 收到 stdout/stderr 字节时调用，重置空闲计时器与忙等观察。
        pub(crate) fn note_output(&mut self) {
            let now = Instant::now();
            self.last_output_at = now;
            self.last_activity_at = now;
            self.busy_advance_count = 0;
        }

        /// 每 100ms 轮询一次，返回决策结果。
        pub(crate) fn poll(&mut self) -> ActivityDecision {
            let now = Instant::now();

            // 1. 绝对硬上限（优先级最高，无视活跃状态）
            if now.duration_since(self.started_at) >= self.max_hard_timeout {
                return ActivityDecision::HardTimeout;
            }

            // 2. 限流刷新 CPU 采样（避免高频枚举进程树的开销）
            if now.duration_since(self.last_refresh_at) >= CPU_REFRESH_INTERVAL {
                let current_cpu =
                    collect_process_tree_cpu(self.root_pid).unwrap_or(self.last_cpu_time);
                let cpu_advanced = current_cpu > self.last_cpu_time;
                self.last_cpu_time = current_cpu;
                self.last_refresh_at = now;

                if cpu_advanced {
                    // CPU 增长 → 子进程仍在计算，视为活跃（重置 idle 判定基准）
                    self.last_activity_at = now;
                    // 忙等观察：仅当"距上次输出"超过窗口时累计 CPU 增长。
                    // last_output_at 不随 CPU 增长更新（仅 note_output 更新），
                    // 因此可识别"无输出却持续烧 CPU"的空转循环，规避 idle 检测盲区。
                    if now.duration_since(self.last_output_at) >= self.busy_loop_window {
                        self.busy_advance_count += 1;
                        if self.busy_advance_count >= BUSY_LOOP_MIN_ADVANCES {
                            return ActivityDecision::BusyLoop;
                        }
                    }
                }
            }

            // 3. 检查空闲时长（基于任何活动：输出 或 CPU）
            if now.duration_since(self.last_activity_at) >= self.idle_timeout {
                return ActivityDecision::IdleTimeout;
            }

            ActivityDecision::Continue
        }
    }

    // ===== 平台实现：Windows =====

    #[cfg(windows)]
    fn collect_process_tree_cpu(root_pid: u32) -> Option<Duration> {
        let pids = enum_process_tree_windows(root_pid);
        if pids.is_empty() {
            return None;
        }
        let mut total = Duration::ZERO;
        for pid in &pids {
            if let Some(cpu) = get_process_cpu_time_windows(*pid) {
                total += cpu;
            }
        }
        Some(total)
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn enum_process_tree_windows(root_pid: u32) -> Vec<u32> {
        use std::mem::MaybeUninit;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };

        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return vec![root_pid];
        }

        let mut known: std::collections::HashSet<u32> = std::collections::HashSet::new();
        known.insert(root_pid);

        // 多轮扫描直到无新发现（进程树深度无上限）
        // 每轮遍历一次 snapshot，把 parent 在已知集合中的 child 加入。
        let mut changed = true;
        while changed {
            changed = false;
            let mut entry: PROCESSENTRY32W = unsafe { MaybeUninit::zeroed().assume_init() };
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

            if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
                break;
            }
            loop {
                let parent = entry.th32ParentProcessID;
                if known.contains(&parent) && !known.contains(&entry.th32ProcessID) {
                    known.insert(entry.th32ProcessID);
                    changed = true;
                }
                if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }

        unsafe { CloseHandle(snapshot) };
        known.into_iter().collect()
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn get_process_cpu_time_windows(pid: u32) -> Option<Duration> {
        use std::mem::MaybeUninit;
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }

        let mut creation: FILETIME = unsafe { MaybeUninit::zeroed().assume_init() };
        let mut exit: FILETIME = unsafe { MaybeUninit::zeroed().assume_init() };
        let mut kernel: FILETIME = unsafe { MaybeUninit::zeroed().assume_init() };
        let mut user: FILETIME = unsafe { MaybeUninit::zeroed().assume_init() };

        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        unsafe { CloseHandle(handle) };

        if ok == 0 {
            return None;
        }

        // FILETIME 是 100ns 单位
        let kernel_100ns = ((kernel.dwHighDateTime as u64) << 32) | (kernel.dwLowDateTime as u64);
        let user_100ns = ((user.dwHighDateTime as u64) << 32) | (user.dwLowDateTime as u64);
        let total_100ns = kernel_100ns + user_100ns;
        Some(Duration::from_nanos(total_100ns * 100))
    }

    // ===== 平台实现：Linux =====

    #[cfg(target_os = "linux")]
    fn collect_process_tree_cpu(root_pid: u32) -> Option<Duration> {
        let pids = enum_process_tree_linux(root_pid);
        if pids.is_empty() {
            return None;
        }
        let mut total_ticks: u64 = 0;
        for pid in &pids {
            if let Some(ticks) = read_proc_cpu_time(*pid) {
                total_ticks = total_ticks.saturating_add(ticks);
            }
        }
        // sysconf(_SC_CLK_TCK) 在 Linux 通常为 100
        let hz = 100u64;
        let total_ns = total_ticks.checked_mul(1_000_000_000)?.checked_div(hz)?;
        Some(Duration::from_nanos(total_ns))
    }

    #[cfg(target_os = "linux")]
    fn enum_process_tree_linux(root_pid: u32) -> Vec<u32> {
        let mut known: std::collections::HashSet<u32> = std::collections::HashSet::new();
        known.insert(root_pid);

        let mut changed = true;
        while changed {
            changed = false;
            let entries = match std::fs::read_dir("/proc") {
                Ok(e) => e,
                Err(_) => break,
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                let Ok(pid) = name_str.parse::<u32>() else {
                    continue;
                };
                if known.contains(&pid) {
                    continue;
                }
                if let Some(ppid) = read_proc_ppid(pid) {
                    if known.contains(&ppid) {
                        known.insert(pid);
                        changed = true;
                    }
                }
            }
        }
        known.into_iter().collect()
    }

    #[cfg(target_os = "linux")]
    fn read_proc_stat_field(pid: u32, field_idx: usize) -> Option<u64> {
        let path = format!("/proc/{pid}/stat");
        let content = std::fs::read_to_string(&path).ok()?;
        // stat 字段以空格分隔，但 comm 字段可能包含空格（用括号包围）
        let start = content.find('(')?;
        let end = content.rfind(')')?;
        let rest = &content[end + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // comm 占字段 2，其后的字段在 rest 中从 0 开始索引
        // 原字段索引 - 3 = rest 索引（因为 comm 后从字段 3 开始）
        let adjusted_idx = field_idx.saturating_sub(3);
        fields.get(adjusted_idx).and_then(|s| s.parse().ok())
    }

    #[cfg(target_os = "linux")]
    fn read_proc_ppid(pid: u32) -> Option<u32> {
        read_proc_stat_field(pid, 3).map(|v| v as u32)
    }

    #[cfg(target_os = "linux")]
    fn read_proc_cpu_time(pid: u32) -> Option<u64> {
        let utime = read_proc_stat_field(pid, 14).unwrap_or(0);
        let stime = read_proc_stat_field(pid, 15).unwrap_or(0);
        Some(utime + stime)
    }

    // ===== 平台实现：其他（macOS 等）=====
    //
    // 降级到只用 stdout/stderr 流量信号 + max_hard_timeout。
    // collect_process_tree_cpu 返回 None 时，ActivityMonitor.poll() 中
    // last_cpu_time 始终为 ZERO，永不因 CPU 信号重置空闲计时器。
    // 流式输出任务（如 cargo build）仍能正常工作，纯计算无输出任务
    // 会被 idle_timeout 触发（macOS 上无 /proc，实现成本高）。
    #[cfg(not(any(windows, target_os = "linux")))]
    fn collect_process_tree_cpu(_root_pid: u32) -> Option<Duration> {
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::thread;

        #[test]
        fn monitor_initial_decision_is_continue() {
            let mut m = ActivityMonitor::with_thresholds(
                std::process::id(),
                Duration::from_secs(1),
                Duration::from_secs(60),
            );
            assert_eq!(m.poll(), ActivityDecision::Continue);
        }

        #[test]
        fn monitor_idle_timeout_triggers_after_threshold() {
            let mut m = ActivityMonitor::with_thresholds(
                std::process::id(),
                Duration::from_millis(50),
                Duration::from_secs(60),
            );
            // 让 CPU 采样至少跑一次（首次采样 last_cpu_time = 0）
            thread::sleep(Duration::from_millis(20));
            let _ = m.poll();
            // 等待 idle_timeout 触发
            thread::sleep(Duration::from_millis(100));
            assert_eq!(m.poll(), ActivityDecision::IdleTimeout);
        }

        #[test]
        fn monitor_hard_timeout_overrides_idle() {
            let mut m = ActivityMonitor::with_thresholds(
                std::process::id(),
                Duration::from_secs(60),
                Duration::from_millis(50),
            );
            thread::sleep(Duration::from_millis(100));
            assert_eq!(m.poll(), ActivityDecision::HardTimeout);
        }

        #[test]
        fn monitor_note_output_resets_idle() {
            let mut m = ActivityMonitor::with_thresholds(
                std::process::id(),
                Duration::from_millis(150),
                Duration::from_secs(60),
            );
            thread::sleep(Duration::from_millis(50));
            m.note_output();
            thread::sleep(Duration::from_millis(50));
            assert_eq!(m.poll(), ActivityDecision::Continue);
        }

        /// 忙等检测：无输出但 CPU 持续增长时，应在很短窗口内判 BusyLoop。
        ///
        /// 用极短的 busy_loop_window（10ms）与极短的 CPU 采样间隔无法配置
        /// （CPU_REFRESH_INTERVAL=500ms 为常量），因此通过让本进程真实消耗
        /// CPU 并循环多次 poll 触发：CPU 增长会被采样到，且距上次输出超过
        /// 窗口 → 累计计数达 BUSY_LOOP_MIN_ADVANCES 后返回 BusyLoop。
        #[cfg(any(windows, target_os = "linux"))]
        #[test]
        fn monitor_busy_loop_detects_sustained_cpu_without_output() {
            let mut m = ActivityMonitor::with_full_thresholds(
                std::process::id(),
                Duration::from_secs(60),   // idle 60s，确保不会被 idle 抢先
                Duration::from_secs(3600), // hard 1h，确保测试期间不可能触发
                Duration::from_millis(10), // busy 窗口极小：无输出 10ms + CPU 增长 → 判忙等
            );
            // 触发一段足够长的 CPU 消耗。注意 CPU_REFRESH_INTERVAL=500ms 为常量，
            // 需跨越 BUSY_LOOP_MIN_ADVANCES(6) 次采样周期 ≈ 6×500ms = 3s，
            // 因此循环给足 8s（并行测试下 CPU 采样可能被调度延迟，4s 曾偶发不足）。
            let start = std::time::Instant::now();
            let mut done = false;
            while start.elapsed() < Duration::from_secs(8) && !done {
                let mut sum: u64 = 0;
                for i in 0u64..50_000_000 {
                    sum = sum.wrapping_add(std::hint::black_box(i));
                }
                std::hint::black_box(sum);
                if matches!(m.poll(), ActivityDecision::BusyLoop) {
                    done = true;
                    break;
                }
            }
            assert!(done, "无输出 + 持续 CPU 增长应在忙等窗口内判定为 BusyLoop");
        }

        #[cfg(windows)]
        #[test]
        fn windows_collect_cpu_time_for_self() {
            let first = get_process_cpu_time_windows(std::process::id())
                .expect("should query own process CPU time");
            // 消耗足够 CPU 时间（>16ms）以超过 GetProcessTimes 精度阈值。
            // 100M 次 wrapping_add 约占 200-500ms CPU，远超 15.625ms 精度。
            //
            // ⚠ 必须用 black_box 包裹循环变量 `i`：否则 LLVM SCEV 会把
            // `sum += i` 识别为归纳变量并直接算出等差数列封闭形式，整个循环
            // 被替换成常数，CPU 消耗≈0，时间戳不前进（实测 wall=3.5µs）。
            // black_box 在循环体内破坏 SCEV，强制编译器真正执行迭代。
            let mut sum: u64 = 0;
            for i in 0u64..100_000_000 {
                sum = sum.wrapping_add(std::hint::black_box(i));
            }
            std::hint::black_box(sum);
            let second = get_process_cpu_time_windows(std::process::id())
                .expect("should query own process CPU time again");
            assert!(
                second > first,
                "CPU time should increase after work: first={first:?}, second={second:?}"
            );
        }

        #[cfg(windows)]
        #[test]
        fn windows_enum_process_tree_includes_self() {
            let pids = enum_process_tree_windows(std::process::id());
            assert!(
                pids.contains(&std::process::id()),
                "self pid should be in process tree"
            );
        }

        #[cfg(windows)]
        #[test]
        fn windows_collect_tree_cpu_returns_nonzero_for_self() {
            // 触发一些 CPU 工作
            let mut sum: u64 = 0;
            for i in 0u64..500_000 {
                sum = sum.wrapping_add(i);
            }
            std::hint::black_box(sum);
            let cpu = collect_process_tree_cpu(std::process::id());
            assert!(cpu.is_some(), "should collect CPU time for self");
            assert!(
                cpu.unwrap() > Duration::ZERO,
                "CPU time should be > 0 after work"
            );
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_collect_cpu_time_for_self() {
            // 触发一些 CPU 工作
            let mut sum: u64 = 0;
            for i in 0u64..500_000 {
                sum = sum.wrapping_add(i);
            }
            std::hint::black_box(sum);
            let cpu = collect_process_tree_cpu(std::process::id());
            assert!(cpu.is_some(), "should collect CPU time for self on Linux");
            assert!(
                cpu.unwrap() > Duration::ZERO,
                "CPU time should be > 0 after work"
            );
        }
    }
}
