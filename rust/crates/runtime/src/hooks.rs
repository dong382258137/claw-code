use std::ffi::OsStr;
use std::fmt::Write as FmtWrite;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, RwLock,
};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{
    FailurePolicy, HookDefinition, HookHandlerType, RuntimeFeatureConfig, RuntimeHookConfig,
};
use crate::permissions::PermissionOverride;

const HOOK_PREVIEW_CHAR_LIMIT: usize = 160;

/// 无 timeout 配置的 hook 的总执行预算(毫秒)。
///
/// design-gaps #1「异步 HookRunner」：决策性事件(PreToolUse/PostToolUse 等)
/// 需要同步结果,但失控/卡死的 hook 不能无限期阻塞对话循环。即使某个 hook
/// 未配置独立 timeout,整体等待也不超过该预算,超时部分按 Failed 处理。
const DEFAULT_HOOK_ASYNC_BUDGET_MS: u64 = 60_000;

/// 测试专用:覆盖全局预算,避免在单测里真的等 60s。
/// `0` = 使用 [`DEFAULT_HOOK_ASYNC_BUDGET_MS`]。
static HOOK_BUDGET_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

fn hook_budget_ms() -> u64 {
    let override_ms = HOOK_BUDGET_OVERRIDE_MS.load(Ordering::Relaxed);
    if override_ms > 0 {
        override_ms
    } else {
        DEFAULT_HOOK_ASYNC_BUDGET_MS
    }
}

pub type HookPermissionDecision = PermissionOverride;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    UserPromptSubmit,
    Notification,
    SessionStart,
    SessionEnd,
    Stop,
    SubagentStop,
    PreCompact,
    PostCustomToolCall,
}

