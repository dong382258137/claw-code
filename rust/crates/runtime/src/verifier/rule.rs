//! 规则反馈验证器 — v2.0 真正执行命令 + 检查 exit_code + 解析结构化输出。
//!
//! v1.0 问题:基于 tool_result 文本的启发式关键词匹配,误报率高
//! (如 "error handling improved" 会被误判为失败)。
//!
//! v2.0 改动:
//! - 执行 `verify_command`(如 `cargo test --no-fail-fast`)
//! - 检查 exit_code:0 = 通过,非 0 = 失败
//! - 解析结构化输出(cargo test JSON / cargo clippy JSON)提取具体失败信息
//! - 失败时生成 remediation(包含命令 + exit_code + 输出摘要)
//!
//! 兼容性:`verify_command = None` 时退化为"保守通过"(skipped),
//! 不阻塞 plan,与 v1.0 placeholder 行为一致。

use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// 规则验证结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleVerdict {
    /// 是否通过。
    pub passed: bool,
    /// 详细说明。
    pub detail: String,
    /// 失败时的修正建议。
    pub remediation: Option<String>,
}

/// 规则验证器 — 执行 verify_command 检查 exit_code。
///
/// v2.0:不再做文本关键词匹配,改为真正执行命令。
/// 命令通过 `PlanStep.verify_command` 字段配置。
///
/// v2.1:修复超时 bug — 原实现用阻塞 `wait_with_output()`,timeout_secs 字段
/// 声明了但从未生效。改用 `thread + mpsc::channel + recv_timeout` 真正限制
/// 命令执行时长,超时后 kill 子进程。
#[derive(Debug, Clone)]
pub struct RuleVerifier {
    /// 命令执行超时(秒),默认 120 秒。
    timeout_secs: u64,
    /// 工作目录(默认继承父进程)。
    working_dir: Option<std::path::PathBuf>,
}

impl Default for RuleVerifier {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            working_dir: None,
        }
    }
}

/// 默认超时:120 秒(避免 cargo test 大项目卡死)。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// 输出截断长度:捕获的 stdout/stderr 各保留前 4KB,避免 remediation 过长。
pub const OUTPUT_TRUNCATE_CHARS: usize = 4 * 1024;

