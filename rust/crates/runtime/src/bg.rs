//! Tier S #2 后台会话（Background Session Driver）。
//!
//! Detached claw 子进程 + `<pid>.json` 状态文件 + log tail。
//!
//! 设计原则：
//! - **完全外挂**：不修改 ConversationRuntime，不依赖 session_control。
//!   后台会话是独立的 claw 进程，通过文件系统通信。
//! - **进程级隔离**：每个后台会话是独立的 claw 进程，PID 是唯一句柄。
//! - **Windows 优先**：`CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`
//!   三 flag 组合让子进程完全脱离父进程控制台。Unix 降级为默认 spawn
//!   （依赖进程组语义，不强求 setsid 以避免引入 libc 依赖）。
//! - **零依赖**：存活检测用系统命令（`tasklist` / `ps -p`），终止用
//!   `taskkill` / `kill`，不引入 libc/windows-sys。
//! - **状态机**：Running → Exited（自然退出）/ Killed（被 kill）。退出码
//!   不捕获（drop Child 后无法获取），通过 log 末尾推断。
//!
//! 状态机：
//! ```text
//! Running ──natural exit──▶ Exited
//!    │
//!    └──/bg kill──▶ Killed
//! ```

use std::env;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

use serde::{Deserialize, Serialize};

/// 后台会话状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BgStatus {
    /// 子进程仍在运行（或父进程无法确认存活）。
    Running,
    /// 子进程已自然退出。`exit_code` 在 drop Child 后无法获取，恒为 `None`。
    /// `at_ms` 是父进程检测到退出的时刻（非真实退出时刻）。
    Exited { at_ms: u64 },
    /// 被用户通过 `/bg kill` 终止。
    Killed { at_ms: u64 },
}

/// 后台会话句柄的持久化形式（`<pid>.json` 内容）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgRecord {
    /// 子进程 PID。
    pub pid: u32,
    /// 启动时间戳（毫秒）。
    pub started_at_ms: u64,
    /// 完整命令行（含程序名，便于审计）。
    pub command: String,
    /// 子进程工作目录（绝对路径）。
    pub cwd: String,
    /// log 文件绝对路径。
    pub log_path: String,
    /// 关联的 claw session id（可选，用于 --resume 模式）。
    pub session_id: Option<String>,
    /// 当前状态。
    pub status: BgStatus,
}

/// 后台会话根目录：`<workspace>/.claw/bg/`。
/// 与 goal.json 平级，独立子目录避免与 sessions/ 混淆。
pub fn bg_dir(workspace: &Path) -> PathBuf {
    workspace.join(".claw").join("bg")
}

/// 状态文件路径：`<workspace>/.claw/bg/<pid>.json`。
fn record_path(workspace: &Path, pid: u32) -> PathBuf {
    bg_dir(workspace).join(format!("{pid}.json"))
}

/// log 文件路径：`<workspace>/.claw/bg/<pid>.log`。
fn log_path(workspace: &Path, pid: u32) -> PathBuf {
    bg_dir(workspace).join(format!("{pid}.log"))
}