impl HookEvent {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Notification => "Notification",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
            Self::PostCustomToolCall => "PostCustomToolCall",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookProgressEvent {
    Started {
        event: HookEvent,
        tool_name: String,
        command: String,
    },
    Completed {
        event: HookEvent,
        tool_name: String,
        command: String,
    },
    Cancelled {
        event: HookEvent,
        tool_name: String,
        command: String,
    },
}

pub trait HookProgressReporter: Send {
    fn on_event(&mut self, event: &HookProgressEvent);
}

#[derive(Debug, Clone, Default)]
pub struct HookAbortSignal {
    aborted: Arc<AtomicBool>,
}

impl HookAbortSignal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Reset the abort flag to false.
    ///
    /// Used by `ClawAgent::prompt` to clear a sticky abort state from a
    /// previous turn before starting a new turn. Without this, a cancel
    /// signal from turn N would immediately abort turn N+1.
    pub fn reset(&self) {
        self.aborted.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    denied: bool,
    failed: bool,
    cancelled: bool,
    messages: Vec<String>,
    permission_override: Option<PermissionOverride>,
    permission_reason: Option<String>,
    updated_input: Option<String>,
    /// 抑制工具原始输出：true 时调用方应丢弃 output，仅保留 messages
    /// （LoopDetector 检测到重复输出/重复调用时置位，避免模型基于
    /// 未变化的旧输出盲目重试）。
    suppress_output: bool,
}

impl HookRunResult {
    #[must_use]
    pub fn allow(messages: Vec<String>) -> Self {
        Self {
            denied: false,
            failed: false,
            cancelled: false,
            messages,
            permission_override: None,
            permission_reason: None,
            updated_input: None,
            suppress_output: false,
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
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// 是否应抑制工具原始输出（LoopDetector 重复检测触发）。
    #[must_use]
    pub fn should_suppress_output(&self) -> bool {
        self.suppress_output
    }

    /// 构造一个抑制原始输出的 HookRunResult（allow 状态），携带提示消息。
    ///
    /// LoopDetector 检测到重复输出/重复调用时返回此结果：调用方丢弃
    /// 工具 output，只把提示消息回灌给模型，切断"看到相同输出 → 继续重试"
    /// 的验证循环。
    #[must_use]
    pub fn suppressed_with_messages(messages: Vec<String>) -> Self {
        Self {
            denied: false,
            failed: false,
            cancelled: false,
            messages,
            permission_override: None,
            permission_reason: None,
            updated_input: None,
            suppress_output: true,
        }
    }

    /// 标记抑制原始输出（保留 denied/failed/cancelled 等既有状态不变）。
    ///
    /// 用于 LoopDetector 重复警告注入路径：在已运行的 base hook 结果上
    /// 追加抑制标记，不改变 hook 的决策状态。
    pub fn mark_suppress_output(&mut self) {
        self.suppress_output = true;
    }

    #[must_use]
    pub fn messages(&self) -> &[String] {
        &self.messages
    }

    #[must_use]
    pub fn permission_override(&self) -> Option<PermissionOverride> {
        self.permission_override
    }

    #[must_use]
    pub fn permission_decision(&self) -> Option<HookPermissionDecision> {
        self.permission_override
    }

    #[must_use]
    pub fn permission_reason(&self) -> Option<&str> {
        self.permission_reason.as_deref()
    }

    #[must_use]
    pub fn updated_input(&self) -> Option<&str> {
        self.updated_input.as_deref()
    }

    #[must_use]
    pub fn updated_input_json(&self) -> Option<&str> {
        self.updated_input()
    }

    /// 构造一个 cancelled=true 的 HookRunResult,携带单条消息。
    ///
    /// BUG-2 修复:LoopDetectionMiddleware 检测到 Doom Loop 时调用此方法,
    /// 阻断当前 turn。详见 docs/harness-engineering-optimization-plan.md Step 2.2。
    #[must_use]
    pub fn cancelled_with_message(message: String) -> Self {
        Self {
            denied: false,
            failed: false,
            cancelled: true,
            messages: vec![message],
            permission_override: None,
            permission_reason: None,
            updated_input: None,
            suppress_output: false,
        }
    }

    /// 追加一条消息到现有 HookRunResult(不改变 denied/failed/cancelled 状态)。
    ///
    /// BUG-2 修复:LoopDetectionMiddleware 触发 InjectContext 时,
    /// 把警告消息附加到 hook 结果,让主 agent 在下一轮看到提示。
    pub fn append_message(&mut self, message: String) {
        self.messages.push(message);
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookRunner {
    /// 配置经 `Arc<RwLock>` 共享可变：支持配置热重载（`reload` 原子替换），
    /// 运行时所有 run_* 方法从读锁获取最新配置。
    config: Arc<RwLock<RuntimeHookConfig>>,
}

impl HookRunner {
    #[must_use]
    pub fn new(config: RuntimeHookConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
        }
    }

    #[must_use]
    pub fn from_feature_config(feature_config: &RuntimeFeatureConfig) -> Self {
        Self::new(feature_config.hooks().clone())
    }

    /// 热重载：原子替换 hooks 配置，后续 hook 调用立即使用新配置。
    /// design-gaps #1「配置热重载」：修改 hooks 配置（settings.json）无需
    /// 重启会话。
    pub fn reload(&self, config: RuntimeHookConfig) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    /// 从 feature config 热重载 hooks 配置。
    pub fn reload_from_feature_config(&self, feature_config: &RuntimeFeatureConfig) {
        self.reload(feature_config.hooks().clone());
    }

    /// 返回当前 hooks 配置快照（读锁 clone）。
    #[must_use]
    pub fn current_config(&self) -> RuntimeHookConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// fire-and-forget：生命周期事件（Stop/SessionStart/Notification 等）在
    /// 后台线程异步执行，立即返回，**不阻塞对话循环**。即使 hook 卡死/失控，
    /// 也只影响后台线程。调用方不关心返回值（conversation.rs 均为 `let _ =`）。
    pub fn spawn_lifecycle_event(&self, event: HookEvent, context: String) {
        let definitions = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .lifecycle()
            .get(event.as_str())
            .cloned()
            .unwrap_or_default();
        if definitions.is_empty() {
            return;
        }
        thread::spawn(move || {
            let _ =
                Self::run_definitions(event, &definitions, &context, "{}", None, false, None, None);
        });
    }

    #[must_use]
    pub fn run_pre_tool_use(&self, tool_name: &str, tool_input: &str) -> HookRunResult {
        self.run_pre_tool_use_with_context(tool_name, tool_input, None, None)
    }

    #[must_use]
    pub fn run_pre_tool_use_with_context(
        &self,
        tool_name: &str,
        tool_input: &str,
        abort_signal: Option<&HookAbortSignal>,
        reporter: Option<&mut dyn HookProgressReporter>,
    ) -> HookRunResult {
        Self::run_definitions(
            HookEvent::PreToolUse,
            self.config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .pre_tool_use(),
            tool_name,
            tool_input,
            None,
            false,
            abort_signal,
            reporter,
        )
    }

    #[must_use]
    pub fn run_pre_tool_use_with_signal(
        &self,
        tool_name: &str,
        tool_input: &str,
        abort_signal: Option<&HookAbortSignal>,
    ) -> HookRunResult {
        self.run_pre_tool_use_with_context(tool_name, tool_input, abort_signal, None)
    }

    #[must_use]
    pub fn run_post_tool_use(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
    ) -> HookRunResult {
        self.run_post_tool_use_with_context(
            tool_name,
            tool_input,
            tool_output,
            is_error,
            None,
            None,
        )
    }

    #[must_use]
    pub fn run_post_tool_use_with_context(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
        abort_signal: Option<&HookAbortSignal>,
        reporter: Option<&mut dyn HookProgressReporter>,
    ) -> HookRunResult {
        Self::run_definitions(
            HookEvent::PostToolUse,
            self.config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .post_tool_use(),
            tool_name,
            tool_input,
            Some(tool_output),
            is_error,
            abort_signal,
            reporter,
        )
    }

    #[must_use]
    pub fn run_post_tool_use_with_signal(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
        abort_signal: Option<&HookAbortSignal>,
    ) -> HookRunResult {
        self.run_post_tool_use_with_context(
            tool_name,
            tool_input,
            tool_output,
            is_error,
            abort_signal,
            None,
        )
    }

    #[must_use]
    pub fn run_post_tool_use_failure(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_error: &str,
    ) -> HookRunResult {
        self.run_post_tool_use_failure_with_context(tool_name, tool_input, tool_error, None, None)
    }

    #[must_use]
    pub fn run_post_tool_use_failure_with_context(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_error: &str,
        abort_signal: Option<&HookAbortSignal>,
        reporter: Option<&mut dyn HookProgressReporter>,
    ) -> HookRunResult {
        Self::run_definitions(
            HookEvent::PostToolUseFailure,
            self.config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .post_tool_use_failure(),
            tool_name,
            tool_input,
            Some(tool_error),
            true,
            abort_signal,
            reporter,
        )
    }

    #[must_use]
    pub fn run_post_tool_use_failure_with_signal(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_error: &str,
        abort_signal: Option<&HookAbortSignal>,
    ) -> HookRunResult {
        self.run_post_tool_use_failure_with_context(
            tool_name,
            tool_input,
            tool_error,
            abort_signal,
            None,
        )
    }

    // ── G9.1: Additional lifecycle event methods ──

    #[must_use]
    pub fn run_lifecycle_event(&self, event: HookEvent, context: &str) -> HookRunResult {
        let event_name = event.as_str();
        let definitions = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .lifecycle()
            .get(event_name)
            .cloned()
            .unwrap_or_default();
        if definitions.is_empty() {
            return HookRunResult::allow(Vec::new());
        }
        Self::run_definitions(event, &definitions, context, "{}", None, false, None, None)
    }

    #[must_use]
    pub fn run_user_prompt_submit(&self, prompt: &str) -> HookRunResult {
        self.run_lifecycle_event(HookEvent::UserPromptSubmit, prompt)
    }
    #[must_use]
    pub fn run_notification(&self, message: &str) -> HookRunResult {
        self.run_lifecycle_event(HookEvent::Notification, message)
    }
    #[must_use]
    pub fn run_session_start(&self, session_id: &str) -> HookRunResult {
        self.run_lifecycle_event(HookEvent::SessionStart, session_id)
    }
    #[must_use]
    pub fn run_session_end(&self, session_id: &str) -> HookRunResult {
        self.run_lifecycle_event(HookEvent::SessionEnd, session_id)
    }
    #[must_use]
    pub fn run_stop(&self, reason: &str) -> HookRunResult {
        self.run_lifecycle_event(HookEvent::Stop, reason)
    }
    #[must_use]
    pub fn run_subagent_stop(&self, subagent_id: &str) -> HookRunResult {
        self.run_lifecycle_event(HookEvent::SubagentStop, subagent_id)
    }
    #[must_use]
    pub fn run_pre_compact(&self, context: &str) -> HookRunResult {
        self.run_lifecycle_event(HookEvent::PreCompact, context)
    }
    #[must_use]
    pub fn run_post_custom_tool_call(&self, tool_name: &str, tool_output: &str) -> HookRunResult {
        // PostCustomToolCall:自定义工具(经 ToolExecutor 注册的外部工具)调用
        // 完成后的监控/审计事件。payload 携带真实 tool_name 与 tool_output,
        // 语义与 PostToolUse 区分开(后者覆盖全部工具)。
        let definitions = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .lifecycle()
            .get(HookEvent::PostCustomToolCall.as_str())
            .cloned()
            .unwrap_or_default();
        if definitions.is_empty() {
            return HookRunResult::allow(Vec::new());
        }
        Self::run_definitions(
            HookEvent::PostCustomToolCall,
            &definitions,
            tool_name,
            "{}",
            Some(tool_output),
            false,
            None,
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn run_definitions(
        event: HookEvent,
        definitions: &[HookDefinition],
        tool_name: &str,
        tool_input: &str,
        tool_output: Option<&str>,
        is_error: bool,
        abort_signal: Option<&HookAbortSignal>,
        mut reporter: Option<&mut dyn HookProgressReporter>,
    ) -> HookRunResult {
        if definitions.is_empty() {
            return HookRunResult::allow(Vec::new());
        }

        if abort_signal.is_some_and(HookAbortSignal::is_aborted) {
            return HookRunResult {
                denied: false,
                failed: false,
                cancelled: true,
                messages: vec![format!(
                    "{} hook cancelled before execution",
                    event.as_str()
                )],
                permission_override: None,
                permission_reason: None,
                updated_input: None,
                suppress_output: false,
            };
        }

        let payload = hook_payload(event, tool_name, tool_input, tool_output, is_error).to_string();
        let mut result = HookRunResult::allow(Vec::new());

        // design-gaps #1「异步 HookRunner」:决策性事件(PreToolUse/PostToolUse
        // 等)保留同步语义,但未配置独立 timeout 的 hook 也不能无限期阻塞对话
        // 循环。从本轮第一次执行起算共享预算,多个 hook 累计占用;预算耗尽后
        // 剩余 hook 按 1ms 超时立即 Failed(调用方按 failure_policy 处理)。
        let budget_deadline = Instant::now() + Duration::from_millis(hook_budget_ms());

        for def in definitions {
            // matcher 正则过滤：任一正则命中 tool_name 才执行该 hook；
            // None 或空列表 = 匹配全部工具（向后兼容，与旧行为一致）。
            if !hook_matches(def, tool_name) {
                continue;
            }

            let label = match def.handler_type {
                HookHandlerType::Command => def.value.clone(),
                HookHandlerType::Script => "<script>".to_string(),
                HookHandlerType::Http => def.value.clone(),
                HookHandlerType::Mcp => format!("mcp:{}", &def.value),
            };

            if let Some(reporter) = reporter.as_deref_mut() {
                reporter.on_event(&HookProgressEvent::Started {
                    event,
                    tool_name: tool_name.to_string(),
                    command: label.clone(),
                });
            }

            // 独立 timeout 优先;未配置时用全局预算的剩余时间兜底。
            let effective_timeout_ms = def.timeout_ms.or_else(|| {
                let remaining = budget_deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64;
                Some(remaining.max(1))
            });

            let outcome = match def.handler_type {
                HookHandlerType::Command => Self::run_command(
                    &def.value,
                    event,
                    tool_name,
                    tool_input,
                    tool_output,
                    is_error,
                    &payload,
                    abort_signal,
                    effective_timeout_ms,
                ),
                HookHandlerType::Script => Self::run_script_handler(
                    &def.value,
                    event,
                    tool_name,
                    tool_input,
                    &payload,
                    abort_signal,
                    effective_timeout_ms,
                ),
                HookHandlerType::Http => Self::run_http_handler(
                    &def.value,
                    event,
                    tool_name,
                    &payload,
                    abort_signal,
                    effective_timeout_ms,
                ),
                HookHandlerType::Mcp => {
                    Self::run_mcp_handler(&def.value, event, tool_name, &payload)
                }
            };

            let completed_label = label.clone();
            match outcome {
                HookCommandOutcome::Allow { parsed } => {
                    if let Some(reporter) = reporter.as_deref_mut() {
                        reporter.on_event(&HookProgressEvent::Completed {
                            event,
                            tool_name: tool_name.to_string(),
                            command: completed_label,
                        });
                    }
                    merge_parsed_hook_output(&mut result, parsed);
                }
                HookCommandOutcome::Deny { parsed } => {
                    if let Some(reporter) = reporter.as_deref_mut() {
                        reporter.on_event(&HookProgressEvent::Completed {
                            event,
                            tool_name: tool_name.to_string(),
                            command: completed_label,
                        });
                    }
                    merge_parsed_hook_output(&mut result, parsed);
                    result.denied = true;
                    if def.failure_policy == FailurePolicy::FailOpen {
                        result.denied = false;
                        continue;
                    }
                    return result;
                }
                HookCommandOutcome::Failed { parsed } => {
                    if let Some(reporter) = reporter.as_deref_mut() {
                        reporter.on_event(&HookProgressEvent::Completed {
                            event,
                            tool_name: tool_name.to_string(),
                            command: completed_label,
                        });
                    }
                    merge_parsed_hook_output(&mut result, parsed);
                    result.failed = true;
                    if def.failure_policy == FailurePolicy::FailOpen {
                        result.failed = false;
                        continue;
                    }
                    return result;
                }
                HookCommandOutcome::Cancelled { message } => {
                    if let Some(reporter) = reporter.as_deref_mut() {
                        reporter.on_event(&HookProgressEvent::Cancelled {
                            event,
                            tool_name: tool_name.to_string(),
                            command: completed_label,
                        });
                    }
                    result.cancelled = true;
                    result.messages.push(message);
                    return result;
                }
            }
        }

        result
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
        abort_signal: Option<&HookAbortSignal>,
        timeout_ms: Option<u64>,
    ) -> HookCommandOutcome {
        let mut child = shell_command(command);
        child.stdin(Stdio::piped());
        child.stdout(Stdio::piped());
        child.stderr(Stdio::piped());
        child.env("HOOK_EVENT", event.as_str());
        child.env("HOOK_TOOL_NAME", tool_name);
        child.env("HOOK_TOOL_INPUT", tool_input);
        child.env("HOOK_TOOL_IS_ERROR", if is_error { "1" } else { "0" });
        if let Some(tool_output) = tool_output {
            child.env("HOOK_TOOL_OUTPUT", tool_output);
        }

        match child.output_with_stdin(
            payload.as_bytes(),
            abort_signal,
            timeout_ms.map(Duration::from_millis),
        ) {
            Ok(CommandExecution::Finished(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let parsed = parse_hook_output(event, tool_name, command, &stdout, &stderr);
                let primary_message = parsed.primary_message().map(ToOwned::to_owned);
                match output.status.code() {
                    Some(0) => {
                        if parsed.deny {
                            HookCommandOutcome::Deny { parsed }
                        } else {
                            HookCommandOutcome::Allow { parsed }
                        }
                    }
                    Some(2) => HookCommandOutcome::Deny {
                        parsed: parsed.with_fallback_message(format!(
                            "{} hook denied tool `{tool_name}`",
                            event.as_str()
                        )),
                    },
                    Some(code) => HookCommandOutcome::Failed {
                        parsed: parsed.with_fallback_message(format_hook_failure(
                            command,
                            code,
                            primary_message.as_deref(),
                            stderr.as_str(),
                        )),
                    },
                    None => HookCommandOutcome::Failed {
                        parsed: parsed.with_fallback_message(format!(
                            "{} hook `{command}` terminated by signal while handling `{}`",
                            event.as_str(),
                            tool_name
                        )),
                    },
                }
            }
            Ok(CommandExecution::Cancelled) => HookCommandOutcome::Cancelled {
                message: format!(
                    "{} hook `{command}` cancelled while handling `{tool_name}`",
                    event.as_str()
                ),
            },
            Ok(CommandExecution::TimedOut) => HookCommandOutcome::Failed {
                parsed: ParsedHookOutput {
                    messages: vec![format!(
                        "{} hook `{command}` timed out after {}ms while handling `{tool_name}`",
                        event.as_str(),
                        timeout_ms.unwrap_or_default()
                    )],
                    ..ParsedHookOutput::default()
                },
            },
            Err(error) => HookCommandOutcome::Failed {
                parsed: ParsedHookOutput {
                    messages: vec![format!(
                        "{} hook `{command}` failed to start for `{}`: {error}",
                        event.as_str(),
                        tool_name
                    )],
                    ..ParsedHookOutput::default()
                },
            },
        }
    }

    // ── G9.3: Script handler ──

    #[allow(clippy::too_many_arguments)]
    fn run_script_handler(
        script: &str,
        event: HookEvent,
        tool_name: &str,
        tool_input: &str,
        payload: &str,
        abort_signal: Option<&HookAbortSignal>,
        timeout_ms: Option<u64>,
    ) -> HookCommandOutcome {
        let (program, flag) = crate::bash::shell_launcher();
        let mut child = CommandWithStdin::new(Command::new(&program));
        child.command.arg(flag);
        child.stdin(Stdio::piped());
        child.stdout(Stdio::piped());
        child.stderr(Stdio::piped());
        child.env("HOOK_EVENT", event.as_str());
        child.env("HOOK_TOOL_NAME", tool_name);
        child.env("HOOK_TOOL_INPUT", tool_input);
        child.env("HOOK_PAYLOAD", payload);
        let stdin_content = format!("{}\n# --- hook payload (JSON) ---\n{}", script, payload);
        let label = bounded_hook_preview(script).unwrap_or_else(|| "<script>".to_string());
        match child.output_with_stdin(
            stdin_content.as_bytes(),
            abort_signal,
            timeout_ms.map(Duration::from_millis),
        ) {
            Ok(CommandExecution::Finished(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let parsed = parse_hook_output(event, tool_name, &label, &stdout, &stderr);
                let primary_message = parsed.primary_message().map(ToOwned::to_owned);
                match output.status.code() {
                    Some(0) => {
                        if parsed.deny {
                            HookCommandOutcome::Deny { parsed }
                        } else {
                            HookCommandOutcome::Allow { parsed }
                        }
                    }
                    Some(2) => HookCommandOutcome::Deny {
                        parsed: parsed.with_fallback_message(format!(
                            "{} hook script denied tool `{tool_name}`",
                            event.as_str()
                        )),
                    },
                    Some(code) => HookCommandOutcome::Failed {
                        parsed: parsed.with_fallback_message(format!(
                            "{} hook script exited with status {code}: {}",
                            event.as_str(),
                            primary_message.as_deref().unwrap_or(&stderr)
                        )),
                    },
                    None => HookCommandOutcome::Failed {
                        parsed: parsed.with_fallback_message(format!(
                            "{} hook script terminated by signal while handling `{tool_name}`",
                            event.as_str()
                        )),
                    },
                }
            }
            Ok(CommandExecution::Cancelled) => HookCommandOutcome::Cancelled {
                message: format!(
                    "{} hook script cancelled while handling `{tool_name}`",
                    event.as_str()
                ),
            },
            Ok(CommandExecution::TimedOut) => HookCommandOutcome::Failed {
                parsed: ParsedHookOutput {
                    messages: vec![format!(
                        "{} hook script timed out after {}ms while handling `{tool_name}`",
                        event.as_str(),
                        timeout_ms.unwrap_or_default()
                    )],
                    ..ParsedHookOutput::default()
                },
            },
            Err(error) => HookCommandOutcome::Failed {
                parsed: ParsedHookOutput {
                    messages: vec![format!(
                        "{} hook script failed to start for `{tool_name}`: {error}",
                        event.as_str()
                    )],
                    ..ParsedHookOutput::default()
                },
            },
        }
    }

    // ── G9.4: HTTP webhook handler (shell-out to curl) ──

    fn run_http_handler(
        url: &str,
        event: HookEvent,
        tool_name: &str,
        payload: &str,
        abort_signal: Option<&HookAbortSignal>,
        timeout_ms: Option<u64>,
    ) -> HookCommandOutcome {
        if abort_signal.is_some_and(HookAbortSignal::is_aborted) {
            return HookCommandOutcome::Cancelled {
                message: format!("{} hook http cancelled before request", event.as_str()),
            };
        }
        let label = bounded_hook_preview(url).unwrap_or_else(|| "<http>".to_string());
        let curl_cmd = format!(
            "curl -s -f -X POST -H 'Content-Type: application/json' -H 'X-Hook-Event: {}' -H 'X-Hook-Tool: {}' -d @- '{}'",
            event.as_str(), tool_name, url
        );
        let (program, flag) = crate::bash::shell_launcher();
        let mut child = CommandWithStdin::new(Command::new(&program));
        child.command.arg(flag).arg(&curl_cmd);
        child.stdin(Stdio::piped());
        child.stdout(Stdio::piped());
        child.stderr(Stdio::piped());
        match child.output_with_stdin(
            payload.as_bytes(),
            abort_signal,
            timeout_ms.map(Duration::from_millis),
        ) {
            Ok(CommandExecution::Finished(output)) => {
                let body = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr_out = String::from_utf8_lossy(&output.stderr).trim().to_string();
                match output.status.code() {
                    Some(0) => {
                        let parsed =
                            parse_hook_output(event, tool_name, &label, &body, &stderr_out);
                        if parsed.deny {
                            HookCommandOutcome::Deny { parsed }
                        } else {
                            HookCommandOutcome::Allow { parsed }
                        }
                    }
                    Some(code) => HookCommandOutcome::Failed {
                        parsed: ParsedHookOutput {
                            messages: vec![format!(
                                "{} hook http `{label}` returned exit {code}: {}",
                                event.as_str(),
                                stderr_out.lines().next().unwrap_or(&body)
                            )],
                            ..ParsedHookOutput::default()
                        },
                    },
                    None => HookCommandOutcome::Failed {
                        parsed: ParsedHookOutput {
                            messages: vec![format!(
                                "{} hook http `{label}` terminated by signal",
                                event.as_str()
                            )],
                            ..ParsedHookOutput::default()
                        },
                    },
                }
            }
            Ok(CommandExecution::Cancelled) => HookCommandOutcome::Cancelled {
                message: format!("{} hook http `{label}` cancelled", event.as_str()),
            },
            Ok(CommandExecution::TimedOut) => HookCommandOutcome::Failed {
                parsed: ParsedHookOutput {
                    messages: vec![format!(
                        "{} hook http `{label}` timed out after {}ms",
                        event.as_str(),
                        timeout_ms.unwrap_or_default()
                    )],
                    ..ParsedHookOutput::default()
                },
            },
            Err(error) => HookCommandOutcome::Failed {
                parsed: ParsedHookOutput {
                    messages: vec![format!(
                        "{} hook http `{label}` failed: {error}",
                        event.as_str()
                    )],
                    ..ParsedHookOutput::default()
                },
            },
        }
    }

    // ── G9.5: MCP tool handler ──

    fn run_mcp_handler(
        mcp_tool: &str,
        event: HookEvent,
        source_tool: &str,
        payload: &str,
    ) -> HookCommandOutcome {
        let _ = (mcp_tool, event, source_tool, payload);
        HookCommandOutcome::Allow {
            parsed: ParsedHookOutput {
                messages: vec![format!(
                    "{} hook mcp: dispatched `{mcp_tool}` (mcp bridge delegate — sync no-op)",
                    event.as_str()
                )],
                ..ParsedHookOutput::default()
            },
        }
    }
}

enum HookCommandOutcome {
    Allow { parsed: ParsedHookOutput },
    Deny { parsed: ParsedHookOutput },
    Failed { parsed: ParsedHookOutput },
    Cancelled { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedHookOutput {
    messages: Vec<String>,
    deny: bool,
    permission_override: Option<PermissionOverride>,
    permission_reason: Option<String>,
    updated_input: Option<String>,
}

impl ParsedHookOutput {
    fn with_fallback_message(mut self, fallback: String) -> Self {
        if self.messages.is_empty() {
            self.messages.push(fallback);
        }
        self
    }

    fn primary_message(&self) -> Option<&str> {
        self.messages.first().map(String::as_str)
    }
}

fn merge_parsed_hook_output(target: &mut HookRunResult, parsed: ParsedHookOutput) {
    target.messages.extend(parsed.messages);
    if parsed.permission_override.is_some() {
        target.permission_override = parsed.permission_override;
    }
    if parsed.permission_reason.is_some() {
        target.permission_reason = parsed.permission_reason;
    }
    if parsed.updated_input.is_some() {
        target.updated_input = parsed.updated_input;
    }
}

fn parse_hook_output(
    event: HookEvent,
    tool_name: &str,
    command: &str,
    stdout: &str,
    stderr: &str,
) -> ParsedHookOutput {
    if stdout.is_empty() {
        return ParsedHookOutput::default();
    }

    let root = match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(root)) => root,
        Ok(value) => {
            return ParsedHookOutput {
                messages: vec![format_invalid_hook_output(
                    event,
                    tool_name,
                    command,
                    &format!(
                        "expected top-level JSON object, got {}",
                        json_type_name(&value)
                    ),
                    stdout,
                    stderr,
                )],
                ..ParsedHookOutput::default()
            };
        }
        Err(error) if looks_like_json_attempt(stdout) => {
            return ParsedHookOutput {
                messages: vec![format_invalid_hook_output(
                    event,
                    tool_name,
                    command,
                    &error.to_string(),
                    stdout,
                    stderr,
                )],
                ..ParsedHookOutput::default()
            };
        }
        Err(_) => {
            return ParsedHookOutput {
                messages: vec![stdout.to_string()],
                ..ParsedHookOutput::default()
            };
        }
    };

    let mut parsed = ParsedHookOutput::default();

    if let Some(message) = root.get("systemMessage").and_then(Value::as_str) {
        parsed.messages.push(message.to_string());
    }
    if let Some(message) = root.get("reason").and_then(Value::as_str) {
        parsed.messages.push(message.to_string());
    }
    if root.get("continue").and_then(Value::as_bool) == Some(false)
        || root.get("decision").and_then(Value::as_str) == Some("block")
    {
        parsed.deny = true;
    }

    if let Some(Value::Object(specific)) = root.get("hookSpecificOutput") {
        if let Some(Value::String(additional_context)) = specific.get("additionalContext") {
            parsed.messages.push(additional_context.clone());
        }
        if let Some(decision) = specific.get("permissionDecision").and_then(Value::as_str) {
            parsed.permission_override = match decision {
                "allow" => Some(PermissionOverride::Allow),
                "deny" => Some(PermissionOverride::Deny),
                "ask" => Some(PermissionOverride::Ask),
                _ => None,
            };
        }
        if let Some(reason) = specific
            .get("permissionDecisionReason")
            .and_then(Value::as_str)
        {
            parsed.permission_reason = Some(reason.to_string());
        }
        if let Some(updated_input) = specific.get("updatedInput") {
            parsed.updated_input = serde_json::to_string(updated_input).ok();
        }
    }

    if parsed.messages.is_empty() {
        parsed.messages.push(stdout.to_string());
    }

    parsed
}

fn hook_payload(
    event: HookEvent,
    tool_name: &str,
    tool_input: &str,
    tool_output: Option<&str>,
    is_error: bool,
) -> Value {
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

fn parse_tool_input(tool_input: &str) -> Value {
    serde_json::from_str(tool_input).unwrap_or_else(|_| json!({ "raw": tool_input }))
}

fn format_invalid_hook_output(
    event: HookEvent,
    tool_name: &str,
    command: &str,
    detail: &str,
    stdout: &str,
    stderr: &str,
) -> String {
    let stdout_preview = bounded_hook_preview(stdout).unwrap_or_else(|| "<empty>".to_string());
    let stderr_preview = bounded_hook_preview(stderr).unwrap_or_else(|| "<empty>".to_string());
    let command_preview = bounded_hook_preview(command).unwrap_or_else(|| "<empty>".to_string());

    format!(
        "hook_invalid_json: phase={} tool={} command={} detail={} stdout_preview={} stderr_preview={}",
        event.as_str(),
        tool_name,
        command_preview,
        detail,
        stdout_preview,
        stderr_preview
    )
}

fn bounded_hook_preview(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut preview = String::new();
    for (count, ch) in trimmed.chars().enumerate() {
        if count == HOOK_PREVIEW_CHAR_LIMIT {
            preview.push('…');
            break;
        }
        match ch {
            '\n' => preview.push_str("\\n"),
            '\r' => preview.push_str("\\r"),
            '\t' => preview.push_str("\\t"),
            control if control.is_control() => {
                let _ = write!(&mut preview, "\\u{{{:x}}}", control as u32);
            }
            _ => preview.push(ch),
        }
    }
    Some(preview)
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn looks_like_json_attempt(value: &str) -> bool {
    matches!(value.trim_start().chars().next(), Some('{' | '['))
}

fn format_hook_failure(command: &str, code: i32, stdout: Option<&str>, stderr: &str) -> String {
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
    // P11-2:复用 bash::shell_launcher() 的 shell 探测逻辑(Git Bash 优先),
    // 避免 hooks 与 execute_bash 行为不一致(之前硬编码 cmd /C 导致
    // printf/sleep 等 Unix 命令在 Windows 上失败)。
    let (program, flag) = crate::bash::shell_launcher();
    let mut command_builder = Command::new(&program);
    command_builder.arg(flag).arg(command);
    CommandWithStdin::new(command_builder)
}

struct CommandWithStdin {
    command: Command,
}

impl CommandWithStdin {
    fn new(command: Command) -> Self {
        Self { command }
    }

    fn stdin(&mut self, cfg: Stdio) -> &mut Self {
        self.command.stdin(cfg);
        self
    }

    fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.command.stdout(cfg);
        self
    }

    fn stderr(&mut self, cfg: Stdio) -> &mut Self {
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

    fn output_with_stdin(
        &mut self,
        stdin: &[u8],
        abort_signal: Option<&HookAbortSignal>,
        timeout: Option<Duration>,
    ) -> std::io::Result<CommandExecution> {
        let mut child = self.command.spawn()?;
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin.write_all(stdin)?;
        }

        let start = Instant::now();
        loop {
            if abort_signal.is_some_and(HookAbortSignal::is_aborted) {
                let _ = child.kill();
                let _ = child.wait_with_output();
                return Ok(CommandExecution::Cancelled);
            }

            // 每 hook 独立超时：超时即 kill 子进程并返回 TimedOut，
            // 避免 hook 卡死永久阻塞对话循环（单点故障）。
            if let Some(timeout) = timeout {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    // kill 仅终止 bash 自身；sleep 等孙进程可能仍持有
                    // stdout 管道，wait_with_output() 会阻塞到孙进程退出
                    // （Windows 上 kill 不递归进程树）→ 放弃读取输出，
                    // 短暂轮询回收 bash 后立即返回 TimedOut。
                    let deadline = Instant::now() + Duration::from_millis(200);
                    loop {
                        match child.try_wait()? {
                            Some(_) => break,
                            None if Instant::now() >= deadline => {
                                let _ = child.wait();
                                break;
                            }
                            None => thread::sleep(Duration::from_millis(10)),
                        }
                    }
                    return Ok(CommandExecution::TimedOut);
                }
            }

            match child.try_wait()? {
                Some(_) => return child.wait_with_output().map(CommandExecution::Finished),
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

enum CommandExecution {
    Finished(std::process::Output),
    Cancelled,
    TimedOut,
}

/// 判断 hook 是否应针对给定工具执行（matcher 正则过滤）。
///
/// 无 matcher 或空列表 → 匹配全部工具（向后兼容，与旧行为一致）。
/// 有 matcher 时任一正则命中 `tool_name` 才执行。正则编译失败视为
/// 不匹配（安全降级：非法正则不会导致 hook 意外执行）。
fn hook_matches(def: &HookDefinition, tool_name: &str) -> bool {
    let Some(matchers) = def.matchers.as_deref() else {
        return true;
    };
    if matchers.is_empty() {
        return true;
    }
    matchers.iter().any(|pattern| {
        regex::Regex::new(pattern)
            .map(|re| re.is_match(tool_name))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        HookAbortSignal, HookDefinition, HookEvent, HookProgressEvent, HookProgressReporter,
        HookRunResult, HookRunner, HOOK_BUDGET_OVERRIDE_MS,
    };
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::permissions::PermissionOverride;

    struct RecordingReporter {
        events: Vec<HookProgressEvent>,
    }

    impl HookProgressReporter for RecordingReporter {
        fn on_event(&mut self, event: &HookProgressEvent) {
            self.events.push(event.clone());
        }
    }

    #[test]
    fn allows_exit_code_zero_and_captures_stdout() {
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![shell_snippet("printf 'pre ok'")],
            Vec::new(),
            Vec::new(),
        ));

        let result = runner.run_pre_tool_use("Read", r#"{"path":"README.md"}"#);

        assert_eq!(result, HookRunResult::allow(vec!["pre ok".to_string()]));
    }

    #[test]
    fn denies_exit_code_two() {
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![shell_snippet("printf 'blocked by hook'; exit 2")],
            Vec::new(),
            Vec::new(),
        ));

        let result = runner.run_pre_tool_use("Bash", r#"{"command":"pwd"}"#);

        assert!(result.is_denied());
        assert_eq!(result.messages(), &["blocked by hook".to_string()]);
    }

    #[test]
    fn propagates_other_non_zero_statuses_as_failures() {
        let runner = HookRunner::from_feature_config(&RuntimeFeatureConfig::default().with_hooks(
            RuntimeHookConfig::new(
                vec![shell_snippet("printf 'warning hook'; exit 1")],
                Vec::new(),
                Vec::new(),
            ),
        ));

        // given
        // when
        let result = runner.run_pre_tool_use("Edit", r#"{"file":"src/lib.rs"}"#);

        // then
        assert!(result.is_failed());
        assert!(result
            .messages()
            .iter()
            .any(|message| message.contains("warning hook")));
    }

    #[test]
    fn parses_pre_hook_permission_override_and_updated_input() {
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![shell_snippet(
                r#"printf '%s' '{"systemMessage":"updated","hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"hook ok","updatedInput":{"command":"git status"}}}'"#,
            )],
            Vec::new(),
            Vec::new(),
        ));

        let result = runner.run_pre_tool_use("bash", r#"{"command":"pwd"}"#);

        assert_eq!(
            result.permission_override(),
            Some(PermissionOverride::Allow)
        );
        assert_eq!(result.permission_reason(), Some("hook ok"));
        assert_eq!(result.updated_input(), Some(r#"{"command":"git status"}"#));
        assert!(result.messages().iter().any(|message| message == "updated"));
    }

    #[test]
    fn runs_post_tool_use_failure_hooks() {
        // given
        let runner = HookRunner::new(RuntimeHookConfig::new(
            Vec::new(),
            Vec::new(),
            vec![shell_snippet("printf 'failure hook ran'")],
        ));

        // when
        let result =
            runner.run_post_tool_use_failure("bash", r#"{"command":"false"}"#, "command failed");

        // then
        assert!(!result.is_denied());
        assert_eq!(result.messages(), &["failure hook ran".to_string()]);
    }

    #[test]
    fn stops_running_failure_hooks_after_failure() {
        // given
        let runner = HookRunner::new(RuntimeHookConfig::new(
            Vec::new(),
            Vec::new(),
            vec![
                shell_snippet("printf 'broken failure hook'; exit 1"),
                shell_snippet("printf 'later failure hook'"),
            ],
        ));

        // when
        let result =
            runner.run_post_tool_use_failure("bash", r#"{"command":"false"}"#, "command failed");

        // then
        assert!(result.is_failed());
        assert!(result
            .messages()
            .iter()
            .any(|message| message.contains("broken failure hook")));
        assert!(!result
            .messages()
            .iter()
            .any(|message| message == "later failure hook"));
    }

    // ── matcher 正则过滤 + 每 hook 独立 timeout（design-gaps #1）──

    #[test]
    fn matcher_filters_hooks_by_tool_name() {
        let defs = vec![HookDefinition::command("printf 'ran'").with_matchers(vec!["bash".into()])];
        let on_bash = HookRunner::run_definitions(
            HookEvent::PreToolUse,
            &defs,
            "bash",
            "{}",
            None,
            false,
            None,
            None,
        );
        assert!(on_bash.messages().iter().any(|m| m == "ran"));
        let on_other = HookRunner::run_definitions(
            HookEvent::PreToolUse,
            &defs,
            "read_file",
            "{}",
            None,
            false,
            None,
            None,
        );
        assert!(
            !on_other.messages().iter().any(|m| m == "ran"),
            "matcher 未命中 read_file 时不应执行 hook"
        );
    }

    #[test]
    fn matcher_supports_regex() {
        let defs =
            vec![HookDefinition::command("printf 'edit'").with_matchers(vec!["^edit_".into()])];
        let on_edit = HookRunner::run_definitions(
            HookEvent::PreToolUse,
            &defs,
            "edit_file",
            "{}",
            None,
            false,
            None,
            None,
        );
        assert!(on_edit.messages().iter().any(|m| m == "edit"));
        let on_bash = HookRunner::run_definitions(
            HookEvent::PreToolUse,
            &defs,
            "bash",
            "{}",
            None,
            false,
            None,
            None,
        );
        assert!(!on_bash.messages().iter().any(|m| m == "edit"));
    }

    #[test]
    fn no_matcher_matches_all_tools() {
        // 向后兼容：无 matcher 时所有工具都触发。
        let defs = vec![HookDefinition::command("printf 'all'")];
        for tool in ["bash", "read_file", "edit_file"] {
            let result = HookRunner::run_definitions(
                HookEvent::PreToolUse,
                &defs,
                tool,
                "{}",
                None,
                false,
                None,
                None,
            );
            assert!(
                result.messages().iter().any(|m| m == "all"),
                "tool {tool} 应触发无 matcher 的 hook"
            );
        }
    }

    #[test]
    fn hook_timeout_kills_hanging_command() {
        // 挂起的 hook（sleep 5s）应被 500ms 超时 kill，不再阻塞调用方。
        let defs = vec![HookDefinition::command("sleep 5").with_timeout_ms(500)];
        let start = std::time::Instant::now();
        let result = HookRunner::run_definitions(
            HookEvent::PreToolUse,
            &defs,
            "bash",
            "{}",
            None,
            false,
            None,
            None,
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "hook 应在超时后快速返回，实际耗时 {elapsed:?}"
        );
        assert!(result.is_failed());
        assert!(result.messages().iter().any(|m| m.contains("timed out")));
    }

    #[test]
    fn post_custom_tool_call_carries_real_tool_name_and_output() {
        // PostCustomToolCall payload 应携带真实 tool_name 与 tool_output,
        // 而非旧实现的 `"{tool_name}: {result}"` 拼接(会污染 tool_name 字段)。
        let mut config = RuntimeHookConfig::new(Vec::new(), Vec::new(), Vec::new());
        config.add_lifecycle(
            "PostCustomToolCall",
            HookDefinition::command("printf '%s|%s' \"$HOOK_TOOL_NAME\" \"$HOOK_TOOL_OUTPUT\""),
        );
        let runner = HookRunner::new(config);
        let result = runner.run_post_custom_tool_call("edit_file", "patched OK");
        assert!(!result.is_failed());
        assert!(result
            .messages()
            .iter()
            .any(|m| m.contains("edit_file|patched OK")));
    }

    #[test]
    fn executes_hooks_in_configured_order() {
        // given
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![
                shell_snippet("printf 'first'"),
                shell_snippet("printf 'second'"),
            ],
            Vec::new(),
            Vec::new(),
        ));
        let mut reporter = RecordingReporter { events: Vec::new() };

        // when
        let result = runner.run_pre_tool_use_with_context(
            "Read",
            r#"{"path":"README.md"}"#,
            None,
            Some(&mut reporter),
        );

        // then
        assert_eq!(
            result,
            HookRunResult::allow(vec!["first".to_string(), "second".to_string()])
        );
        assert_eq!(reporter.events.len(), 4);
        assert!(matches!(
            &reporter.events[0],
            HookProgressEvent::Started {
                event: HookEvent::PreToolUse,
                command,
                ..
            } if command == "printf 'first'"
        ));
        assert!(matches!(
            &reporter.events[1],
            HookProgressEvent::Completed {
                event: HookEvent::PreToolUse,
                command,
                ..
            } if command == "printf 'first'"
        ));
        assert!(matches!(
            &reporter.events[2],
            HookProgressEvent::Started {
                event: HookEvent::PreToolUse,
                command,
                ..
            } if command == "printf 'second'"
        ));
        assert!(matches!(
            &reporter.events[3],
            HookProgressEvent::Completed {
                event: HookEvent::PreToolUse,
                command,
                ..
            } if command == "printf 'second'"
        ));
    }

    #[test]
    fn stops_running_hooks_after_failure() {
        // given
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![
                shell_snippet("printf 'broken'; exit 1"),
                shell_snippet("printf 'later'"),
            ],
            Vec::new(),
            Vec::new(),
        ));

        // when
        let result = runner.run_pre_tool_use("Edit", r#"{"file":"src/lib.rs"}"#);

        // then
        assert!(result.is_failed());
        assert!(result
            .messages()
            .iter()
            .any(|message| message.contains("broken")));
        assert!(!result.messages().iter().any(|message| message == "later"));
    }

    #[test]
    fn malformed_nonempty_hook_output_reports_explicit_diagnostic_with_previews() {
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![shell_snippet(
                "printf '{not-json\nsecond line'; printf 'stderr warning' >&2; exit 1",
            )],
            Vec::new(),
            Vec::new(),
        ));

        let result = runner.run_pre_tool_use("Edit", r#"{"file":"src/lib.rs"}"#);

        assert!(result.is_failed());
        let rendered = result.messages().join("\n");
        assert!(rendered.contains("hook_invalid_json:"));
        assert!(rendered.contains("phase=PreToolUse"));
        assert!(rendered.contains("tool=Edit"));
        assert!(rendered.contains("command=printf '{not-json"));
        assert!(rendered.contains("printf 'stderr warning' >&2; exit 1"));
        assert!(rendered.contains("detail=key must be a string"));
        assert!(rendered.contains("stdout_preview={not-json"));
        assert!(rendered.contains("second line stderr_preview=stderr warning"));
        assert!(rendered.contains("stderr_preview=stderr warning"));
    }

    #[test]
    fn abort_signal_cancels_long_running_hook_and_reports_progress() {
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![shell_snippet("sleep 5")],
            Vec::new(),
            Vec::new(),
        ));
        let abort_signal = HookAbortSignal::new();
        let abort_signal_for_thread = abort_signal.clone();
        let mut reporter = RecordingReporter { events: Vec::new() };

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            abort_signal_for_thread.abort();
        });

        let result = runner.run_pre_tool_use_with_context(
            "bash",
            r#"{"command":"sleep 5"}"#,
            Some(&abort_signal),
            Some(&mut reporter),
        );

        assert!(result.is_cancelled());
        assert!(reporter.events.iter().any(|event| matches!(
            event,
            HookProgressEvent::Started {
                event: HookEvent::PreToolUse,
                ..
            }
        )));
        assert!(reporter.events.iter().any(|event| matches!(
            event,
            HookProgressEvent::Cancelled {
                event: HookEvent::PreToolUse,
                ..
            }
        )));
    }

    #[test]
    fn hot_reload_switches_config_atomically() {
        // design-gaps #1「配置热重载」:reload 后下一次 hook 调用立即使用新配置,
        // 无需重建 HookRunner(等价于无需重启会话)。
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![shell_snippet("printf 'old-hook'")],
            Vec::new(),
            Vec::new(),
        ));

        let before = runner.run_pre_tool_use("Read", r#"{"path":"a.txt"}"#);
        assert!(before.messages().iter().any(|m| m == "old-hook"));

        let new_config = RuntimeHookConfig::new(
            vec![shell_snippet("printf 'new-hook'")],
            Vec::new(),
            Vec::new(),
        );
        runner.reload(new_config.clone());
        assert_eq!(runner.current_config(), new_config);

        let after = runner.run_pre_tool_use("Read", r#"{"path":"a.txt"}"#);
        assert!(after.messages().iter().any(|m| m == "new-hook"));
        assert!(!after.messages().iter().any(|m| m == "old-hook"));
    }

    #[test]
    fn reload_from_feature_config_applies_new_hooks() {
        // 与 CLI 接线一致:reload_from_feature_config 用于从重新加载的
        // RuntimeConfig 原子替换 hooks。
        let runner = HookRunner::new(RuntimeHookConfig::new(
            vec![shell_snippet("printf 'v1'")],
            Vec::new(),
            Vec::new(),
        ));

        let new_feature = RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
            vec![shell_snippet("printf 'v2'")],
            Vec::new(),
            Vec::new(),
        ));
        runner.reload_from_feature_config(&new_feature);

        let result = runner.run_pre_tool_use("Read", r#"{"path":"a.txt"}"#);
        assert!(result.messages().iter().any(|m| m == "v2"));
    }

    #[test]
    fn spawn_lifecycle_event_returns_immediately_and_runs_in_background() {
        // design-gaps #1「异步 HookRunner」:生命周期事件 fire-and-forget,
        // spawn 立即返回(不阻塞对话循环),hook 在后台线程执行。
        let marker =
            std::env::temp_dir().join(format!("claw-hook-spawn-{}.marker", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let marker_bash = marker.display().to_string().replace('\\', "/");
        let mut config = RuntimeHookConfig::new(Vec::new(), Vec::new(), Vec::new());
        config.add_lifecycle(
            "Stop",
            HookDefinition::command(format!("sleep 1; echo done > {marker_bash}")),
        );
        let runner = HookRunner::new(config);

        let start = Instant::now();
        runner.spawn_lifecycle_event(HookEvent::Stop, "turn_completed".to_string());
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "spawn 应立即返回，实际耗时 {elapsed:?}"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(marker.exists(), "后台 hook 应在 ~1s 后写入 marker 文件");
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn unconfigured_timeout_respects_global_budget() {
        // design-gaps #1:即使 hook 未配置独立 timeout,整体执行也不超过全局
        // 预算(这里用测试 override 缩短到 400ms),避免失控 hook 无限期阻塞。
        HOOK_BUDGET_OVERRIDE_MS.store(400, Ordering::Relaxed);
        let defs = vec![HookDefinition::command("sleep 5")];
        let start = Instant::now();
        let result = HookRunner::run_definitions(
            HookEvent::PreToolUse,
            &defs,
            "bash",
            "{}",
            None,
            false,
            None,
            None,
        );
        HOOK_BUDGET_OVERRIDE_MS.store(0, Ordering::Relaxed);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "预算应限制总执行时间，实际耗时 {elapsed:?}"
        );
        assert!(result.is_failed());
        assert!(result.messages().iter().any(|m| m.contains("timed out")));
    }

    #[cfg(windows)]
    // P11-2:shell_command 现在复用 shell_launcher()(Git Bash 优先),
    // bash 能正确处理单引号,不需要做单引号→双引号替换。
    // 之前的替换会破坏 JSON 结构(如 printf '%s' '{"key":"value"}' 中的
    // JSON 内部双引号与替换后的外层双引号冲突)。
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    // ---- suppress_output(LoopDetector 重复输出抑制)----

    #[test]
    fn allow_result_does_not_suppress_output_by_default() {
        let result = HookRunResult::allow(vec!["ok".to_string()]);
        assert!(!result.should_suppress_output());
    }

    #[test]
    fn suppressed_with_messages_flags_suppress_output() {
        let result = HookRunResult::suppressed_with_messages(vec![
            "consider reconsidering your approach — tool 'bash' returned identical output 5 times"
                .to_string(),
        ]);
        assert!(result.should_suppress_output());
        assert_eq!(result.messages().len(), 1);
        assert!(!result.is_denied());
        assert!(!result.is_failed());
        assert!(!result.is_cancelled());
    }

    #[test]
    fn mark_suppress_output_preserves_decision_state() {
        // 用 cancelled_with_message 模拟非 allow 状态，打抑制标记不应覆盖它
        let mut result = HookRunResult::cancelled_with_message("blocked".to_string());
        result.mark_suppress_output();
        assert!(result.should_suppress_output());
        assert!(result.is_cancelled());
        // 再打一次仍为 true(幂等)
        result.mark_suppress_output();
        assert!(result.should_suppress_output());
    }
}
