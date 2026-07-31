use std::ffi::OsStr;
#[cfg(not(windows))]
use std::path::Path;
use std::process::Command;

use serde_json::json;

use crate::{PluginError, PluginHooks, PluginRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    denied: bool,
    failed: bool,
    messages: Vec<String>,
}

impl HookRunResult {
    #[must_use]
    pub fn allow(messages: Vec<String>) -> Self {
        Self {
            denied: false,
            failed: false,
            messages,
        }
    }

    #[must_use]
    pub fn is_denied(&self) -> bool {
        self.denied
    }

    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    #[must_use]
    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookRunner {
    hooks: PluginHooks,
}

impl HookRunner {
    #[must_use]
    pub fn new(hooks: PluginHooks) -> Self {
        Self { hooks }
    }

    pub fn from_registry(plugin_registry: &PluginRegistry) -> Result<Self, PluginError> {
        Ok(Self::new(plugin_registry.aggregated_hooks()?))
    }

    #[must_use]
    pub fn run_pre_tool_use(&self, tool_name: &str, tool_input: &str) -> HookRunResult {
        Self::run_commands(
            HookEvent::PreToolUse,
            &self.hooks.pre_tool_use,
            tool_name,
            tool_input,
            None,
            false,
        )
    }

    #[must_use]
    pub fn run_post_tool_use(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
    ) -> HookRunResult {
        Self::run_commands(
            HookEvent::PostToolUse,
            &self.hooks.post_tool_use,
            tool_name,
            tool_input,
            Some(tool_output),
            is_error,
        )
    }

    #[must_use]
    pub fn run_post_tool_use_failure(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_error: &str,
    ) -> HookRunResult {
        Self::run_commands(
            HookEvent::PostToolUseFailure,
            &self.hooks.post_tool_use_failure,
            tool_name,
            tool_input,
            Some(tool_error),
            true,
        )
    }

    fn run_commands(
        event: HookEvent,
        commands: &[String],
        tool_name: &str,
        tool_input: &str,
        tool_output: Option<&str>,
        is_error: bool,
    ) -> HookRunResult {
        if commands.is_empty() {
            return HookRunResult::allow(Vec::new());
        }

        let payload = hook_payload(event, tool_name, tool_input, tool_output, is_error).to_string();

        let mut messages = Vec::new();

        for command in commands {
            match Self::run_command(
                command,
                event,
                tool_name,
                tool_input,
                tool_output,
                is_error,
                &payload,
            ) {
                HookCommandOutcome::Allow { message } => {
                    if let Some(message) = message {
                        messages.push(message);
                    }
                }
                HookCommandOutcome::Deny { message } => {
                    messages.push(message.unwrap_or_else(|| {
                        format!("{} hook denied tool `{tool_name}`", event.as_str())
                    }));
                    return HookRunResult {
                        denied: true,
                        failed: false,
                        messages,
                    };
                }
                HookCommandOutcome::Failed { message } => {
                    messages.push(message);
                    return HookRunResult {
                        denied: false,
                        failed: true,
                        messages,
                    };
                }
            }
        }

        HookRunResult::allow(messages)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_command(
        command: &str,
        event: HookEvent,
        tool_name: &str,
        tool_input: &str,
        tool_output: Option<&str>,
        is_error: bool,
        payload: &str,
    ) -> HookCommandOutcome {
        let mut child = shell_command(command);
        child.stdin(std::process::Stdio::piped());
        child.stdout(std::process::Stdio::piped());
        child.stderr(std::process::Stdio::piped());
        child.env("HOOK_EVENT", event.as_str());
        child.env("HOOK_TOOL_NAME", tool_name);
        child.env("HOOK_TOOL_INPUT", tool_input);
        child.env("HOOK_TOOL_IS_ERROR", if is_error { "1" } else { "0" });
        if let Some(tool_output) = tool_output {
            child.env("HOOK_TOOL_OUTPUT", tool_output);
        }

        match child.output_with_stdin(payload.as_bytes()) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let message = (!stdout.is_empty()).then_some(stdout);
                match output.status.code() {
                    Some(0) => HookCommandOutcome::Allow { message },
                    Some(2) => HookCommandOutcome::Deny { message },
                    Some(code) => HookCommandOutcome::Failed {
                        message: format_hook_warning(
                            command,
                            code,
                            message.as_deref(),
                            stderr.as_str(),
                        ),
                    },
                    None => HookCommandOutcome::Failed {
                        message: format!(
                            "{} hook `{command}` terminated by signal while handling `{tool_name}`",
                            event.as_str()
                        ),
                    },
                }
            }
            Err(error) => HookCommandOutcome::Failed {
                message: format!(
                    "{} hook `{command}` failed to start for `{tool_name}`: {error}",
                    event.as_str()
                ),
            },
        }
    }
}

