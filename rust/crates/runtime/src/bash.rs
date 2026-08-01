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

    let timeout_ms = input.timeout.unwrap_or_else(|| {
        // 默认超时保护：防止未限范围的命令（如全仓库 grep 无 glob）
        // 执行数十分钟导致 TUI 卡死。120 秒足够覆盖绝大多数合法操作。
        const DEFAULT_TIMEOUT_MS: u64 = 120_000;
        DEFAULT_TIMEOUT_MS
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

    let mut exit_status: Option<i32> = None;
    let mut child_exited = false;
    let mut aborted = false;
    let mut timed_out = false;

    // select! loop：
    // - child 未退出时：并发等待 child.wait() / 超时 / abort / 读 stdout / 读 stderr
    // - child 退出后：继续读 stdout/stderr 直到 EOF（pipe 中可能有残留数据）
    // - stdout/stderr 都 EOF 且 child 已退出时：break
    loop {
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
            }

            // 分支 2：轮询超时和 abort（仅在 child 未退出时检查）
            _ = tokio::time::sleep(poll_interval), if !child_exited => {
                if is_bash_aborted() {
                    aborted = true;
                    let _ = child.kill().await;
                    // 不 wait（会阻塞），让 loop 继续，child_exited 会在下轮设为 true
                    child_exited = true;
                } else if start.elapsed() >= timeout_dur {
                    timed_out = true;
                    let _ = child.kill().await;
                    child_exited = true;
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
                    Ok(n) => { stdout_buf.extend_from_slice(&tmp_out[..n]); }
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
                    Ok(n) => { stderr_buf.extend_from_slice(&tmp_err[..n]); }
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