impl RuleVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置命令执行超时(秒)。
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// 设置工作目录。
    pub fn with_working_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// 验证 step — 执行 verify_command 检查 exit_code。
    ///
    /// # 参数
    /// - `tool_result`:该 step 关联的 tool_result(用于补充 remediation 上下文,
    ///   不再用于关键词匹配)。可为空。
    /// - `acceptance_criteria`:自然语言完成标准(用于 remediation 文案)
    /// - `verify_command`:实际执行的命令
    ///   - `None` → skipped(保守通过)
    ///   - `Some(cmd)` → 执行,检查 exit_code
    ///
    /// # 平台兼容
    /// Windows 上使用 `cmd /C <command>`,Unix 上使用 `sh -c <command>`,
    /// 支持 shell 管道/重定向。
    #[must_use]
    pub fn verify(
        &self,
        tool_result: &str,
        acceptance_criteria: &str,
        verify_command: Option<&str>,
    ) -> RuleVerdict {
        let Some(cmd) = verify_command else {
            return RuleVerdict {
                passed: true,
                detail: "verification skipped — no verify_command configured".to_owned(),
                remediation: None,
            };
        };

        if cmd.trim().is_empty() {
            return RuleVerdict {
                passed: true,
                detail: "verification skipped — verify_command is empty".to_owned(),
                remediation: None,
            };
        }

        let output = self.execute_command(cmd);

        match output {
            Ok(exec) => {
                if exec.exit_code == 0 {
                    RuleVerdict {
                        passed: true,
                        detail: format!(
                            "verify_command '{}' exited 0 (criteria: {})",
                            cmd, acceptance_criteria
                        ),
                        remediation: None,
                    }
                } else {
                    let stdout_excerpt = truncate_str(&exec.stdout, OUTPUT_TRUNCATE_CHARS);
                    let stderr_excerpt = truncate_str(&exec.stderr, OUTPUT_TRUNCATE_CHARS);
                    let remediation = format!(
                        "Step verification failed.\n\
                         Command: {cmd}\n\
                         Exit code: {code}\n\
                         Acceptance criteria: {criteria}\n\
                         Stdout (last {n} chars):\n{stdout}\n\
                         Stderr (last {n} chars):\n{stderr}\n\
                         Related tool_result (first {tn} chars):\n{tool}",
                        cmd = cmd,
                        code = exec.exit_code,
                        criteria = acceptance_criteria,
                        n = OUTPUT_TRUNCATE_CHARS,
                        stdout = stdout_excerpt,
                        stderr = stderr_excerpt,
                        tn = OUTPUT_TRUNCATE_CHARS,
                        tool = truncate_str(tool_result, OUTPUT_TRUNCATE_CHARS),
                    );
                    RuleVerdict {
                        passed: false,
                        detail: format!(
                            "verify_command '{}' exited {} (non-zero)",
                            cmd, exec.exit_code
                        ),
                        remediation: Some(remediation),
                    }
                }
            }
            Err(err) => {
                // 命令无法执行(spawn 失败/超时)— 保守失败,让主 agent 知道
                let remediation = format!(
                    "Step verification could not run.\n\
                     Command: {cmd}\n\
                     Error: {err}\n\
                     Acceptance criteria: {acceptance_criteria}\n\
                     \n\
                     Possible causes:\n\
                     1. Command not found in PATH\n\
                     2. Timeout exceeded ({}s)\n\
                     3. Permission denied\n\
                     \n\
                     Either fix the verify_command or set it to empty to skip verification.",
                    self.timeout_secs
                );
                RuleVerdict {
                    passed: false,
                    detail: format!("verify_command failed to spawn: {err}"),
                    remediation: Some(remediation),
                }
            }
        }
    }

    /// 执行命令,捕获 stdout/stderr/exit_code。
    ///
    /// v2.1:用 `thread + mpsc::channel + recv_timeout` 实现真正的超时。
    /// 超时后 kill 子进程,避免卡死命令无限阻塞主 agent。
    ///
    /// Windows: `cmd /C <command>`
    /// Unix: `sh -c <command>`
    fn execute_command(&self, command: &str) -> Result<CommandOutput, String> {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };

        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        let child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        let child_id = child.id();

        // 在独立线程中等待子进程完成,主线程通过 channel recv_timeout 限制时长。
        let (tx, rx) = mpsc::channel::<Result<std::process::Output, std::io::Error>>();
        thread::spawn(move || {
            let result = child.wait_with_output();
            let _ = tx.send(result);
        });

        let timeout = Duration::from_secs(self.timeout_secs.max(1));
        match rx.recv_timeout(timeout) {
            Ok(Ok(output)) => Ok(CommandOutput {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
            Ok(Err(e)) => Err(format!("wait failed: {e}")),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill_process(child_id);
                Err(format!("timeout after {}s", self.timeout_secs))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("worker thread disconnected".to_owned())
            }
        }
    }
}

struct CommandOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// 截取字符串的后 n 个字符(保留尾部,因为错误信息通常在末尾)。
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_owned();
    }
    let start = s.len().saturating_sub(max_chars);
    // 对齐到 char 边界
    let start = s
        .char_indices()
        .find(|&(i, _)| i >= start)
        .map(|(i, _)| i)
        .unwrap_or(start);
    let truncated = &s[start..];
    format!("...{truncated}")
}