enum HookCommandOutcome {
    Allow { message: Option<String> },
    Deny { message: Option<String> },
    Failed { message: String },
}

fn hook_payload(
    event: HookEvent,
    tool_name: &str,
    tool_input: &str,
    tool_output: Option<&str>,
    is_error: bool,
) -> serde_json::Value {
    match event {
        HookEvent::PostToolUseFailure => json!({
            "hook_event_name": event.as_str(),
            "tool_name": tool_name,
            "tool_input": parse_tool_input(tool_input),
            "tool_input_json": tool_input,
            "tool_error": tool_output,
            "tool_result_is_error": true,
        }),
        _ => json!({
            "hook_event_name": event.as_str(),
            "tool_name": tool_name,
            "tool_input": parse_tool_input(tool_input),
            "tool_input_json": tool_input,
            "tool_output": tool_output,
            "tool_result_is_error": is_error,
        }),
    }
}

fn parse_tool_input(tool_input: &str) -> serde_json::Value {
    serde_json::from_str(tool_input).unwrap_or_else(|_| json!({ "raw": tool_input }))
}

fn format_hook_warning(command: &str, code: i32, stdout: Option<&str>, stderr: &str) -> String {
    let mut message = format!("Hook `{command}` exited with status {code}");
    if let Some(stdout) = stdout.filter(|stdout| !stdout.is_empty()) {
        message.push_str(": ");
        message.push_str(stdout);
    } else if !stderr.is_empty() {
        message.push_str(": ");
        message.push_str(stderr);
    }
    message
}

fn shell_command(command: &str) -> CommandWithStdin {
    #[cfg(windows)]
    let command_builder = {
        let mut command_builder = Command::new("cmd");
        command_builder.arg("/C").arg(command);
        CommandWithStdin::new(command_builder)
    };

    #[cfg(not(windows))]
    let command_builder = if Path::new(command).exists() {
        let mut command_builder = Command::new("sh");
        command_builder.arg(command);
        CommandWithStdin::new(command_builder)
    } else {
        let mut command_builder = Command::new("sh");
        command_builder.arg("-lc").arg(command);
        CommandWithStdin::new(command_builder)
    };

    command_builder
}

struct CommandWithStdin {
    command: Command,
}

impl CommandWithStdin {
    fn new(command: Command) -> Self {
        Self { command }
    }

    fn stdin(&mut self, cfg: std::process::Stdio) -> &mut Self {
        self.command.stdin(cfg);
        self
    }

    fn stdout(&mut self, cfg: std::process::Stdio) -> &mut Self {
        self.command.stdout(cfg);
        self
    }

    fn stderr(&mut self, cfg: std::process::Stdio) -> &mut Self {
        self.command.stderr(cfg);
        self
    }