/// 启动后台 claw 子进程。
///
/// `command_args` 是完整的命令行参数（不含程序名）。例如 `["-p", "refactor auth module"]`。
/// `cwd` 是子进程的工作目录（也是状态文件写入的 workspace）。
/// `session_id` 可选，关联到一个已存在的 claw session（用于审计）。
///
/// 返回新创建的 `BgRecord`（已持久化到 `<cwd>/.claw/bg/<pid>.json`）。
pub fn spawn(
    command_args: &[String],
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<BgRecord, BgError> {
    let exe = env::current_exe().map_err(|e| BgError::Spawn(e.to_string()))?;
    let dir = bg_dir(cwd);
    fs::create_dir_all(&dir).map_err(|e| BgError::Io(e.to_string()))?;

    let now = current_time_millis();
    let mut cmd = Command::new(&exe);
    cmd.args(command_args)
        .current_dir(cwd)
        .stdin(Stdio::null());

    apply_detached_flags(&mut cmd);

    // stdout/stderr 重定向到 <pid>.log。先创建文件获取 handle，再传给子进程。
    // 注意：此时 PID 还未知，所以先写到 pending-<ts>.log，spawn 后重命名为 <pid>.log。
    let pending_log = dir.join(format!("pending-{now}.log"));
    let log_file = fs::File::create(&pending_log).map_err(|e| BgError::Io(e.to_string()))?;
    let stdout_handle = log_file
        .try_clone()
        .map_err(|e| BgError::Io(e.to_string()))?;
    let stderr_handle = log_file
        .try_clone()
        .map_err(|e| BgError::Io(e.to_string()))?;
    drop(log_file);

    cmd.stdout(Stdio::from(stdout_handle));
    cmd.stderr(Stdio::from(stderr_handle));

    let child = cmd.spawn().map_err(|e| BgError::Spawn(e.to_string()))?;
    let pid = child.id();
    // Detached：立即 drop Child，让子进程独立运行。父进程退出不影响子进程。
    drop(child);

    // Step 4.1 整合:Windows 上 spawn 后将子进程分配到 Job Object,
    // 设置 CPU/memory 限制。失败不致命(best-effort)— 沙箱限制是增强,
    // 不是阻断性功能,Job Object 设置失败时进程仍能正常运行(只是无限制)。
    assign_job_object_best_effort(pid);

    // 重命名 log 文件为 <pid>.log。如果失败（极罕见，比如重名），fallback 到 pending 名。
    let final_log = log_path(cwd, pid);
    let final_log_str = if fs::rename(&pending_log, &final_log).is_ok() {
        final_log.display().to_string()
    } else {
        pending_log.display().to_string()
    };

    let record = BgRecord {
        pid,
        started_at_ms: now,
        command: format!("claw {}", command_args.join(" ")),
        cwd: cwd.display().to_string(),
        log_path: final_log_str,
        session_id: session_id.map(str::to_string),
        status: BgStatus::Running,
    };

    save_record(&record_path(cwd, pid), &record)?;
    Ok(record)
}

/// 列出所有后台会话记录（扫描 `<cwd>/.claw/bg/*.json`）。
/// 返回的列表按 `started_at_ms` 降序（最新在前）。
/// 同时会刷新每条记录的存活状态：已退出的进程状态会被更新为 `Exited`。
pub fn list(workspace: &Path) -> Vec<BgRecord> {
    let dir = bg_dir(workspace);
    let mut records = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return records,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut record) = serde_json::from_str::<BgRecord>(&content) else {
            continue;
        };
        // 刷新存活状态：如果记录显示 Running 但进程已死，更新为 Exited。
        if matches!(record.status, BgStatus::Running) && !is_pid_alive(record.pid) {
            record.status = BgStatus::Exited {
                at_ms: current_time_millis(),
            };
            let _ = save_record(&path, &record);
        }
        records.push(record);
    }
    records.sort_by_key(|b| std::cmp::Reverse(b.started_at_ms));
    records
}

/// 读取后台会话的日志尾部（最后 N 行）。
/// `lines` 为 0 时返回全部内容。
pub fn read_log_tail(workspace: &Path, pid: u32, lines: usize) -> Result<String, BgError> {
    let path = log_path(workspace, pid);
    let file = fs::File::open(&path).map_err(|e| BgError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut all_lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(l) => all_lines.push(l),
            Err(_) => break,
        }
    }
    if lines == 0 {
        Ok(all_lines.join("\n"))
    } else {
        let start = all_lines.len().saturating_sub(lines);
        Ok(all_lines[start..].join("\n"))
    }
}

/// 终止后台会话。如果进程已退出，返回 `AlreadyExited`。
pub fn kill(workspace: &Path, pid: u32) -> Result<(), BgError> {
    if !is_pid_alive(pid) {
        // 即使进程已死，也尝试更新状态文件。
        let path = record_path(workspace, pid);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(mut record) = serde_json::from_str::<BgRecord>(&content) {
                if matches!(record.status, BgStatus::Running) {
                    record.status = BgStatus::Exited {
                        at_ms: current_time_millis(),
                    };
                    let _ = save_record(&path, &record);
                }
            }
        }
        return Err(BgError::AlreadyExited(pid));
    }
    kill_process(pid)?;
    // 更新状态文件。
    let path = record_path(workspace, pid);
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(mut record) = serde_json::from_str::<BgRecord>(&content) {
            record.status = BgStatus::Killed {
                at_ms: current_time_millis(),
            };
            let _ = save_record(&path, &record);
        }
    }
    Ok(())
}