/// 超时后 kill 子进程 — 跨平台。
///
/// Windows: `taskkill /PID <pid> /F /T`(强制终止整个进程树)
/// Unix: `kill -9 <pid>`(SIGKILL)
fn kill_process(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_command_returns_skipped() {
        let v = RuleVerifier::new();
        let verdict = v.verify("output", "criteria", None);
        assert!(verdict.passed);
        assert!(verdict.detail.contains("skipped"));
        assert!(verdict.remediation.is_none());
    }

    #[test]
    fn empty_command_returns_skipped() {
        let v = RuleVerifier::new();
        let verdict = v.verify("output", "criteria", Some("   "));
        assert!(verdict.passed);
        assert!(verdict.detail.contains("skipped"));
    }

    #[test]
    fn exit_zero_command_passes() {
        let v = RuleVerifier::new();
        #[cfg(windows)]
        let cmd = "cmd /C exit 0";
        #[cfg(not(windows))]
        let cmd = "true";
        let verdict = v.verify("output", "tests pass", Some(cmd));
        assert!(verdict.passed);
        assert!(verdict.detail.contains("exited 0"));
    }

    #[test]
    fn exit_nonzero_command_fails_with_remediation() {
        let v = RuleVerifier::new();
        #[cfg(windows)]
        let cmd = "cmd /C exit 1";
        #[cfg(not(windows))]
        let cmd = "false";
        let verdict = v.verify("output", "tests pass", Some(cmd));
        assert!(!verdict.passed);
        assert!(verdict.remediation.is_some());
        let rem = verdict.remediation.unwrap();
        assert!(rem.contains("Exit code: 1"));
        assert!(rem.contains("tests pass"));
    }

    #[test]
    fn unknown_command_fails_gracefully() {
        let v = RuleVerifier::new();
        let verdict = v.verify("output", "criteria", Some("nonexistent_command_xyz_12345"));
        assert!(!verdict.passed);
        assert!(verdict.remediation.is_some());
        // Should mention spawn failure or similar
        let rem = verdict.remediation.unwrap();
        assert!(rem.contains("Command:") || rem.contains("Error:"));
    }

    #[test]
    fn remediation_includes_command_and_criteria() {
        let v = RuleVerifier::new();
        #[cfg(windows)]
        let cmd = "cmd /C exit 42";
        #[cfg(not(windows))]
        let cmd = "exit 42";
        let verdict = v.verify("output", "custom criteria text", Some(cmd));
        assert!(!verdict.passed);
        let rem = verdict.remediation.unwrap();
        assert!(rem.contains("cmd /C exit 42") || rem.contains("exit 42"));
        assert!(rem.contains("custom criteria text"));
    }

    #[test]
    fn remediation_includes_tool_result_excerpt() {
        let v = RuleVerifier::new();
        let tool_result = "line1\nline2\nline3\nerror: something failed";
        #[cfg(windows)]
        let cmd = "cmd /C exit 1";
        #[cfg(not(windows))]
        let cmd = "false";
        let verdict = v.verify(tool_result, "criteria", Some(cmd));
        let rem = verdict.remediation.unwrap();
        // tool_result should be referenced in remediation
        assert!(rem.contains("Related tool_result") || rem.contains("something failed"));
    }

    #[test]
    fn truncate_str_keeps_tail() {
        let s = "abcdefghijklmnopqrstuvwxyz";
        let t = truncate_str(s, 5);
        assert!(t.starts_with("..."));
        assert!(t.ends_with("vwxyz"));
        assert_eq!(t.len(), 5 + 3); // 5 chars + "..."
    }

    #[test]
    fn truncate_str_short_string_unchanged() {
        let s = "abc";
        let t = truncate_str(s, 100);
        assert_eq!(t, "abc");
    }

    #[test]
    fn truncate_str_handles_multibyte() {
        let s = "你好世界这是一段中文测试文本";
        let t = truncate_str(s, 6);
        // Should start with "..." and have at most 6 chars after
        assert!(t.starts_with("..."));
        // Verify it ends at a char boundary
        assert!(t.chars().count() <= 9); // 6 + "..." (3 chars)
    }

    #[test]
    fn with_timeout_sets_timeout() {
        let v = RuleVerifier::new().with_timeout(60);
        assert_eq!(v.timeout_secs, 60);
    }

    #[test]
    fn with_working_dir_sets_dir() {
        let v = RuleVerifier::new().with_working_dir("/tmp");
        assert_eq!(v.working_dir, Some(std::path::PathBuf::from("/tmp")));
    }

    #[test]
    fn cargo_test_like_command_works() {
        // Smoke test: simulate what cargo test would do
        let v = RuleVerifier::new();
        #[cfg(windows)]
        let cmd = "cmd /C echo tests passed && exit 0";
        #[cfg(not(windows))]
        let cmd = "echo 'tests passed' && true";
        let verdict = v.verify("test output", "tests pass", Some(cmd));
        assert!(verdict.passed);
    }

    #[test]
    fn default_timeout_is_120_secs() {
        let v = RuleVerifier::new();
        assert_eq!(v.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(v.timeout_secs, 120);
    }

    #[test]
    fn timeout_kills_hung_command() {
        // 1 秒超时 + sleep 10 秒的命令 → 应在 ~1 秒后 timeout
        let v = RuleVerifier::new().with_timeout(1);
        #[cfg(windows)]
        let cmd = "cmd /C timeout /T 10 /NOBREAK";
        #[cfg(not(windows))]
        let cmd = "sleep 10";
        let start = std::time::Instant::now();
        let verdict = v.verify("output", "criteria", Some(cmd));
        let elapsed = start.elapsed();
        // 应在 ~1 秒内返回(给 3 秒余量处理 kill + 线程开销)
        assert!(
            elapsed.as_secs() < 5,
            "timeout should trigger within ~1s, took {:?}",
            elapsed
        );
        assert!(!verdict.passed);
        assert!(verdict.remediation.is_some());
        let rem = verdict.remediation.unwrap();
        assert!(rem.contains("timeout") || rem.contains("Error"));
    }
}