    fn env<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.env(key, value);
        self
    }

    fn output_with_stdin(&mut self, stdin: &[u8]) -> std::io::Result<std::process::Output> {
        let mut child = self.command.spawn()?;
        if let Some(mut child_stdin) = child.stdin.take() {
            use std::io::Write as _;
            // Tolerate BrokenPipe: a hook script that runs to completion
            // (or exits early without reading stdin) closes its stdin
            // before the parent finishes writing the JSON payload, and
            // the kernel raises EPIPE on the parent's write_all. That is
            // not a hook failure — the child still exited cleanly and we
            // still need to wait_with_output() to capture stdout/stderr
            // and the real exit code. Other write errors (e.g. EIO,
            // permission, OOM) still propagate.
            //
            // This was the root cause of the Linux CI flake on
            // hooks::tests::collects_and_runs_hooks_from_enabled_plugins
            // (ROADMAP #25, runs 24120271422 / 24120538408 / 24121392171
            // / 24121776826): the test hook scripts run in microseconds
            // and the parent's stdin write races against child exit.
            // macOS pipes happen to buffer the small payload before the
            // child exits; Linux pipes do not, so the race shows up
            // deterministically on ubuntu runners.
            match child_stdin.write_all(stdin) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(error) => return Err(error),
            }
        }
        child.wait_with_output()
    }
}