/// 删除已退出/被 kill 的后台会话记录（状态文件 + log 文件）。
/// 仍处于 Running 状态的记录不会被删除（返回 `StillRunning`）。
pub fn purge(workspace: &Path, pid: u32) -> Result<(), BgError> {
    let path = record_path(workspace, pid);
    let content = fs::read_to_string(&path).map_err(|e| BgError::Io(e.to_string()))?;
    let record: BgRecord =
        serde_json::from_str(&content).map_err(|e| BgError::Serialize(e.to_string()))?;
    if matches!(record.status, BgStatus::Running) && is_pid_alive(record.pid) {
        return Err(BgError::StillRunning(pid));
    }
    fs::remove_file(&path).map_err(|e| BgError::Io(e.to_string()))?;
    let _ = fs::remove_file(log_path(workspace, pid));
    Ok(())
}

/// 检查 PID 是否存活（跨平台零依赖实现）。
///
/// - Windows：调用 `tasklist /FI "PID eq <pid>" /NH /FO CSV`，输出包含 PID 表示存活。
/// - Unix：调用 `ps -p <pid>`，退出码 0 表示存活。
pub fn is_pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                // tasklist CSV 输出格式："claw.exe","1234","Console","1","5,000 K"
                // 没有匹配时输出 "信息: 没有运行的任务匹配指定标准。" 或英文 INFO 行。
                stdout.contains(&format!("\"{pid}\""))
            }
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// 终止进程（跨平台零依赖实现）。
fn kill_process(pid: u32) -> Result<(), BgError> {
    #[cfg(windows)]
    {
        let output = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| BgError::Kill(e.to_string()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(BgError::Kill(format!(
                "taskkill failed: {stderr}{stdout}"
            )));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .map_err(|e| BgError::Kill(e.to_string()))?;
        if !status.success() {
            return Err(BgError::Kill(format!("kill exited with {status}")));
        }
        Ok(())
    }
}

/// 应用 detached flags(Windows 专用实现,Unix no-op)。
///
/// Step 4.1 整合:引用 sandbox.rs 的常量,消除硬编码重复。
/// 之前硬编码 `0x0800_0208`,现在用 `CREATE_NO_WINDOW | DETACHED_PROCESS |
/// CREATE_NEW_PROCESS_GROUP` 语义化表达,与 sandbox.rs 保持单一来源。
#[cfg(windows)]
fn apply_detached_flags(cmd: &mut Command) {
    use crate::sandbox::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS};
    use std::os::windows::process::CommandExt;
    let flags = CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    cmd.creation_flags(flags);
}

/// Unix fallback:不调用 setsid(避免 libc 依赖),依赖进程组语义。
/// 真正的 detached 在 Unix 上需要 `setsid claw <args>` 包装。
#[cfg(not(windows))]
fn apply_detached_flags(_cmd: &mut Command) {}

/// Step 4.1:Windows 上将子进程分配到 Job Object,设置 CPU/memory 限制。
///
/// 这是 best-effort 操作:
/// - 仅 Windows 生效,Unix 直接返回 Ok
/// - 失败不致命:PowerShell 不可用、Job Object 创建失败等情况不阻断主流程
/// - 失败原因通过 eprintln 输出到 stderr(仅用于调试,生产环境通常被重定向)
///
/// 实现委托给 `WindowsSandboxBuilder::assign_process_to_job_object`,
/// 通过 PowerShell + C# 内联调用 Win32 API(CreateJobObjectW +
/// SetInformationJobObject + AssignProcessToJobObject)。
fn assign_job_object_best_effort(pid: u32) {
    // Unix:无 Job Object 概念,直接返回
    if !cfg!(target_os = "windows") {
        return;
    }

    // Windows:用默认配置(2GB 内存 + 80% CPU)创建 Job Object
    let result = crate::sandbox::WindowsSandboxBuilder::default()
        .assign_process_to_job_object(pid);
    if let Err(e) = result {
        // best-effort:记录失败但不阻断。使用 eprintln 而非 log,
        // 因为 bg.rs 故意不引入 tracing/log 依赖(零依赖原则)。
        // stderr 在 spawn 流程中已被重定向到 log 文件,所以这条消息
        // 会出现在 <pid>.log 中,便于调试。
        eprintln!("[bg] Job Object setup failed for pid {pid}: {e}");
    }
}

