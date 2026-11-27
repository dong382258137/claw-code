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

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// 「跳过验证」结果的 detail 前缀。
///
/// 用于区分两种语义截然不同的非通过结果：
/// - **命令执行了但失败** → `passed = false`，`detail` 不含本前缀
/// - **命令在当前平台根本无法执行** → `passed = true` + 本前缀（skipped）
///
/// 后者若被当成 failed，会让 POSIX-only 写法或不存在的脚本把任务误判为失败
/// （假阴性），且用户无法区分「任务真失败」与「验收命令本身跑不起来」。
pub const SKIP_PREFIX: &str = "verification skipped";

/// 「命令在当前平台无法执行」结果的 detail 前缀。
///
/// 与 [`SKIP_PREFIX`] 的区别在于语义归属：
/// - [`SKIP_PREFIX`]：**未配置** verify_command（主动跳过，passed=true）
/// - 本前缀：**配置了但跑不起来**（验证没发生，passed=false）
///
/// 后者必须 `passed=false` —— 否则下游会把步骤标成 Succeeded，构成
/// 静默假通过（比假阴性更危险）。同时它也不能算 failed：任务本身可能
/// 是好的，只是验收命令在当前平台无法执行。故由调用方据此标记
/// `StepStatus::Skipped`。
pub const UNEXECUTABLE_PREFIX: &str = "verification unexecutable";

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

impl RuleVerdict {
    /// 验证被**主动跳过**（未配置 verify_command）。
    ///
    /// 判定依据是 detail 前缀而非新增字段：既有 skip 分支已统一使用
    /// [`SKIP_PREFIX`] 开头，无需改动任何构造点。
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        self.passed && self.detail.starts_with(SKIP_PREFIX)
    }

    /// 命令在当前平台**无法执行**（不是任务失败，是验收命令跑不起来）。
    ///
    /// 调用方应据此标记 `StepStatus::Skipped` 而非 `Failed`，避免把
    /// POSIX-only 写法或不存在的脚本误判成任务失败。
    #[must_use]
    pub fn is_unexecutable(&self) -> bool {
        self.detail.starts_with(UNEXECUTABLE_PREFIX)
    }
}

/// 预检：命令在当前平台上是否**根本无法执行**。
///
/// 返回 `Some(原因)` 表示不可执行，调用方应记为 skipped 而非 failed。
///
/// 设计约束：**只在能确证时返回 Some**。误判为不可执行会跳过真实验证，
/// 让有问题的步骤静默通过，比假阴性更危险。故仅覆盖三类确定性情形：
///
/// 1. POSIX-only shell 内建（`test`/`[`/`export` 等），cmd.exe 无对应内建；
/// 2. 显式脚本路径（`.sh`/`.ps1`/`.bat`/`.cmd`）在磁盘上不存在；
/// 3. 裸命令名不在 PATH 中。
///
/// 不能用 stderr 文本匹配：中文 Windows 下 cmd.exe 输出 GBK，经
/// `from_utf8_lossy` 解码后是乱码，任何英文/中文特征串都匹配不到。
fn preflight_unexecutable(command: &str) -> Option<String> {
    let head = command.split_whitespace().next()?.trim_matches('"');
    if head.is_empty() {
        return None;
    }

    // 1) POSIX-only shell 内建：Windows cmd.exe 不提供。
    #[cfg(windows)]
    {
        const POSIX_ONLY_BUILTINS: &[&str] = &[
            "test", "[", "[[", "export", "source", "unset", "alias", "declare", "local", "trap",
            "umask", "readonly",
        ];
        if POSIX_ONLY_BUILTINS.contains(&head) {
            return Some(format!(
                "`{head}` is a POSIX shell builtin with no cmd.exe equivalent"
            ));
        }
    }

    // 2) 显式脚本路径：存在性可直接判定。
    let lower = head.to_ascii_lowercase();
    if [".sh", ".ps1", ".bat", ".cmd", ".bash"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        if !std::path::Path::new(head).exists() {
            return Some(format!("script `{head}` does not exist"));
        }
        return None;
    }

    // 3) 裸命令名：查 PATH。带路径分隔符的交给系统解析。
    if !head.contains('/') && !head.contains('\\') && !resolves_in_path(head) {
        return Some(format!("`{head}` not found in PATH"));
    }

    None
}