#[cfg(test)]
mod tests {
    use super::{HookRunResult, HookRunner};
    use crate::{PluginManager, PluginManagerConfig};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// RAII guard:测试 panic 时自动清理临时目录(同 lib.rs 的 TempDirGuard)。
    ///
    /// 使用 CARGO_TARGET_TMPDIR(target/tmp/)而非 std::env::temp_dir()
    /// (%TEMP%),避免 .sh 文件被 TRAE CN 文件监视器检测并自动打开,
    /// 导致控制序列泄漏到 claw TUI 终端污染 InputLine。
    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time should be after epoch")
                .as_nanos();
            // 使用 target/tmp/ 而非 %TEMP%,避免 .sh 文件被 TRAE CN
            // 文件监视器检测并自动打开(同 lib.rs 的 TempDirGuard)。
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            let target_tmp = manifest_dir
                .parent()
                .and_then(|p| p.parent())
                .map(|root| root.join("target").join("tmp"))
                .unwrap_or_else(std::env::temp_dir);
            let path = target_tmp.join(format!("plugins-hook-runner-{label}-{nanos}"));
            fs::create_dir_all(&path).expect("create target/tmp");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            // Windows 上文件句柄释放有延迟,重试几次。
            let mut delay_ms = 10;
            for attempt in 0..5u32 {
                match fs::remove_dir_all(&self.path) {
                    Ok(()) => return,
                    Err(_) if attempt < 4 => {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        delay_ms *= 2;
                    }
                    Err(_) => return,
                }
            }
        }
    }

    impl std::ops::Deref for TempDirGuard {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<Path> for TempDirGuard {
        fn as_ref(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<std::ffi::OsStr> for TempDirGuard {
        fn as_ref(&self) -> &std::ffi::OsStr {
            self.path.as_os_str()
        }
    }

    fn temp_dir(label: &str) -> TempDirGuard {
        TempDirGuard::new(label)
    }

    fn write_hook_plugin(
        root: &Path,
        name: &str,
        pre_message: &str,
        post_message: &str,
        failure_message: &str,
    ) {
        fs::create_dir_all(root.join(".claude-plugin")).expect("manifest dir");

        // 跨平台 inline 命令:不创建 .sh 文件,避免文件被 TRAE CN 文件
        // 监视器检测并自动打开后控制序列泄漏到 TUI 终端。
        let (pre_cmd, post_cmd, failure_cmd) = if cfg!(windows) {
            (
                format!("echo {pre_message}"),
                format!("echo {post_message}"),
                format!("echo {failure_message}"),
            )
        } else {
            (
                format!("printf '%s\\n' '{pre_message}'"),
                format!("printf '%s\\n' '{post_message}'"),
                format!("printf '%s\\n' '{failure_message}'"),
            )
        };

        let manifest = serde_json::json!({
            "name": name,
            "version": "1.0.0",
            "description": "hook plugin",
            "hooks": {
                "PreToolUse": [pre_cmd],
                "PostToolUse": [post_cmd],
                "PostToolUseFailure": [failure_cmd]
            }
        });
        fs::write(
            root.join(".claude-plugin").join("plugin.json"),
            manifest.to_string(),
        )
        .expect("write plugin manifest");
    }

    #[test]
    fn collects_and_runs_hooks_from_enabled_plugins() {
        // given
        let config_home = temp_dir("config");
        let first_source_root = temp_dir("source-a");
        let second_source_root = temp_dir("source-b");
        write_hook_plugin(
            &first_source_root,
            "first",
            "plugin pre one",
            "plugin post one",
            "plugin failure one",
        );
        write_hook_plugin(
            &second_source_root,
            "second",
            "plugin pre two",
            "plugin post two",
            "plugin failure two",
        );

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        manager
            .install(first_source_root.to_str().expect("utf8 path"))
            .expect("first plugin install should succeed");
        manager
            .install(second_source_root.to_str().expect("utf8 path"))
            .expect("second plugin install should succeed");
        let registry = manager.plugin_registry().expect("registry should build");

        // when
        let runner = HookRunner::from_registry(&registry).expect("plugin hooks should load");

        // then
        assert_eq!(
            runner.run_pre_tool_use("Read", r#"{"path":"README.md"}"#),
            HookRunResult::allow(vec![
                "plugin pre one".to_string(),
                "plugin pre two".to_string(),
            ])
        );
        assert_eq!(
            runner.run_post_tool_use("Read", r#"{"path":"README.md"}"#, "ok", false),
            HookRunResult::allow(vec![
                "plugin post one".to_string(),
                "plugin post two".to_string(),
            ])
        );
        assert_eq!(
            runner.run_post_tool_use_failure("Read", r#"{"path":"README.md"}"#, "tool failed",),
            HookRunResult::allow(vec![
                "plugin failure one".to_string(),
                "plugin failure two".to_string(),
            ])
        );

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(first_source_root);
        let _ = fs::remove_dir_all(second_source_root);
    }

    #[test]
    fn pre_tool_use_denies_when_plugin_hook_exits_two() {
        // given
        let deny_command = if cfg!(windows) {
            // cmd /C "echo blocked by plugin& exit /b 2"
            "echo blocked by plugin& exit /b 2"
        } else {
            "printf 'blocked by plugin'; exit 2"
        };
        let runner = HookRunner::new(crate::PluginHooks {
            pre_tool_use: vec![deny_command.to_string()],
            post_tool_use: Vec::new(),
            post_tool_use_failure: Vec::new(),
        });

        // when
        let result = runner.run_pre_tool_use("Bash", r#"{"command":"pwd"}"#);

        // then
        assert!(result.is_denied());
        assert_eq!(result.messages(), &["blocked by plugin".to_string()]);
    }

    #[test]
    fn propagates_plugin_hook_failures() {
        // given
        let (broken_cmd, later_cmd) = if cfg!(windows) {
            (
                "echo broken plugin hook& exit /b 1".to_string(),
                "echo later plugin hook".to_string(),
            )
        } else {
            (
                "printf 'broken plugin hook'; exit 1".to_string(),
                "printf 'later plugin hook'".to_string(),
            )
        };
        let runner = HookRunner::new(crate::PluginHooks {
            pre_tool_use: vec![broken_cmd, later_cmd],
            post_tool_use: Vec::new(),
            post_tool_use_failure: Vec::new(),
        });

        // when
        let result = runner.run_pre_tool_use("Bash", r#"{"command":"pwd"}"#);

        // then
        assert!(result.is_failed());
        assert!(result
            .messages()
            .iter()
            .any(|message| message.contains("broken plugin hook")));
        assert!(!result
            .messages()
            .iter()
            .any(|message| message == "later plugin hook"));
    }
}