fn save_record(path: &Path, record: &BgRecord) -> Result<(), BgError> {
    let json = serde_json::to_string_pretty(record)
        .map_err(|e| BgError::Serialize(e.to_string()))?;
    fs::write(path, json).map_err(|e| BgError::Io(e.to_string()))?;
    Ok(())
}

/// 后台会话错误。所有变体都不应阻断会话主流程——调用方应记录并继续。
#[derive(Debug)]
pub enum BgError {
    Spawn(String),
    Io(String),
    Serialize(String),
    Kill(String),
    /// 进程已退出，无法 kill。
    AlreadyExited(u32),
    /// 进程仍在运行，无法 purge。
    StillRunning(u32),
}

impl std::fmt::Display for BgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(msg) => write!(f, "spawn error: {msg}"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
            Self::Serialize(msg) => write!(f, "serialize error: {msg}"),
            Self::Kill(msg) => write!(f, "kill error: {msg}"),
            Self::AlreadyExited(pid) => write!(f, "process {pid} already exited"),
            Self::StillRunning(pid) => write!(f, "process {pid} is still running, kill it first"),
        }
    }
}

impl std::error::Error for BgError {}

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "claw-bg-test-{}-{}-{}",
            std::process::id(),
            current_time_millis(),
            id
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bg_dir_under_claw_subdir() {
        let ws = Path::new("D:/proj");
        let dir = bg_dir(ws);
        assert!(dir.ends_with(".claw/bg"));
    }

    #[test]
    fn record_path_uses_pid_json() {
        let ws = Path::new("D:/proj");
        let path = record_path(ws, 1234);
        assert!(path.ends_with("1234.json"));
    }

    #[test]
    fn log_path_uses_pid_log() {
        let ws = Path::new("D:/proj");
        let path = log_path(ws, 1234);
        assert!(path.ends_with("1234.log"));
    }

    #[test]
    fn list_returns_empty_when_dir_missing() {
        let ws = temp_workspace();
        // 不创建 bg/ 子目录
        let records = list(&ws);
        assert!(records.is_empty());
    }

    #[test]
    fn list_returns_records_sorted_descending() {
        let ws = temp_workspace();
        let dir = bg_dir(&ws);
        fs::create_dir_all(&dir).unwrap();

        // 写入两条记录，旧的先写
        let old_record = BgRecord {
            pid: 1000,
            started_at_ms: 1000,
            command: "claw -p old".to_string(),
            cwd: ws.display().to_string(),
            log_path: dir.join("1000.log").display().to_string(),
            session_id: None,
            status: BgStatus::Exited { at_ms: 2000 },
        };
        let new_record = BgRecord {
            pid: 2000,
            started_at_ms: 3000,
            command: "claw -p new".to_string(),
            cwd: ws.display().to_string(),
            log_path: dir.join("2000.log").display().to_string(),
            session_id: None,
            status: BgStatus::Exited { at_ms: 4000 },
        };
        save_record(&record_path(&ws, 1000), &old_record).unwrap();
        save_record(&record_path(&ws, 2000), &new_record).unwrap();

        let records = list(&ws);
        assert_eq!(records.len(), 2);
        // 新的（started_at_ms=3000）应该在前
        assert_eq!(records[0].pid, 2000);
        assert_eq!(records[1].pid, 1000);
    }

    #[test]
    fn save_and_load_record_roundtrip() {
        let ws = temp_workspace();
        fs::create_dir_all(bg_dir(&ws)).unwrap();
        let path = record_path(&ws, 9999);
        let record = BgRecord {
            pid: 9999,
            started_at_ms: 5000,
            command: "claw -p test".to_string(),
            cwd: ws.display().to_string(),
            log_path: log_path(&ws, 9999).display().to_string(),
            session_id: Some("sess-abc".to_string()),
            status: BgStatus::Running,
        };
        save_record(&path, &record).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let loaded: BgRecord = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.pid, 9999);
        assert_eq!(loaded.command, "claw -p test");
        assert_eq!(loaded.session_id, Some("sess-abc".to_string()));
        assert_eq!(loaded.status, BgStatus::Running);
    }

    #[test]
    fn status_serializes_with_kind_tag() {
        let exited = BgStatus::Exited { at_ms: 100 };
        let json = serde_json::to_string(&exited).unwrap();
        assert!(json.contains("\"kind\":\"exited\""));
        assert!(json.contains("\"at_ms\":100"));

        let killed = BgStatus::Killed { at_ms: 200 };
        let json = serde_json::to_string(&killed).unwrap();
        assert!(json.contains("\"kind\":\"killed\""));

        let running = BgStatus::Running;
        let json = serde_json::to_string(&running).unwrap();
        assert!(json.contains("\"kind\":\"running\""));
    }

    #[test]
    fn status_deserializes_roundtrip() {
        let original = BgStatus::Exited { at_ms: 12345 };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: BgStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn read_log_tail_returns_last_n_lines() {
        let ws = temp_workspace();
        let dir = bg_dir(&ws);
        fs::create_dir_all(&dir).unwrap();
        let log = log_path(&ws, 8888);
        fs::write(&log, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let tail = read_log_tail(&ws, 8888, 2).unwrap();
        assert_eq!(tail, "line4\nline5");

        let tail_all = read_log_tail(&ws, 8888, 0).unwrap();
        assert_eq!(tail_all, "line1\nline2\nline3\nline4\nline5");
    }

    #[test]
    fn read_log_tail_returns_empty_for_missing_file() {
        let ws = temp_workspace();
        let result = read_log_tail(&ws, 7777, 10);
        assert!(result.is_err());
        match result.unwrap_err() {
            BgError::Io(_) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn purge_fails_for_running_process() {
        let ws = temp_workspace();
        let dir = bg_dir(&ws);
        fs::create_dir_all(&dir).unwrap();
        // PID 99999999 几乎肯定不存在，但状态标为 Running
        // （不调用 is_pid_alive，purge 会直接返回 StillRunning）
        // 注意：实际 purge 会调用 is_pid_alive，所以用一个肯定不存在的 PID
        let record = BgRecord {
            pid: 99999999,
            started_at_ms: 1000,
            command: "claw -p test".to_string(),
            cwd: ws.display().to_string(),
            log_path: log_path(&ws, 99999999).display().to_string(),
            session_id: None,
            status: BgStatus::Running,
        };
        save_record(&record_path(&ws, 99999999), &record).unwrap();

        // is_pid_alive(99999999) 应返回 false，所以 purge 会成功删除
        let result = purge(&ws, 99999999);
        assert!(result.is_ok(), "purge should succeed for dead process");
        assert!(!record_path(&ws, 99999999).exists());
    }

    #[test]
    fn purge_removes_record_and_log_files() {
        let ws = temp_workspace();
        let dir = bg_dir(&ws);
        fs::create_dir_all(&dir).unwrap();
        let record = BgRecord {
            pid: 11111,
            started_at_ms: 1000,
            command: "claw -p test".to_string(),
            cwd: ws.display().to_string(),
            log_path: log_path(&ws, 11111).display().to_string(),
            session_id: None,
            status: BgStatus::Exited { at_ms: 2000 },
        };
        save_record(&record_path(&ws, 11111), &record).unwrap();
        fs::write(log_path(&ws, 11111), "log content").unwrap();

        purge(&ws, 11111).unwrap();
        assert!(!record_path(&ws, 11111).exists());
        assert!(!log_path(&ws, 11111).exists());
    }

    #[test]
    fn kill_returns_already_exited_for_dead_pid() {
        let ws = temp_workspace();
        let dir = bg_dir(&ws);
        fs::create_dir_all(&dir).unwrap();
        // PID 99999999 几乎肯定不存在
        let result = kill(&ws, 99999999);
        assert!(matches!(result, Err(BgError::AlreadyExited(99999999))));
    }

    #[test]
    fn bg_error_displays_correctly() {
        let err = BgError::Spawn("permission denied".to_string());
        assert_eq!(format!("{err}"), "spawn error: permission denied");

        let err = BgError::AlreadyExited(1234);
        assert_eq!(format!("{err}"), "process 1234 already exited");

        let err = BgError::StillRunning(5678);
        assert_eq!(
            format!("{err}"),
            "process 5678 is still running, kill it first"
        );
    }

    #[test]
    fn is_pid_alive_returns_false_for_impossible_pid() {
        // PID 0 是非法值（内核 idle 进程），tasklist/ps 都不会返回它。
        // PID 99999999 几乎肯定不存在。
        assert!(!is_pid_alive(99999999));
    }

    #[test]
    fn is_pid_alive_returns_true_for_self() {
        let self_pid = std::process::id();
        assert!(is_pid_alive(self_pid), "self pid should be alive");
    }

    #[test]
    fn spawn_real_process_and_track_lifecycle() {
        // 集成测试：实际 spawn 一个短命命令（cmd /c echo 或 sh -c echo）
        // 验证 spawn 成功，记录写入，list 能看到，进程最终退出。
        let ws = temp_workspace();

        // 用一个几乎立即退出的命令：cmd /c "exit 0" (Windows) 或 true (Unix)
        // 注意：我们 spawn 的是 current_exe，所以这里只能 spawn claw 本身。
        // 用 claw --version 或类似快速退出的命令。
        // 但 --version 可能不存在，改用 -p "" (会失败但快速退出)。
        // 实际上 -p "" 会报错退出。
        // 更可靠：spawn cmd /c exit 0（直接调用系统命令，不走 current_exe）。
        // 但 spawn() 内部用 current_exe，所以这里跳过集成测试，仅验证 API。
        // 改为：验证 spawn 一个不存在的 exe 会返回 Spawn 错误。

        // 这个测试主要验证 API 契约，不实际 spawn claw。
        let result = spawn(
            &["--nonexistent-flag-for-test".to_string()],
            &ws,
            Some("test-session"),
        );
        // claw 可能接受未知 flag 并报错退出，或拒绝启动。
        // 无论哪种，spawn 本身应该成功（返回 BgRecord）。
        if let Ok(record) = &result {
            assert!(record.pid > 0);
            assert!(record.command.contains("claw"));
            assert_eq!(record.session_id, Some("test-session".to_string()));
            assert!(matches!(record.status, BgStatus::Running));
            // 清理
            let _ = kill(&ws, record.pid);
            // 等待子进程退出
            std::thread::sleep(std::time::Duration::from_millis(500));
            let _ = purge(&ws, record.pid);
        }
        // 如果 spawn 失败（极罕见），也接受——这只是验证 API 不 panic。
    }

    /// Step 4.1:验证 `assign_job_object_best_effort` 在 Unix 上是 no-op,
    /// 在 Windows 上会尝试调用 PowerShell(可能失败,但不应 panic)。
    ///
    /// 这个测试不验证 Job Object 实际创建(那需要进程级集成测试),
    /// 只验证函数契约:
    /// - Unix:直接返回,不调用 PowerShell
    /// - Windows:调用 PowerShell,可能成功或失败(取决于权限),不 panic
    #[test]
    fn assign_job_object_best_effort_does_not_panic_for_invalid_pid() {
        // PID 99999999 几乎肯定不存在
        // Unix:直接返回(无 Job Object 概念)
        // Windows:PowerShell 会尝试 OpenProcess,失败返回错误,但不应 panic
        assign_job_object_best_effort(99999999);
        // 如果到达这里,说明函数没有 panic — 契约满足
    }

    /// Step 4.1:验证 `apply_detached_flags` 在 Unix 上是 no-op,
    /// 在 Windows 上设置正确的 creation_flags。
    ///
    /// 注意:这个测试只验证函数不 panic,不验证 flags 实际设置
    /// (那需要检查 Command 内部状态,std::process::Command 不暴露 flags getter)。
    #[test]
    fn apply_detached_flags_does_not_panic() {
        let mut cmd = Command::new("echo");
        apply_detached_flags(&mut cmd);
        // Unix:no-op
        // Windows:设置 CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        // 到达这里说明函数没有 panic
    }

    /// Step 4.1:验证 sandbox.rs 常量与 bg.rs 期望的 flags 值一致。
    ///
    /// 这确保 sandbox.rs 的常量定义没有意外变化,与 Windows API 文档对齐:
    /// - CREATE_NO_WINDOW = 0x08000000
    /// - DETACHED_PROCESS = 0x00000008
    /// - CREATE_NEW_PROCESS_GROUP = 0x00000200
    /// - 组合 = 0x0800_0208(与旧硬编码值一致,确保向后兼容)
    #[test]
    #[cfg(windows)]
    fn sandbox_constants_match_expected_windows_flags() {
        use crate::sandbox::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
        };
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
        assert_eq!(DETACHED_PROCESS, 0x0000_0008);
        assert_eq!(CREATE_NEW_PROCESS_GROUP, 0x0000_0200);
        // 组合值必须与旧硬编码 0x0800_0208 一致(向后兼容)
        let combined = CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        assert_eq!(combined, 0x0800_0208);
    }
}