/// 在 PATH 中查找可执行文件（不启子进程，跨平台）。
fn resolves_in_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        // 无 PATH 信息时不做判断，避免误判为不可执行。
        return true;
    };
    // Windows 需要尝试 PATHEXT 后缀；Unix 直接匹配裸名。
    #[cfg(windows)]
    let suffixes: &[&str] = &["", ".exe", ".bat", ".cmd", ".com"];
    #[cfg(not(windows))]
    let suffixes: &[&str] = &[""];

    for dir in std::env::split_paths(&path) {
        for suffix in suffixes {
            if dir.join(format!("{name}{suffix}")).is_file() {
                return true;
            }
        }
    }
    false
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

    /// 可变版本：`&mut self` 设置工作目录。
    ///
    /// 用于运行期补注入 —— `VerifierAgent` 由 app 层在构造 `ConversationRuntime`
    /// 之前创建，此时拿不到 workspace_root；而 `with_workspace_root` 在之后调用。
    pub fn set_working_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        self.working_dir = Some(dir.into());
    }

    /// 当前工作目录（诊断用）。
    #[must_use]
    pub fn working_dir(&self) -> Option<&std::path::Path> {
        self.working_dir.as_deref()
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

        // 2026-11-27 修复(verify 假阴性):先做平台可执行性预检。
        // 命令在当前平台根本无法执行时，语义是「验证没发生」——
        // 既不是失败（任务可能本身是好的），也绝不能当作通过（否则就是
        // 静默假通过）。故返回 passed=false + 可识别的 unexecutable 标记，
        // 由调用方据此标记 StepStatus::Skipped 而非 Failed。
        if let Some(reason) = preflight_unexecutable(cmd) {
            return RuleVerdict {
                passed: false,
                detail: format!(
                    "{UNEXECUTABLE_PREFIX} '{cmd}': {reason} (criteria: {acceptance_criteria})"
                ),
                remediation: Some(format!(
                    "Step verification could not run — the verify_command is not executable \
                     on this platform.\n\
                     Command: {cmd}\n\
                     Error: {reason}\n\
                     Acceptance criteria: {acceptance_criteria}\n\
                     \n\
                     This step is marked SKIPPED (not failed): the work may be fine, but it \
                     was not verified. Replace verify_command with a command runnable on this \
                     platform, or set it to null to skip verification explicitly.",
                )),
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
            // quiet_command:验证命令在 Web 端执行,不能弹控制台窗口。
            let mut c = crate::sandbox::quiet_command("cmd");
            c.arg("/C").arg(command);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = crate::sandbox::quiet_command("sh");
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
        let _ = crate::sandbox::quiet_command("taskkill")
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

    /// 平台无法执行的命令 → 标记为 unexecutable（由调用方转 Skipped），
    /// 而非普通 failed。避免 POSIX-only 写法把正常任务误判为失败。
    #[test]
    fn posix_only_builtin_is_classified_unexecutable_on_windows() {
        let v = RuleVerifier::new();
        let verdict = v.verify("output", "criteria", Some("test 1 -eq 1"));
        #[cfg(windows)]
        {
            assert!(
                verdict.is_unexecutable(),
                "Windows 下 test 内建应判为 unexecutable，实际 detail: {}",
                verdict.detail
            );
            // 不能算通过，否则就是静默假通过。
            assert!(!verdict.passed, "unexecutable 不得当作通过");
            // 也不该被当作普通失败：它不是任务失败。
            assert!(!verdict.is_skipped(), "它不是主动跳过");
        }
        #[cfg(not(windows))]
        {
            // Unix 上 test 是真命令，应当真的执行并通过。
            assert!(verdict.passed, "Unix 下 test 1 -eq 1 应通过");
            assert!(!verdict.is_unexecutable());
        }
    }

    /// 显式脚本路径不存在 → unexecutable。
    #[test]
    fn missing_script_path_is_classified_unexecutable() {
        let v = RuleVerifier::new();
        let verdict = v.verify(
            "output",
            "criteria",
            Some("scripts/definitely_missing_script_xyz.sh --env e2e"),
        );
        assert!(
            verdict.is_unexecutable(),
            "不存在的脚本应判为 unexecutable，实际: {}",
            verdict.detail
        );
        assert!(!verdict.passed);
    }

    /// 可执行但退出码非零 → 仍是 failed，不能被 unexecutable 误吞。
    #[test]
    fn executable_with_nonzero_exit_stays_failed() {
        let v = RuleVerifier::new();
        #[cfg(windows)]
        let cmd = "cmd /C exit 7";
        #[cfg(not(windows))]
        let cmd = "exit 7";
        let verdict = v.verify("output", "criteria", Some(cmd));
        assert!(!verdict.passed, "非零退出码必须是失败");
        assert!(
            !verdict.is_unexecutable(),
            "能执行但失败，不得归为 unexecutable: {}",
            verdict.detail
        );
        assert!(!verdict.is_skipped(), "不是主动跳过");
    }

    /// 可执行且退出码 0 → 通过，且不属于任何 skipped/unexecutable 分类。
    #[test]
    fn executable_zero_exit_is_plain_pass() {
        let v = RuleVerifier::new();
        #[cfg(windows)]
        let cmd = "cmd /C exit 0";
        #[cfg(not(windows))]
        let cmd = "true";
        let verdict = v.verify("output", "criteria", Some(cmd));
        assert!(verdict.passed);
        assert!(!verdict.is_skipped());
        assert!(!verdict.is_unexecutable());
    }

    /// PATH 中真实存在的命令不得被误判为 unexecutable。
    #[test]
    fn command_present_in_path_is_not_unexecutable() {
        let v = RuleVerifier::new();
        // cargo 一定在 PATH 中（本测试由 cargo 驱动）。
        let verdict = v.verify("output", "criteria", Some("cargo --version"));
        assert!(
            !verdict.is_unexecutable(),
            "cargo 在 PATH 中，不应判为 unexecutable: {}",
            verdict.detail
        );
        assert!(verdict.passed, "cargo --version 应退出 0");
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
