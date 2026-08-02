use std::env;
use std::io;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;
use tokio::runtime::Builder;

use crate::lane_events::{LaneEvent, ShipMergeMethod, ShipProvenance};
use crate::sandbox::{
    build_linux_sandbox_command, resolve_sandbox_status_for_request, FilesystemIsolationMode,
    SandboxConfig, SandboxStatus,
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
    #[serde(rename = "backgroundTaskId")]
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

    // 用 spawn + select! 替代 command.output()，支持：
    // 1. 超时 kill 子进程（原 timeout 只放弃 await，子进程可能残留）
    // 2. Ctrl+C 中断 kill 子进程（原实现完全无法中断）
    // 3. 并发读 stdout/stderr 避免管道死锁（child 输出超过 64KB pipe buffer 会阻塞）
    let mut command = prepare_tokio_command(&input.command, &cwd, &sandbox_status, true);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(Stdio::null());

    let mut child = command.spawn()?;

    // take stdout/stderr 用于并发读取，避免管道死锁
    let mut child_stdout = child.stdout.take();
    let mut child_stderr = child.stderr.take();

    // 独立 buffer 避免 select! 分支间数据竞争
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    let mut tmp_out = [0u8; 8192];
    let mut tmp_err = [0u8; 8192];

    let start = Instant::now();
    let timeout_dur = Duration::from_millis(timeout_ms);
    let poll_interval = Duration::from_millis(100);
    /// child 退出后 pipe 排空宽限期。
    ///
    /// 背景：bash `&` 启动的后台进程会继承 pipe 写端，导致 child 退出后
    /// pipe 永不 EOF。给 2s 宽限期读取 child 退出前写入的残留数据，
    /// 超时强制关闭 pipe，防止 select! loop 永久阻塞。
    const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

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
    // child 退出时间，用于 pipe 排空宽限期判断
    let mut child_exit_time: Option<Instant> = None;

    // select! loop：
    // - child 未退出时：并发等待 child.wait() / 超时 / abort / 读 stdout / 读 stderr
    // - child 退出后：继续读 stdout/stderr 直到 EOF（pipe 中可能有残留数据）
    // - child 退出后若 pipe 长时间未 EOF（后台 `&` 进程继承 pipe 写端），
    //   超过 PIPE_DRAIN_GRACE 后强制关闭 pipe，防止死锁
    // - stdout/stderr 都 EOF 且 child 已退出时：break
    loop {
        // PIPE_DRAIN_GRACE 检查（必须在 select! 之前，不依赖 sleep 分支）：
        // biased select! 下，若后台进程持续输出数据，stdout read 分支总是
        // 立即 ready，sleep 分支永远不到期 → PIPE_DRAIN_GRACE 检查被饿死。
        // 在循环顶部检查确保即使 stdout 持续有数据，2s 后仍强制关闭 pipe。
        if let Some(exit_time) = child_exit_time {
            if exit_time.elapsed() >= PIPE_DRAIN_GRACE {
                child_stdout = None;
                child_stderr = None;
            }
        }
        if child_exited && child_stdout.is_none() && child_stderr.is_none() {
            break;
        }

        tokio::select! {
            biased;

            // 分支 1：子进程退出（仅在未退出时等待）
            status = child.wait(), if !child_exited => {
                match status {
                    Ok(s) => { exit_status = s.code(); }
                    Err(_) => { exit_status = None; }
                }
                child_exited = true;
                child_exit_time = Some(Instant::now());
            }

            // 分支 2：轮询超时、abort 和 pipe 排空宽限期
            //   不带 `if !child_exited` guard：child 退出后仍需检查 pipe 宽限期。
            //   原实现带此 guard 导致死锁：bash `&` 启动的后台进程继承 pipe 写端，
            //   child 退出后 pipe 永不 EOF，timeout 检查被 guard 禁用，loop 永久阻塞。
            _ = tokio::time::sleep(poll_interval) => {
                if is_bash_aborted() {
                    aborted = true;
                    if !child_exited {
                        let _ = child.kill().await;
                        child_exited = true;
                    }
                    // abort 时立即关闭 pipe，快速退出（不等残留数据）
                    child_stdout = None;
                    child_stderr = None;
                } else if !child_exited {
                    if let Some(ref mut m) = monitor {
                        // 智能模式：基于子进程树活跃度决策
                        match m.poll() {
                            activity_monitor::ActivityDecision::Continue => {}
                            activity_monitor::ActivityDecision::IdleTimeout => {
                                timed_out = true;
                                smart_idle = true;
                                let _ = child.kill().await;
                                child_exited = true;
                            }
                            activity_monitor::ActivityDecision::HardTimeout => {
                                timed_out = true;
                                let _ = child.kill().await;
                                child_exited = true;
                            }
                        }
                    } else if start.elapsed() >= timeout_dur {
                        // 固定超时模式（input.timeout 显式指定时）
                        timed_out = true;
                        let _ = child.kill().await;
                        child_exited = true;
                    }
                } else if let Some(exit_time) = child_exit_time {
                    // child 已退出但 pipe 未 EOF：后台进程（如 `&` 启动的服务）
                    // 继承了 pipe 写端。给 PIPE_DRAIN_GRACE 宽限期读取残留数据，
                    // 超时强制关闭 pipe，防止永久阻塞。
                    if exit_time.elapsed() >= PIPE_DRAIN_GRACE {
                        child_stdout = None;
                        child_stderr = None;
                    }
                }
            }

            // 分支 3：读 stdout
            n = async {
                if let Some(ref mut stdout) = child_stdout {
                    stdout.read(&mut tmp_out).await
                } else {
                    std::future::pending::<io::Result<usize>>().await
                }
            }, if child_stdout.is_some() => {
                match n {
                    Ok(0) => { child_stdout = None; }
                    Ok(n) => {
                        stdout_buf.extend_from_slice(&tmp_out[..n]);
                        if let Some(ref mut m) = monitor {
                            m.note_output();
                        }
                    }
                    Err(_) => { child_stdout = None; }
                }
            }

            // 分支 4：读 stderr
            n = async {
                if let Some(ref mut stderr) = child_stderr {
                    stderr.read(&mut tmp_err).await
                } else {
                    std::future::pending::<io::Result<usize>>().await
                }
            }, if child_stderr.is_some() => {
                match n {
                    Ok(0) => { child_stderr = None; }
                    Ok(n) => {
                        stderr_buf.extend_from_slice(&tmp_err[..n]);
                        if let Some(ref mut m) = monitor {
                            m.note_output();
                        }
                    }
                    Err(_) => { child_stderr = None; }
                }
            }
        }
    }

    // abort：用户 Ctrl+C 中断
    if aborted {
        let stdout = truncate_output(&String::from_utf8_lossy(&stdout_buf));
        let stderr = truncate_output(&String::from_utf8_lossy(&stderr_buf));
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
        // 智能模式 idle 触发：给出不同消息引导模型重试
        if smart_idle {
            return Ok(idle_timeout_output(&input, sandbox_status));
        }
        return Ok(timeout_output(&input, timeout_ms, sandbox_status));
    }

    // 正常完成
    let stdout = truncate_output(&String::from_utf8_lossy(&stdout_buf));
    let stderr = truncate_output(&String::from_utf8_lossy(&stderr_buf));
    let no_output_expected = Some(stdout.trim().is_empty() && stderr.trim().is_empty());
    let return_code_interpretation = exit_status.and_then(|code| {
        if code == 0 {
            None
        } else {
            Some(format!("exit_code:{code}"))
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
        stdout: String::new(),
        stderr: format!("Command exceeded timeout of {timeout_ms} ms{guidance}"),
        raw_output_path: None,
        interrupted: true,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation: Some(String::from(return_code_interpretation)),
        no_output_expected: Some(true),
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
        return prepared;
    }

    let kind = shell_kind();
    let mut prepared = Command::new(&kind.program);
    prepared.arg(kind.flag).arg(command).current_dir(cwd);
    if sandbox_status.filesystem_active {
        prepared.env("HOME", cwd.join(".sandbox-home"));
        prepared.env("TMPDIR", cwd.join(".sandbox-tmp"));
    }
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
        return prepared;
    }

    let kind = shell_kind();
    let mut prepared = TokioCommand::new(&kind.program);
    prepared.arg(kind.flag).arg(command).current_dir(cwd);
    if sandbox_status.filesystem_active {
        prepared.env("HOME", cwd.join(".sandbox-home"));
        prepared.env("TMPDIR", cwd.join(".sandbox-tmp"));
    }
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

/// 进程级 shell 探测缓存。
/// 首次调用 `shell_kind()` 时执行探测（~0.2ms 命中固定路径，~2ms 命中 PATH），
/// 之后所有 bash 命令直接读缓存，O(1)。
static SHELL_KIND_CACHE: std::sync::OnceLock<ShellKind> = std::sync::OnceLock::new();

/// 返回当前进程使用的 shell 启动器，首次调用时探测并缓存。
/// Windows 探测顺序：CLAW_GIT_BASH 环境变量 → Program Files 固定路径 → PATH 搜索（过滤 WSL）。
/// Unix 直接用 sh -lc。
fn shell_kind() -> ShellKind {
    SHELL_KIND_CACHE.get_or_init(detect_shell_kind).clone()
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
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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

#[cfg(test)]
mod tests {
    use super::{execute_bash, BashCommandInput};
    use crate::sandbox::FilesystemIsolationMode;

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
}

/// Maximum output bytes before truncation (64 KiB).
///
/// Previous 16 KiB limit caused permanent loss of `cargo test` / `cargo build`
/// output before microcompact could archive it. 64 KiB provides enough headroom
/// for typical compiler/test output while still bounding context usage.
const MAX_OUTPUT_BYTES: usize = 65_536;

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

    /// 活跃度检测决策。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ActivityDecision {
        /// 继续等待子进程。
        Continue,
        /// 空闲超时：长时间无输出也无 CPU 活动，疑似死锁。
        IdleTimeout,
        /// 硬上限：超过绝对执行时间上限。
        HardTimeout,
    }

    /// 进程活跃度监视器。
    ///
    /// 在 execute_bash_async 的 select! loop 中每 100ms 调用 `poll()`。
    /// stdout/stderr 读到字节时调用 `note_output()` 重置空闲计时器。
    pub(crate) struct ActivityMonitor {
        /// 子进程 root PID（bash shell 进程）。
        root_pid: u32,
        /// 最后一次观察到 stdout/stderr 字节的时间。
        last_output_at: Instant,
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
    }

    impl ActivityMonitor {
        pub(crate) fn new(root_pid: u32) -> Self {
            let now = Instant::now();
            Self {
                root_pid,
                last_output_at: now,
                last_cpu_time: Duration::ZERO,
                last_refresh_at: now,
                started_at: now,
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                max_hard_timeout: DEFAULT_MAX_HARD_TIMEOUT,
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

        /// 收到 stdout/stderr 字节时调用，重置空闲计时器。
        pub(crate) fn note_output(&mut self) {
            self.last_output_at = Instant::now();
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

                // CPU 时间增长 → 子进程树仍在计算，重置空闲计时器
                if cpu_advanced {
                    self.last_output_at = now;
                }
            }

            // 3. 检查空闲时长
            if now.duration_since(self.last_output_at) >= self.idle_timeout {
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

        let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
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
                let Some(name_str) = name.to_str() else { continue };
                let Ok(pid) = name_str.parse::<u32>() else { continue };
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

        #[cfg(windows)]
        #[test]
        fn windows_collect_cpu_time_for_self() {
            let first = get_process_cpu_time_windows(std::process::id())
                .expect("should query own process CPU time");
            // 消耗足够 CPU 时间（>16ms）以超过 GetProcessTimes 精度阈值。
            // 100M 次 wrapping_add 约占 200-500ms CPU，远超 15.625ms 精度。
            let mut sum: u64 = 0;
            for i in 0u64..100_000_000 {
                sum = sum.wrapping_add(i);
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
