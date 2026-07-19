#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    clippy::unneeded_struct_pattern,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]
mod init;
mod input;
mod paste;
mod render;
mod suggestion;
mod ultraplan;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpListener;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, UNIX_EPOCH};

use api::{
    detect_provider_kind, model_family_identity_for,
    model_requires_reasoning_content_in_history, resolve_startup_auth_source, AnthropicClient,
    AuthSource, CacheControl, ContentBlockDelta, InputContentBlock, InputMessage, MessageRequest,
    MessageResponse, OutputContentBlock, PromptCache, ProviderClient as ApiProviderClient,
    ProviderKind, StreamEvent as ApiStreamEvent, SystemBlock, SystemContent, ToolChoice,
    ToolDefinition, ToolResultContentBlock,
};

use commands::{
    classify_skills_slash_command, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command, handle_mcp_slash_command_json, handle_plugins_slash_command,
    handle_skills_slash_command, handle_skills_slash_command_json, render_slash_command_help,
    render_slash_command_help_filtered, resolve_skill_invocation, resume_supported_slash_commands,
    slash_command_specs, validate_slash_command_input, PluginsCommandResult, SkillSlashDispatch,
    SlashCommand,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use init::initialize_repo;
use plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use render::{MarkdownStreamState, OutputVerbosity, Spinner, TerminalRenderer};
use runtime::{
    check_base_commit, format_stale_base_warning, format_usd, load_oauth_credentials,
    load_system_prompt, load_system_prompt_with_extras, pricing_for_model, resolve_expected_base,
    resolve_sandbox_status, ApiClient, ApiRequest, AssistantEvent, BaseCommitState,
    CompactionConfig, ConfigLoader, ConfigSource, ContentBlock, ConversationMessage,
    ConversationRuntime, HistoryIndex, McpServer, McpServerManager, McpServerSpec, McpTool,
    MessageRole, ModelPricing, PermissionMode, PermissionPolicy, ProjectContext, PromptCacheEvent,
    RepoMap, ResolvedPermissionMode, RuntimeError, Session, SystemPromptExtras,
    SystemPromptSplit, TokenUsage, ToolError, ToolExecutor, UsageTracker,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tools::{
    execute_tool, mvp_tool_specs, GlobalToolRegistry, RuntimeToolDefinition, ToolSearchOutput,
};
use paste::{
    expand_paste_placeholders, fold_pasted_input, format_pasted_text_ref, paste_cache_path,
    paste_cache_root, pasted_text_ref_num_lines, read_clipboard_text, should_fold_paste,
    store_paste_and_make_placeholder, try_auto_expand_clipboard, PASTE_FOLD_CHAR_THRESHOLD,
    PASTE_FOLD_LINE_THRESHOLD,
};
use suggestion::{
    common_prefix_len, levenshtein_distance, looks_like_subcommand_typo, ranked_suggestions,
    render_suggestion_line, suggest_closest_term, suggest_similar_subcommand,
    suggest_slash_commands, CLI_OPTION_SUGGESTIONS,
};
use ultraplan::{
    describe_tool_progress, format_internal_prompt_progress_line,
    INTERNAL_PROGRESS_HEARTBEAT_INTERVAL, InternalPromptProgressEvent,
    InternalPromptProgressReporter, InternalPromptProgressRun, InternalPromptProgressShared,
    InternalPromptProgressState,
};

const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// #148: Model provenance for `claw status` JSON/text output. Records where
/// the resolved model string came from so claws don't have to re-read argv
/// to audit whether their `--model` flag was honored vs falling back to env
/// or config or default.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelSource {
    /// Explicit `--model` / `--model=` CLI flag.
    Flag,
    /// ANTHROPIC_MODEL environment variable (when no flag was passed).
    Env,
    /// `model` key in `.claw.json` / `.claw/settings.json` (when neither
    /// flag nor env set it).
    Config,
    /// Compiled-in DEFAULT_MODEL fallback.
    Default,
}

impl ModelSource {
    fn as_str(&self) -> &'static str {
        match self {
            ModelSource::Flag => "flag",
            ModelSource::Env => "env",
            ModelSource::Config => "config",
            ModelSource::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelProvenance {
    /// Resolved model string (after alias expansion).
    resolved: String,
    /// Raw user input before alias resolution. None when source is Default.
    raw: Option<String>,
    /// Where the resolved model string originated.
    source: ModelSource,
}

impl ModelProvenance {
    fn default_fallback() -> Self {
        Self {
            resolved: DEFAULT_MODEL.to_string(),
            raw: None,
            source: ModelSource::Default,
        }
    }

    fn from_flag(raw: &str) -> Self {
        Self {
            resolved: resolve_model_alias_with_config(raw),
            raw: Some(raw.to_string()),
            source: ModelSource::Flag,
        }
    }

    fn from_env_or_config_or_default(cli_model: &str) -> Self {
        // Only called when no --model flag was passed. Probe env first,
        // then config, else fall back to default. Mirrors the logic in
        // resolve_repl_model() but captures the source.
        if cli_model != DEFAULT_MODEL {
            // Already resolved from some prior path; treat as flag.
            return Self {
                resolved: cli_model.to_string(),
                raw: Some(cli_model.to_string()),
                source: ModelSource::Flag,
            };
        }
        if let Some(env_model) = env::var("ANTHROPIC_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Self {
                resolved: resolve_model_alias_with_config(&env_model),
                raw: Some(env_model),
                source: ModelSource::Env,
            };
        }
        if let Some(config_model) = config_model_for_current_dir() {
            return Self {
                resolved: resolve_model_alias_with_config(&config_model),
                raw: Some(config_model),
                source: ModelSource::Config,
            };
        }
        Self::default_fallback()
    }
}

fn max_tokens_for_model(model: &str) -> u32 {
    api::max_tokens_for_model(model)
}
// Build-time constants injected by build.rs (fall back to static values when
// build.rs hasn't run, e.g. in doc-test or unusual toolchain environments).
const DEFAULT_DATE: &str = match option_env!("BUILD_DATE") {
    Some(d) => d,
    None => "unknown",
};
const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 4545;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: Option<&str> = option_env!("TARGET");
const GIT_SHA: Option<&str> = option_env!("GIT_SHA");
const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);
const PRIMARY_SESSION_EXTENSION: &str = "jsonl";
const LEGACY_SESSION_EXTENSION: &str = "json";
const OFFICIAL_REPO_URL: &str = "https://github.com/ultraworkers/claw-code";
const OFFICIAL_REPO_SLUG: &str = "ultraworkers/claw-code";
const DEPRECATED_INSTALL_COMMAND: &str = "cargo install claw-code";
const LATEST_SESSION_REFERENCE: &str = "latest";
const SESSION_REFERENCE_ALIASES: &[&str] = &[LATEST_SESSION_REFERENCE, "last", "recent"];

type AllowedToolSet = BTreeSet<String>;
type RuntimePluginStateBuildOutput = (
    Option<Arc<Mutex<RuntimeMcpState>>>,
    Vec<RuntimeToolDefinition>,
);

fn main() {
    if let Err(error) = run() {
        let message = error.to_string();
        // When --output-format json is active, emit errors as JSON so downstream
        // tools can parse failures the same way they parse successes (ROADMAP #42).
        let argv: Vec<String> = std::env::args().collect();
        let json_output = argv
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "json")
            || argv.iter().any(|a| a == "--output-format=json");
        if json_output {
            // #77: classify error by prefix so downstream claws can route without
            // regex-scraping the prose. Split short-reason from hint-runbook.
            let kind = classify_error_kind(&message);
            let (short_reason, hint) = split_error_hint(&message);
            eprintln!(
                "{}",
                serde_json::json!({
                    "type": "error",
                    "error": short_reason,
                    "kind": kind,
                    "hint": hint,
                    "exit_code": 1,
                })
            );
        } else {
            // #156: Add machine-readable error kind to text output so stderr observers
            // don't need to regex-scrape the prose.
            let kind = classify_error_kind(&message);
            if message.contains("`claw --help`") {
                eprintln!(
                    "[error-kind: {kind}]
error: {message}"
                );
            } else {
                eprintln!(
                    "[error-kind: {kind}]
error: {message}

Run `claw --help` for usage."
                );
            }
        }
        std::process::exit(1);
    }
}

/// #77: Classify a stringified error message into a machine-readable kind.
///
/// Returns a snake_case token that downstream consumers can switch on instead
/// of regex-scraping the prose. The classification is best-effort prefix/keyword
/// matching against the error messages produced throughout the CLI surface.
fn classify_error_kind(message: &str) -> &'static str {
    // Check specific patterns first (more specific before generic)
    if message.contains("missing Anthropic credentials") {
        "missing_credentials"
    } else if message.contains("Manifest source files are missing") {
        "missing_manifests"
    } else if message.contains("no worker state file found") {
        "missing_worker_state"
    } else if message.contains("session not found") {
        "session_not_found"
    } else if message.contains("failed to restore session") {
        "session_load_failed"
    } else if message.contains("no managed sessions found") {
        "no_managed_sessions"
    } else if message.contains("unsupported ACP invocation") {
        "unsupported_acp_invocation"
    } else if message.contains("unrecognized argument") || message.contains("unknown option") {
        "cli_parse"
    } else if message.contains("invalid model syntax") {
        "invalid_model_syntax"
    } else if message.contains("is not yet implemented") {
        "unsupported_command"
    } else if message.contains("unsupported resumed command") {
        "unsupported_resumed_command"
    } else if message.contains("confirmation required") {
        "confirmation_required"
    } else if message.contains("api failed") || message.contains("api returned") {
        "api_http_error"
    } else {
        "unknown"
    }
}

/// #77: Split a multi-line error message into (short_reason, optional_hint).
///
/// The short_reason is the first line (up to the first newline), and the hint
/// is the remaining text or `None` if there's no newline. This prevents the
/// runbook prose from being stuffed into the `error` field that downstream
/// parsers expect to be the short reason alone.
fn split_error_hint(message: &str) -> (String, Option<String>) {
    match message.split_once('\n') {
        Some((short, hint)) => (short.to_string(), Some(hint.trim().to_string())),
        None => (message.to_string(), None),
    }
}

/// Read piped stdin content when stdin is not a terminal.
///
/// Returns `None` when stdin is attached to a terminal (interactive REPL use),
/// when reading fails, or when the piped content is empty after trimming.
/// Returns `Some(raw_content)` when a pipe delivered non-empty content.
fn read_piped_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    if io::stdin().read_to_string(&mut buffer).is_err() {
        return None;
    }
    if buffer.trim().is_empty() {
        return None;
    }
    Some(buffer)
}

/// Merge a piped stdin payload into a prompt argument.
///
/// When `stdin_content` is `None` or empty after trimming, the prompt is
/// returned unchanged. Otherwise the trimmed stdin content is appended to the
/// prompt separated by a blank line so the model sees the prompt first and the
/// piped context immediately after it.
fn merge_prompt_with_stdin(prompt: &str, stdin_content: Option<&str>) -> String {
    let Some(raw) = stdin_content else {
        return prompt.to_string();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return prompt.to_string();
    }
    if prompt.is_empty() {
        return trimmed.to_string();
    }
    format!("{prompt}\n\n{trimmed}")
}

fn plugin_command_json(
    action: &str,
    target: Option<&str>,
    result: &commands::PluginsCommandResult,
    report: &plugins::PluginRegistryReport,
) -> Value {
    let failures = report.failures();
    json!({
        "kind": "plugin",
        "action": action,
        "target": target,
        "status": if failures.is_empty() { "ok" } else { "degraded" },
        "message": result.message,
        "reload_runtime": result.reload_runtime,
        "plugins": report.summaries().iter().map(plugin_summary_json).collect::<Vec<_>>(),
        "load_failures": failures.iter().map(plugin_load_failure_json).collect::<Vec<_>>(),
    })
}

fn plugin_summary_json(plugin: &plugins::PluginSummary) -> Value {
    json!({
        "id": &plugin.metadata.id,
        "name": &plugin.metadata.name,
        "version": &plugin.metadata.version,
        "description": &plugin.metadata.description,
        "kind": plugin.metadata.kind.to_string(),
        "source": &plugin.metadata.source,
        "enabled": plugin.enabled,
        "lifecycle_state": plugin.lifecycle_state(),
        "lifecycle": {
            "configured": !plugin.lifecycle.is_empty(),
            "init": {
                "configured": !plugin.lifecycle.init.is_empty(),
                "command_count": plugin.lifecycle.init.len(),
            },
            "shutdown": {
                "configured": !plugin.lifecycle.shutdown.is_empty(),
                "command_count": plugin.lifecycle.shutdown.len(),
            },
        },
    })
}

fn plugin_load_failure_json(failure: &plugins::PluginLoadFailure) -> Value {
    json!({
        "plugin_root": failure.plugin_root.display().to_string(),
        "kind": failure.kind.to_string(),
        "source": &failure.source,
        "lifecycle_state": "load_failed",
        "error": failure.error().to_string(),
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_args(&args)? {
        CliAction::DumpManifests {
            output_format,
            manifests_dir,
        } => dump_manifests(manifests_dir.as_deref(), output_format)?,
        CliAction::BootstrapPlan { output_format } => print_bootstrap_plan(output_format)?,
        CliAction::Agents {
            args,
            output_format,
        } => LiveCli::print_agents(args.as_deref(), output_format)?,
        CliAction::Mcp {
            args,
            output_format,
        } => LiveCli::print_mcp(args.as_deref(), output_format)?,
        CliAction::Skills {
            args,
            output_format,
        } => LiveCli::print_skills(args.as_deref(), output_format)?,
        CliAction::Plugins {
            action,
            target,
            output_format,
        } => LiveCli::print_plugins(action.as_deref(), target.as_deref(), output_format)?,
        CliAction::PrintSystemPrompt {
            cwd,
            date,
            model,
            output_format,
        } => print_system_prompt(cwd, date, &model, output_format)?,
        CliAction::Version { output_format } => print_version(output_format)?,
        CliAction::ResumeSession {
            session_path,
            commands,
            output_format,
        } => resume_session(&session_path, &commands, output_format),
        CliAction::Status {
            model,
            model_flag_raw,
            permission_mode,
            output_format,
            allowed_tools,
        } => print_status_snapshot(
            &model,
            model_flag_raw.as_deref(),
            permission_mode,
            output_format,
            allowed_tools.as_ref(),
        )?,
        CliAction::Sandbox { output_format } => print_sandbox_status_snapshot(output_format)?,
        CliAction::Prompt {
            prompt,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            additional_workspace_roots,
            output_verbosity,
        } => {
            enforce_broad_cwd_policy(allow_broad_cwd, output_format)?;
            run_stale_base_preflight(base_commit.as_deref());
            // Only consume piped stdin as prompt context when the permission
            // mode is fully unattended. In modes where the permission
            // prompter may invoke CliPermissionPrompter::decide(), stdin
            // must remain available for interactive approval; otherwise the
            // prompter's read_line() would hit EOF and deny every request.
            let stdin_context = if matches!(permission_mode, PermissionMode::DangerFullAccess) {
                read_piped_stdin()
            } else {
                None
            };
            let effective_prompt = merge_prompt_with_stdin(&prompt, stdin_context.as_deref());
            let mut cli = LiveCli::new(
                model,
                true,
                allowed_tools,
                permission_mode,
                additional_workspace_roots,
                output_verbosity,
            )?;
            cli.set_reasoning_effort(reasoning_effort);
            cli.run_turn_with_output(&effective_prompt, output_format, compact)?;
        }
        CliAction::Doctor { output_format } => run_doctor(output_format)?,
        CliAction::Acp { output_format } => print_acp_status(output_format)?,
        CliAction::State { output_format } => run_worker_state(output_format)?,
        CliAction::Init { output_format } => run_init(output_format)?,
        // #146: dispatch pure-local introspection. Text mode uses existing
        // render_config_report/render_diff_report; JSON mode uses the
        // corresponding _json helpers already exposed for resume sessions.
        CliAction::Config {
            section,
            output_format,
        } => match output_format {
            CliOutputFormat::Text => {
                println!("{}", render_config_report(section.as_deref())?);
            }
            CliOutputFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&render_config_json(section.as_deref())?)?
                );
            }
        },
        CliAction::Diff { output_format } => match output_format {
            CliOutputFormat::Text => {
                println!("{}", render_diff_report()?);
            }
            CliOutputFormat::Json => {
                let cwd = env::current_dir()?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&render_diff_json_for(&cwd)?)?
                );
            }
        },
        CliAction::Export {
            session_reference,
            output_path,
            output_format,
        } => run_export(&session_reference, output_path.as_deref(), output_format)?,
        CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            additional_workspace_roots,
            output_verbosity,
        } => run_repl(
            model,
            allowed_tools,
            permission_mode,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            additional_workspace_roots,
            output_verbosity,
        )?,
        CliAction::HelpTopic {
            topic,
            output_format,
        } => print_help_topic(topic, output_format)?,
        CliAction::Help { output_format } => print_help(output_format)?,
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    DumpManifests {
        output_format: CliOutputFormat,
        manifests_dir: Option<PathBuf>,
    },
    BootstrapPlan {
        output_format: CliOutputFormat,
    },
    Agents {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Mcp {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Skills {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Plugins {
        action: Option<String>,
        target: Option<String>,
        output_format: CliOutputFormat,
    },
    PrintSystemPrompt {
        cwd: PathBuf,
        date: String,
        model: String,
        output_format: CliOutputFormat,
    },
    Version {
        output_format: CliOutputFormat,
    },
    ResumeSession {
        session_path: PathBuf,
        commands: Vec<String>,
        output_format: CliOutputFormat,
    },
    Status {
        model: String,
        // #148: raw `--model` flag input (pre-alias-resolution), if any.
        // None means no flag was supplied; env/config/default fallback is
        // resolved inside `print_status_snapshot`.
        model_flag_raw: Option<String>,
        permission_mode: PermissionMode,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
    },
    Sandbox {
        output_format: CliOutputFormat,
    },
    Prompt {
        prompt: String,
        model: String,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        compact: bool,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        /// 额外允许的工作区根目录（由 `--add-dir` CLI flag 添加）。
        /// 空表示仅允许 cwd；非空时与 cwd 一起构成多根白名单。
        additional_workspace_roots: Vec<PathBuf>,
        /// 启动时设定的输出冗度（由 `--verbose`/`--quiet`/`--silent` 设置）。
        /// 默认 `Full`。REPL 中仍可用 `/output-style` 实时切换。
        output_verbosity: OutputVerbosity,
    },
    Doctor {
        output_format: CliOutputFormat,
    },
    Acp {
        output_format: CliOutputFormat,
    },
    State {
        output_format: CliOutputFormat,
    },
    Init {
        output_format: CliOutputFormat,
    },
    // #146: `claw config` and `claw diff` are pure-local read-only
    // introspection commands; wire them as standalone CLI subcommands.
    Config {
        section: Option<String>,
        output_format: CliOutputFormat,
    },
    Diff {
        output_format: CliOutputFormat,
    },
    Export {
        session_reference: String,
        output_path: Option<PathBuf>,
        output_format: CliOutputFormat,
    },
    Repl {
        model: String,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        additional_workspace_roots: Vec<PathBuf>,
        /// 启动时设定的输出冗度（由 `--verbose`/`--quiet`/`--silent` 设置）。
        output_verbosity: OutputVerbosity,
    },
    HelpTopic {
        topic: LocalHelpTopic,
        output_format: CliOutputFormat,
    },
    // prompt-mode formatting is only supported for non-interactive runs
    Help {
        output_format: CliOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalHelpTopic {
    Status,
    Sandbox,
    Doctor,
    Acp,
    // #141: extend the local-help pattern to every subcommand so
    // `claw <subcommand> --help` has one consistent contract.
    Init,
    State,
    Export,
    Version,
    SystemPrompt,
    DumpManifests,
    BootstrapPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliOutputFormat {
    Text,
    Json,
}

impl CliOutputFormat {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unsupported value for --output-format: {other} (expected text or json)"
            )),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn parse_args(args: &[String]) -> Result<CliAction, String> {
    let mut model = DEFAULT_MODEL.to_string();
    // #148: when user passes --model/--model=, capture the raw input so we
    // can attribute source: "flag" later. None means no flag was supplied.
    let mut model_flag_raw: Option<String> = None;
    let mut output_format = CliOutputFormat::Text;
    let mut permission_mode_override = None;
    let mut wants_help = false;
    let mut wants_version = false;
    let mut allowed_tool_values = Vec::new();
    let mut compact = false;
    let mut base_commit: Option<String> = None;
    let mut reasoning_effort: Option<String> = None;
    let mut allow_broad_cwd = false;
    let mut additional_workspace_roots: Vec<PathBuf> = Vec::new();
    // `--verbose`/`--quiet`/`--silent` 设定的输出冗度。默认 `Full`。
    // 多次出现时后覆盖先，与多数 CLI 工具行为一致。
    let mut output_verbosity = OutputVerbosity::default();
    let mut rest: Vec<String> = Vec::new();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" if rest.is_empty() => {
                wants_help = true;
                index += 1;
            }
            "--help" | "-h"
                if !rest.is_empty()
                    && matches!(rest[0].as_str(), "prompt" | "commit" | "pr" | "issue") =>
            {
                // `--help` following a subcommand that would otherwise forward
                // the arg to the API (e.g. `claw prompt --help`) should show
                // top-level help instead. Subcommands that consume their own
                // args (agents, mcp, plugins, skills) and local help-topic
                // subcommands (status, sandbox, doctor, init, state, export,
                // version, system-prompt, dump-manifests, bootstrap-plan) must
                // NOT be intercepted here — they handle --help in their own
                // dispatch paths via parse_local_help_action(). See #141.
                wants_help = true;
                index += 1;
            }
            "--version" | "-V" => {
                wants_version = true;
                index += 1;
            }
            "--model" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --model".to_string())?;
                validate_model_syntax(value)?;
                model = resolve_model_alias_with_config(value);
                model_flag_raw = Some(value.clone()); // #148
                index += 2;
            }
            flag if flag.starts_with("--model=") => {
                let value = &flag[8..];
                validate_model_syntax(value)?;
                model = resolve_model_alias_with_config(value);
                model_flag_raw = Some(value.to_string()); // #148
                index += 1;
            }
            "--output-format" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --output-format".to_string())?;
                output_format = CliOutputFormat::parse(value)?;
                index += 2;
            }
            "--permission-mode" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --permission-mode".to_string())?;
                permission_mode_override = Some(parse_permission_mode_arg(value)?);
                index += 2;
            }
            flag if flag.starts_with("--output-format=") => {
                output_format = CliOutputFormat::parse(&flag[16..])?;
                index += 1;
            }
            flag if flag.starts_with("--permission-mode=") => {
                permission_mode_override = Some(parse_permission_mode_arg(&flag[18..])?);
                index += 1;
            }
            "--dangerously-skip-permissions" => {
                permission_mode_override = Some(PermissionMode::DangerFullAccess);
                index += 1;
            }
            "--compact" => {
                compact = true;
                index += 1;
            }
            "--base-commit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --base-commit".to_string())?;
                base_commit = Some(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--base-commit=") => {
                base_commit = Some(flag[14..].to_string());
                index += 1;
            }
            "--add-dir" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --add-dir".to_string())?;
                let path = PathBuf::from(value);
                if !path.exists() {
                    return Err(format!(
                        "--add-dir path does not exist: {}",
                        path.display()
                    ));
                }
                additional_workspace_roots.push(path);
                index += 2;
            }
            flag if flag.starts_with("--add-dir=") => {
                let path = PathBuf::from(&flag[10..]);
                if !path.exists() {
                    return Err(format!(
                        "--add-dir path does not exist: {}",
                        path.display()
                    ));
                }
                additional_workspace_roots.push(path);
                index += 1;
            }
            // 冗度控制：后覆盖先。`--verbose` 显式回到 Full（覆盖之前的 --quiet）。
            "--verbose" => {
                output_verbosity = OutputVerbosity::Full;
                index += 1;
            }
            "--quiet" => {
                output_verbosity = OutputVerbosity::Compact;
                index += 1;
            }
            "--silent" => {
                output_verbosity = OutputVerbosity::Silent;
                index += 1;
            }
            flag if flag.starts_with("--output-verbosity=") => {
                let arg = &flag["--output-verbosity=".len()..];
                output_verbosity = OutputVerbosity::from_style_arg(arg).ok_or_else(|| {
                    format!(
                        "invalid value for --output-verbosity: '{arg}'; expected full, compact, silent, or minimal"
                    )
                })?;
                index += 1;
            }
            "--reasoning-effort" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --reasoning-effort".to_string())?;
                if !matches!(value.as_str(), "low" | "medium" | "high") {
                    return Err(format!(
                        "invalid value for --reasoning-effort: '{value}'; must be low, medium, or high"
                    ));
                }
                reasoning_effort = Some(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--reasoning-effort=") => {
                let value = &flag[19..];
                if !matches!(value, "low" | "medium" | "high") {
                    return Err(format!(
                        "invalid value for --reasoning-effort: '{value}'; must be low, medium, or high"
                    ));
                }
                reasoning_effort = Some(value.to_string());
                index += 1;
            }
            "--allow-broad-cwd" => {
                allow_broad_cwd = true;
                index += 1;
            }
            "-p" => {
                // Claw Code compat: -p "prompt" = one-shot prompt
                let prompt = args[index + 1..].join(" ");
                if prompt.trim().is_empty() {
                    return Err("-p requires a prompt string".to_string());
                }
                return Ok(CliAction::Prompt {
                    prompt,
                    model: resolve_model_alias_with_config(&model),
                    output_format,
                    allowed_tools: normalize_allowed_tools(&allowed_tool_values)?,
                    permission_mode: permission_mode_override
                        .unwrap_or_else(default_permission_mode),
                    compact,
                    base_commit: base_commit.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                    allow_broad_cwd,
                    additional_workspace_roots: additional_workspace_roots.clone(),
                    output_verbosity: output_verbosity.clone(),
                });
            }
            "--print" => {
                // Claw Code compat: --print makes output non-interactive
                output_format = CliOutputFormat::Text;
                index += 1;
            }
            "--resume" if rest.is_empty() => {
                rest.push("--resume".to_string());
                index += 1;
            }
            flag if rest.is_empty() && flag.starts_with("--resume=") => {
                rest.push("--resume".to_string());
                rest.push(flag[9..].to_string());
                index += 1;
            }
            "--acp" | "-acp" => {
                rest.push("acp".to_string());
                index += 1;
            }
            "--allowedTools" | "--allowed-tools" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --allowedTools".to_string())?;
                allowed_tool_values.push(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--allowedTools=") => {
                allowed_tool_values.push(flag[15..].to_string());
                index += 1;
            }
            flag if flag.starts_with("--allowed-tools=") => {
                allowed_tool_values.push(flag[16..].to_string());
                index += 1;
            }
            other if rest.is_empty() && other.starts_with('-') => {
                return Err(format_unknown_option(other))
            }
            other => {
                rest.push(other.to_string());
                index += 1;
            }
        }
    }

    if wants_help {
        return Ok(CliAction::Help { output_format });
    }

    if wants_version {
        return Ok(CliAction::Version { output_format });
    }

    let allowed_tools = normalize_allowed_tools(&allowed_tool_values)?;

    if rest.is_empty() {
        let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
        // When stdin is not a terminal (pipe/redirect) and no prompt is given on the
        // command line, read stdin as the prompt and dispatch as a one-shot Prompt
        // rather than starting the interactive REPL (which would consume the pipe and
        // print the startup banner, then exit without sending anything to the API).
        if !std::io::stdin().is_terminal() {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
            let piped = buf.trim().to_string();
            if !piped.is_empty() {
                return Ok(CliAction::Prompt {
                    model,
                    prompt: piped,
                    allowed_tools,
                    permission_mode,
                    output_format,
                    compact: false,
                    base_commit,
                    reasoning_effort,
                    allow_broad_cwd,
                    additional_workspace_roots: additional_workspace_roots.clone(),
                    output_verbosity: output_verbosity.clone(),
                });
            }
        }
        return Ok(CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            base_commit,
            reasoning_effort: reasoning_effort.clone(),
            allow_broad_cwd,
            additional_workspace_roots: additional_workspace_roots.clone(),
            output_verbosity: output_verbosity.clone(),
        });
    }
    if rest.first().map(String::as_str) == Some("--resume") {
        return parse_resume_args(&rest[1..], output_format);
    }
    if let Some(action) = parse_local_help_action(&rest, output_format) {
        return action;
    }
    if let Some(action) = parse_single_word_command_alias(
        &rest,
        &model,
        model_flag_raw.as_deref(),
        permission_mode_override,
        output_format,
        allowed_tools.clone(),
    ) {
        return action;
    }

    let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);

    match rest[0].as_str() {
        "dump-manifests" => parse_dump_manifests_args(&rest[1..], output_format),
        "bootstrap-plan" => Ok(CliAction::BootstrapPlan { output_format }),
        "agents" => Ok(CliAction::Agents {
            args: join_optional_args(&rest[1..]),
            output_format,
        }),
        "mcp" => Ok(CliAction::Mcp {
            args: join_optional_args(&rest[1..]),
            output_format,
        }),
        // #145: `plugins` was routed through the prompt fallback because no
        // top-level parser arm produced CliAction::Plugins. That made `claw
        // plugins` (and `claw plugins --help`, `claw plugins list`, ...)
        // attempt an Anthropic network call, surfacing the misleading error
        // `missing Anthropic credentials` even though the command is purely
        // local introspection. Mirror `agents`/`mcp`/`skills`: action is the
        // first positional arg, target is the second.
        // `plugin` (singular) and `marketplace` are aliases for `plugins`.
        // All three must route to the same local handler so that no form
        // falls through to the LLM/prompt path.
        "plugins" | "plugin" | "marketplace" => {
            let tail = &rest[1..];
            let action = tail.first().cloned();
            let target = tail.get(1).cloned();
            if tail.len() > 2 {
                return Err(format!(
                    "unexpected extra arguments after `claw {} {}`: {}",
                    rest[0],
                    tail[..2].join(" "),
                    tail[2..].join(" ")
                ));
            }
            Ok(CliAction::Plugins {
                action,
                target,
                output_format,
            })
        }
        // #146: `config` is pure-local read-only introspection (merges
        // `.claw.json` + `.claw/settings.json` from disk, no network, no
        // state mutation). Previously callers had to spin up a session with
        // `claw --resume SESSION.jsonl /config` to see their own config,
        // which is synthetic friction. Accepts an optional section name
        // (env|hooks|model|plugins) matching the slash command shape.
        "config" => {
            let tail = &rest[1..];
            let section = tail.first().cloned();
            if tail.len() > 1 {
                return Err(format!(
                    "unexpected extra arguments after `claw config {}`: {}",
                    tail[0],
                    tail[1..].join(" ")
                ));
            }
            Ok(CliAction::Config {
                section,
                output_format,
            })
        }
        // #146: `diff` is pure-local (shells out to `git diff --cached` +
        // `git diff`). No session needed to inspect the working tree.
        "diff" => {
            if rest.len() > 1 {
                return Err(format!(
                    "unexpected extra arguments after `claw diff`: {}",
                    rest[1..].join(" ")
                ));
            }
            Ok(CliAction::Diff { output_format })
        }
        // `claw permissions <mode>` falls through to the LLM when called
        // with a subcommand argument because parse_single_word_command_alias
        // only intercepts the bare single-word form. Catch all multi-word
        // forms here and return a structured guidance error so no network
        // call or session is created.
        "permissions" => Err(
            "`claw permissions` is a slash command. Start `claw` and run `/permissions` inside the REPL.\n  Usage  /permissions [read-only|workspace-write|danger-full-access]"
                .to_string(),
        ),
        "skills" => {
            let args = join_optional_args(&rest[1..]);
            match classify_skills_slash_command(args.as_deref()) {
                SkillSlashDispatch::Invoke(prompt) => Ok(CliAction::Prompt {
                    prompt,
                    model,
                    output_format,
                    allowed_tools,
                    permission_mode,
                    compact,
                    base_commit,
                    reasoning_effort: reasoning_effort.clone(),
                    allow_broad_cwd,
                    additional_workspace_roots: additional_workspace_roots.clone(),
                    output_verbosity: output_verbosity.clone(),
                }),
                SkillSlashDispatch::Local => Ok(CliAction::Skills {
                    args,
                    output_format,
                }),
            }
        }
        "system-prompt" => parse_system_prompt_args(&rest[1..], model, output_format),
        "acp" => parse_acp_args(&rest[1..], output_format),
        "login" | "logout" => Err(removed_auth_surface_error(rest[0].as_str())),
        "init" => Ok(CliAction::Init { output_format }),
        "export" => parse_export_args(&rest[1..], output_format),
        "prompt" => {
            let prompt = rest[1..].join(" ");
            if prompt.trim().is_empty() {
                return Err("prompt subcommand requires a prompt string".to_string());
            }
            Ok(CliAction::Prompt {
                prompt,
                model,
                output_format,
                allowed_tools,
                permission_mode,
                compact,
                base_commit: base_commit.clone(),
                reasoning_effort: reasoning_effort.clone(),
                allow_broad_cwd,
                additional_workspace_roots: additional_workspace_roots.clone(),
                output_verbosity: output_verbosity.clone(),
            })
        }
        other if other.starts_with('/') => parse_direct_slash_cli_action(
            &rest,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            additional_workspace_roots.clone(),
            output_verbosity.clone(),
        ),
        other => {
            if rest.len() == 1 && looks_like_subcommand_typo(other) {
                if let Some(suggestions) = suggest_similar_subcommand(other) {
                    let mut message = format!("unknown subcommand: {other}.");
                    if let Some(line) = render_suggestion_line("Did you mean", &suggestions) {
                        message.push('\n');
                        message.push_str(&line);
                    }
                    message.push_str(
                        "\nRun `claw --help` for the full list. If you meant to send a prompt literally, use `claw prompt <text>`.",
                    );
                    return Err(message);
                }
            }
            // #147: guard empty/whitespace-only prompts at the fallthrough
            // path the same way `"prompt"` arm above does. Without this,
            // `claw ""`, `claw "   "`, and `claw "" ""` silently route to
            // the Anthropic call and surface a misleading
            // `missing Anthropic credentials` error (or burn API tokens on
            // an empty prompt when credentials are present).
            let joined = rest.join(" ");
            if joined.trim().is_empty() {
                return Err(
                    "empty prompt: provide a subcommand (run `claw --help`) or a non-empty prompt string"
                        .to_string(),
                );
            }
            Ok(CliAction::Prompt {
                prompt: joined,
                model,
                output_format,
                allowed_tools,
                permission_mode,
                compact,
                base_commit,
                reasoning_effort: reasoning_effort.clone(),
                allow_broad_cwd,
                additional_workspace_roots: additional_workspace_roots.clone(),
                output_verbosity: output_verbosity.clone(),
            })
        }
    }
}

fn parse_local_help_action(
    rest: &[String],
    output_format: CliOutputFormat,
) -> Option<Result<CliAction, String>> {
    if rest.len() != 2 || !is_help_flag(&rest[1]) {
        return None;
    }

    let topic = match rest[0].as_str() {
        "status" => LocalHelpTopic::Status,
        "sandbox" => LocalHelpTopic::Sandbox,
        "doctor" => LocalHelpTopic::Doctor,
        "acp" => LocalHelpTopic::Acp,
        // #141: add the subcommands that were previously falling back
        // to global help (init/state/export/version) or erroring out
        // (system-prompt/dump-manifests) or printing their primary
        // output instead of help text (bootstrap-plan).
        "init" => LocalHelpTopic::Init,
        "state" => LocalHelpTopic::State,
        "export" => LocalHelpTopic::Export,
        "version" => LocalHelpTopic::Version,
        "system-prompt" => LocalHelpTopic::SystemPrompt,
        "dump-manifests" => LocalHelpTopic::DumpManifests,
        "bootstrap-plan" => LocalHelpTopic::BootstrapPlan,
        _ => return None,
    };
    Some(Ok(CliAction::HelpTopic {
        topic,
        output_format,
    }))
}

fn is_help_flag(value: &str) -> bool {
    matches!(value, "--help" | "-h")
}

fn parse_single_word_command_alias(
    rest: &[String],
    model: &str,
    // #148: raw --model flag input for status provenance. None = no flag.
    model_flag_raw: Option<&str>,
    permission_mode_override: Option<PermissionMode>,
    output_format: CliOutputFormat,
    allowed_tools: Option<AllowedToolSet>,
) -> Option<Result<CliAction, String>> {
    if rest.is_empty() {
        return None;
    }

    // Diagnostic verbs (help, version, status, sandbox, doctor, state) accept only the verb itself
    // or --help / -h as a suffix. Any other suffix args are unrecognized.
    let verb = &rest[0];
    let is_diagnostic = matches!(
        verb.as_str(),
        "help" | "version" | "status" | "sandbox" | "doctor" | "state"
    );

    if is_diagnostic && rest.len() > 1 {
        // Diagnostic verb with trailing args: reject unrecognized suffix
        if is_help_flag(&rest[1]) && rest.len() == 2 {
            // "doctor --help" is valid, routed to parse_local_help_action() instead
            return None;
        }
        // Unrecognized suffix like "--json"
        let mut msg = format!(
            "unrecognized argument `{}` for subcommand `{}`",
            rest[1], verb
        );
        // #152: common mistake — users type `--json` expecting JSON output.
        // Hint at the correct flag so they don't have to re-read --help.
        if rest[1] == "--json" {
            msg.push_str("\nDid you mean `--output-format json`?");
        }
        return Some(Err(msg));
    }

    if rest.len() != 1 {
        return None;
    }

    match rest[0].as_str() {
        "help" => Some(Ok(CliAction::Help { output_format })),
        "version" => Some(Ok(CliAction::Version { output_format })),
        "status" => Some(Ok(CliAction::Status {
            model: model.to_string(),
            model_flag_raw: model_flag_raw.map(str::to_string), // #148
            permission_mode: permission_mode_override.unwrap_or_else(default_permission_mode),
            output_format,
            allowed_tools,
        })),
        "sandbox" => Some(Ok(CliAction::Sandbox { output_format })),
        "doctor" => Some(Ok(CliAction::Doctor { output_format })),
        "state" => Some(Ok(CliAction::State { output_format })),
        // #146: let `config` and `diff` fall through to parse_subcommand
        // where they are wired as pure-local introspection, instead of
        // producing the "is a slash command" guidance. Zero-arg cases
        // reach parse_subcommand too via this None.
        "config" | "diff" => None,
        other => bare_slash_command_guidance(other).map(Err),
    }
}

fn bare_slash_command_guidance(command_name: &str) -> Option<String> {
    if matches!(
        command_name,
        "dump-manifests"
            | "bootstrap-plan"
            | "agents"
            | "mcp"
            | "plugin"
            | "plugins"
            | "marketplace"
            | "skills"
            | "system-prompt"
            | "init"
            | "prompt"
            | "export"
    ) {
        return None;
    }
    let slash_command = slash_command_specs()
        .iter()
        .find(|spec| spec.name == command_name)?;
    let guidance = if slash_command.resume_supported {
        format!(
            "`claw {command_name}` is a slash command. Use `claw --resume SESSION.jsonl /{command_name}` or start `claw` and run `/{command_name}`."
        )
    } else {
        format!(
            "`claw {command_name}` is a slash command. Start `claw` and run `/{command_name}` inside the REPL."
        )
    };
    Some(guidance)
}

fn removed_auth_surface_error(command_name: &str) -> String {
    format!(
        "`claw {command_name}` has been removed. Set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN instead."
    )
}

fn parse_acp_args(args: &[String], output_format: CliOutputFormat) -> Result<CliAction, String> {
    match args {
        [] => Ok(CliAction::Acp { output_format }),
        [subcommand] if subcommand == "serve" => Ok(CliAction::Acp { output_format }),
        _ => Err(String::from(
            "unsupported ACP invocation. Use `claw acp`, `claw acp serve`, `claw --acp`, or `claw -acp`.",
        )),
    }
}

fn try_resolve_bare_skill_prompt(cwd: &Path, trimmed: &str) -> Option<String> {
    let bare_first_token = trimmed.split_whitespace().next().unwrap_or_default();
    let looks_like_skill_name = !bare_first_token.is_empty()
        && !bare_first_token.starts_with('/')
        && bare_first_token
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if !looks_like_skill_name {
        return None;
    }
    match resolve_skill_invocation(cwd, Some(trimmed)) {
        Ok(SkillSlashDispatch::Invoke(prompt)) => Some(prompt),
        _ => None,
    }
}

fn join_optional_args(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn parse_direct_slash_cli_action(
    rest: &[String],
    model: String,
    output_format: CliOutputFormat,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    compact: bool,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    allow_broad_cwd: bool,
    additional_workspace_roots: Vec<PathBuf>,
    output_verbosity: OutputVerbosity,
) -> Result<CliAction, String> {
    let raw = rest.join(" ");
    match SlashCommand::parse(&raw) {
        Ok(Some(SlashCommand::Help)) => Ok(CliAction::Help { output_format }),
        Ok(Some(SlashCommand::Agents { args })) => Ok(CliAction::Agents {
            args,
            output_format,
        }),
        Ok(Some(SlashCommand::Mcp { action, target })) => Ok(CliAction::Mcp {
            args: match (action, target) {
                (None, None) => None,
                (Some(action), None) => Some(action),
                (Some(action), Some(target)) => Some(format!("{action} {target}")),
                (None, Some(target)) => Some(target),
            },
            output_format,
        }),
        Ok(Some(SlashCommand::Skills { args })) => {
            match classify_skills_slash_command(args.as_deref()) {
                SkillSlashDispatch::Invoke(prompt) => Ok(CliAction::Prompt {
                    prompt,
                    model,
                    output_format,
                    allowed_tools,
                    permission_mode,
                    compact,
                    base_commit,
                    reasoning_effort: reasoning_effort.clone(),
                    allow_broad_cwd,
                    additional_workspace_roots: additional_workspace_roots.clone(),
                    output_verbosity: output_verbosity.clone(),
                }),
                SkillSlashDispatch::Local => Ok(CliAction::Skills {
                    args,
                    output_format,
                }),
            }
        }
        Ok(Some(SlashCommand::Unknown(name))) => Err(format_unknown_direct_slash_command(&name)),
        Ok(Some(command)) => Err({
            let _ = command;
            format!(
                "slash command {command_name} is interactive-only. Start `claw` and run it there, or use `claw --resume SESSION.jsonl {command_name}` / `claw --resume {latest} {command_name}` when the command is marked [resume] in /help.",
                command_name = rest[0],
                latest = LATEST_SESSION_REFERENCE,
            )
        }),
        Ok(None) => Err(format!("unknown subcommand: {}", rest[0])),
        Err(error) => Err(error.to_string()),
    }
}

fn format_unknown_option(option: &str) -> String {
    let mut message = format!("unknown option: {option}");
    if let Some(suggestion) = suggest_closest_term(option, CLI_OPTION_SUGGESTIONS) {
        message.push_str("\nDid you mean ");
        message.push_str(suggestion);
        message.push('?');
    }
    message.push_str("\nRun `claw --help` for usage.");
    message
}

fn format_unknown_direct_slash_command(name: &str) -> String {
    let mut message = format!("unknown slash command outside the REPL: /{name}");
    if let Some(suggestions) = render_suggestion_line("Did you mean", &suggest_slash_commands(name))
    {
        message.push('\n');
        message.push_str(&suggestions);
    }
    if let Some(note) = omc_compatibility_note_for_unknown_slash_command(name) {
        message.push('\n');
        message.push_str(note);
    }
    message.push_str("\nRun `claw --help` for CLI usage, or start `claw` and use /help.");
    message
}

fn format_unknown_slash_command(name: &str) -> String {
    let mut message = format!("Unknown slash command: /{name}");
    if let Some(suggestions) = render_suggestion_line("Did you mean", &suggest_slash_commands(name))
    {
        message.push('\n');
        message.push_str(&suggestions);
    }
    if let Some(note) = omc_compatibility_note_for_unknown_slash_command(name) {
        message.push('\n');
        message.push_str(note);
    }
    message.push_str("\n  Help             /help lists available slash commands");
    message
}

fn omc_compatibility_note_for_unknown_slash_command(name: &str) -> Option<&'static str> {
    name.starts_with("oh-my-claudecode:")
        .then_some(
            "Compatibility note: `/oh-my-claudecode:*` is a Claude Code/OMC plugin command. `claw` does not yet load plugin slash commands, Claude statusline stdin, or OMC session hooks.",
        )
}

fn resolve_model_alias(model: &str) -> &str {
    match model {
        "opus" => "claude-opus-4-6",
        "sonnet" => "claude-sonnet-4-6",
        "haiku" => "claude-haiku-4-5-20251213",
        _ => model,
    }
}

/// Resolve a model name through user-defined config aliases first, then fall
/// back to the built-in alias table. This is the entry point used wherever a
/// user-supplied model string is about to be dispatched to a provider.
fn resolve_model_alias_with_config(model: &str) -> String {
    let trimmed = model.trim();
    if let Some(resolved) = config_alias_for_current_dir(trimmed) {
        return resolve_model_alias(&resolved).to_string();
    }
    resolve_model_alias(trimmed).to_string()
}

/// Validate model syntax at parse time.
/// Accepts: known aliases (opus, sonnet, haiku) or provider/model pattern.
/// Rejects: empty, whitespace-only, strings with spaces, or invalid chars.
fn validate_model_syntax(model: &str) -> Result<(), String> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return Err("model string cannot be empty".to_string());
    }
    // Known aliases are always valid
    match trimmed {
        "opus" | "sonnet" | "haiku" => return Ok(()),
        _ => {}
    }
    // Check for spaces (malformed)
    if trimmed.contains(' ') {
        return Err(format!(
            "invalid model syntax: '{}' contains spaces. Use provider/model format or known alias",
            trimmed
        ));
    }
    // Check provider/model format: provider_id/model_id
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        // #154: hint if the model looks like it belongs to a different provider
        let mut err_msg = format!(
            "invalid model syntax: '{}'. Expected provider/model (e.g., anthropic/claude-opus-4-6) or known alias (opus, sonnet, haiku)",
            trimmed
        );
        if trimmed.starts_with("gpt-") || trimmed.starts_with("gpt_") {
            err_msg.push_str("\nDid you mean `openai/");
            err_msg.push_str(trimmed);
            err_msg.push_str("`? (Requires OPENAI_API_KEY env var)");
        } else if trimmed.starts_with("qwen") {
            err_msg.push_str("\nDid you mean `qwen/");
            err_msg.push_str(trimmed);
            err_msg.push_str("`? (Requires DASHSCOPE_API_KEY env var)");
        } else if trimmed.starts_with("grok") {
            err_msg.push_str("\nDid you mean `xai/");
            err_msg.push_str(trimmed);
            err_msg.push_str("`? (Requires XAI_API_KEY env var)");
        }
        return Err(err_msg);
    }
    Ok(())
}

fn config_alias_for_current_dir(alias: &str) -> Option<String> {
    if alias.is_empty() {
        return None;
    }
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    let config = loader.load().ok()?;
    config.aliases().get(alias).cloned()
}

fn normalize_allowed_tools(values: &[String]) -> Result<Option<AllowedToolSet>, String> {
    if values.is_empty() {
        return Ok(None);
    }
    current_tool_registry()?.normalize_allowed_tools(values)
}

fn current_tool_registry() -> Result<GlobalToolRegistry, String> {
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load().map_err(|error| error.to_string())?;
    let state = build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
        .map_err(|error| error.to_string())?;
    let registry = state.tool_registry.clone();
    if let Some(mcp_state) = state.mcp_state {
        mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown()
            .map_err(|error| error.to_string())?;
    }
    Ok(registry)
}

fn parse_permission_mode_arg(value: &str) -> Result<PermissionMode, String> {
    normalize_permission_mode(value)
        .ok_or_else(|| {
            format!(
                "unsupported permission mode '{value}'. Use read-only, workspace-write, or danger-full-access."
            )
        })
        .map(permission_mode_from_label)
}

fn permission_mode_from_label(mode: &str) -> PermissionMode {
    match mode {
        "read-only" => PermissionMode::ReadOnly,
        "workspace-write" => PermissionMode::WorkspaceWrite,
        "danger-full-access" => PermissionMode::DangerFullAccess,
        other => panic!("unsupported permission mode label: {other}"),
    }
}

fn permission_mode_from_resolved(mode: ResolvedPermissionMode) -> PermissionMode {
    match mode {
        ResolvedPermissionMode::ReadOnly => PermissionMode::ReadOnly,
        ResolvedPermissionMode::WorkspaceWrite => PermissionMode::WorkspaceWrite,
        ResolvedPermissionMode::DangerFullAccess => PermissionMode::DangerFullAccess,
    }
}

fn default_permission_mode() -> PermissionMode {
    env::var("RUSTY_CLAUDE_PERMISSION_MODE")
        .ok()
        .as_deref()
        .and_then(normalize_permission_mode)
        .map(permission_mode_from_label)
        .or_else(config_permission_mode_for_current_dir)
        .unwrap_or(PermissionMode::DangerFullAccess)
}

fn config_permission_mode_for_current_dir() -> Option<PermissionMode> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader
        .load()
        .ok()?
        .permission_mode()
        .map(permission_mode_from_resolved)
}

fn config_model_for_current_dir() -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader.load().ok()?.model().map(ToOwned::to_owned)
}

fn resolve_repl_model(cli_model: String) -> String {
    if cli_model != DEFAULT_MODEL {
        return cli_model;
    }
    if let Some(env_model) = env::var("ANTHROPIC_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return resolve_model_alias_with_config(&env_model);
    }
    if let Some(config_model) = config_model_for_current_dir() {
        return resolve_model_alias_with_config(&config_model);
    }
    cli_model
}

fn provider_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Xai => "xai",
        ProviderKind::OpenAi => "openai",
    }
}

fn format_connected_line(model: &str) -> String {
    let provider = provider_label(detect_provider_kind(model));
    format!("Connected: {model} via {provider}")
}

fn filter_tool_specs(
    tool_registry: &GlobalToolRegistry,
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<ToolDefinition> {
    tool_registry.definitions(allowed_tools)
}

fn parse_system_prompt_args(
    args: &[String],
    model: String,
    output_format: CliOutputFormat,
) -> Result<CliAction, String> {
    let mut cwd = env::current_dir().map_err(|error| error.to_string())?;
    let mut date = DEFAULT_DATE.to_string();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --cwd".to_string())?;
                cwd = PathBuf::from(value);
                index += 2;
            }
            "--date" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --date".to_string())?;
                date.clone_from(value);
                index += 2;
            }
            other => {
                // #152: hint `--output-format json` when user types `--json`.
                let mut msg = format!("unknown system-prompt option: {other}");
                if other == "--json" {
                    msg.push_str("\nDid you mean `--output-format json`?");
                }
                return Err(msg);
            }
        }
    }

    Ok(CliAction::PrintSystemPrompt {
        cwd,
        date,
        model,
        output_format,
    })
}

fn parse_export_args(args: &[String], output_format: CliOutputFormat) -> Result<CliAction, String> {
    let mut session_reference = LATEST_SESSION_REFERENCE.to_string();
    let mut output_path: Option<PathBuf> = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--session" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --session".to_string())?;
                session_reference.clone_from(value);
                index += 2;
            }
            flag if flag.starts_with("--session=") => {
                session_reference = flag[10..].to_string();
                index += 1;
            }
            "--output" | "-o" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value for {}", args[index]))?;
                output_path = Some(PathBuf::from(value));
                index += 2;
            }
            flag if flag.starts_with("--output=") => {
                output_path = Some(PathBuf::from(&flag[9..]));
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown export option: {other}"));
            }
            other if output_path.is_none() => {
                output_path = Some(PathBuf::from(other));
                index += 1;
            }
            other => {
                return Err(format!("unexpected export argument: {other}"));
            }
        }
    }

    Ok(CliAction::Export {
        session_reference,
        output_path,
        output_format,
    })
}

fn parse_dump_manifests_args(
    args: &[String],
    output_format: CliOutputFormat,
) -> Result<CliAction, String> {
    let mut manifests_dir: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--manifests-dir" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| String::from("--manifests-dir requires a path"))?;
            manifests_dir = Some(PathBuf::from(value));
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--manifests-dir=") {
            if value.is_empty() {
                return Err(String::from("--manifests-dir requires a path"));
            }
            manifests_dir = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        return Err(format!("unknown dump-manifests option: {arg}"));
    }

    Ok(CliAction::DumpManifests {
        output_format,
        manifests_dir,
    })
}

fn parse_resume_args(args: &[String], output_format: CliOutputFormat) -> Result<CliAction, String> {
    let (session_path, command_tokens): (PathBuf, &[String]) = match args.first() {
        None => (PathBuf::from(LATEST_SESSION_REFERENCE), &[]),
        Some(first) if looks_like_slash_command_token(first) => {
            (PathBuf::from(LATEST_SESSION_REFERENCE), args)
        }
        Some(first) => (PathBuf::from(first), &args[1..]),
    };
    let mut commands = Vec::new();
    let mut current_command = String::new();

    for token in command_tokens {
        if token.trim_start().starts_with('/') {
            if resume_command_can_absorb_token(&current_command, token) {
                current_command.push(' ');
                current_command.push_str(token);
                continue;
            }
            if !current_command.is_empty() {
                commands.push(current_command);
            }
            current_command = String::from(token.as_str());
            continue;
        }

        if current_command.is_empty() {
            return Err("--resume trailing arguments must be slash commands".to_string());
        }

        current_command.push(' ');
        current_command.push_str(token);
    }

    if !current_command.is_empty() {
        commands.push(current_command);
    }

    Ok(CliAction::ResumeSession {
        session_path,
        commands,
        output_format,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticLevel {
    Ok,
    Warn,
    Fail,
}

impl DiagnosticLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    fn is_failure(self) -> bool {
        matches!(self, Self::Fail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticCheck {
    name: &'static str,
    level: DiagnosticLevel,
    summary: String,
    details: Vec<String>,
    data: Map<String, Value>,
}

impl DiagnosticCheck {
    fn new(name: &'static str, level: DiagnosticLevel, summary: impl Into<String>) -> Self {
        Self {
            name,
            level,
            summary: summary.into(),
            details: Vec::new(),
            data: Map::new(),
        }
    }

    fn with_details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }

    fn with_data(mut self, data: Map<String, Value>) -> Self {
        self.data = data;
        self
    }

    fn json_value(&self) -> Value {
        let mut value = Map::from_iter([
            (
                "name".to_string(),
                Value::String(self.name.to_ascii_lowercase()),
            ),
            (
                "status".to_string(),
                Value::String(self.level.label().to_string()),
            ),
            ("summary".to_string(), Value::String(self.summary.clone())),
            (
                "details".to_string(),
                Value::Array(
                    self.details
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>(),
                ),
            ),
        ]);
        value.extend(self.data.clone());
        Value::Object(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorReport {
    checks: Vec<DiagnosticCheck>,
}

impl DoctorReport {
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Ok)
                .count(),
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Warn)
                .count(),
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Fail)
                .count(),
        )
    }

    fn has_failures(&self) -> bool {
        self.checks.iter().any(|check| check.level.is_failure())
    }

    fn status(&self) -> &'static str {
        let (_, warn_count, fail_count) = self.counts();
        if fail_count > 0 {
            "fail"
        } else if warn_count > 0 {
            "warn"
        } else {
            "ok"
        }
    }

    fn render(&self) -> String {
        let (ok_count, warn_count, fail_count) = self.counts();
        let mut lines = vec![
            "Doctor".to_string(),
            format!(
                "Summary\n  OK               {ok_count}\n  Warnings         {warn_count}\n  Failures         {fail_count}"
            ),
        ];
        lines.extend(self.checks.iter().map(render_diagnostic_check));
        lines.join("\n\n")
    }

    fn json_value(&self) -> Value {
        let report = self.render();
        let (ok_count, warn_count, fail_count) = self.counts();
        json!({
            "kind": "doctor",
            "status": self.status(),
            "message": report,
            "report": report,
            "has_failures": self.has_failures(),
            "summary": {
                "total": self.checks.len(),
                "ok": ok_count,
                "warnings": warn_count,
                "failures": fail_count,
            },
            "checks": self
                .checks
                .iter()
                .map(DiagnosticCheck::json_value)
                .collect::<Vec<_>>(),
        })
    }
}

fn render_diagnostic_check(check: &DiagnosticCheck) -> String {
    let mut lines = vec![format!(
        "{}\n  Status           {}\n  Summary          {}",
        check.name,
        check.level.label(),
        check.summary
    )];
    if !check.details.is_empty() {
        lines.push("  Details".to_string());
        lines.extend(check.details.iter().map(|detail| format!("    - {detail}")));
    }
    lines.join("\n")
}

fn render_doctor_report() -> Result<DoctorReport, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let config_loader = ConfigLoader::default_for(&cwd);
    let config = config_loader.load();
    let discovered_config = config_loader.discover();
    let project_context = ProjectContext::discover_with_git(&cwd, DEFAULT_DATE)?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    let git_summary = parse_git_workspace_summary(project_context.git_status.as_deref());
    let branch_freshness = BranchFreshness::from_git_status(project_context.git_status.as_deref());
    let stale_base_state = stale_base_state_for(&cwd, None);
    let empty_config = runtime::RuntimeConfig::empty();
    let sandbox_config = config.as_ref().ok().unwrap_or(&empty_config);
    let boot_preflight = build_boot_preflight_snapshot(
        &cwd,
        project_root.as_deref(),
        project_context.git_status.as_deref(),
        config.as_ref().ok(),
        config.as_ref().err().map(ToString::to_string).as_deref(),
    );
    let context = StatusContext {
        cwd: cwd.clone(),
        session_path: None,
        loaded_config_files: config
            .as_ref()
            .ok()
            .map_or(0, |runtime_config| runtime_config.loaded_entries().len()),
        discovered_config_files: discovered_config.len(),
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
        git_summary,
        branch_freshness,
        stale_base_state,
        session_lifecycle: classify_session_lifecycle_for(&cwd),
        boot_preflight,
        sandbox_status: resolve_sandbox_status(sandbox_config.sandbox(), &cwd),
        // Doctor path has its own config check; StatusContext here is only
        // fed into health renderers that don't read config_load_error.
        config_load_error: config.as_ref().err().map(ToString::to_string),
    };
    Ok(DoctorReport {
        checks: vec![
            check_auth_health(),
            check_config_health(&config_loader, config.as_ref()),
            check_install_source_health(),
            check_workspace_health(&context),
            check_boot_preflight_health(&context),
            check_sandbox_health(&context.sandbox_status),
            check_system_health(&cwd, config.as_ref().ok()),
        ],
    })
}

fn run_doctor(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let report = render_doctor_report()?;
    let message = report.render();
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report.json_value())?);
        }
    }
    if report.has_failures() {
        return Err("doctor found failing checks".into());
    }
    Ok(())
}

/// Starts a minimal Model Context Protocol server that exposes claw's
/// built-in tools over stdio.
///
/// Tool descriptors come from [`tools::mvp_tool_specs`] and calls are
/// dispatched through [`tools::execute_tool`], so this server exposes exactly
/// Read `.claw/worker-state.json` from the current working directory and print it.
/// This is the file-based worker observability surface: `push_event()` in `worker_boot.rs`
/// atomically writes state transitions here so external observers (clawhip, orchestrators)
/// can poll current `WorkerStatus` without needing an HTTP route on the opencode binary.
fn run_worker_state(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let state_path = cwd.join(".claw").join("worker-state.json");
    if !state_path.exists() {
        // #139: this error used to say "run a worker first" without telling
        // callers how to run one. "worker" is an internal concept (there is
        // no `claw worker` subcommand), so claws/CI had no discoverable path
        // from the error to a fix. Emit an actionable, structured error that
        // names the two concrete commands that produce worker state.
        //
        // Format in both text and JSON modes is stable so scripts can match:
        //   error: no worker state file found at <path>
        //     Hint: worker state is written by the interactive REPL or a non-interactive prompt.
        //     Run:   claw               # start the REPL (writes state on first turn)
        //     Or:    claw prompt <text> # run one non-interactive turn
        //     Then rerun: claw state [--output-format json]
        return Err(format!(
            "no worker state file found at {path}\n  Hint: worker state is written by the interactive REPL or a non-interactive prompt.\n  Run:   claw               # start the REPL (writes state on first turn)\n  Or:    claw prompt <text> # run one non-interactive turn\n  Then rerun: claw state [--output-format json]",
            path = state_path.display()
        )
        .into());
    }
    let raw = std::fs::read_to_string(&state_path)?;
    match output_format {
        CliOutputFormat::Text => println!("{raw}"),
        CliOutputFormat::Json => {
            // Validate it parses as JSON before re-emitting
            let _: serde_json::Value = serde_json::from_str(&raw)?;
            println!("{raw}");
        }
    }
    Ok(())
}

/// the same surface the in-process agent loop uses.
fn run_mcp_serve() -> Result<(), Box<dyn std::error::Error>> {
    let tools = mvp_tool_specs()
        .into_iter()
        .map(|spec| McpTool {
            name: spec.name.to_string(),
            description: Some(spec.description.to_string()),
            input_schema: Some(spec.input_schema),
            annotations: None,
            meta: None,
        })
        .collect();

    let spec = McpServerSpec {
        server_name: "claw".to_string(),
        server_version: VERSION.to_string(),
        tools,
        tool_handler: Box::new(execute_tool),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut server = McpServer::new(spec);
        server.run().await
    })?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn check_auth_health() -> DiagnosticCheck {
    let api_key_present = env::var("ANTHROPIC_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let auth_token_present = env::var("ANTHROPIC_AUTH_TOKEN")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let env_details = format!(
        "Environment       api_key={} auth_token={}",
        if api_key_present { "present" } else { "absent" },
        if auth_token_present {
            "present"
        } else {
            "absent"
        }
    );

    match load_oauth_credentials() {
        Ok(Some(token_set)) => DiagnosticCheck::new(
            "Auth",
            if api_key_present || auth_token_present {
                DiagnosticLevel::Ok
            } else {
                DiagnosticLevel::Warn
            },
            if api_key_present || auth_token_present {
                "supported auth env vars are configured; legacy saved OAuth is ignored"
            } else {
                "legacy saved OAuth credentials are present but unsupported"
            },
        )
        .with_details(vec![
            env_details,
            format!(
                "Legacy OAuth      expires_at={} refresh_token={} scopes={}",
                token_set
                    .expires_at
                    .map_or_else(|| "<none>".to_string(), |value| value.to_string()),
                if token_set.refresh_token.is_some() {
                    "present"
                } else {
                    "absent"
                },
                if token_set.scopes.is_empty() {
                    "<none>".to_string()
                } else {
                    token_set.scopes.join(",")
                }
            ),
            "Suggested action  set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN; `claw login` is removed"
                .to_string(),
        ])
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            ("auth_token_present".to_string(), json!(auth_token_present)),
            ("legacy_saved_oauth_present".to_string(), json!(true)),
            (
                "legacy_saved_oauth_expires_at".to_string(),
                json!(token_set.expires_at),
            ),
            (
                "legacy_refresh_token_present".to_string(),
                json!(token_set.refresh_token.is_some()),
            ),
            ("legacy_scopes".to_string(), json!(token_set.scopes)),
        ])),
        Ok(None) => DiagnosticCheck::new(
            "Auth",
            if api_key_present || auth_token_present {
                DiagnosticLevel::Ok
            } else {
                DiagnosticLevel::Warn
            },
            if api_key_present || auth_token_present {
                "supported auth env vars are configured"
            } else {
                "no supported auth env vars were found"
            },
        )
        .with_details(vec![env_details])
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            ("auth_token_present".to_string(), json!(auth_token_present)),
            ("legacy_saved_oauth_present".to_string(), json!(false)),
            ("legacy_saved_oauth_expires_at".to_string(), Value::Null),
            ("legacy_refresh_token_present".to_string(), json!(false)),
            ("legacy_scopes".to_string(), json!(Vec::<String>::new())),
        ])),
        Err(error) => DiagnosticCheck::new(
            "Auth",
            DiagnosticLevel::Fail,
            format!("failed to inspect legacy saved credentials: {error}"),
        )
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            ("auth_token_present".to_string(), json!(auth_token_present)),
            ("legacy_saved_oauth_present".to_string(), Value::Null),
            ("legacy_saved_oauth_expires_at".to_string(), Value::Null),
            ("legacy_refresh_token_present".to_string(), Value::Null),
            ("legacy_scopes".to_string(), Value::Null),
            ("legacy_saved_oauth_error".to_string(), json!(error.to_string())),
        ])),
    }
}

fn check_config_health(
    config_loader: &ConfigLoader,
    config: Result<&runtime::RuntimeConfig, &runtime::ConfigError>,
) -> DiagnosticCheck {
    let discovered = config_loader.discover();
    let discovered_count = discovered.len();
    // Separate candidate paths that actually exist from those that don't.
    // Showing non-existent paths as "Discovered file" implies they loaded
    // but something went wrong, which is confusing. We only surface paths
    // that exist on disk as discovered; non-existent ones are silently
    // omitted from the display (they are just the standard search locations).
    let present_paths: Vec<String> = discovered
        .iter()
        .filter(|e| e.path.exists())
        .map(|e| e.path.display().to_string())
        .collect();
    let discovered_paths = discovered
        .iter()
        .map(|entry| entry.path.display().to_string())
        .collect::<Vec<_>>();
    match config {
        Ok(runtime_config) => {
            let loaded_entries = runtime_config.loaded_entries();
            let loaded_count = loaded_entries.len();
            let present_count = present_paths.len();
            let mut details = vec![format!(
                "Config files      loaded {}/{}",
                loaded_count, present_count
            )];
            if let Some(model) = runtime_config.model() {
                details.push(format!("Resolved model    {model}"));
            }
            details.push(format!(
                "MCP servers       {}",
                runtime_config.mcp().servers().len()
            ));
            if present_paths.is_empty() {
                details.push("Discovered files  <none> (defaults active)".to_string());
            } else {
                details.extend(
                    present_paths
                        .iter()
                        .map(|path| format!("Discovered file   {path}")),
                );
            }
            DiagnosticCheck::new(
                "Config",
                DiagnosticLevel::Ok,
                if present_count == 0 {
                    "no config files present; defaults are active"
                } else {
                    "runtime config loaded successfully"
                },
            )
            .with_details(details)
            .with_data(Map::from_iter([
                ("discovered_files".to_string(), json!(present_paths)),
                ("discovered_files_count".to_string(), json!(present_count)),
                ("loaded_config_files".to_string(), json!(loaded_count)),
                ("resolved_model".to_string(), json!(runtime_config.model())),
                (
                    "mcp_servers".to_string(),
                    json!(runtime_config.mcp().servers().len()),
                ),
            ]))
        }
        Err(error) => DiagnosticCheck::new(
            "Config",
            DiagnosticLevel::Fail,
            format!("runtime config failed to load: {error}"),
        )
        .with_details(if discovered_paths.is_empty() {
            vec!["Discovered files  <none>".to_string()]
        } else {
            discovered_paths
                .iter()
                .map(|path| format!("Discovered file   {path}"))
                .collect()
        })
        .with_data(Map::from_iter([
            ("discovered_files".to_string(), json!(discovered_paths)),
            (
                "discovered_files_count".to_string(),
                json!(discovered_count),
            ),
            ("loaded_config_files".to_string(), json!(0)),
            ("resolved_model".to_string(), Value::Null),
            ("mcp_servers".to_string(), Value::Null),
            ("load_error".to_string(), json!(error.to_string())),
        ])),
    }
}

fn check_install_source_health() -> DiagnosticCheck {
    DiagnosticCheck::new(
        "Install source",
        DiagnosticLevel::Ok,
        format!(
            "official source of truth is {OFFICIAL_REPO_SLUG}; avoid `{DEPRECATED_INSTALL_COMMAND}`"
        ),
    )
    .with_details(vec![
        format!("Official repo     {OFFICIAL_REPO_URL}"),
        "Recommended path  build from this repo or use the upstream binary documented in README.md"
            .to_string(),
        format!(
            "Deprecated crate  `{DEPRECATED_INSTALL_COMMAND}` installs a deprecated stub and does not provide the `claw` binary"
        )
            .to_string(),
    ])
    .with_data(Map::from_iter([
        ("official_repo".to_string(), json!(OFFICIAL_REPO_URL)),
        (
            "deprecated_install".to_string(),
            json!(DEPRECATED_INSTALL_COMMAND),
        ),
        (
            "recommended_install".to_string(),
            json!("build from source or follow the upstream binary instructions in README.md"),
        ),
    ]))
}

fn check_workspace_health(context: &StatusContext) -> DiagnosticCheck {
    let in_repo = context.project_root.is_some();
    let stale_base_warning = format_stale_base_warning(&context.stale_base_state);
    DiagnosticCheck::new(
        "Workspace",
        if in_repo && stale_base_warning.is_none() {
            DiagnosticLevel::Ok
        } else {
            DiagnosticLevel::Warn
        },
        if in_repo {
            format!(
                "project root detected on branch {}",
                context.git_branch.as_deref().unwrap_or("unknown")
            )
        } else {
            "current directory is not inside a git project".to_string()
        },
    )
    .with_details(vec![
        format!("Cwd              {}", context.cwd.display()),
        format!(
            "Project root     {}",
            context
                .project_root
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |path| path.display().to_string())
        ),
        format!(
            "Git branch       {}",
            context.git_branch.as_deref().unwrap_or("unknown")
        ),
        format!("Git state        {}", context.git_summary.headline()),
        format!("Changed files    {}", context.git_summary.changed_files),
        format!(
            "Memory files     {} · config files loaded {}/{}",
            context.memory_file_count, context.loaded_config_files, context.discovered_config_files
        ),
        format!(
            "Stale base      {}",
            stale_base_warning.as_deref().unwrap_or("ok")
        ),
    ])
    .with_data(Map::from_iter([
        ("cwd".to_string(), json!(context.cwd.display().to_string())),
        (
            "project_root".to_string(),
            json!(context
                .project_root
                .as_ref()
                .map(|path| path.display().to_string())),
        ),
        ("in_git_repo".to_string(), json!(in_repo)),
        ("git_branch".to_string(), json!(context.git_branch)),
        (
            "git_state".to_string(),
            json!(context.git_summary.headline()),
        ),
        (
            "changed_files".to_string(),
            json!(context.git_summary.changed_files),
        ),
        (
            "memory_file_count".to_string(),
            json!(context.memory_file_count),
        ),
        (
            "loaded_config_files".to_string(),
            json!(context.loaded_config_files),
        ),
        (
            "discovered_config_files".to_string(),
            json!(context.discovered_config_files),
        ),
        (
            "stale_base".to_string(),
            stale_base_json_value(&context.stale_base_state),
        ),
    ]))
}

fn check_boot_preflight_health(context: &StatusContext) -> DiagnosticCheck {
    let preflight = &context.boot_preflight;
    let missing_binaries = preflight
        .required_binaries
        .iter()
        .filter(|binary| !binary.available)
        .map(|binary| binary.name)
        .collect::<Vec<_>>();
    let socket_details = preflight
        .control_sockets
        .iter()
        .map(|socket| {
            format!(
                "Control socket  {} configured={} exists={} path={}",
                socket.name,
                socket.configured,
                socket.exists,
                socket.path.as_deref().unwrap_or("<none>")
            )
        })
        .collect::<Vec<_>>();
    let mut details = vec![
        format!("Repo exists      {}", preflight.repo_exists),
        format!("Worktree exists  {}", preflight.worktree_exists),
        format!("Git dir exists   {}", preflight.git_dir_exists),
        format!("Branch behind    {}", preflight.branch_freshness.behind),
        format!("Trust allowlist  {:?}", preflight.trust_gate_allowed),
        format!("Trusted roots    {}", preflight.trusted_roots_count),
        format!(
            "MCP eligible     {} · servers {}",
            preflight.mcp_startup_eligible, preflight.mcp_servers_configured
        ),
        format!(
            "Plugin eligible  {} · configured {}",
            preflight.plugin_startup_eligible, preflight.plugins_configured
        ),
        format!(
            "Last failed boot {}",
            preflight
                .last_failed_boot_reason
                .as_deref()
                .unwrap_or("<none>")
        ),
    ];
    details.extend(preflight.required_binaries.iter().map(|binary| {
        format!(
            "Required binary {} available={}",
            binary.name, binary.available
        )
    }));
    details.extend(socket_details);
    DiagnosticCheck::new(
        "Boot preflight",
        if preflight.repo_exists && preflight.worktree_exists && missing_binaries.is_empty() {
            DiagnosticLevel::Ok
        } else {
            DiagnosticLevel::Warn
        },
        preflight.summary(),
    )
    .with_details(details)
    .with_data(Map::from_iter([(
        "boot_preflight".to_string(),
        preflight.json_value(),
    )]))
}

fn check_sandbox_health(status: &runtime::SandboxStatus) -> DiagnosticCheck {
    let degraded = status.enabled && !status.active;
    let mut details = vec![
        format!("Enabled          {}", status.enabled),
        format!("Active           {}", status.active),
        format!("Supported        {}", status.supported),
        format!("Filesystem mode  {}", status.filesystem_mode.as_str()),
        format!("Filesystem live  {}", status.filesystem_active),
    ];
    if let Some(reason) = &status.fallback_reason {
        details.push(format!("Fallback reason  {reason}"));
    }
    DiagnosticCheck::new(
        "Sandbox",
        if degraded {
            DiagnosticLevel::Warn
        } else {
            DiagnosticLevel::Ok
        },
        if degraded {
            "sandbox was requested but is not currently active"
        } else if status.active {
            "sandbox protections are active"
        } else {
            "sandbox is not active for this session"
        },
    )
    .with_details(details)
    .with_data(Map::from_iter([
        ("enabled".to_string(), json!(status.enabled)),
        ("active".to_string(), json!(status.active)),
        ("supported".to_string(), json!(status.supported)),
        (
            "namespace_supported".to_string(),
            json!(status.namespace_supported),
        ),
        (
            "namespace_active".to_string(),
            json!(status.namespace_active),
        ),
        (
            "network_supported".to_string(),
            json!(status.network_supported),
        ),
        ("network_active".to_string(), json!(status.network_active)),
        (
            "filesystem_mode".to_string(),
            json!(status.filesystem_mode.as_str()),
        ),
        (
            "filesystem_active".to_string(),
            json!(status.filesystem_active),
        ),
        ("allowed_mounts".to_string(), json!(status.allowed_mounts)),
        ("in_container".to_string(), json!(status.in_container)),
        (
            "container_markers".to_string(),
            json!(status.container_markers),
        ),
        ("fallback_reason".to_string(), json!(status.fallback_reason)),
    ]))
}

fn check_system_health(cwd: &Path, config: Option<&runtime::RuntimeConfig>) -> DiagnosticCheck {
    let default_model = config.and_then(runtime::RuntimeConfig::model);
    let mut details = vec![
        format!("OS               {} {}", env::consts::OS, env::consts::ARCH),
        format!("Working dir      {}", cwd.display()),
        format!("Version          {}", VERSION),
        format!("Build target     {}", BUILD_TARGET.unwrap_or("<unknown>")),
        format!("Git SHA          {}", GIT_SHA.unwrap_or("<unknown>")),
    ];
    if let Some(model) = default_model {
        details.push(format!("Default model    {model}"));
    }
    DiagnosticCheck::new(
        "System",
        DiagnosticLevel::Ok,
        "captured local runtime metadata",
    )
    .with_details(details)
    .with_data(Map::from_iter([
        ("os".to_string(), json!(env::consts::OS)),
        ("arch".to_string(), json!(env::consts::ARCH)),
        ("working_dir".to_string(), json!(cwd.display().to_string())),
        ("version".to_string(), json!(VERSION)),
        ("build_target".to_string(), json!(BUILD_TARGET)),
        ("git_sha".to_string(), json!(GIT_SHA)),
        ("default_model".to_string(), json!(default_model)),
    ]))
}

fn resume_command_can_absorb_token(current_command: &str, token: &str) -> bool {
    matches!(
        SlashCommand::parse(current_command),
        Ok(Some(SlashCommand::Export { path: None }))
    ) && !looks_like_slash_command_token(token)
}

fn looks_like_slash_command_token(token: &str) -> bool {
    let trimmed = token.trim_start();
    let Some(name) = trimmed.strip_prefix('/').and_then(|value| {
        value
            .split_whitespace()
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }) else {
        return false;
    };

    slash_command_specs()
        .iter()
        .any(|spec| spec.name == name || spec.aliases.contains(&name))
}

fn dump_manifests(
    manifests_dir: Option<&Path>,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    dump_manifests_at_path(&workspace_dir, manifests_dir, output_format)
}

const DUMP_MANIFESTS_OVERRIDE_HINT: &str =
    "Hint: set CLAUDE_CODE_UPSTREAM=/path/to/upstream or pass `claw dump-manifests --manifests-dir /path/to/upstream`.";

// Internal function for testing that accepts a workspace directory path.
fn dump_manifests_at_path(
    workspace_dir: &std::path::Path,
    manifests_dir: Option<&Path>,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths = if let Some(dir) = manifests_dir {
        let resolved = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        UpstreamPaths::from_repo_root(resolved)
    } else {
        // Surface the resolved path in the error so users can diagnose missing
        // manifest files without guessing what path the binary expected.
        let resolved = workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace_dir.to_path_buf());
        UpstreamPaths::from_workspace_dir(&resolved)
    };

    let source_root = paths.repo_root();
    if !source_root.exists() {
        return Err(format!(
            "Manifest source directory does not exist.\n  looked in: {}\n  {DUMP_MANIFESTS_OVERRIDE_HINT}",
            source_root.display(),
        )
        .into());
    }

    let required_paths = [
        ("src/commands.ts", paths.commands_path()),
        ("src/tools.ts", paths.tools_path()),
        ("src/entrypoints/cli.tsx", paths.cli_path()),
    ];
    let missing = required_paths
        .iter()
        .filter_map(|(label, path)| (!path.is_file()).then_some(*label))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "Manifest source files are missing.\n  repo root: {}\n  missing: {}\n  {DUMP_MANIFESTS_OVERRIDE_HINT}",
            source_root.display(),
            missing.join(", "),
        )
        .into());
    }

    match extract_manifest(&paths) {
        Ok(manifest) => {
            match output_format {
                CliOutputFormat::Text => {
                    println!("commands: {}", manifest.commands.entries().len());
                    println!("tools: {}", manifest.tools.entries().len());
                    println!("bootstrap phases: {}", manifest.bootstrap.phases().len());
                }
                CliOutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "kind": "dump-manifests",
                        "commands": manifest.commands.entries().len(),
                        "tools": manifest.tools.entries().len(),
                        "bootstrap_phases": manifest.bootstrap.phases().len(),
                    }))?
                ),
            }
            Ok(())
        }
        Err(error) => Err(format!(
            "failed to extract manifests: {error}\n  looked in: {path}\n  {DUMP_MANIFESTS_OVERRIDE_HINT}",
            path = paths.repo_root().display()
        )
        .into()),
    }
}

fn print_bootstrap_plan(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let phases = runtime::BootstrapPlan::claude_code_default()
        .phases()
        .iter()
        .map(|phase| format!("{phase:?}"))
        .collect::<Vec<_>>();
    match output_format {
        CliOutputFormat::Text => {
            for phase in &phases {
                println!("- {phase}");
            }
        }
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "bootstrap-plan",
                "phases": phases,
            }))?
        ),
    }
    Ok(())
}

fn print_system_prompt(
    cwd: PathBuf,
    date: String,
    model: &str,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let sections = load_system_prompt(
        cwd,
        date,
        env::consts::OS,
        "unknown",
        model_family_identity_for(model),
    )?;
    let message = sections.join(
        "

",
    );
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "system-prompt",
                "message": message,
                "sections": sections,
            }))?
        ),
    }
    Ok(())
}

fn print_version(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => println!("{}", render_version_report()),
        CliOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&version_json_value())?);
        }
    }
    Ok(())
}

fn version_json_value() -> serde_json::Value {
    let executable_path = env::current_exe().ok().map(|p| p.display().to_string());
    json!({
        "kind": "version",
        "message": render_version_report(),
        "version": VERSION,
        "git_sha": GIT_SHA,
        "target": BUILD_TARGET,
        "build_date": DEFAULT_DATE,
        "executable_path": executable_path,
    })
}

#[allow(clippy::too_many_lines)]
fn resume_session(session_path: &Path, commands: &[String], output_format: CliOutputFormat) {
    let session_reference = session_path.display().to_string();
    let (handle, session) = match load_session_reference(&session_reference) {
        Ok(loaded) => loaded,
        Err(error) => {
            if output_format == CliOutputFormat::Json {
                // #77: classify session load errors for downstream consumers
                let full_message = format!("failed to restore session: {error}");
                let kind = classify_error_kind(&full_message);
                let (short_reason, hint) = split_error_hint(&full_message);
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "type": "error",
                        "error": short_reason,
                        "kind": kind,
                        "hint": hint,
                    })
                );
            } else {
                eprintln!("failed to restore session: {error}");
            }
            std::process::exit(1);
        }
    };
    let resolved_path = handle.path.clone();

    if commands.is_empty() {
        if output_format == CliOutputFormat::Json {
            println!(
                "{}",
                serde_json::json!({
                    "kind": "restored",
                    "session_id": session.session_id,
                    "path": handle.path.display().to_string(),
                    "message_count": session.messages.len(),
                })
            );
        } else {
            println!(
                "Restored session from {} ({} messages).",
                handle.path.display(),
                session.messages.len()
            );
        }
        return;
    }

    let mut session = session;
    for raw_command in commands {
        // Intercept spec commands that have no parse arm before calling
        // SlashCommand::parse — they return Err(SlashCommandParseError) which
        // formats as the confusing circular "Did you mean /X?" message.
        // STUB_COMMANDS covers both completions-filtered stubs and parse-less
        // spec entries; treat both as unsupported in resume mode.
        {
            let cmd_root = raw_command
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            if STUB_COMMANDS.contains(&cmd_root) {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": format!("/{cmd_root} is not yet implemented in this build"),
                            "kind": "unsupported_command",
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("/{cmd_root} is not yet implemented in this build");
                }
                std::process::exit(2);
            }
        }
        let command = match SlashCommand::parse(raw_command) {
            Ok(Some(command)) => command,
            Ok(None) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": format!("unsupported resumed command: {raw_command}"),
                            "kind": "unsupported_resumed_command",
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("unsupported resumed command: {raw_command}");
                }
                std::process::exit(2);
            }
            Err(error) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": error.to_string(),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("{error}");
                }
                std::process::exit(2);
            }
        };
        match run_resume_command(&resolved_path, &session, &command) {
            Ok(ResumeCommandOutcome {
                session: next_session,
                message,
                json,
            }) => {
                session = next_session;
                if output_format == CliOutputFormat::Json {
                    if let Some(value) = json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&value)
                                .expect("resume command json output")
                        );
                    } else if let Some(message) = message {
                        println!("{message}");
                    }
                } else if let Some(message) = message {
                    println!("{message}");
                }
            }
            Err(error) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": error.to_string(),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("{error}");
                }
                std::process::exit(2);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ResumeCommandOutcome {
    session: Session,
    message: Option<String>,
    json: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct StatusContext {
    cwd: PathBuf,
    session_path: Option<PathBuf>,
    loaded_config_files: usize,
    discovered_config_files: usize,
    memory_file_count: usize,
    project_root: Option<PathBuf>,
    git_branch: Option<String>,
    git_summary: GitWorkspaceSummary,
    branch_freshness: BranchFreshness,
    stale_base_state: BaseCommitState,
    session_lifecycle: SessionLifecycleSummary,
    boot_preflight: BootPreflightSnapshot,
    sandbox_status: runtime::SandboxStatus,
    /// #143: when `.claw.json` (or another loaded config file) fails to parse,
    /// we capture the parse error here and still populate every field that
    /// doesn't depend on runtime config (workspace, git, sandbox defaults,
    /// discovery counts). Top-level JSON output then reports
    /// `status: "degraded"` so claws can distinguish "status ran but config
    /// is broken" from "status ran cleanly".
    config_load_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BranchFreshness {
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    fresh: Option<bool>,
}

impl BranchFreshness {
    fn from_git_status(status: Option<&str>) -> Self {
        let first_line = status
            .and_then(|status| status.lines().next())
            .unwrap_or_default();
        let upstream = first_line
            .split_once("...")
            .and_then(|(_, rest)| rest.split([' ', '[']).next())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let mut ahead = 0;
        let mut behind = 0;
        if let Some((_, bracketed)) = first_line.split_once('[') {
            let bracketed = bracketed.trim_end_matches(']');
            for part in bracketed.split(',').map(str::trim) {
                if let Some(value) = part.strip_prefix("ahead ") {
                    ahead = value.parse().unwrap_or(0);
                } else if let Some(value) = part.strip_prefix("behind ") {
                    behind = value.parse().unwrap_or(0);
                }
            }
        }
        let fresh = upstream.as_ref().map(|_| behind == 0);
        Self {
            upstream,
            ahead,
            behind,
            fresh,
        }
    }

    fn json_value(&self) -> serde_json::Value {
        json!({
            "upstream": self.upstream,
            "ahead": self.ahead,
            "behind": self.behind,
            "fresh": self.fresh,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryPreflight {
    name: &'static str,
    available: bool,
}

impl BinaryPreflight {
    fn json_value(&self) -> serde_json::Value {
        json!({
            "name": self.name,
            "available": self.available,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlSocketPreflight {
    name: &'static str,
    configured: bool,
    exists: bool,
    path: Option<String>,
}

impl ControlSocketPreflight {
    fn json_value(&self) -> serde_json::Value {
        json!({
            "name": self.name,
            "configured": self.configured,
            "exists": self.exists,
            "path": self.path,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BootPreflightSnapshot {
    repo_exists: bool,
    worktree_exists: bool,
    git_dir_exists: bool,
    branch_freshness: BranchFreshness,
    trust_gate_allowed: Option<bool>,
    trusted_roots_count: usize,
    required_binaries: Vec<BinaryPreflight>,
    control_sockets: Vec<ControlSocketPreflight>,
    mcp_startup_eligible: bool,
    mcp_servers_configured: usize,
    plugin_startup_eligible: bool,
    plugins_configured: usize,
    last_failed_boot_reason: Option<String>,
}

impl BootPreflightSnapshot {
    fn json_value(&self) -> serde_json::Value {
        json!({
            "repo": {
                "exists": self.repo_exists,
                "worktree_exists": self.worktree_exists,
                "git_dir_exists": self.git_dir_exists,
            },
            "branch_freshness": self.branch_freshness.json_value(),
            "trust_gate": {
                "allowlisted": self.trust_gate_allowed,
                "trusted_roots_count": self.trusted_roots_count,
            },
            "required_binaries": self.required_binaries.iter().map(BinaryPreflight::json_value).collect::<Vec<_>>(),
            "control_sockets": self.control_sockets.iter().map(ControlSocketPreflight::json_value).collect::<Vec<_>>(),
            "mcp_startup": {
                "eligible": self.mcp_startup_eligible,
                "servers_configured": self.mcp_servers_configured,
            },
            "plugin_startup": {
                "eligible": self.plugin_startup_eligible,
                "plugins_configured": self.plugins_configured,
            },
            "last_failed_boot_reason": self.last_failed_boot_reason,
        })
    }

    fn summary(&self) -> String {
        let trust = self
            .trust_gate_allowed
            .map(|value| {
                if value {
                    "allowlisted"
                } else {
                    "not allowlisted"
                }
            })
            .unwrap_or("unknown");
        let freshness = self
            .branch_freshness
            .fresh
            .map(|fresh| if fresh { "fresh" } else { "behind" })
            .unwrap_or("no upstream");
        format!(
            "repo={} worktree={} branch={} trust={} mcp={} plugins={} last_failed={}",
            self.repo_exists,
            self.worktree_exists,
            freshness,
            trust,
            self.mcp_startup_eligible,
            self.plugin_startup_eligible,
            self.last_failed_boot_reason.as_deref().unwrap_or("none")
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct StatusUsage {
    message_count: usize,
    turns: u32,
    latest: TokenUsage,
    cumulative: TokenUsage,
    estimated_tokens: usize,
}

#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GitWorkspaceSummary {
    changed_files: usize,
    staged_files: usize,
    unstaged_files: usize,
    untracked_files: usize,
    conflicted_files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLifecycleKind {
    RunningProcess,
    IdleShell,
    SavedOnly,
}

impl SessionLifecycleKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::RunningProcess => "running_process",
            Self::IdleShell => "idle_shell",
            Self::SavedOnly => "saved_only",
        }
    }

    fn human_label(self) -> &'static str {
        match self {
            Self::RunningProcess => "running process",
            Self::IdleShell => "idle shell",
            Self::SavedOnly => "saved only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionLifecycleSummary {
    kind: SessionLifecycleKind,
    pane_id: Option<String>,
    pane_command: Option<String>,
    pane_path: Option<PathBuf>,
    workspace_dirty: bool,
    abandoned: bool,
}

impl SessionLifecycleSummary {
    fn signal(&self) -> String {
        let mut parts = vec![self.kind.human_label().to_string()];
        if self.workspace_dirty {
            parts.push("dirty worktree".to_string());
        }
        if self.abandoned {
            parts.push("abandoned?".to_string());
        }
        if let Some(command) = self.pane_command.as_deref() {
            parts.push(format!("cmd={command}"));
        }
        parts.join(" · ")
    }

    fn json_value(&self) -> serde_json::Value {
        json!({
            "kind": self.kind.as_str(),
            "pane_id": self.pane_id,
            "pane_command": self.pane_command,
            "pane_path": self.pane_path.as_ref().map(|path| path.display().to_string()),
            "workspace_dirty": self.workspace_dirty,
            "abandoned": self.abandoned,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TmuxPaneSnapshot {
    pane_id: String,
    current_command: String,
    current_path: PathBuf,
}

impl GitWorkspaceSummary {
    fn is_clean(self) -> bool {
        self.changed_files == 0
    }

    fn headline(self) -> String {
        if self.is_clean() {
            "clean".to_string()
        } else {
            let mut details = Vec::new();
            if self.staged_files > 0 {
                details.push(format!("{} staged", self.staged_files));
            }
            if self.unstaged_files > 0 {
                details.push(format!("{} unstaged", self.unstaged_files));
            }
            if self.untracked_files > 0 {
                details.push(format!("{} untracked", self.untracked_files));
            }
            if self.conflicted_files > 0 {
                details.push(format!("{} conflicted", self.conflicted_files));
            }
            format!(
                "dirty · {} files · {}",
                self.changed_files,
                details.join(", ")
            )
        }
    }
}

fn classify_session_lifecycle_for(workspace: &Path) -> SessionLifecycleSummary {
    classify_session_lifecycle_from_panes(workspace, discover_tmux_panes())
}

fn classify_session_lifecycle_from_panes(
    workspace: &Path,
    panes: Vec<TmuxPaneSnapshot>,
) -> SessionLifecycleSummary {
    let workspace_dirty = git_worktree_is_dirty(workspace);
    let mut idle_shell = None;
    for pane in panes {
        if !pane_path_matches_workspace(&pane.current_path, workspace) {
            continue;
        }
        if is_idle_shell_command(&pane.current_command) {
            idle_shell.get_or_insert(pane);
        } else {
            return SessionLifecycleSummary {
                kind: SessionLifecycleKind::RunningProcess,
                pane_id: Some(pane.pane_id),
                pane_command: Some(pane.current_command),
                pane_path: Some(pane.current_path),
                workspace_dirty,
                abandoned: false,
            };
        }
    }

    if let Some(pane) = idle_shell {
        SessionLifecycleSummary {
            kind: SessionLifecycleKind::IdleShell,
            pane_id: Some(pane.pane_id),
            pane_command: Some(pane.current_command),
            pane_path: Some(pane.current_path),
            workspace_dirty,
            abandoned: workspace_dirty,
        }
    } else {
        SessionLifecycleSummary {
            kind: SessionLifecycleKind::SavedOnly,
            pane_id: None,
            pane_command: None,
            pane_path: None,
            workspace_dirty,
            abandoned: workspace_dirty,
        }
    }
}

fn discover_tmux_panes() -> Vec<TmuxPaneSnapshot> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{pane_current_command}\t#{pane_current_path}",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_tmux_pane_snapshots(&stdout)
}

fn parse_tmux_pane_snapshots(output: &str) -> Vec<TmuxPaneSnapshot> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let pane_id = fields.next()?.trim();
            let current_command = fields.next()?.trim();
            let current_path = fields.next()?.trim();
            if pane_id.is_empty() || current_path.is_empty() {
                return None;
            }
            Some(TmuxPaneSnapshot {
                pane_id: pane_id.to_string(),
                current_command: current_command.to_string(),
                current_path: PathBuf::from(current_path),
            })
        })
        .collect()
}

fn pane_path_matches_workspace(pane_path: &Path, workspace: &Path) -> bool {
    if pane_path == workspace || pane_path.starts_with(workspace) {
        return true;
    }
    let pane_path = fs::canonicalize(pane_path).unwrap_or_else(|_| pane_path.to_path_buf());
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    pane_path == workspace || pane_path.starts_with(&workspace)
}

fn is_idle_shell_command(command: &str) -> bool {
    let command = command.rsplit('/').next().unwrap_or(command);
    matches!(
        command,
        "bash" | "zsh" | "sh" | "fish" | "nu" | "pwsh" | "powershell" | "cmd"
    )
}

fn git_worktree_is_dirty(workspace: &Path) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain"])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty())
}

#[cfg(test)]
fn format_unknown_slash_command_message(name: &str) -> String {
    let suggestions = suggest_slash_commands(name);
    let mut message = format!("unknown slash command: /{name}.");
    if !suggestions.is_empty() {
        message.push_str(" Did you mean ");
        message.push_str(&suggestions.join(", "));
        message.push('?');
    }
    if let Some(note) = omc_compatibility_note_for_unknown_slash_command(name) {
        message.push(' ');
        message.push_str(note);
    }
    message.push_str(" Use /help to list available commands.");
    message
}

fn format_model_report(model: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Model
  Current model    {model}
  Session messages {message_count}
  Session turns    {turns}

Usage
  Inspect current model with /model
  Switch models with /model <name>"
    )
}

fn format_model_switch_report(previous: &str, next: &str, message_count: usize) -> String {
    format!(
        "Model updated
  Previous         {previous}
  Current          {next}
  Preserved msgs   {message_count}"
    )
}

fn format_permissions_report(mode: &str) -> String {
    let modes = [
        ("read-only", "Read/search tools only", mode == "read-only"),
        (
            "workspace-write",
            "Edit files inside the workspace",
            mode == "workspace-write",
        ),
        (
            "danger-full-access",
            "Unrestricted tool access",
            mode == "danger-full-access",
        ),
    ]
    .into_iter()
    .map(|(name, description, is_current)| {
        let marker = if is_current {
            "● current"
        } else {
            "○ available"
        };
        format!("  {name:<18} {marker:<11} {description}")
    })
    .collect::<Vec<_>>()
    .join(
        "
",
    );

    format!(
        "Permissions
  Active mode      {mode}
  Mode status      live session default

Modes
{modes}

Usage
  Inspect current mode with /permissions
  Switch modes with /permissions <mode>"
    )
}

fn format_permissions_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Permissions updated
  Result           mode switched
  Previous mode    {previous}
  Active mode      {next}
  Applies to       subsequent tool calls
  Usage            /permissions to inspect current mode"
    )
}

fn format_cost_report(usage: TokenUsage) -> String {
    let estimated_cost = usage.estimate_cost_usd();
    format!(
        "Cost
  Input tokens     {}
  Output tokens    {}
  Cache create     {}
  Cache read       {}
  Total tokens     {}
  Estimated cost   {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        usage.total_tokens(),
        format_usd(estimated_cost.total_cost_usd()),
    )
}

fn format_resume_report(session_path: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Session resumed
  Session file     {session_path}
  Messages         {message_count}
  Turns            {turns}"
    )
}

fn render_resume_usage() -> String {
    format!(
        "Resume
  Usage            /resume <session-path|session-id|{LATEST_SESSION_REFERENCE}>
  Auto-save        .claw/sessions/<workspace-fingerprint>/<session-id>.{PRIMARY_SESSION_EXTENSION}
  Tip              use /session list to inspect saved sessions"
    )
}

fn format_compact_report(removed: usize, resulting_messages: usize, skipped: bool) -> String {
    if skipped {
        format!(
            "Compact
  Result           skipped
  Reason           session below compaction threshold
  Messages kept    {resulting_messages}"
        )
    } else {
        format!(
            "Compact
  Result           compacted
  Messages removed {removed}
  Messages kept    {resulting_messages}"
        )
    }
}

fn format_auto_compaction_notice(removed: usize) -> String {
    format!("[auto-compacted: removed {removed} messages]")
}

fn parse_git_status_metadata(status: Option<&str>) -> (Option<PathBuf>, Option<String>) {
    parse_git_status_metadata_for(
        &env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        status,
    )
}

fn parse_git_status_branch(status: Option<&str>) -> Option<String> {
    let status = status?;
    let first_line = status.lines().next()?;
    let line = first_line.strip_prefix("## ")?;
    if line.starts_with("HEAD") {
        return Some("detached HEAD".to_string());
    }
    let branch = line.split(['.', ' ']).next().unwrap_or_default().trim();
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

fn parse_git_workspace_summary(status: Option<&str>) -> GitWorkspaceSummary {
    let mut summary = GitWorkspaceSummary::default();
    let Some(status) = status else {
        return summary;
    };

    for line in status.lines() {
        if line.starts_with("## ") || line.trim().is_empty() {
            continue;
        }

        summary.changed_files += 1;
        let mut chars = line.chars();
        let index_status = chars.next().unwrap_or(' ');
        let worktree_status = chars.next().unwrap_or(' ');

        if index_status == '?' && worktree_status == '?' {
            summary.untracked_files += 1;
            continue;
        }

        if index_status != ' ' {
            summary.staged_files += 1;
        }
        if worktree_status != ' ' {
            summary.unstaged_files += 1;
        }
        if (matches!(index_status, 'U' | 'A') && matches!(worktree_status, 'U' | 'A'))
            || index_status == 'U'
            || worktree_status == 'U'
        {
            summary.conflicted_files += 1;
        }
    }

    summary
}

fn build_boot_preflight_snapshot(
    cwd: &Path,
    project_root: Option<&Path>,
    git_status: Option<&str>,
    runtime_config: Option<&runtime::RuntimeConfig>,
    config_load_error: Option<&str>,
) -> BootPreflightSnapshot {
    let branch_freshness = BranchFreshness::from_git_status(git_status);
    let worktree_exists = run_git_bool(cwd, &["rev-parse", "--is-inside-work-tree"]);
    let git_dir_exists = run_git_capture_in(cwd, &["rev-parse", "--git-dir"])
        .map(|path| {
            let path = PathBuf::from(path.trim());
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .is_some_and(|path| path.exists());
    let trusted_roots = runtime_config
        .map(runtime::RuntimeConfig::trusted_roots)
        .unwrap_or(&[]);
    let trust_gate_allowed = runtime_config.map(|_| {
        trusted_roots
            .iter()
            .any(|root| path_matches_trusted_root_local(cwd, root))
    });
    let plugin_configured = runtime_config
        .map(|config| config.plugins().enabled_plugins().len())
        .unwrap_or_default();
    let mcp_configured = runtime_config
        .map(|config| config.mcp().servers().len())
        .unwrap_or_default();
    let config_ok = config_load_error.is_none();
    BootPreflightSnapshot {
        repo_exists: project_root.is_some_and(Path::exists),
        worktree_exists,
        git_dir_exists,
        branch_freshness,
        trust_gate_allowed,
        trusted_roots_count: trusted_roots.len(),
        required_binaries: vec![
            BinaryPreflight {
                name: "claw",
                available: env::current_exe().is_ok_and(|path| path.exists()),
            },
            BinaryPreflight {
                name: "git",
                available: command_available("git"),
            },
            BinaryPreflight {
                name: "tmux",
                available: command_available("tmux"),
            },
        ],
        control_sockets: vec![tmux_control_socket_preflight()],
        mcp_startup_eligible: config_ok,
        mcp_servers_configured: mcp_configured,
        plugin_startup_eligible: config_ok,
        plugins_configured: plugin_configured,
        last_failed_boot_reason: last_failed_boot_reason(cwd),
    }
}

fn run_git_bool(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn tmux_control_socket_preflight() -> ControlSocketPreflight {
    let path = env::var("TMUX")
        .ok()
        .and_then(|value| value.split(',').next().map(str::to_string))
        .filter(|value| !value.is_empty());
    let exists = path.as_ref().is_some_and(|path| Path::new(path).exists());
    ControlSocketPreflight {
        name: "tmux",
        configured: path.is_some(),
        exists,
        path,
    }
}

fn last_failed_boot_reason(cwd: &Path) -> Option<String> {
    env::var("CLAW_LAST_FAILED_BOOT_REASON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            fs::read_to_string(cwd.join(".claw").join("last-failed-boot.txt"))
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn path_matches_trusted_root_local(cwd: &Path, trusted_root: &str) -> bool {
    let cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let trusted_root = Path::new(trusted_root);
    let trusted_root = if trusted_root.is_absolute() {
        trusted_root.to_path_buf()
    } else {
        cwd.join(trusted_root)
    };
    let trusted_root = fs::canonicalize(&trusted_root).unwrap_or(trusted_root);
    cwd == trusted_root || cwd.starts_with(trusted_root)
}

fn resolve_git_branch_for(cwd: &Path) -> Option<String> {
    let branch = run_git_capture_in(cwd, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if !branch.is_empty() {
        return Some(branch.to_string());
    }

    let fallback = run_git_capture_in(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let fallback = fallback.trim();
    if fallback.is_empty() {
        None
    } else if fallback == "HEAD" {
        Some("detached HEAD".to_string())
    } else {
        Some(fallback.to_string())
    }
}

fn run_git_capture_in(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn find_git_root_in(cwd: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        return Err("not a git repository".into());
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    if path.is_empty() {
        return Err("empty git root".into());
    }
    Ok(PathBuf::from(path))
}

fn parse_git_status_metadata_for(
    cwd: &Path,
    status: Option<&str>,
) -> (Option<PathBuf>, Option<String>) {
    let branch = resolve_git_branch_for(cwd).or_else(|| parse_git_status_branch(status));
    let project_root = find_git_root_in(cwd).ok();
    (project_root, branch)
}

#[allow(clippy::too_many_lines)]
fn run_resume_command(
    session_path: &Path,
    session: &Session,
    command: &SlashCommand,
) -> Result<ResumeCommandOutcome, Box<dyn std::error::Error>> {
    let session_list_outcome = || -> Result<ResumeCommandOutcome, Box<dyn std::error::Error>> {
        let sessions = list_managed_sessions().unwrap_or_default();
        let session_ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
        let session_details: Vec<serde_json::Value> = sessions
            .iter()
            .map(|session| {
                serde_json::json!({
                    "id": session.id,
                    "path": session.path.display().to_string(),
                    "message_count": session.message_count,
                    "updated_at_ms": session.updated_at_ms,
                    "lifecycle": session.lifecycle.json_value(),
                })
            })
            .collect();
        let active_id = session.session_id.clone();
        let text = render_session_list(&active_id).unwrap_or_else(|e| format!("error: {e}"));
        Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(text),
            json: Some(serde_json::json!({
                "kind": "session_list",
                "sessions": session_ids,
                "session_details": session_details,
                "active": active_id,
            })),
        })
    };

    match command {
        SlashCommand::Help => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_repl_help()),
            json: Some(serde_json::json!({ "kind": "help", "text": render_repl_help() })),
        }),
        SlashCommand::Compact => {
            let result = runtime::compact_session(
                session,
                CompactionConfig {
                    max_estimated_tokens: 0,
                    ..CompactionConfig::default()
                },
            );
            let removed = result.removed_message_count;
            let kept = result.compacted_session.messages.len();
            let skipped = removed == 0;
            result.compacted_session.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: result.compacted_session,
                message: Some(format_compact_report(removed, kept, skipped)),
                json: Some(serde_json::json!({
                    "kind": "compact",
                    "skipped": skipped,
                    "removed_messages": removed,
                    "kept_messages": kept,
                })),
            })
        }
        SlashCommand::Clear { confirm } => {
            if !confirm {
                return Ok(ResumeCommandOutcome {
                    session: session.clone(),
                    message: Some(
                        "clear: confirmation required; rerun with /clear --confirm".to_string(),
                    ),
                    json: Some(serde_json::json!({
                        "kind": "error",
                        "error": "confirmation required",
                        "hint": "rerun with /clear --confirm",
                    })),
                });
            }
            let backup_path = write_session_clear_backup(session, session_path)?;
            let previous_session_id = session.session_id.clone();
            let cleared = new_cli_session()?;
            let new_session_id = cleared.session_id.clone();
            cleared.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: cleared,
                message: Some(format!(
                    "Session cleared\n  Mode             resumed session reset\n  Previous session {previous_session_id}\n  Backup           {}\n  Resume previous  claw --resume {}\n  New session      {new_session_id}\n  Session file     {}",
                    backup_path.display(),
                    backup_path.display(),
                    session_path.display()
                )),
                json: Some(serde_json::json!({
                    "kind": "clear",
                    "previous_session_id": previous_session_id,
                    "new_session_id": new_session_id,
                    "backup": backup_path.display().to_string(),
                    "session_file": session_path.display().to_string(),
                })),
            })
        }
        SlashCommand::Status => {
            let tracker = UsageTracker::from_session(session);
            let usage = tracker.cumulative_usage();
            let context = status_context(Some(session_path))?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_status_report(
                    session.model.as_deref().unwrap_or("restored-session"),
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &context,
                    None, // #148: resumed sessions don't have flag provenance
                )),
                json: Some(status_json_value(
                    session.model.as_deref(),
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &context,
                    None, // #148: resumed sessions don't have flag provenance
                    None,
                )),
            })
        }
        SlashCommand::Sandbox => {
            let cwd = env::current_dir()?;
            let loader = ConfigLoader::default_for(&cwd);
            let runtime_config = loader.load()?;
            let status = resolve_sandbox_status(runtime_config.sandbox(), &cwd);
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_sandbox_report(&status)),
                json: Some(sandbox_json_value(&status)),
            })
        }
        SlashCommand::Cost => {
            let usage = UsageTracker::from_session(session).cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_cost_report(usage)),
                json: Some(serde_json::json!({
                    "kind": "cost",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "total_tokens": usage.total_tokens(),
                    "estimated_cost_usd": format_usd(usage.estimate_cost_usd().total_cost_usd()),
                    "pricing": "estimated-default",
                })),
            })
        }
        SlashCommand::Config { section } => {
            let message = render_config_report(section.as_deref())?;
            let json = render_config_json(section.as_deref())?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json),
            })
        }
        SlashCommand::Mcp { action, target } => {
            let cwd = env::current_dir()?;
            let args = match (action.as_deref(), target.as_deref()) {
                (None, None) => None,
                (Some(action), None) => Some(action.to_string()),
                (Some(action), Some(target)) => Some(format!("{action} {target}")),
                (None, Some(target)) => Some(target.to_string()),
            };
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_mcp_slash_command(args.as_deref(), &cwd)?),
                json: Some(handle_mcp_slash_command_json(args.as_deref(), &cwd)?),
            })
        }
        SlashCommand::Memory => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_memory_report()?),
            json: Some(render_memory_json()?),
        }),
        SlashCommand::Init => {
            // #142: run the init once, then render both text + structured JSON
            // from the same InitReport so both surfaces stay in sync.
            let cwd = env::current_dir()?;
            let report = crate::init::initialize_repo(&cwd)?;
            let message = report.render();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message.clone()),
                json: Some(init_json_value(&report, &message)),
            })
        }
        SlashCommand::Diff => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let message = render_diff_report_for(&cwd)?;
            let json = render_diff_json_for(&cwd)?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json),
            })
        }
        SlashCommand::Version => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_version_report()),
            json: Some(version_json_value()),
        }),
        SlashCommand::Export { path } => {
            let export_path = resolve_export_path(path.as_deref(), session)?;
            fs::write(&export_path, render_export_text(session))?;
            let msg_count = session.messages.len();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
                    export_path.display(),
                    msg_count,
                )),
                json: Some(serde_json::json!({
                    "kind": "export",
                    "file": export_path.display().to_string(),
                    "message_count": msg_count,
                })),
            })
        }
        SlashCommand::Agents { args } => {
            let cwd = env::current_dir()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_agents_slash_command(args.as_deref(), &cwd)?),
                json: Some(
                    serde_json::to_value(handle_agents_slash_command_json(args.as_deref(), &cwd)?)
                        .unwrap_or(Value::Null),
                ),
            })
        }
        SlashCommand::Skills { args } => {
            if let SkillSlashDispatch::Invoke(_) = classify_skills_slash_command(args.as_deref()) {
                return Err(
                    "resumed /skills invocations are interactive-only; start `claw` and run `/skills <skill>` in the REPL".into(),
                );
            }
            let cwd = env::current_dir()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_skills_slash_command(args.as_deref(), &cwd)?),
                json: Some(handle_skills_slash_command_json(args.as_deref(), &cwd)?),
            })
        }
        SlashCommand::Plugins { action, target } => {
            // Only list is supported in resume mode (no runtime to reload)
            match action.as_deref() {
                Some("install") | Some("uninstall") | Some("enable") | Some("disable")
                | Some("update") => {
                    return Err(
                        "resumed /plugins mutations are interactive-only; start `claw` and run `/plugins` in the REPL".into(),
                    );
                }
                _ => {}
            }
            let cwd = env::current_dir()?;
            let payload = plugins_command_payload_for(&cwd, action.as_deref(), target.as_deref())?;
            let action_str = action.as_deref().unwrap_or("list");
            let json = serde_json::json!({
                "kind": "plugin",
                "action": action_str,
                "target": target,
                "status": payload.status,
                "config_load_error": payload.config_load_error,
                "message": &payload.message,
                "reload_runtime": payload.reload_runtime,
                "plugins": payload.plugins,
                "load_failures": payload.load_failures,
            });
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(payload.message),
                json: Some(json),
            })
        }
        SlashCommand::Doctor => {
            let report = render_doctor_report()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(report.render()),
                json: Some(report.json_value()),
            })
        }
        SlashCommand::Stats => {
            let usage = UsageTracker::from_session(session).cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_cost_report(usage)),
                json: Some(serde_json::json!({
                    "kind": "stats",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "total_tokens": usage.total_tokens(),
                    "estimated_cost_usd": format_usd(usage.estimate_cost_usd().total_cost_usd()),
                    "pricing": "estimated-default",
                })),
            })
        }
        SlashCommand::History { count } => {
            let limit = parse_history_count(count.as_deref())
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            let entries = collect_session_prompt_history(session);
            let shown: Vec<_> = entries.iter().rev().take(limit).rev().collect();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_prompt_history_report(&entries, limit)),
                json: Some(serde_json::json!({
                    "kind": "history",
                    "total": entries.len(),
                    "showing": shown.len(),
                    "entries": shown.iter().map(|e| serde_json::json!({
                        "timestamp_ms": e.timestamp_ms,
                        "text": e.text,
                    })).collect::<Vec<_>>(),
                })),
            })
        }
        SlashCommand::Poor { action } => {
            // Tier S #3 穷鬼模式：在 resume 模式下也可查询/切换。切换仅影响
            // 运行时全局 AtomicBool，不写回 settings.json。
            let (new_state, message) = handle_poor_mode_action(action.as_deref());
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(serde_json::json!({
                    "kind": "poor",
                    "active": new_state,
                })),
            })
        }
        SlashCommand::Goal { args } => {
            // Tier S #1 Goal 持续驱动：resume 模式下查询/管理 goal。
            // 从 <cwd>/.claw/goal.json 加载临时 manager 处理，不写回文件（resume 模式只读）。
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let mut manager = runtime::GoalManager::load(runtime::goal_json_path(&cwd));
            let message = handle_goal_command(&mut manager, args.as_deref());
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(serde_json::json!({
                    "kind": "goal",
                    "has_active": manager.active().is_some(),
                })),
            })
        }
        SlashCommand::Bg { args } => {
            // Tier S #2 后台会话：resume 模式下也可查询/管理后台进程。
            // 通过文件系统通信，不需要 LiveCli 实例。
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let (message, json_value) = handle_bg_command(args.as_deref(), &cwd);
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json_value),
            })
        }
        SlashCommand::Unknown(name) => Err(format_unknown_slash_command(name).into()),
        // /session list/exists/delete can be served from the managed sessions directory
        // in resume mode without starting an interactive REPL. Mutating delete remains
        // opt-in through /session delete <id> --force so JSON callers never hang on a prompt.
        SlashCommand::Session { action, target } => {
            run_resumed_session_command(session_path, session, action.as_deref(), target.as_deref())
        }
        SlashCommand::Bughunter { .. }
        | SlashCommand::Commit { .. }
        | SlashCommand::Pr { .. }
        | SlashCommand::Issue { .. }
        | SlashCommand::Ultraplan { .. }
        | SlashCommand::Teleport { .. }
        | SlashCommand::DebugToolCall { .. }
        | SlashCommand::Resume { .. }
        | SlashCommand::Model { .. }
        | SlashCommand::Permissions { .. }
        | SlashCommand::Login
        | SlashCommand::Logout
        | SlashCommand::Vim
        | SlashCommand::Upgrade
        | SlashCommand::Share
        | SlashCommand::Feedback
        | SlashCommand::Files
        | SlashCommand::Fast
        | SlashCommand::Exit
        | SlashCommand::Summary
        | SlashCommand::Desktop
        | SlashCommand::Brief
        | SlashCommand::Advisor
        | SlashCommand::Stickers
        | SlashCommand::Insights
        | SlashCommand::Thinkback
        | SlashCommand::ReleaseNotes
        | SlashCommand::SecurityReview
        | SlashCommand::Keybindings
        | SlashCommand::PrivacySettings
        | SlashCommand::Plan { .. }
        | SlashCommand::Review { .. }
        | SlashCommand::Tasks { .. }
        | SlashCommand::Theme { .. }
        | SlashCommand::Voice { .. }
        | SlashCommand::Usage { .. }
        | SlashCommand::Rename { .. }
        | SlashCommand::Copy { .. }
        | SlashCommand::Hooks { .. }
        | SlashCommand::Context { .. }
        | SlashCommand::Color { .. }
        | SlashCommand::Effort { .. }
        | SlashCommand::Branch { .. }
        | SlashCommand::Rewind { .. }
        | SlashCommand::Ide { .. }
        | SlashCommand::Tag { .. }
        | SlashCommand::OutputStyle { .. }
        | SlashCommand::AddDir { .. } => Err("unsupported resumed slash command".into()),
    }
}

/// Detect if the current working directory is "broad" (home directory or
/// filesystem root). Returns the cwd path if broad, None otherwise.
fn detect_broad_cwd() -> Option<PathBuf> {
    let Ok(cwd) = env::current_dir() else {
        return None;
    };
    let is_home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .is_some_and(|h| Path::new(&h) == cwd);
    let is_root = cwd.parent().is_none();
    if is_home || is_root {
        Some(cwd)
    } else {
        None
    }
}

/// Enforce the broad-CWD policy: when running from home or root, either
/// require the --allow-broad-cwd flag, or prompt for confirmation (interactive),
/// or exit with an error (non-interactive).
fn enforce_broad_cwd_policy(
    allow_broad_cwd: bool,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    if allow_broad_cwd {
        return Ok(());
    }
    let Some(cwd) = detect_broad_cwd() else {
        return Ok(());
    };

    let is_interactive = io::stdin().is_terminal();

    if is_interactive {
        // Interactive mode: print warning and ask for confirmation.
        // 用 print!/stdout 而非 eprint!/stderr：Windows Terminal 在某些配置下
        // 对 stderr 的行缓冲策略不同，可能导致提示符不显示，用户看到"卡住"。
        // stdout + 显式 flush 是最可靠的方式。
        println!(
            "Warning: claw is running from a very broad directory ({}).\n\
             The agent can read and search everything under this path.\n\
             Consider running from inside your project: cd /path/to/project && claw",
            cwd.display()
        );
        print!("Continue anyway? [y/N]: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            println!("Aborted.");
            std::process::exit(0);
        }
        Ok(())
    } else {
        // Non-interactive mode: exit with error (JSON or text)
        let message = format!(
            "claw is running from a very broad directory ({}). \
             The agent can read and search everything under this path. \
             Use --allow-broad-cwd to proceed anyway, \
             or run from inside your project: cd /path/to/project && claw",
            cwd.display()
        );
        match output_format {
            CliOutputFormat::Json => {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "type": "error",
                        "error": message,
                    })
                );
            }
            CliOutputFormat::Text => {
                eprintln!("error: {message}");
            }
        }
        std::process::exit(1);
    }
}

fn stale_base_state_for(cwd: &Path, flag_value: Option<&str>) -> BaseCommitState {
    let source = resolve_expected_base(flag_value, cwd);
    check_base_commit(cwd, source.as_ref())
}

fn stale_base_json_value(state: &BaseCommitState) -> serde_json::Value {
    match state {
        BaseCommitState::Matches => json!({"status": "matches", "fresh": true}),
        BaseCommitState::Diverged { expected, actual } => json!({
            "status": "diverged",
            "fresh": false,
            "expected": expected,
            "actual": actual,
        }),
        BaseCommitState::NoExpectedBase => json!({"status": "no_expected_base", "fresh": null}),
        BaseCommitState::NotAGitRepo => json!({"status": "not_git_repo", "fresh": null}),
    }
}

fn run_stale_base_preflight(flag_value: Option<&str>) {
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let state = stale_base_state_for(&cwd, flag_value);
    if let Some(warning) = format_stale_base_warning(&state) {
        eprintln!("{warning}");
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    allow_broad_cwd: bool,
    additional_workspace_roots: Vec<PathBuf>,
    output_verbosity: OutputVerbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    enforce_broad_cwd_policy(allow_broad_cwd, CliOutputFormat::Text)?;
    run_stale_base_preflight(base_commit.as_deref());
    // P3 多行粘贴折叠：本会话的 paste id 自增生成器。
    // 每次超阈值粘贴时 +1，用于 paste-cache 文件名和占位符编号。
    let mut paste_id_gen: u32 = 0;
    // P3 自动剪贴板检测：当检测到右键粘贴多行内容时，把剩余行存入此 Vec。
    // conhost 右键粘贴会逐行发送，第一行 Submit 后，后续行会快速触发 Submit。
    // claw 用此 Vec 丢弃后续行（与剪贴板剩余行匹配）。
    let mut pending_paste_lines: Vec<String> = Vec::new();
    let resolved_model = resolve_repl_model(model);
    let t0 = std::time::Instant::now();
    let mut cli = LiveCli::new(
        resolved_model,
        true,
        allowed_tools,
        permission_mode,
        additional_workspace_roots,
        output_verbosity,
    )?;
    let t_cli = t0.elapsed();
    cli.set_reasoning_effort(reasoning_effort);
    let mut editor =
        input::LineEditor::new("> ", cli.repl_completion_candidates().unwrap_or_default());
    let t_editor = t0.elapsed();
    println!("{}", cli.startup_banner());
    let t_banner = t0.elapsed();
    println!("{}", format_connected_line(&cli.model));
    let t_total = t0.elapsed();
    eprintln!(
        "[timing] LiveCli::new={:?} editor={:?} banner={:?} total={:?}",
        t_cli, t_editor, t_banner, t_total
    );

    loop {
        editor.set_completions(cli.repl_completion_candidates().unwrap_or_default());
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed.as_str(), "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                // P3 自动剪贴板检测：如果 pending_paste_lines 非空，说明上一次检测到
                // 右键粘贴多行内容，此行是 conhost 逐行发送的后续行之一。
                // 如果匹配 pending_paste_lines 的第一行，丢弃此行；否则清空并正常处理。
                if !pending_paste_lines.is_empty() {
                    if pending_paste_lines[0] == trimmed {
                        pending_paste_lines.remove(0);
                        continue;
                    } else {
                        // 不匹配，说明用户开始输入新内容，清空 pending 列表
                        pending_paste_lines.clear();
                    }
                }
                // P3 多行粘贴模式：/paste 直接读取 Windows 剪贴板内容。
                // 绕过终端粘贴机制（conhost 右键粘贴会逐行触发 AcceptLine，无法可靠收集多行）。
                // 用户先 Ctrl+C 复制内容到剪贴板，然后在 claw 里输入 /paste。
                // claw 用 PowerShell Get-Clipboard 读取剪贴板原始文本。
                if trimmed == "/paste" {
                    println!("\x1b[2m--- 从剪贴板读取 ---\x1b[0m");
                    let pasted = match read_clipboard_text() {
                        Ok(text) if !text.trim().is_empty() => text,
                        Ok(_) => {
                            println!("\x1b[2m（剪贴板为空，请先 Ctrl+C 复制内容）\x1b[0m");
                            continue;
                        }
                        Err(error) => {
                            eprintln!("\x1b[31m读取剪贴板失败: {error}\x1b[0m");
                            continue;
                        }
                    };
                    let (display, expanded) =
                        fold_pasted_input(&pasted, &cli.session.id, &mut paste_id_gen);
                    editor.push_history(&display);
                    cli.record_prompt_history(&display);
                    print_user_card(&display);
                    cli.run_turn(&expanded)?;
                    continue;
                }
                match SlashCommand::parse(&trimmed) {
                    Ok(Some(command)) => {
                        if cli.handle_repl_command(command)? {
                            cli.persist_session()?;
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("{error}");
                        continue;
                    }
                }
                // P3 自动剪贴板检测：如果用户输入是单行（conhost 右键粘贴的第一行），
                // 且不以 / 开头（slash 命令不检测），检查剪贴板是否有多行内容。
                // 如果剪贴板第一行等于用户输入，则用剪贴板完整内容替换，并设置
                // pending_paste_lines 以丢弃后续被 conhost 逐行发送的行。
                let (display, expanded) = if !trimmed.contains('\n')
                    && !trimmed.starts_with('/')
                    && pending_paste_lines.is_empty()
                {
                    try_auto_expand_clipboard(&trimmed, &cli.session.id, &mut paste_id_gen, &mut pending_paste_lines)
                        .unwrap_or_else(|| {
                            fold_pasted_input(&trimmed, &cli.session.id, &mut paste_id_gen)
                        })
                } else {
                    fold_pasted_input(&trimmed, &cli.session.id, &mut paste_id_gen)
                };
                // Bare-word skill dispatch: if the first token of the input
                // matches a known skill name, invoke it as `/skills <input>`
                // rather than forwarding raw text to the LLM (ROADMAP #36).
                let cwd = std::env::current_dir().unwrap_or_default();
                if let Some(prompt) = try_resolve_bare_skill_prompt(&cwd, &display) {
                    editor.push_history(&display);
                    cli.record_prompt_history(&display);
                    // 卡片显示用户原始输入 display（而非 skill 展开后的 prompt）
                    print_user_card(&display);
                    cli.run_turn(&prompt)?;
                    continue;
                }
                editor.push_history(&display);
                cli.record_prompt_history(&display);
                print_user_card(&display);
                cli.run_turn(&expanded)?;
            }
            input::ReadOutcome::Cancel => {}
            input::ReadOutcome::Exit => {
                cli.persist_session()?;
                break;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SessionHandle {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct ManagedSessionSummary {
    id: String,
    path: PathBuf,
    updated_at_ms: u64,
    modified_epoch_millis: u128,
    message_count: usize,
    parent_session_id: Option<String>,
    branch_name: Option<String>,
    lifecycle: SessionLifecycleSummary,
}

struct LiveCli {
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    system_prompt: Vec<String>,
    runtime: BuiltRuntime,
    session: SessionHandle,
    prompt_history: Vec<PromptHistoryEntry>,
    output_verbosity: OutputVerbosity,
    // P2 富状态栏：累计本会话所有回合的 token usage，用于状态栏持续显示 cost/token。
    // 在 run_turn / run_prompt_* 成功路径里累加 summary.usage。
    cumulative_usage: runtime::TokenUsage,
    // Tier S #1 Goal 持续驱动：外挂式 GoalManager，跨轮 prompt 注入。
    // 在 run_turn 调 runtime.run_turn 之前 prepend goal 前缀；
    // 网络错误时 pause；GoalTool 失败时 record_blocked。
    goal_manager: runtime::GoalManager,
}

#[derive(Debug, Clone)]
struct PromptHistoryEntry {
    timestamp_ms: u64,
    text: String,
}

struct RuntimePluginState {
    feature_config: runtime::RuntimeFeatureConfig,
    tool_registry: GlobalToolRegistry,
    plugin_registry: PluginRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

struct RuntimeMcpState {
    runtime: tokio::runtime::Runtime,
    manager: McpServerManager,
    pending_servers: Vec<String>,
    degraded_report: Option<runtime::McpDegradedReport>,
}

struct BuiltRuntime {
    runtime: Option<ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>>,
    plugin_registry: PluginRegistry,
    plugins_active: bool,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    mcp_active: bool,
}

impl BuiltRuntime {
    fn new(
        runtime: ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>,
        plugin_registry: PluginRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            plugin_registry,
            plugins_active: true,
            mcp_state,
            mcp_active: true,
        }
    }

    fn with_hook_abort_signal(mut self, hook_abort_signal: runtime::HookAbortSignal) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing hook abort signal");
        self.runtime = Some(runtime.with_hook_abort_signal(hook_abort_signal));
        self
    }

    fn set_tool_verbosity(&mut self, verbosity: OutputVerbosity) {
        if let Some(ref mut rt) = self.runtime {
            rt.tool_executor_mut().output_verbosity = verbosity;
        }
    }

    fn shutdown_plugins(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.plugins_active {
            self.plugin_registry.shutdown()?;
            self.plugins_active = false;
        }
        Ok(())
    }

    fn shutdown_mcp(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mcp_active {
            if let Some(mcp_state) = &self.mcp_state {
                mcp_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .shutdown()?;
            }
            self.mcp_active = false;
        }
        Ok(())
    }
}

impl Deref for BuiltRuntime {
    type Target = ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>;

    fn deref(&self) -> &Self::Target {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl DerefMut for BuiltRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.runtime
            .as_mut()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl Drop for BuiltRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_mcp();
        let _ = self.shutdown_plugins();
    }
}

#[derive(Debug, Deserialize)]
struct ToolSearchRequest {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct McpToolRequest {
    #[serde(rename = "qualifiedName")]
    qualified_name: Option<String>,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ListMcpResourcesRequest {
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadMcpResourceRequest {
    server: String,
    uri: String,
}

impl RuntimeMcpState {
    fn new(
        runtime_config: &runtime::RuntimeConfig,
    ) -> Result<Option<(Self, runtime::McpToolDiscoveryReport)>, Box<dyn std::error::Error>> {
        let mut manager = McpServerManager::from_runtime_config(runtime_config);
        if manager.server_names().is_empty() && manager.unsupported_servers().is_empty() {
            return Ok(None);
        }

        let runtime = tokio::runtime::Runtime::new()?;
        let discovery = runtime.block_on(manager.discover_tools_best_effort());
        let pending_servers = discovery
            .failed_servers
            .iter()
            .map(|failure| failure.server_name.clone())
            .chain(
                discovery
                    .unsupported_servers
                    .iter()
                    .map(|server| server.server_name.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let available_tools = discovery
            .tools
            .iter()
            .map(|tool| tool.qualified_name.clone())
            .collect::<Vec<_>>();
        let failed_server_names = pending_servers.iter().cloned().collect::<BTreeSet<_>>();
        let working_servers = manager
            .server_names()
            .into_iter()
            .filter(|server_name| !failed_server_names.contains(server_name))
            .collect::<Vec<_>>();
        let failed_servers =
            discovery
                .failed_servers
                .iter()
                .map(|failure| runtime::McpFailedServer {
                    server_name: failure.server_name.clone(),
                    phase: runtime::McpLifecyclePhase::ToolDiscovery,
                    error: runtime::McpErrorSurface::new(
                        runtime::McpLifecyclePhase::ToolDiscovery,
                        Some(failure.server_name.clone()),
                        failure.error.clone(),
                        std::collections::BTreeMap::from([(
                            "required".to_string(),
                            failure.required.to_string(),
                        )]),
                        true,
                    ),
                })
                .chain(discovery.unsupported_servers.iter().map(|server| {
                    runtime::McpFailedServer {
                        server_name: server.server_name.clone(),
                        phase: runtime::McpLifecyclePhase::ServerRegistration,
                        error: runtime::McpErrorSurface::new(
                            runtime::McpLifecyclePhase::ServerRegistration,
                            Some(server.server_name.clone()),
                            server.reason.clone(),
                            std::collections::BTreeMap::from([
                                (
                                    "transport".to_string(),
                                    format!("{:?}", server.transport).to_ascii_lowercase(),
                                ),
                                ("required".to_string(), server.required.to_string()),
                            ]),
                            false,
                        ),
                    }
                }))
                .collect::<Vec<_>>();
        let degraded_report = (!failed_servers.is_empty()).then(|| {
            runtime::McpDegradedReport::new(
                working_servers,
                failed_servers,
                available_tools.clone(),
                available_tools,
            )
        });

        Ok(Some((
            Self {
                runtime,
                manager,
                pending_servers,
                degraded_report,
            },
            discovery,
        )))
    }

    fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.block_on(self.manager.shutdown())?;
        Ok(())
    }

    fn pending_servers(&self) -> Option<Vec<String>> {
        (!self.pending_servers.is_empty()).then(|| self.pending_servers.clone())
    }

    fn degraded_report(&self) -> Option<runtime::McpDegradedReport> {
        self.degraded_report.clone()
    }

    fn server_names(&self) -> Vec<String> {
        self.manager.server_names()
    }

    fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<String, ToolError> {
        let response = self
            .runtime
            .block_on(self.manager.call_tool(qualified_tool_name, arguments))
            .map_err(|error| ToolError::new(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned JSON-RPC error: {} ({})",
                error.message, error.code
            )));
        }

        let result = response.result.ok_or_else(|| {
            ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned no result payload"
            ))
        })?;
        serde_json::to_string_pretty(&result).map_err(|error| ToolError::new(error.to_string()))
    }

    fn list_resources_for_server(&mut self, server_name: &str) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(self.manager.list_resources(server_name))
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "resources": result.resources,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn list_resources_for_all_servers(&mut self) -> Result<String, ToolError> {
        let mut resources = Vec::new();
        let mut failures = Vec::new();

        for server_name in self.server_names() {
            match self
                .runtime
                .block_on(self.manager.list_resources(&server_name))
            {
                Ok(result) => resources.push(json!({
                    "server": server_name,
                    "resources": result.resources,
                })),
                Err(error) => failures.push(json!({
                    "server": server_name,
                    "error": error.to_string(),
                })),
            }
        }

        if resources.is_empty() && !failures.is_empty() {
            let message = failures
                .iter()
                .filter_map(|failure| failure.get("error").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ToolError::new(message));
        }

        serde_json::to_string_pretty(&json!({
            "resources": resources,
            "failures": failures,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn read_resource(&mut self, server_name: &str, uri: &str) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(self.manager.read_resource(server_name, uri))
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "contents": result.contents,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }
}

fn build_runtime_mcp_state(
    runtime_config: &runtime::RuntimeConfig,
) -> Result<RuntimePluginStateBuildOutput, Box<dyn std::error::Error>> {
    let Some((mcp_state, discovery)) = RuntimeMcpState::new(runtime_config)? else {
        return Ok((None, Vec::new()));
    };

    let mut runtime_tools = discovery
        .tools
        .iter()
        .map(mcp_runtime_tool_definition)
        .collect::<Vec<_>>();
    if !mcp_state.server_names().is_empty() {
        runtime_tools.extend(mcp_wrapper_tool_definitions());
    }

    Ok((Some(Arc::new(Mutex::new(mcp_state))), runtime_tools))
}

fn mcp_runtime_tool_definition(tool: &runtime::ManagedMcpTool) -> RuntimeToolDefinition {
    RuntimeToolDefinition {
        name: tool.qualified_name.clone(),
        description: Some(
            tool.tool
                .description
                .clone()
                .unwrap_or_else(|| format!("Invoke MCP tool `{}`.", tool.qualified_name)),
        ),
        input_schema: tool
            .tool
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({ "type": "object", "additionalProperties": true })),
        required_permission: permission_mode_for_mcp_tool(&tool.tool),
    }
}

fn mcp_wrapper_tool_definitions() -> Vec<RuntimeToolDefinition> {
    vec![
        RuntimeToolDefinition {
            name: "MCPTool".to_string(),
            description: Some(
                "Call a configured MCP tool by its qualified name and JSON arguments.".to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "qualifiedName": { "type": "string" },
                    "arguments": {}
                },
                "required": ["qualifiedName"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        RuntimeToolDefinition {
            name: "ListMcpResourcesTool".to_string(),
            description: Some(
                "List MCP resources from one configured server or from every connected server."
                    .to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        RuntimeToolDefinition {
            name: "ReadMcpResourceTool".to_string(),
            description: Some("Read a specific MCP resource from a configured server.".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}

fn permission_mode_for_mcp_tool(tool: &McpTool) -> PermissionMode {
    let read_only = mcp_annotation_flag(tool, "readOnlyHint");
    let destructive = mcp_annotation_flag(tool, "destructiveHint");
    let open_world = mcp_annotation_flag(tool, "openWorldHint");

    if read_only && !destructive && !open_world {
        PermissionMode::ReadOnly
    } else if destructive || open_world {
        PermissionMode::DangerFullAccess
    } else {
        PermissionMode::WorkspaceWrite
    }
}

fn mcp_annotation_flag(tool: &McpTool, key: &str) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

struct HookAbortMonitor {
    stop_tx: Option<Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl HookAbortMonitor {
    fn spawn(abort_signal: runtime::HookAbortSignal) -> Self {
        Self::spawn_with_waiter(abort_signal, move |stop_rx, abort_signal| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };

            runtime.block_on(async move {
                let wait_for_stop = tokio::task::spawn_blocking(move || {
                    let _ = stop_rx.recv();
                });

                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            abort_signal.abort();
                        }
                    }
                    _ = wait_for_stop => {}
                }
            });
        })
    }

    fn spawn_with_waiter<F>(abort_signal: runtime::HookAbortSignal, wait_for_interrupt: F) -> Self
    where
        F: FnOnce(Receiver<()>, runtime::HookAbortSignal) + Send + 'static,
    {
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = thread::spawn(move || wait_for_interrupt(stop_rx, abort_signal));

        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }
    }

    fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl LiveCli {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        additional_workspace_roots: Vec<PathBuf>,
        output_verbosity: OutputVerbosity,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let t0 = std::time::Instant::now();
        let system_prompt = build_system_prompt(&model)?;
        let t_sp = t0.elapsed();
        let session_state = new_cli_session_with_roots(additional_workspace_roots)?;
        let t_sess = t0.elapsed();
        let session = create_managed_session_handle(&session_state.session_id)?;
        let t_handle = t0.elapsed();
        let runtime = build_runtime(
            session_state.with_persistence_path(session.path.clone()),
            &session.id,
            model.clone(),
            system_prompt.clone(),
            enable_tools,
            true,
            allowed_tools.clone(),
            permission_mode,
            None,
        )?;
        let t_rt = t0.elapsed();
        eprintln!(
            "[timing] build_system_prompt={:?} new_cli_session={:?} create_handle={:?} build_runtime={:?} total={:?}",
            t_sp, t_sess, t_handle, t_rt, t_rt
        );
        // Tier S #3 穷鬼模式：从 settings.json 的 `poorMode` 字段读取初始值，
        // 写入进程级全局 AtomicBool。运行时通过 `/poor` 命令可立即切换。
        // 加载失败时保持默认（inactive），不阻断启动。
        // Tier S #1 Goal 持续驱动：从 `<cwd>/.claw/goal.json` 加载已持久化的 goal，
        // 恢复上次会话的 goal 状态。文件不存在或解析失败时为空管理器。
        let goal_manager = if let Ok(cwd) = env::current_dir() {
            let loader = ConfigLoader::default_for(&cwd);
            if let Ok(config) = loader.load() {
                if let Some(poor) = config.feature_config().poor_mode() {
                    runtime::poor_mode::set_active(poor);
                }
            }
            runtime::GoalManager::load(runtime::goal_json_path(&cwd))
        } else {
            runtime::GoalManager::new(PathBuf::from(".claw/goal.json"))
        };
        let cli = Self {
            model,
            allowed_tools,
            permission_mode,
            output_verbosity,
            system_prompt,
            runtime,
            session,
            prompt_history: Vec::new(),
            cumulative_usage: runtime::TokenUsage::default(),
            goal_manager,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_reasoning_effort(effort);
        }
    }

    /// P2 状态栏：累加本次回合的 usage 到 cumulative_usage。
    /// 在 run_turn / run_prompt_* 成功路径调用。
    fn accumulate_usage(&mut self, usage: runtime::TokenUsage) {
        self.cumulative_usage.input_tokens += usage.input_tokens;
        self.cumulative_usage.output_tokens += usage.output_tokens;
        self.cumulative_usage.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        self.cumulative_usage.cache_read_input_tokens += usage.cache_read_input_tokens;
    }

    /// P2 状态栏：打印当前会话的累计状态栏。
    /// 在每次回合完成后（run_turn 成功路径末尾）调用。
    /// Tier S #1 Goal 持续驱动：追加 goal 徽章（🎯 active 绿色 / ⚠ blocked 橙色 / paused 不显示）。
    fn print_status_bar(&self) {
        let cwd = std::env::current_dir().unwrap_or_default();
        let short_cwd = shorten_cwd_for_statusbar(&cwd);
        let base = format_status_bar(&self.model, &short_cwd, self.cumulative_usage);
        // Tier S #1 Goal 徽章：在状态栏末尾追加 goal 状态。
        let goal_badge = self.render_goal_badge();
        if let Some(badge) = goal_badge {
            // 把 badge 插到末尾 `│` 之前。
            let trimmed = base.trim_end_matches("\x1b[0m");
            println!("{trimmed}{badge}\x1b[0m");
        } else {
            println!("{base}");
        }
    }

    /// 渲染 goal 徽章字符串（不含 ANSI reset 后缀）。无 goal 或 paused 状态返回 None。
    fn render_goal_badge(&self) -> Option<String> {
        let goal = self.goal_manager.active()?;
        match &goal.state {
            runtime::GoalState::Active => Some(
                "\x1b[38;5;240m │ \x1b[32m🎯 goal\x1b[0;38;5;240m".to_string(),
            ),
            runtime::GoalState::Blocked { .. } => Some(format!(
                "\x1b[38;5;240m │ \x1b[33m⚠ goal ({}/3)\x1b[0;38;5;240m",
                goal.blocked_count
            )),
            runtime::GoalState::Paused { .. } => None,
        }
    }

    fn startup_banner(&self) -> String {
        let cwd = env::current_dir().map_or_else(
            |_| "<unknown>".to_string(),
            |path| path.display().to_string(),
        );
        let status = status_context(None).ok();
        let git_branch = status
            .as_ref()
            .and_then(|context| context.git_branch.as_deref())
            .unwrap_or("unknown");
        let workspace = status.as_ref().map_or_else(
            || "unknown".to_string(),
            |context| context.git_summary.headline(),
        );
        let session_path = self.session.path.strip_prefix(Path::new(&cwd)).map_or_else(
            |_| self.session.path.display().to_string(),
            |path| path.display().to_string(),
        );
        format!(
            "\x1b[38;5;196m\
 ██████╗██╗      █████╗ ██╗    ██╗\n\
██╔════╝██║     ██╔══██╗██║    ██║\n\
██║     ██║     ███████║██║ █╗ ██║\n\
██║     ██║     ██╔══██║██║███╗██║\n\
╚██████╗███████╗██║  ██║╚███╔███╔╝\n\
 ╚═════╝╚══════╝╚═╝  ╚═╝ ╚══╝╚══╝\x1b[0m \x1b[38;5;208mCode\x1b[0m 🦞\n\n\
  \x1b[2mModel\x1b[0m            {}\n\
  \x1b[2mPermissions\x1b[0m      {}\n\
  \x1b[2mBranch\x1b[0m           {}\n\
  \x1b[2mWorkspace\x1b[0m        {}\n\
  \x1b[2mDirectory\x1b[0m        {}\n\
  \x1b[2mSession\x1b[0m          {}\n\
  \x1b[2mAuto-save\x1b[0m        {}\n\n\
  Type \x1b[1m/help\x1b[0m for commands · \x1b[1m/status\x1b[0m for live context · \x1b[2m/resume latest\x1b[0m jumps back to the newest session · \x1b[1m/diff\x1b[0m then \x1b[1m/commit\x1b[0m to ship · \x1b[2mTab\x1b[0m for workflow completions · \x1b[2mShift+Enter\x1b[0m for newline",
            self.model,
            self.permission_mode.as_str(),
            git_branch,
            workspace,
            cwd,
            self.session.id,
            session_path,
        )
    }

    fn repl_completion_candidates(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        Ok(slash_command_completion_candidates_with_sessions(
            &self.model,
            Some(&self.session.id),
            list_managed_sessions()?
                .into_iter()
                .map(|session| session.id)
                .collect(),
        ))
    }

    fn prepare_turn_runtime(
        &self,
        emit_output: bool,
    ) -> Result<(BuiltRuntime, HookAbortMonitor), Box<dyn std::error::Error>> {
        let hook_abort_signal = runtime::HookAbortSignal::new();
        let mut runtime = build_runtime(
            self.runtime.session().clone(),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            emit_output,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?
        .with_hook_abort_signal(hook_abort_signal.clone());
        runtime.set_tool_verbosity(self.output_verbosity);
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal);

        Ok((runtime, hook_abort_monitor))
    }

    fn replace_runtime(&mut self, runtime: BuiltRuntime) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_plugins()?;
        self.runtime = runtime;
        Ok(())
    }

    fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(true)?;
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        spinner.tick(
            "🦀 Thinking...",
            TerminalRenderer::new().color_theme(),
            &mut stdout,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        // Tier S #1 Goal 持续驱动：在调 runtime.run_turn 之前 prepend goal 前缀。
        // 前缀包含 goal 文本、状态（active/blocked）、blocked 计数、token 用量。
        // LLM 每轮都看到 goal 上下文，驱动持续工作。Paused 状态不注入。
        let goal_prefix = self.goal_manager.render_prompt_prefix();
        let full_input = match &goal_prefix {
            Some(prefix) => format!("{prefix}{input}"),
            None => input.to_string(),
        };
        let result = runtime.run_turn(&full_input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        match result {
            Ok(summary) => {
                self.replace_runtime(runtime)?;
                spinner.finish(
                    "✨ Done",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                let final_text = final_assistant_text(&summary);
                if !final_text.is_empty() {
                    println!("{final_text}");
                }
                println!();
                if let Some(event) = summary.auto_compaction {
                    println!(
                        "{}",
                        format_auto_compaction_notice(event.removed_message_count)
                    );
                }
                // P2 富状态栏：累加本次 usage 并打印状态行。
                // run_turn 只在 REPL 交互模式被调用（非交互走 run_prompt_*），
                // 所以无需额外门控 emit_output。
                self.accumulate_usage(summary.usage);
                // Tier S #1 Goal 持续驱动：累加本次回合的 token 用量到 goal_manager。
                // 用于 budget 跟踪。失败不阻断主流程。
                let turn_tokens = u64::from(summary.usage.total_tokens());
                let _ = self.goal_manager.record_tokens(turn_tokens);
                self.print_status_bar();
                println!();
                self.persist_session()?;
                Ok(())
            }
            Err(error) => {
                runtime.shutdown_plugins()?;
                spinner.fail(
                    "❌ Request failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                // Tier S #1 Goal 持续驱动：检测网络错误关键词，自动 pause goal。
                // 避免网络中断期间 goal 持续注入无效 prompt。
                let error_str = error.to_string().to_ascii_lowercase();
                let is_network_error = NETWORK_ERROR_KEYWORDS
                    .iter()
                    .any(|kw| error_str.contains(kw));
                if is_network_error {
                    let _ = self.goal_manager.pause("network error");
                    eprintln!(
                        "\x1b[33m⚠ Goal auto-paused due to network error. Use /goal resume when network is restored.\x1b[0m"
                    );
                }
                Err(Box::new(error))
            }
        }
    }

    fn run_turn_with_output(
        &mut self,
        input: &str,
        output_format: CliOutputFormat,
        compact: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match output_format {
            CliOutputFormat::Json if compact => self.run_prompt_compact_json(input),
            CliOutputFormat::Text if compact => self.run_prompt_compact(input),
            CliOutputFormat::Text => self.run_turn(input),
            CliOutputFormat::Json => self.run_prompt_json(input),
        }
    }

    fn run_prompt_compact(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        let final_text = final_assistant_text(&summary);
        println!("{final_text}");
        Ok(())
    }

    fn run_prompt_compact_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "compact": true,
                "model": self.model,
                "usage": {
                    "input_tokens": summary.usage.input_tokens,
                    "output_tokens": summary.usage.output_tokens,
                    "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                },
            })
        );
        Ok(())
    }

    fn run_prompt_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "model": self.model,
                "iterations": summary.iterations,
                "auto_compaction": summary.auto_compaction.map(|event| json!({
                    "removed_messages": event.removed_message_count,
                    "notice": format_auto_compaction_notice(event.removed_message_count),
                })),
                "tool_uses": collect_tool_uses(&summary),
                "tool_results": collect_tool_results(&summary),
                "prompt_cache_events": collect_prompt_cache_events(&summary),
                "usage": {
                    "input_tokens": summary.usage.input_tokens,
                    "output_tokens": summary.usage.output_tokens,
                    "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                },
                "estimated_cost": format_usd(
                    summary.usage.estimate_cost_usd_with_pricing(
                        pricing_for_model(&self.model)
                            .unwrap_or_else(runtime::ModelPricing::default_sonnet_tier)
                    ).total_cost_usd()
                )
            })
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_repl_command(
        &mut self,
        command: SlashCommand,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(match command {
            SlashCommand::Help => {
                println!("{}", render_repl_help());
                false
            }
            SlashCommand::Status => {
                self.print_status();
                false
            }
            SlashCommand::Bughunter { scope } => {
                self.run_bughunter(scope.as_deref())?;
                false
            }
            SlashCommand::Commit => {
                self.run_commit(None)?;
                false
            }
            SlashCommand::Pr { context } => {
                self.run_pr(context.as_deref())?;
                false
            }
            SlashCommand::Issue { context } => {
                self.run_issue(context.as_deref())?;
                false
            }
            SlashCommand::Ultraplan { task } => {
                self.run_ultraplan(task.as_deref())?;
                false
            }
            SlashCommand::Teleport { target } => {
                Self::run_teleport(target.as_deref())?;
                false
            }
            SlashCommand::DebugToolCall => {
                self.run_debug_tool_call(None)?;
                false
            }
            SlashCommand::Sandbox => {
                Self::print_sandbox_status();
                false
            }
            SlashCommand::Compact => {
                self.compact()?;
                false
            }
            SlashCommand::Model { model } => self.set_model(model)?,
            SlashCommand::Permissions { mode } => self.set_permissions(mode)?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => self.resume_session(session_path)?,
            SlashCommand::Config { section } => {
                Self::print_config(section.as_deref())?;
                false
            }
            SlashCommand::Mcp { action, target } => {
                let args = match (action.as_deref(), target.as_deref()) {
                    (None, None) => None,
                    (Some(action), None) => Some(action.to_string()),
                    (Some(action), Some(target)) => Some(format!("{action} {target}")),
                    (None, Some(target)) => Some(target.to_string()),
                };
                Self::print_mcp(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Memory => {
                Self::print_memory()?;
                false
            }
            SlashCommand::Init => {
                run_init(CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Diff => {
                Self::print_diff()?;
                false
            }
            SlashCommand::Version => {
                Self::print_version(CliOutputFormat::Text);
                false
            }
            SlashCommand::Export { path } => {
                self.export_session(path.as_deref())?;
                false
            }
            SlashCommand::Session { action, target } => {
                self.handle_session_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Plugins { action, target } => {
                self.handle_plugins_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Agents { args } => {
                Self::print_agents(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Skills { args } => {
                match classify_skills_slash_command(args.as_deref()) {
                    SkillSlashDispatch::Invoke(prompt) => self.run_turn(&prompt)?,
                    SkillSlashDispatch::Local => {
                        Self::print_skills(args.as_deref(), CliOutputFormat::Text)?;
                    }
                }
                false
            }
            SlashCommand::Doctor => {
                println!("{}", render_doctor_report()?.render());
                false
            }
            SlashCommand::History { count } => {
                self.print_prompt_history(count.as_deref());
                false
            }
            SlashCommand::Stats => {
                let usage = UsageTracker::from_session(self.runtime.session()).cumulative_usage();
                println!("{}", format_cost_report(usage));
                false
            }
            SlashCommand::Poor { action } => {
                let (_, message) = handle_poor_mode_action(action.as_deref());
                println!("{message}");
                false
            }
            SlashCommand::Goal { args } => {
                let message = handle_goal_command(&mut self.goal_manager, args.as_deref());
                println!("{message}");
                false
            }
            SlashCommand::Bg { args } => {
                // Tier S #2 后台会话：REPL 模式下查询/管理后台进程。
                // 与 resume 模式共用 handle_bg_command，通过文件系统通信。
                let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let (message, _json) = handle_bg_command(args.as_deref(), &cwd);
                println!("{message}");
                false
            }
            SlashCommand::Login
            | SlashCommand::Logout
            | SlashCommand::Vim
            | SlashCommand::Upgrade
            | SlashCommand::Share
            | SlashCommand::Feedback
            | SlashCommand::Files
            | SlashCommand::Fast
            | SlashCommand::Exit
            | SlashCommand::Summary
            | SlashCommand::Desktop
            | SlashCommand::Brief
            | SlashCommand::Advisor
            | SlashCommand::Stickers
            | SlashCommand::Insights
            | SlashCommand::Thinkback
            | SlashCommand::ReleaseNotes
            | SlashCommand::SecurityReview
            | SlashCommand::Keybindings
            | SlashCommand::PrivacySettings
            | SlashCommand::Plan { .. }
            | SlashCommand::Review { .. }
            | SlashCommand::Tasks { .. }
            | SlashCommand::Theme { .. }
            | SlashCommand::Voice { .. }
            | SlashCommand::Usage { .. }
            | SlashCommand::Rename { .. }
            | SlashCommand::Copy { .. }
            | SlashCommand::Hooks { .. }
            | SlashCommand::Context { .. }
            | SlashCommand::Color { .. }
            | SlashCommand::Effort { .. }
            | SlashCommand::Branch { .. }
            | SlashCommand::Rewind { .. }
            | SlashCommand::Ide { .. }
            | SlashCommand::Tag { .. }
            | SlashCommand::AddDir { .. } => {
                let cmd_name = command.slash_name();
                eprintln!("{cmd_name} is not yet implemented in this build.");
                false
            }
            SlashCommand::OutputStyle { style } => {
                if let Some(verbosity) = style
                    .as_deref()
                    .and_then(OutputVerbosity::from_style_arg)
                {
                    self.output_verbosity = verbosity;
                    println!("Output style set to: {}", verbosity.label());
                } else {
                    let current = self.output_verbosity.label();
                    println!("Current output style: {current}");
                    println!(
                        "Available styles: full, compact, silent, minimal\nUsage: /output-style [style]"
                    );
                }
                false
            }
            SlashCommand::Unknown(name) => {
                eprintln!("{}", format_unknown_slash_command(&name));
                false
            }
        })
    }

    fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        println!(
            "{}",
            format_status_report(
                &self.model,
                StatusUsage {
                    message_count: self.runtime.session().messages.len(),
                    turns: self.runtime.usage().turns(),
                    latest,
                    cumulative,
                    estimated_tokens: self.runtime.estimated_tokens(),
                },
                self.permission_mode.as_str(),
                &status_context(Some(&self.session.path)).expect("status context should load"),
                None, // #148: REPL /status doesn't carry flag provenance
            )
        );
    }

    fn record_prompt_history(&mut self, prompt: &str) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map_or(self.runtime.session().updated_at_ms, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        let entry = PromptHistoryEntry {
            timestamp_ms,
            text: prompt.to_string(),
        };
        self.prompt_history.push(entry);
        if let Err(error) = self.runtime.session_mut().push_prompt_entry(prompt) {
            eprintln!("warning: failed to persist prompt history: {error}");
        }
    }

    fn print_prompt_history(&self, count: Option<&str>) {
        let limit = match parse_history_count(count) {
            Ok(limit) => limit,
            Err(message) => {
                eprintln!("{message}");
                return;
            }
        };
        let session_entries = &self.runtime.session().prompt_history;
        let entries = if session_entries.is_empty() {
            if self.prompt_history.is_empty() {
                collect_session_prompt_history(self.runtime.session())
            } else {
                self.prompt_history
                    .iter()
                    .map(|entry| PromptHistoryEntry {
                        timestamp_ms: entry.timestamp_ms,
                        text: entry.text.clone(),
                    })
                    .collect()
            }
        } else {
            session_entries
                .iter()
                .map(|entry| PromptHistoryEntry {
                    timestamp_ms: entry.timestamp_ms,
                    text: entry.text.clone(),
                })
                .collect()
        };
        println!("{}", render_prompt_history_report(&entries, limit));
    }

    fn print_sandbox_status() {
        let cwd = env::current_dir().expect("current dir");
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader
            .load()
            .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
        println!(
            "{}",
            format_sandbox_report(&resolve_sandbox_status(runtime_config.sandbox(), &cwd))
        );
    }

    fn set_model(&mut self, model: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(model) = model else {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        };

        let model = resolve_model_alias_with_config(&model);

        if model == self.model {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        }

        let previous = self.model.clone();
        let session = self.runtime.session().clone();
        let message_count = session.messages.len();
        let runtime = build_runtime(
            session,
            &self.session.id,
            model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.model.clone_from(&model);
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        Ok(true)
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            println!(
                "{}",
                format_permissions_report(self.permission_mode.as_str())
            );
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            )
        })?;

        if normalized == self.permission_mode.as_str() {
            println!("{}", format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = self.permission_mode.as_str().to_string();
        let session = self.runtime.session().clone();
        self.permission_mode = permission_mode_from_label(normalized);
        let runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        println!(
            "{}",
            format_permissions_switch_report(&previous, normalized)
        );
        Ok(true)
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            println!(
                "clear: confirmation required; run /clear --confirm to start a fresh session."
            );
            return Ok(false);
        }

        let previous_session = self.session.clone();
        let session_state = new_cli_session()?;
        self.session = create_managed_session_handle(&session_state.session_id)?;
        let runtime = build_runtime(
            session_state.with_persistence_path(self.session.path.clone()),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        println!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            self.model,
            self.permission_mode.as_str(),
            self.session.id,
            self.session.path.display(),
        );
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        println!("{}", format_cost_report(cumulative));
    }

    fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            println!("{}", render_resume_usage());
            return Ok(false);
        };

        let (handle, session) = load_session_reference(&session_ref)?;
        let message_count = session.messages.len();
        let session_id = session.session_id.clone();
        let runtime = build_runtime(
            session,
            &handle.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.session = SessionHandle {
            id: session_id,
            path: handle.path,
        };
        println!(
            "{}",
            format_resume_report(
                &self.session.path.display().to_string(),
                message_count,
                self.runtime.usage().turns(),
            )
        );
        Ok(true)
    }

    fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_config_report(section)?);
        Ok(())
    }

    fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_memory_report()?);
        Ok(())
    }

    fn print_agents(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_agents_slash_command(args, &cwd)?),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&handle_agents_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    fn print_mcp(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // `claw mcp serve` starts a stdio MCP server exposing claw's built-in
        // tools. All other `mcp` subcommands fall through to the existing
        // configured-server reporter (`list`, `status`, ...).
        if matches!(args.map(str::trim), Some("serve")) {
            return run_mcp_serve();
        }
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_mcp_slash_command(args, &cwd)?),
            CliOutputFormat::Json => {
                let value = handle_mcp_slash_command_json(args, &cwd)?;
                // Propagate ok:false → non-zero exit so automation callers
                // can rely on exit code instead of inspecting the envelope.
                // (#68: mcp error envelopes previously always exited 0.)
                let is_error = value.get("ok").and_then(|v| v.as_bool()) == Some(false);
                println!("{}", serde_json::to_string_pretty(&value)?);
                if is_error {
                    std::process::exit(1);
                }
            }
        }
        Ok(())
    }

    fn print_skills(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_skills_slash_command(args, &cwd)?),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&handle_skills_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    fn print_plugins(
        action: Option<&str>,
        target: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let payload = plugins_command_payload_for(&cwd, action, target)?;
        match output_format {
            CliOutputFormat::Text => println!("{}", payload.message),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "kind": "plugin",
                    "action": action.unwrap_or("list"),
                    "target": target,
                    "status": payload.status,
                    "config_load_error": payload.config_load_error,
                    "message": payload.message,
                    "reload_runtime": payload.reload_runtime,
                    "plugins": payload.plugins,
                    "load_failures": payload.load_failures,
                }))?
            ),
        }
        Ok(())
    }

    fn print_diff() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_diff_report()?);
        Ok(())
    }

    fn print_version(output_format: CliOutputFormat) {
        let _ = crate::print_version(output_format);
    }

    fn export_session(
        &self,
        requested_path: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let export_path = resolve_export_path(requested_path, self.runtime.session())?;
        fs::write(&export_path, render_export_text(self.runtime.session()))?;
        println!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_session_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None | Some("list") => {
                println!("{}", render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("exists") => {
                let Some(target) = target else {
                    println!("Usage: /session exists <session-id>");
                    return Ok(false);
                };
                let exists = session_reference_exists(target)?;
                let handle = resolve_session_reference(target).ok();
                println!(
                    "Session exists\n  Session          {target}\n  Exists           {exists}{}",
                    handle
                        .as_ref()
                        .map(|handle| format!("\n  File             {}", handle.path.display()))
                        .unwrap_or_default()
                );
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    println!("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let (handle, session) = load_session_reference(target)?;
                let message_count = session.messages.len();
                let session_id = session.session_id.clone();
                let runtime = build_runtime(
                    session,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = SessionHandle {
                    id: session_id,
                    path: handle.path,
                };
                println!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("fork") => {
                let forked = self.runtime.fork_session(target.map(ToOwned::to_owned));
                let parent_session_id = self.session.id.clone();
                let handle = create_managed_session_handle(&forked.session_id)?;
                let branch_name = forked
                    .fork
                    .as_ref()
                    .and_then(|fork| fork.branch_name.clone());
                let forked = forked.with_persistence_path(handle.path.clone());
                let message_count = forked.messages.len();
                forked.save_to_path(&handle.path)?;
                let runtime = build_runtime(
                    forked,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = handle;
                println!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    self.session.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("delete") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                if !confirm_session_deletion(&handle.id) {
                    println!("delete: cancelled.");
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some("delete-force") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some(other) => {
                println!(
                    "Unknown /session action '{other}'. Use /session list, /session exists <session-id>, /session switch <session-id>, /session fork [branch-name], or /session delete <session-id> [--force]."
                );
                Ok(false)
            }
        }
    }

    fn handle_plugins_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let payload = plugins_command_payload_for(&cwd, action, target)?;
        println!("{}", payload.message);
        if payload.reload_runtime {
            self.reload_runtime_features()?;
        }
        Ok(false)
    }

    fn reload_runtime_features(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = build_runtime(
            self.runtime.session().clone(),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()
    }

    fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.runtime.compact(CompactionConfig::default());
        let removed = result.removed_message_count;
        let kept = result.compacted_session.messages.len();
        let skipped = removed == 0;
        let runtime = build_runtime(
            result.compacted_session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!("{}", format_compact_report(removed, kept, skipped));
        Ok(())
    }

    fn run_internal_prompt_text_with_progress(
        &self,
        prompt: &str,
        enable_tools: bool,
        progress: Option<InternalPromptProgressReporter>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let mut runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            enable_tools,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            progress,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let summary = runtime.run_turn(prompt, Some(&mut permission_prompter))?;
        let text = final_assistant_text(&summary).trim().to_string();
        runtime.shutdown_plugins()?;
        Ok(text)
    }

    fn run_internal_prompt_text(
        &self,
        prompt: &str,
        enable_tools: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.run_internal_prompt_text_with_progress(prompt, enable_tools, None)
    }

    fn run_bughunter(&self, scope: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_bughunter_report(scope));
        Ok(())
    }

    fn run_ultraplan(&self, task: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_ultraplan_report(task));
        Ok(())
    }

    fn run_teleport(target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
            println!("Usage: /teleport <symbol-or-path>");
            return Ok(());
        };

        println!("{}", render_teleport_report(target)?);
        Ok(())
    }

    fn run_debug_tool_call(&self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/debug-tool-call", args)?;
        println!("{}", render_last_tool_debug_report(self.runtime.session())?);
        Ok(())
    }

    fn run_commit(&mut self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/commit", args)?;
        let status = git_output(&["status", "--short", "--branch"])?;
        let summary = parse_git_workspace_summary(Some(&status));
        let branch = parse_git_status_branch(Some(&status));
        if summary.is_clean() {
            println!("{}", format_commit_skipped_report());
            return Ok(());
        }

        println!(
            "{}",
            format_commit_preflight_report(branch.as_deref(), summary)
        );
        Ok(())
    }

    fn run_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let branch =
            resolve_git_branch_for(&env::current_dir()?).unwrap_or_else(|| "unknown".to_string());
        println!("{}", format_pr_report(&branch, context));
        Ok(())
    }

    fn run_issue(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_issue_report(context));
        Ok(())
    }
}

fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(current_session_store()?.sessions_dir().to_path_buf())
}

fn current_session_store() -> Result<runtime::SessionStore, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    runtime::SessionStore::from_cwd(&cwd).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

fn new_cli_session() -> Result<Session, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let mut session = Session::new().with_workspace_root(cwd.clone());
    // Attach a SQLite FTS5 history index so the `session_search` tool can
    // recall past messages. Best-effort: if the DB cannot be opened (e.g.
    // permission denied), the session still works — only search is disabled.
    let db_path = cwd.join(".claw").join("history.db");
    if let Ok(index) = HistoryIndex::open(&db_path) {
        session = session.with_history_index(std::sync::Arc::new(index));
    }
    Ok(session)
}

/// 与 [`new_cli_session`] 类似，但额外附加 `--add-dir` 提供的工作区根。
/// 这些根与主工作区根一起构成多根校验集合（见 `WorkspacePathScope`）。
fn new_cli_session_with_roots(
    additional_roots: Vec<PathBuf>,
) -> Result<Session, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let mut session = Session::new()
        .with_workspace_root(cwd.clone())
        .with_additional_workspace_roots(additional_roots);
    // === PRESERVE upstream's HistoryIndex wiring from new_cli_session() ===
    // Attach a SQLite FTS5 history index so the `session_search` tool can
    // recall past messages. Best-effort: if the DB cannot be opened (e.g.
    // permission denied), the session still works — only search is disabled.
    let db_path = cwd.join(".claw").join("history.db");
    if let Ok(index) = HistoryIndex::open(&db_path) {
        session = session.with_history_index(std::sync::Arc::new(index));
    }
    Ok(session)
}

fn create_managed_session_handle(
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let handle = current_session_store()?.create_handle(session_id);
    Ok(SessionHandle {
        id: handle.id,
        path: handle.path,
    })
}

fn resolve_session_reference(reference: &str) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let handle = current_session_store()?
        .resolve_reference(reference)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(SessionHandle {
        id: handle.id,
        path: handle.path,
    })
}

fn session_reference_exists(reference: &str) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(current_session_store()?.session_exists(reference))
}

fn resolve_managed_session_path(session_id: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    current_session_store()?
        .resolve_managed_path(session_id)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

fn list_managed_sessions() -> Result<Vec<ManagedSessionSummary>, Box<dyn std::error::Error>> {
    let store = current_session_store()?;
    let lifecycle = classify_session_lifecycle_for(store.workspace_root());
    Ok(store
        .list_sessions()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?
        .into_iter()
        .map(|session| ManagedSessionSummary {
            id: session.id,
            path: session.path,
            updated_at_ms: session.updated_at_ms,
            modified_epoch_millis: session.modified_epoch_millis,
            message_count: session.message_count,
            parent_session_id: session.parent_session_id,
            branch_name: session.branch_name,
            lifecycle: lifecycle.clone(),
        })
        .collect())
}

fn latest_managed_session() -> Result<ManagedSessionSummary, Box<dyn std::error::Error>> {
    let store = current_session_store()?;
    let lifecycle = classify_session_lifecycle_for(store.workspace_root());
    let session = store
        .latest_session()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(ManagedSessionSummary {
        id: session.id,
        path: session.path,
        updated_at_ms: session.updated_at_ms,
        modified_epoch_millis: session.modified_epoch_millis,
        message_count: session.message_count,
        parent_session_id: session.parent_session_id,
        branch_name: session.branch_name,
        lifecycle,
    })
}

fn load_session_reference(
    reference: &str,
) -> Result<(SessionHandle, Session), Box<dyn std::error::Error>> {
    let loaded = current_session_store()?
        .load_session(reference)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok((
        SessionHandle {
            id: loaded.handle.id,
            path: loaded.handle.path,
        },
        loaded.session,
    ))
}

fn delete_managed_session(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("session file does not exist: {}", path.display()).into());
    }
    fs::remove_file(path)?;
    Ok(())
}

fn confirm_session_deletion(session_id: &str) -> bool {
    print!("Delete session '{session_id}'? This cannot be undone. [y/N]: ");
    io::stdout().flush().unwrap_or(());
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

fn session_details_json(sessions: &[ManagedSessionSummary]) -> Vec<serde_json::Value> {
    sessions
        .iter()
        .map(|session| {
            serde_json::json!({
                "id": session.id,
                "path": session.path.display().to_string(),
                "message_count": session.message_count,
                "updated_at_ms": session.updated_at_ms,
                "modified_epoch_millis": session.modified_epoch_millis,
                "parent_session_id": session.parent_session_id,
                "branch_name": session.branch_name,
                "lifecycle": session.lifecycle.json_value(),
            })
        })
        .collect()
}

fn session_exists_json(
    target: &str,
    active_session_id: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let handle = create_managed_session_handle(target)?;
    let resolved = resolve_session_reference(target).ok();
    let exists = resolved.is_some();
    let resolved_id = resolved
        .as_ref()
        .map_or(target, |handle| handle.id.as_str());
    Ok(serde_json::json!({
        "kind": "session_exists",
        "session_id": resolved_id,
        "session": target,
        "requested": target,
        "exists": exists,
        "active": resolved_id == active_session_id,
        "path": resolved
            .as_ref()
            .map(|handle| handle.path.display().to_string()),
        "candidate_path": handle.path.display().to_string(),
    }))
}

fn run_resumed_session_command(
    session_path: &Path,
    session: &Session,
    action: Option<&str>,
    target: Option<&str>,
) -> Result<ResumeCommandOutcome, Box<dyn std::error::Error>> {
    match action {
        None | Some("list") => {
            let sessions = list_managed_sessions().unwrap_or_default();
            let session_ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
            let active_id = session.session_id.clone();
            let text = render_session_list(&active_id).unwrap_or_else(|e| format!("error: {e}"));
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(text),
                json: Some(serde_json::json!({
                    "kind": "session_list",
                    "sessions": session_ids,
                    "session_details": session_details_json(&sessions),
                    "active": active_id,
                })),
            })
        }
        Some("exists") => {
            let Some(target) = target else {
                return Err("/session exists requires a session id".into());
            };
            let value = session_exists_json(target, &session.session_id)?;
            let exists = value
                .get("exists")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "Session exists\n  Session          {}\n  Exists           {}",
                    target,
                    if exists { "yes" } else { "no" }
                )),
                json: Some(value),
            })
        }
        Some("delete") => {
            let Some(target) = target else {
                return Err("/session delete requires a session id".into());
            };
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "delete: confirmation required; rerun with /session delete {target} --force"
                )),
                json: Some(serde_json::json!({
                    "kind": "error",
                    "error": "confirmation required",
                    "hint": format!("rerun with /session delete {target} --force"),
                    "session_id": target,
                })),
            })
        }
        Some("delete-force") => {
            let Some(target) = target else {
                return Err("/session delete requires a session id".into());
            };
            let handle = resolve_session_reference(target)?;
            if handle.id == session.session_id || handle.path == session_path {
                return Err(format!(
                    "delete: refusing to delete the active session '{}'. Resume or switch to another session first.",
                    handle.id
                )
                .into());
            }
            delete_managed_session(&handle.path)?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                )),
                json: Some(serde_json::json!({
                    "kind": "session_delete",
                    "deleted": true,
                    "session_id": handle.id,
                    "path": handle.path.display().to_string(),
                })),
            })
        }
        Some("switch" | "fork") => Err("unsupported resumed slash command".into()),
        Some(other) => Err(format!("unsupported resumed /session action: {other}").into()),
    }
}

fn render_session_list(active_session_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let sessions = list_managed_sessions()?;
    let mut lines = vec![
        "Sessions".to_string(),
        format!("  Directory         {}", sessions_dir()?.display()),
    ];
    if sessions.is_empty() {
        lines.push("  No managed sessions saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    for session in sessions {
        let marker = if session.id == active_session_id {
            "● current"
        } else {
            "○ saved"
        };
        let lineage = match (
            session.branch_name.as_deref(),
            session.parent_session_id.as_deref(),
        ) {
            (Some(branch_name), Some(parent_session_id)) => {
                format!(" branch={branch_name} from={parent_session_id}")
            }
            (None, Some(parent_session_id)) => format!(" from={parent_session_id}"),
            (Some(branch_name), None) => format!(" branch={branch_name}"),
            (None, None) => String::new(),
        };
        lines.push(format!(
            "  {id:<20} {marker:<10} lifecycle={lifecycle} msgs={msgs:<4} modified={modified}{lineage} path={path}",
            id = session.id,
            lifecycle = session.lifecycle.signal(),
            msgs = session.message_count,
            modified = format_session_modified_age(session.modified_epoch_millis),
            lineage = lineage,
            path = session.path.display(),
        ));
    }
    Ok(lines.join("\n"))
}

fn format_session_modified_age(modified_epoch_millis: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(modified_epoch_millis, |duration| duration.as_millis());
    let delta_seconds = now
        .saturating_sub(modified_epoch_millis)
        .checked_div(1_000)
        .unwrap_or_default();
    match delta_seconds {
        0..=4 => "just-now".to_string(),
        5..=59 => format!("{delta_seconds}s-ago"),
        60..=3_599 => format!("{}m-ago", delta_seconds / 60),
        3_600..=86_399 => format!("{}h-ago", delta_seconds / 3_600),
        _ => format!("{}d-ago", delta_seconds / 86_400),
    }
}

fn write_session_clear_backup(
    session: &Session,
    session_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let backup_path = session_clear_backup_path(session_path);
    session.save_to_path(&backup_path)?;
    Ok(backup_path)
}

fn session_clear_backup_path(session_path: &Path) -> PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_millis());
    let file_name = session_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("session.jsonl");
    session_path.with_file_name(format!("{file_name}.before-clear-{timestamp}.bak"))
}

fn render_repl_help() -> String {
    [
        "REPL".to_string(),
        "  /exit                Quit the REPL".to_string(),
        "  /quit                Quit the REPL".to_string(),
        "  Up/Down              Navigate prompt history".to_string(),
        "  Ctrl-R               Reverse-search prompt history".to_string(),
        "  Tab                  Complete commands, modes, and recent sessions".to_string(),
        "  Ctrl-C               Clear input (or exit on empty prompt)".to_string(),
        "  Shift+Enter/Ctrl+J   Insert a newline".to_string(),
        "  Auto-save            .claw/sessions/<workspace-fingerprint>/<session-id>.jsonl"
            .to_string(),
        "  Resume latest        /resume latest".to_string(),
        "  Browse sessions      /session list".to_string(),
        "  Show prompt history  /history [count]".to_string(),
        String::new(),
        render_slash_command_help_filtered(STUB_COMMANDS),
    ]
    .join(
        "
",
    )
}

fn print_status_snapshot(
    model: &str,
    model_flag_raw: Option<&str>,
    permission_mode: PermissionMode,
    output_format: CliOutputFormat,
    allowed_tools: Option<&AllowedToolSet>,
) -> Result<(), Box<dyn std::error::Error>> {
    let usage = StatusUsage {
        message_count: 0,
        turns: 0,
        latest: TokenUsage::default(),
        cumulative: TokenUsage::default(),
        estimated_tokens: 0,
    };
    let context = status_context(None)?;
    // #148: resolve model provenance. If user passed --model, source is
    // "flag" with the raw input preserved. Otherwise probe env -> config
    // -> default and record the winning source.
    let provenance = match model_flag_raw {
        Some(raw) => ModelProvenance {
            resolved: model.to_string(),
            raw: Some(raw.to_string()),
            source: ModelSource::Flag,
        },
        None => ModelProvenance::from_env_or_config_or_default(model),
    };
    match output_format {
        CliOutputFormat::Text => println!(
            "{}",
            format_status_report(
                &provenance.resolved,
                usage,
                permission_mode.as_str(),
                &context,
                Some(&provenance)
            )
        ),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&status_json_value(
                Some(&provenance.resolved),
                usage,
                permission_mode.as_str(),
                &context,
                Some(&provenance),
                allowed_tools,
            ))?
        ),
    }
    Ok(())
}

fn status_json_value(
    model: Option<&str>,
    usage: StatusUsage,
    permission_mode: &str,
    context: &StatusContext,
    // #148: optional provenance for `model` field. Surfaces `model_source`
    // ("flag" | "env" | "config" | "default") and `model_raw` (user input
    // before alias resolution, or null when source is "default"). Callers
    // that don't have provenance (legacy resume paths) pass None, in which
    // case both new fields are omitted.
    provenance: Option<&ModelProvenance>,
    allowed_tools: Option<&AllowedToolSet>,
) -> serde_json::Value {
    // #143: top-level `status` marker so claws can distinguish
    // a clean run from a degraded run (config parse failed but other fields
    // are still populated). `config_load_error` carries the parse-error string
    // when present; it's a string rather than a typed object in Phase 1 and
    // will join the typed-error taxonomy in Phase 2 (ROADMAP §4.44).
    let degraded = context.config_load_error.is_some();
    let model_source = provenance.map(|p| p.source.as_str());
    let model_raw = provenance.and_then(|p| p.raw.clone());
    let allowed_tool_entries = allowed_tools.map(|tools| tools.iter().cloned().collect::<Vec<_>>());
    json!({
        "kind": "status",
        "status": if degraded { "degraded" } else { "ok" },
        "config_load_error": context.config_load_error,
        "model": model,
        "model_source": model_source,
        "model_raw": model_raw,
        "permission_mode": permission_mode,
        "allowed_tools": {
            "source": if allowed_tools.is_some() { "flag" } else { "default" },
            "restricted": allowed_tools.is_some(),
            "entries": allowed_tool_entries,
        },
        "usage": {
            "messages": usage.message_count,
            "turns": usage.turns,
            "latest_input": usage.latest.input_tokens,
            "latest_output": usage.latest.output_tokens,
            "latest_cache_creation_input": usage.latest.cache_creation_input_tokens,
            "latest_cache_read_input": usage.latest.cache_read_input_tokens,
            "latest_total": usage.latest.total_tokens(),
            "cumulative_input": usage.cumulative.input_tokens,
            "cumulative_output": usage.cumulative.output_tokens,
            "cumulative_cache_creation_input": usage.cumulative.cache_creation_input_tokens,
            "cumulative_cache_read_input": usage.cumulative.cache_read_input_tokens,
            "cumulative_total": usage.cumulative.total_tokens(),
            "estimated_cost_usd": format_usd(usage.cumulative.estimate_cost_usd().total_cost_usd()),
            "pricing": "estimated-default",
            "estimated_tokens": usage.estimated_tokens,
        },
        "lane_board": {
            "schema": "task_registry_v1",
            "status_json_supported": true,
            "heartbeat_freshness_supported": true,
            "states": ["active", "blocked", "finished"],
            "freshness_states": ["healthy", "stalled", "transport_dead", "unknown"],
        },
        "workspace": {
            "cwd": context.cwd,
            "project_root": context.project_root,
            "git_branch": context.git_branch,
            "git_state": context.git_summary.headline(),
            "changed_files": context.git_summary.changed_files,
            "staged_files": context.git_summary.staged_files,
            "unstaged_files": context.git_summary.unstaged_files,
            "untracked_files": context.git_summary.untracked_files,
            "session": context.session_path.as_ref().map_or_else(|| "live-repl".to_string(), |path| path.display().to_string()),
            "session_id": context.session_path.as_ref().and_then(|path| {
                // Session files are named <session-id>.jsonl directly under
                // .claw/sessions/. Extract the stem (drop the .jsonl extension).
                path.file_stem().map(|n| n.to_string_lossy().into_owned())
            }),
            "session_lifecycle": context.session_lifecycle.json_value(),
            "branch_freshness": context.branch_freshness.json_value(),
            "boot_preflight": context.boot_preflight.json_value(),
            "loaded_config_files": context.loaded_config_files,
            "discovered_config_files": context.discovered_config_files,
            "memory_file_count": context.memory_file_count,
        },
        "sandbox": {
            "enabled": context.sandbox_status.enabled,
            "active": context.sandbox_status.active,
            "supported": context.sandbox_status.supported,
            "in_container": context.sandbox_status.in_container,
            "requested_namespace": context.sandbox_status.requested.namespace_restrictions,
            "active_namespace": context.sandbox_status.namespace_active,
            "requested_network": context.sandbox_status.requested.network_isolation,
            "active_network": context.sandbox_status.network_active,
            "filesystem_mode": context.sandbox_status.filesystem_mode.as_str(),
            "filesystem_active": context.sandbox_status.filesystem_active,
            "allowed_mounts": context.sandbox_status.allowed_mounts,
            "markers": context.sandbox_status.container_markers,
            "fallback_reason": context.sandbox_status.fallback_reason,
        }
    })
}

fn status_context(
    session_path: Option<&Path>,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered_config_files = loader.discover().len();
    // #143: degrade gracefully on config parse failure rather than hard-fail.
    // `claw doctor` already does this; `claw status` now matches that contract
    // so that one malformed `mcpServers.*` entry doesn't take down the whole
    // health surface (workspace, git, model, permission, sandbox can still be
    // reported independently).
    let runtime_config = loader.load();
    let (loaded_config_files, sandbox_status, config_load_error) = match runtime_config.as_ref() {
        Ok(runtime_config) => (
            runtime_config.loaded_entries().len(),
            resolve_sandbox_status(runtime_config.sandbox(), &cwd),
            None,
        ),
        Err(err) => (
            0,
            // Fall back to defaults for sandbox resolution so claws still see
            // a populated sandbox section instead of a missing field. Defaults
            // produce the same output as a runtime config with no sandbox
            // overrides, which is the right degraded-mode shape: we cannot
            // report what the user *intended*, only what is actually in effect.
            resolve_sandbox_status(&runtime::SandboxConfig::default(), &cwd),
            Some(err.to_string()),
        ),
    };
    let project_context = ProjectContext::discover_with_git(&cwd, DEFAULT_DATE)?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    let git_summary = parse_git_workspace_summary(project_context.git_status.as_deref());
    let branch_freshness = BranchFreshness::from_git_status(project_context.git_status.as_deref());
    let stale_base_state = stale_base_state_for(&cwd, None);
    let boot_preflight = build_boot_preflight_snapshot(
        &cwd,
        project_root.as_deref(),
        project_context.git_status.as_deref(),
        runtime_config.as_ref().ok(),
        config_load_error.as_deref(),
    );
    Ok(StatusContext {
        cwd: cwd.clone(),
        session_path: session_path.map(Path::to_path_buf),
        loaded_config_files,
        discovered_config_files,
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
        git_summary,
        branch_freshness,
        stale_base_state,
        session_lifecycle: classify_session_lifecycle_for(&cwd),
        boot_preflight,
        sandbox_status,
        config_load_error,
    })
}

fn format_status_report(
    model: &str,
    usage: StatusUsage,
    permission_mode: &str,
    context: &StatusContext,
    // #148: optional model provenance to surface in a `Model source` line.
    // Callers without provenance (legacy resume paths) pass None and the
    // source line is omitted for backward compat.
    provenance: Option<&ModelProvenance>,
) -> String {
    // #143: if config failed to parse, surface a degraded banner at the top
    // of the text report so humans see the parse error before the body, while
    // the body below still reports everything that could be resolved without
    // config (workspace, git, sandbox defaults, etc.).
    let status_line = if context.config_load_error.is_some() {
        "Status (degraded)"
    } else {
        "Status"
    };
    let mut blocks: Vec<String> = Vec::new();
    if let Some(err) = context.config_load_error.as_deref() {
        blocks.push(format!(
            "Config load error\n  Status           fail\n  Summary          runtime config failed to load; reporting partial status\n  Details          {err}\n  Hint             `claw doctor` classifies config parse errors; fix the listed field and rerun"
        ));
    }
    // #148: render Model source line after Model, showing where the string
    // came from (flag / env / config / default) and the raw input if any.
    let model_source_line = provenance
        .map(|p| match &p.raw {
            Some(raw) if raw != model => {
                format!("\n  Model source     {} (raw: {raw})", p.source.as_str())
            }
            Some(_) => format!("\n  Model source     {}", p.source.as_str()),
            None => format!("\n  Model source     {}", p.source.as_str()),
        })
        .unwrap_or_default();
    blocks.extend([
        format!(
            "{status_line}
  Model            {model}{model_source_line}
  Permission mode  {permission_mode}
  Messages         {}
  Turns            {}
  Estimated tokens {}",
            usage.message_count, usage.turns, usage.estimated_tokens,
        ),
        format!(
            "Usage
  Latest total     {}
  Cumulative input {}
  Cumulative output {}
  Cache create     {}
  Cache read       {}
  Cumulative total {}
  Estimated cost   {}",
            usage.latest.total_tokens(),
            usage.cumulative.input_tokens,
            usage.cumulative.output_tokens,
            usage.cumulative.cache_creation_input_tokens,
            usage.cumulative.cache_read_input_tokens,
            usage.cumulative.total_tokens(),
            format_usd(usage.cumulative.estimate_cost_usd().total_cost_usd()),
        ),
        format!(
            "Workspace
  Cwd              {}
  Project root     {}
  Git branch       {}
  Git state        {}
  Changed files    {}
  Staged           {}
  Unstaged         {}
  Untracked        {}
  Session          {}
  Lifecycle        {}
  Branch fresh     {}
  Boot preflight   {}
  Config files     loaded {}/{}
  Memory files     {}
  Suggested flow   /status → /diff → /commit",
            context.cwd.display(),
            context
                .project_root
                .as_ref()
                .map_or_else(|| "unknown".to_string(), |path| path.display().to_string()),
            context.git_branch.as_deref().unwrap_or("unknown"),
            context.git_summary.headline(),
            context.git_summary.changed_files,
            context.git_summary.staged_files,
            context.git_summary.unstaged_files,
            context.git_summary.untracked_files,
            context.session_path.as_ref().map_or_else(
                || "live-repl".to_string(),
                |path| path.display().to_string()
            ),
            context.session_lifecycle.signal(),
            context
                .branch_freshness
                .fresh
                .map(|fresh| if fresh { "yes" } else { "behind" })
                .unwrap_or("no upstream"),
            context.boot_preflight.summary(),
            context.loaded_config_files,
            context.discovered_config_files,
            context.memory_file_count,
        ),
        format_sandbox_report(&context.sandbox_status),
    ]);
    blocks.join("\n\n")
}

fn format_sandbox_report(status: &runtime::SandboxStatus) -> String {
    format!(
        "Sandbox
  Enabled           {}
  Active            {}
  Supported         {}
  In container      {}
  Requested ns      {}
  Active ns         {}
  Requested net     {}
  Active net        {}
  Filesystem mode   {}
  Filesystem active {}
  Allowed mounts    {}
  Markers           {}
  Fallback reason   {}",
        status.enabled,
        status.active,
        status.supported,
        status.in_container,
        status.requested.namespace_restrictions,
        status.namespace_active,
        status.requested.network_isolation,
        status.network_active,
        status.filesystem_mode.as_str(),
        status.filesystem_active,
        if status.allowed_mounts.is_empty() {
            "<none>".to_string()
        } else {
            status.allowed_mounts.join(", ")
        },
        if status.container_markers.is_empty() {
            "<none>".to_string()
        } else {
            status.container_markers.join(", ")
        },
        status
            .fallback_reason
            .clone()
            .unwrap_or_else(|| "<none>".to_string()),
    )
}

fn format_commit_preflight_report(branch: Option<&str>, summary: GitWorkspaceSummary) -> String {
    format!(
        "Commit
  Result           ready
  Branch           {}
  Workspace        {}
  Changed files    {}
  Action           create a git commit from the current workspace changes",
        branch.unwrap_or("unknown"),
        summary.headline(),
        summary.changed_files,
    )
}

fn format_commit_skipped_report() -> String {
    "Commit
  Result           skipped
  Reason           no workspace changes
  Action           create a git commit from the current workspace changes
  Next             /status to inspect context · /diff to inspect repo changes"
        .to_string()
}

fn print_sandbox_status_snapshot(
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader
        .load()
        .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
    let status = resolve_sandbox_status(runtime_config.sandbox(), &cwd);
    match output_format {
        CliOutputFormat::Text => println!("{}", format_sandbox_report(&status)),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&sandbox_json_value(&status))?
        ),
    }
    Ok(())
}

fn sandbox_json_value(status: &runtime::SandboxStatus) -> serde_json::Value {
    json!({
        "kind": "sandbox",
        "enabled": status.enabled,
        "active": status.active,
        "supported": status.supported,
        "in_container": status.in_container,
        "requested_namespace": status.requested.namespace_restrictions,
        "active_namespace": status.namespace_active,
        "requested_network": status.requested.network_isolation,
        "active_network": status.network_active,
        "filesystem_mode": status.filesystem_mode.as_str(),
        "filesystem_active": status.filesystem_active,
        "allowed_mounts": status.allowed_mounts,
        "markers": status.container_markers,
        "fallback_reason": status.fallback_reason,
    })
}

fn render_help_topic(topic: LocalHelpTopic) -> String {
    match topic {
        LocalHelpTopic::Status => "Status
  Usage            claw status [--output-format <format>]
  Purpose          show the local workspace snapshot without entering the REPL
  Output           model, permissions, git state, config files, and sandbox status
  Formats          text (default), json
  Related          /status · claw --resume latest /status"
            .to_string(),
        LocalHelpTopic::Sandbox => "Sandbox
  Usage            claw sandbox [--output-format <format>]
  Purpose          inspect the resolved sandbox and isolation state for the current directory
  Output           namespace, network, filesystem, and fallback details
  Formats          text (default), json
  Related          /sandbox · claw status"
            .to_string(),
        LocalHelpTopic::Doctor => "Doctor
  Usage            claw doctor [--output-format <format>]
  Purpose          diagnose local auth, config, workspace, sandbox, and build metadata
  Output           local-only health report; no provider request or session resume required
  Formats          text (default), json
  Related          /doctor · claw --resume latest /doctor"
            .to_string(),
        LocalHelpTopic::Acp => "ACP / Zed
  Usage            claw acp [serve] [--output-format <format>]
  Aliases          claw --acp · claw -acp
  Purpose          explain the current editor-facing ACP/Zed launch contract without starting the runtime
  Status           discoverability only; `serve` is a status alias and does not launch a daemon yet
  Formats          text (default), json
  Related          ROADMAP #64a (discoverability) · ROADMAP #76 (real ACP support) · claw --help"
            .to_string(),
        LocalHelpTopic::Init => "Init
  Usage            claw init [--output-format <format>]
  Purpose          create .claw/, .claw.json, .gitignore, and CLAUDE.md in the current project
  Output           list of created vs. skipped files (idempotent: safe to re-run)
  Formats          text (default), json
  Related          claw status · claw doctor"
            .to_string(),
        LocalHelpTopic::State => "State
  Usage            claw state [--output-format <format>]
  Purpose          read .claw/worker-state.json written by the interactive REPL or a one-shot prompt
  Output           worker id, model, permissions, session reference (text or json)
  Formats          text (default), json
  Produces state   `claw` (interactive REPL) or `claw prompt <text>` (one non-interactive turn)
  Observes state   `claw state` reads; clawhip/CI may poll this file without HTTP
  Exit codes       0 if state file exists and parses; 1 with actionable hint otherwise
  Related          claw status · ROADMAP #139 (this worker-concept contract)"
            .to_string(),
        LocalHelpTopic::Export => "Export
  Usage            claw export [--session <id|latest>] [--output <path>] [--output-format <format>]
  Purpose          serialize a managed session to JSON for review, transfer, or archival
  Defaults         --session latest (most recent managed session in .claw/sessions/)
  Formats          text (default), json
  Related          /session list · claw --resume latest"
            .to_string(),
        LocalHelpTopic::Version => "Version
  Usage            claw version [--output-format <format>]
  Aliases          claw --version · claw -V
  Purpose          print the claw CLI version and build metadata
  Formats          text (default), json
  Related          claw doctor (full build/auth/config diagnostic)"
            .to_string(),
        LocalHelpTopic::SystemPrompt => "System Prompt
  Usage            claw system-prompt [--cwd <path>] [--date YYYY-MM-DD] [--output-format <format>]
  Purpose          render the resolved system prompt that `claw` would send for the given cwd + date
  Options          --cwd overrides the workspace dir · --date injects a deterministic date stamp
  Formats          text (default), json
  Related          claw doctor · claw dump-manifests"
            .to_string(),
        LocalHelpTopic::DumpManifests => "Dump Manifests
  Usage            claw dump-manifests [--manifests-dir <path>] [--output-format <format>]
  Purpose          emit every skill/agent/tool manifest the resolver would load for the current cwd
  Options          --manifests-dir scopes discovery to a specific directory
  Formats          text (default), json
  Related          claw skills · claw agents · claw doctor"
            .to_string(),
        LocalHelpTopic::BootstrapPlan => "Bootstrap Plan
  Usage            claw bootstrap-plan [--output-format <format>]
  Purpose          list the ordered startup phases the CLI would execute before dispatch
  Output           phase names (text) or structured phase list (json) — primary output is the plan itself
  Formats          text (default), json
  Related          claw doctor · claw status"
            .to_string(),
    }
}

fn local_help_topic_command(topic: LocalHelpTopic) -> &'static str {
    match topic {
        LocalHelpTopic::Status => "status",
        LocalHelpTopic::Sandbox => "sandbox",
        LocalHelpTopic::Doctor => "doctor",
        LocalHelpTopic::Acp => "acp",
        LocalHelpTopic::Init => "init",
        LocalHelpTopic::State => "state",
        LocalHelpTopic::Export => "export",
        LocalHelpTopic::Version => "version",
        LocalHelpTopic::SystemPrompt => "system-prompt",
        LocalHelpTopic::DumpManifests => "dump-manifests",
        LocalHelpTopic::BootstrapPlan => "bootstrap-plan",
    }
}

fn render_export_help_json() -> serde_json::Value {
    json!({
        "kind": "help",
        "topic": "export",
        "command": "export",
        "usage": "claw export [--session <id|latest>] [--output <path>] [--output-format <format>]",
        "purpose": "serialize a managed session to JSON for review, transfer, or archival",
        "defaults": {
            "session": LATEST_SESSION_REFERENCE,
            "session_source": ".claw/sessions/",
            "output": "derived from the selected session when omitted"
        },
        "formats": ["text", "json"],
        "options": [
            {
                "name": "--session",
                "value": "<id|latest>",
                "default": LATEST_SESSION_REFERENCE,
                "description": "managed session to export"
            },
            {
                "name": "--output",
                "aliases": ["-o"],
                "value": "<path>",
                "description": "write the exported transcript to this path"
            },
            {
                "name": "--output-format",
                "value": "<format>",
                "values": ["text", "json"],
                "default": "text",
                "description": "format for the command result envelope"
            },
            {
                "name": "--help",
                "aliases": ["-h"],
                "description": "show help for the export command"
            }
        ],
        "related": ["/session list", "claw --resume latest"]
    })
}

fn render_help_topic_json(topic: LocalHelpTopic) -> serde_json::Value {
    if topic == LocalHelpTopic::Export {
        return render_export_help_json();
    }

    json!({
        "kind": "help",
        "topic": local_help_topic_command(topic),
        "command": local_help_topic_command(topic),
        "message": render_help_topic(topic),
    })
}

fn print_help_topic(
    topic: LocalHelpTopic,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => println!("{}", render_help_topic(topic)),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&render_help_topic_json(topic))?
        ),
    }
    Ok(())
}

fn acp_status_message() -> &'static str {
    "ACP/Zed editor integration is not implemented in claw-code yet. `claw acp serve` is only a discoverability alias today; it does not launch a daemon, JSON-RPC endpoint, or Zed-specific protocol endpoint. Use the normal terminal surfaces for now and track ROADMAP #76 for real ACP support."
}

fn acp_status_json() -> serde_json::Value {
    json!({
        "schema_version": "1.0",
        "kind": "acp",
        "status": "unsupported",
        "phase": "discoverability_only",
        "supported": false,
        "exit_code": 0,
        "serve_alias_only": true,
        "message": acp_status_message(),
        "launch_command": serde_json::Value::Null,
        "protocol": {
            "name": "ACP/Zed",
            "json_rpc": false,
            "daemon": false,
            "endpoint": serde_json::Value::Null,
            "serve_starts_daemon": false
        },
        "contracts": {
            "blocking_gates": [
                "task_packet_schema",
                "session_control_schema",
                "event_report_schema"
            ],
            "stable_status_surface": "claw acp [serve] --output-format json",
            "unsupported_invocation_kind": "unsupported_acp_invocation"
        },
        "aliases": ["acp", "--acp", "-acp"],
        "discoverability_tracking": "ROADMAP #64a",
        "tracking": "ROADMAP #76 / #3033 / #3004",
        "recommended_workflows": [
            "claw prompt TEXT",
            "claw",
            "claw doctor"
        ],
    })
}

fn print_acp_status(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => {
            println!(
                "ACP / Zed\n  Status           unsupported (discoverability only)\n  Exit code        0 for status queries; unsupported invocations exit 1\n  Launch           `claw acp serve` / `claw --acp` / `claw -acp` report status only; no editor daemon or JSON-RPC endpoint is available yet\n  Today            use `claw prompt`, the REPL, or `claw doctor` for local verification\n  Tracking         ROADMAP #76 / #3033 / #3004\n  Message          {}",
                acp_status_message()
            );
        }
        CliOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&acp_status_json())?);
        }
    }
    Ok(())
}

fn render_config_report(section: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let mut lines = vec![
        format!(
            "Config
  Working directory {}
  Loaded files      {}
  Merged keys       {}",
            cwd.display(),
            runtime_config.loaded_entries().len(),
            runtime_config.merged().len()
        ),
        "Discovered files".to_string(),
    ];
    for entry in discovered {
        let source = match entry.source {
            ConfigSource::User => "user",
            ConfigSource::Project => "project",
            ConfigSource::Local => "local",
        };
        let status = if runtime_config
            .loaded_entries()
            .iter()
            .any(|loaded_entry| loaded_entry.path == entry.path)
        {
            "loaded"
        } else {
            "missing"
        };
        lines.push(format!(
            "  {source:<7} {status:<7} {}",
            entry.path.display()
        ));
    }

    if let Some(section) = section {
        lines.push(format!("Merged section: {section}"));
        let value = match section {
            "env" => runtime_config.get("env"),
            "hooks" => runtime_config.get("hooks"),
            "model" => runtime_config.get("model"),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins")),
            other => {
                lines.push(format!(
                    "  Unsupported config section '{other}'. Use env, hooks, model, or plugins."
                ));
                return Ok(lines.join(
                    "
",
                ));
            }
        };
        lines.push(format!(
            "  {}",
            match value {
                Some(value) => value.render(),
                None => "<unset>".to_string(),
            }
        ));
        return Ok(lines.join(
            "
",
        ));
    }

    lines.push("Merged JSON".to_string());
    lines.push(format!("  {}", runtime_config.as_json().render()));
    Ok(lines.join(
        "
",
    ))
}

fn render_config_json(
    section: Option<&str>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let loaded_paths: Vec<_> = runtime_config
        .loaded_entries()
        .iter()
        .map(|e| e.path.display().to_string())
        .collect();

    let files: Vec<_> = discovered
        .iter()
        .map(|e| {
            let source = match e.source {
                ConfigSource::User => "user",
                ConfigSource::Project => "project",
                ConfigSource::Local => "local",
            };
            let is_loaded = runtime_config
                .loaded_entries()
                .iter()
                .any(|le| le.path == e.path);
            serde_json::json!({
                "path": e.path.display().to_string(),
                "source": source,
                "loaded": is_loaded,
            })
        })
        .collect();

    let base = serde_json::json!({
        "kind": "config",
        "cwd": cwd.display().to_string(),
        "loaded_files": loaded_paths.len(),
        "merged_keys": runtime_config.merged().len(),
        "files": files,
    });

    if let Some(section) = section {
        let section_rendered: Option<String> = match section {
            "env" => runtime_config.get("env").map(|v| v.render()),
            "hooks" => runtime_config.get("hooks").map(|v| v.render()),
            "model" => runtime_config.get("model").map(|v| v.render()),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins"))
                .map(|v| v.render()),
            other => {
                return Ok(serde_json::json!({
                    "kind": "config",
                    "section": other,
                    "ok": false,
                    "error": format!("Unsupported config section '{other}'. Use env, hooks, model, or plugins."),
                    "cwd": cwd.display().to_string(),
                    "loaded_files": loaded_paths.len(),
                    "files": files,
                }));
            }
        };
        // Parse the rendered JSON string back into serde_json::Value so that
        // section_value is a real JSON object/array in the envelope, not a quoted string.
        let section_value: serde_json::Value = section_rendered
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        let mut obj = base;
        let map = obj.as_object_mut().expect("base is object");
        map.insert(
            "section".to_string(),
            serde_json::Value::String(section.to_string()),
        );
        map.insert("section_value".to_string(), section_value);
        return Ok(obj);
    }

    Ok(base)
}

fn render_memory_report() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, DEFAULT_DATE)?;
    let mut lines = vec![format!(
        "Memory
  Working directory {}
  Instruction files {}",
        cwd.display(),
        project_context.instruction_files.len()
    )];
    if project_context.instruction_files.is_empty() {
        lines.push("Discovered files".to_string());
        lines.push(
            "  No CLAUDE instruction files discovered in the current directory ancestry."
                .to_string(),
        );
    } else {
        lines.push("Discovered files".to_string());
        for (index, file) in project_context.instruction_files.iter().enumerate() {
            let preview = file.content.lines().next().unwrap_or("").trim();
            let preview = if preview.is_empty() {
                "<empty>"
            } else {
                preview
            };
            lines.push(format!("  {}. {}", index + 1, file.path.display(),));
            lines.push(format!(
                "     lines={} preview={}",
                file.content.lines().count(),
                preview
            ));
        }
    }
    Ok(lines.join(
        "
",
    ))
}

fn render_memory_json() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, DEFAULT_DATE)?;
    let files: Vec<_> = project_context
        .instruction_files
        .iter()
        .map(|f| {
            json!({
                "path": f.path.display().to_string(),
                "lines": f.content.lines().count(),
                "preview": f.content.lines().next().unwrap_or("").trim(),
            })
        })
        .collect();
    Ok(json!({
        "kind": "memory",
        "cwd": cwd.display().to_string(),
        "instruction_files": files.len(),
        "files": files,
    }))
}

fn init_claude_md() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(initialize_repo(&cwd)?.render())
}

fn run_init(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let report = initialize_repo(&cwd)?;
    let message = report.render();
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&init_json_value(&report, &message))?
        ),
    }
    Ok(())
}

/// #142: emit first-class structured fields alongside the legacy `message`
/// string so claws can detect per-artifact state without substring matching.
fn init_json_value(report: &crate::init::InitReport, message: &str) -> serde_json::Value {
    use crate::init::InitStatus;
    json!({
        "kind": "init",
        "project_path": report.project_root.display().to_string(),
        "created": report.artifacts_with_status(InitStatus::Created),
        "updated": report.artifacts_with_status(InitStatus::Updated),
        "skipped": report.artifacts_with_status(InitStatus::Skipped),
        "artifacts": report.artifact_json_entries(),
        "next_step": crate::init::InitReport::NEXT_STEP,
        "message": message,
    })
}

fn normalize_permission_mode(mode: &str) -> Option<&'static str> {
    match mode.trim() {
        "read-only" => Some("read-only"),
        "workspace-write" => Some("workspace-write"),
        "danger-full-access" => Some("danger-full-access"),
        _ => None,
    }
}

fn render_diff_report() -> Result<String, Box<dyn std::error::Error>> {
    render_diff_report_for(&env::current_dir()?)
}

fn render_diff_report_for(cwd: &Path) -> Result<String, Box<dyn std::error::Error>> {
    // Verify we are inside a git repository before calling `git diff`.
    // Running `git diff --cached` outside a git tree produces a misleading
    // "unknown option `cached`" error because git falls back to --no-index mode.
    let in_git_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_git_repo {
        return Ok(format!(
            "Diff\n  Result           no git repository\n  Detail           {} is not inside a git project",
            cwd.display()
        ));
    }
    let staged = run_git_diff_command_in(cwd, &["diff", "--cached"])?;
    let unstaged = run_git_diff_command_in(cwd, &["diff"])?;
    if staged.trim().is_empty() && unstaged.trim().is_empty() {
        return Ok(
            "Diff\n  Result           clean working tree\n  Detail           no current changes"
                .to_string(),
        );
    }

    let mut sections = Vec::new();
    if !staged.trim().is_empty() {
        sections.push(format!("Staged changes:\n{}", staged.trim_end()));
    }
    if !unstaged.trim().is_empty() {
        sections.push(format!("Unstaged changes:\n{}", unstaged.trim_end()));
    }

    Ok(format!("Diff\n\n{}", sections.join("\n\n")))
}

fn render_diff_json_for(cwd: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let in_git_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_git_repo {
        return Ok(serde_json::json!({
            "kind": "diff",
            "result": "no_git_repo",
            "detail": format!("{} is not inside a git project", cwd.display()),
        }));
    }
    let staged = run_git_diff_command_in(cwd, &["diff", "--cached"])?;
    let unstaged = run_git_diff_command_in(cwd, &["diff"])?;
    Ok(serde_json::json!({
        "kind": "diff",
        "result": if staged.trim().is_empty() && unstaged.trim().is_empty() { "clean" } else { "changes" },
        "staged": staged.trim(),
        "unstaged": unstaged.trim(),
    }))
}

fn run_git_diff_command_in(
    cwd: &Path,
    args: &[&str],
) -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn render_teleport_report(target: &str) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;

    let file_list = Command::new("rg")
        .args(["--files"])
        .current_dir(&cwd)
        .output()?;
    let file_matches = if file_list.status.success() {
        String::from_utf8(file_list.stdout)?
            .lines()
            .filter(|line| line.contains(target))
            .take(10)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let content_output = Command::new("rg")
        .args(["-n", "-S", "--color", "never", target, "."])
        .current_dir(&cwd)
        .output()?;

    let mut lines = vec![
        "Teleport".to_string(),
        format!("  Target           {target}"),
        "  Action           search workspace files and content for the target".to_string(),
    ];
    if !file_matches.is_empty() {
        lines.push(String::new());
        lines.push("File matches".to_string());
        lines.extend(file_matches.into_iter().map(|path| format!("  {path}")));
    }

    if content_output.status.success() {
        let matches = String::from_utf8(content_output.stdout)?;
        if !matches.trim().is_empty() {
            lines.push(String::new());
            lines.push("Content matches".to_string());
            lines.push(truncate_for_prompt(&matches, 4_000));
        }
    }

    if lines.len() == 1 {
        lines.push("  Result           no matches found".to_string());
    }

    Ok(lines.join("\n"))
}

fn render_last_tool_debug_report(session: &Session) -> Result<String, Box<dyn std::error::Error>> {
    let last_tool_use = session
        .messages
        .iter()
        .rev()
        .find_map(|message| {
            message.blocks.iter().rev().find_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
        })
        .ok_or_else(|| "no prior tool call found in session".to_string())?;

    let tool_result = session.messages.iter().rev().find_map(|message| {
        message.blocks.iter().rev().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } if tool_use_id == &last_tool_use.0 => {
                Some((tool_name.clone(), output.clone(), *is_error))
            }
            _ => None,
        })
    });

    let mut lines = vec![
        "Debug tool call".to_string(),
        "  Action           inspect the last recorded tool call and its result".to_string(),
        format!("  Tool id          {}", last_tool_use.0),
        format!("  Tool name        {}", last_tool_use.1),
        "  Input".to_string(),
        indent_block(&last_tool_use.2, 4),
    ];

    match tool_result {
        Some((tool_name, output, is_error)) => {
            lines.push("  Result".to_string());
            lines.push(format!("    name           {tool_name}"));
            lines.push(format!(
                "    status         {}",
                if is_error { "error" } else { "ok" }
            ));
            lines.push(indent_block(&output, 4));
        }
        None => lines.push("  Result           missing tool result".to_string()),
    }

    Ok(lines.join("\n"))
}

fn indent_block(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_no_args(
    command_name: &str,
    args: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(args) = args.map(str::trim).filter(|value| !value.is_empty()) {
        return Err(format!(
            "{command_name} does not accept arguments. Received: {args}\nUsage: {command_name}"
        )
        .into());
    }
    Ok(())
}

fn format_bughunter_report(scope: Option<&str>) -> String {
    format!(
        "Bughunter
  Scope            {}
  Action           inspect the selected code for likely bugs and correctness issues
  Output           findings should include file paths, severity, and suggested fixes",
        scope.unwrap_or("the current repository")
    )
}

fn format_ultraplan_report(task: Option<&str>) -> String {
    format!(
        "Ultraplan
  Task             {}
  Action           break work into a multi-step execution plan
  Output           plan should cover goals, risks, sequencing, verification, and rollback",
        task.unwrap_or("the current repo work")
    )
}

fn format_pr_report(branch: &str, context: Option<&str>) -> String {
    format!(
        "PR
  Branch           {branch}
  Context          {}
  Action           draft or create a pull request for the current branch
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

fn format_issue_report(context: Option<&str>) -> String {
    format!(
        "Issue
  Context          {}
  Action           draft or create a GitHub issue from the current context
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

fn git_output(args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn git_status_ok(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")).into());
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn write_temp_text_file(
    filename: &str,
    contents: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = env::temp_dir().join(filename);
    fs::write(&path, contents)?;
    Ok(path)
}

const DEFAULT_HISTORY_LIMIT: usize = 20;

fn parse_history_count(raw: Option<&str>) -> Result<usize, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_HISTORY_LIMIT);
    };
    let parsed: usize = raw
        .parse()
        .map_err(|_| format!("history: invalid count '{raw}'. Expected a positive integer."))?;
    if parsed == 0 {
        return Err("history: count must be greater than 0.".to_string());
    }
    Ok(parsed)
}

fn format_history_timestamp(timestamp_ms: u64) -> String {
    let secs = timestamp_ms / 1_000;
    let subsec_ms = timestamp_ms % 1_000;
    let days_since_epoch = secs / 86_400;
    let seconds_of_day = secs % 86_400;
    let hours = seconds_of_day / 3_600;
    let minutes = (seconds_of_day % 3_600) / 60;
    let seconds = seconds_of_day % 60;

    let (year, month, day) = civil_from_days(i64::try_from(days_since_epoch).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{subsec_ms:03}Z")
}

// Computes civil (Gregorian) year/month/day from days since the Unix epoch
// (1970-01-01) using Howard Hinnant's `civil_from_days` algorithm.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64; // [0, 146_096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = y + i64::from(m <= 2);
    (y as i32, m as u32, d as u32)
}

fn render_prompt_history_report(entries: &[PromptHistoryEntry], limit: usize) -> String {
    if entries.is_empty() {
        return "Prompt history\n  Result           no prompts recorded yet".to_string();
    }

    let total = entries.len();
    let start = total.saturating_sub(limit);
    let shown = &entries[start..];
    let mut lines = vec![
        "Prompt history".to_string(),
        format!("  Total            {total}"),
        format!("  Showing          {} most recent", shown.len()),
        format!("  Reverse search   Ctrl-R in the REPL"),
        String::new(),
    ];
    for (offset, entry) in shown.iter().enumerate() {
        let absolute_index = start + offset + 1;
        let timestamp = format_history_timestamp(entry.timestamp_ms);
        let first_line = entry.text.lines().next().unwrap_or("").trim();
        let display = if first_line.chars().count() > 80 {
            let truncated: String = first_line.chars().take(77).collect();
            format!("{truncated}...")
        } else {
            first_line.to_string()
        };
        lines.push(format!("  {absolute_index:>3}. [{timestamp}] {display}"));
    }
    lines.join("\n")
}

fn collect_session_prompt_history(session: &Session) -> Vec<PromptHistoryEntry> {
    if !session.prompt_history.is_empty() {
        return session
            .prompt_history
            .iter()
            .map(|entry| PromptHistoryEntry {
                timestamp_ms: entry.timestamp_ms,
                text: entry.text.clone(),
            })
            .collect();
    }
    let timestamp_ms = session.updated_at_ms;
    session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter_map(|message| {
            message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(PromptHistoryEntry {
                    timestamp_ms,
                    text: text.clone(),
                }),
                _ => None,
            })
        })
        .collect()
}

fn recent_user_context(session: &Session, limit: usize) -> String {
    let requests = session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter_map(|message| {
            message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.trim().to_string()),
                _ => None,
            })
        })
        .rev()
        .take(limit)
        .collect::<Vec<_>>();

    if requests.is_empty() {
        "<no prior user messages>".to_string()
    } else {
        requests
            .into_iter()
            .rev()
            .enumerate()
            .map(|(index, text)| format!("{}. {}", index + 1, text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn truncate_for_prompt(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.trim().to_string()
    } else {
        let truncated = value.chars().take(limit).collect::<String>();
        format!("{}\n…[truncated]", truncated.trim_end())
    }
}

fn sanitize_generated_message(value: &str) -> String {
    value.trim().trim_matches('`').trim().replace("\r\n", "\n")
}

fn parse_titled_body(value: &str) -> Option<(String, String)> {
    let normalized = sanitize_generated_message(value);
    let title = normalized
        .lines()
        .find_map(|line| line.strip_prefix("TITLE:").map(str::trim))?;
    let body_start = normalized.find("BODY:")?;
    let body = normalized[body_start + "BODY:".len()..].trim();
    Some((title.to_string(), body.to_string()))
}

fn render_version_report() -> String {
    let git_sha = GIT_SHA.unwrap_or("unknown");
    let target = BUILD_TARGET.unwrap_or("unknown");
    format!(
        "Claw Code\n  Version          {VERSION}\n  Git SHA          {git_sha}\n  Target           {target}\n  Build date       {DEFAULT_DATE}"
    )
}

fn render_export_text(session: &Session) -> String {
    let mut lines = vec!["# Conversation Export".to_string(), String::new()];
    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        lines.push(format!("## {}. {role}", index + 1));
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => lines.push(text.clone()),
                ContentBlock::Thinking { .. } => {}
                ContentBlock::ToolUse { id, name, input } => {
                    lines.push(format!("[tool_use id={id} name={name}] {input}"));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => {
                    lines.push(format!(
                        "[tool_result id={tool_use_id} name={tool_name} error={is_error}] {output}"
                    ));
                }
            }
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

fn default_export_filename(session: &Session) -> String {
    let stem = session
        .messages
        .iter()
        .find_map(|message| match message.role {
            MessageRole::User => message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .map_or("conversation", |text| {
            text.lines().next().unwrap_or("conversation")
        })
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let fallback = if stem.is_empty() {
        "conversation"
    } else {
        &stem
    };
    format!("{fallback}.txt")
}

fn resolve_export_path(
    requested_path: Option<&str>,
    session: &Session,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let file_name =
        requested_path.map_or_else(|| default_export_filename(session), ToOwned::to_owned);
    let final_name = if Path::new(&file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
    {
        file_name
    } else {
        format!("{file_name}.txt")
    };
    Ok(cwd.join(final_name))
}

const SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT: usize = 280;

fn summarize_tool_payload_for_markdown(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.split_whitespace().collect::<Vec<_>>().join(" "),
    };
    if compact.is_empty() {
        return String::new();
    }
    truncate_for_summary(&compact, SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT)
}

fn run_export(
    session_reference: &str,
    output_path: Option<&Path>,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let (handle, session) = load_session_reference(session_reference)?;
    let markdown = render_session_markdown(&session, &handle.id, &handle.path);

    if let Some(path) = output_path {
        fs::write(path, &markdown)?;
        let report = format!(
            "Export\n  Result           wrote markdown transcript\n  File             {}\n  Session          {}\n  Messages         {}",
            path.display(),
            handle.id,
            session.messages.len(),
        );
        match output_format {
            CliOutputFormat::Text => println!("{report}"),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "kind": "export",
                    "message": report,
                    "session_id": handle.id,
                    "file": path.display().to_string(),
                    "messages": session.messages.len(),
                }))?
            ),
        }
        return Ok(());
    }

    match output_format {
        CliOutputFormat::Text => {
            print!("{markdown}");
            if !markdown.ends_with('\n') {
                println!();
            }
        }
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "export",
                "session_id": handle.id,
                "file": handle.path.display().to_string(),
                "messages": session.messages.len(),
                "markdown": markdown,
            }))?
        ),
    }
    Ok(())
}

fn render_session_markdown(session: &Session, session_id: &str, session_path: &Path) -> String {
    let mut lines = vec![
        "# Conversation Export".to_string(),
        String::new(),
        format!("- **Session**: `{session_id}`"),
        format!("- **File**: `{}`", session_path.display()),
        format!("- **Messages**: {}", session.messages.len()),
    ];
    if let Some(workspace_root) = session.workspace_root() {
        lines.push(format!("- **Workspace**: `{}`", workspace_root.display()));
    }
    if let Some(fork) = &session.fork {
        let branch = fork.branch_name.as_deref().unwrap_or("(unnamed)");
        lines.push(format!(
            "- **Forked from**: `{}` (branch `{branch}`)",
            fork.parent_session_id
        ));
    }
    if let Some(compaction) = &session.compaction {
        lines.push(format!(
            "- **Compactions**: {} (last removed {} messages)",
            compaction.count, compaction.removed_message_count
        ));
    }
    lines.push(String::new());
    lines.push("---".to_string());
    lines.push(String::new());

    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "System",
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Tool => "Tool",
        };
        lines.push(format!("## {}. {role}", index + 1));
        lines.push(String::new());
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => {
                    let trimmed = text.trim_end();
                    if !trimmed.is_empty() {
                        lines.push(trimmed.to_string());
                        lines.push(String::new());
                    }
                }
                ContentBlock::Thinking { .. } => {}
                ContentBlock::ToolUse { id, name, input } => {
                    lines.push(format!(
                        "**Tool call** `{name}` _(id `{}`)_",
                        short_tool_id(id)
                    ));
                    let summary = summarize_tool_payload_for_markdown(input);
                    if !summary.is_empty() {
                        lines.push(format!("> {summary}"));
                    }
                    lines.push(String::new());
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => {
                    let status = if *is_error { "error" } else { "ok" };
                    lines.push(format!(
                        "**Tool result** `{tool_name}` _(id `{}`, {status})_",
                        short_tool_id(tool_use_id)
                    ));
                    let summary = summarize_tool_payload_for_markdown(output);
                    if !summary.is_empty() {
                        lines.push(format!("> {summary}"));
                    }
                    lines.push(String::new());
                }
            }
        }
        if let Some(usage) = message.usage {
            lines.push(format!(
                "_tokens: in={} out={} cache_create={} cache_read={}_",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
            ));
            lines.push(String::new());
        }
    }
    lines.join("\n")
}

fn short_tool_id(id: &str) -> String {
    let char_count = id.chars().count();
    if char_count <= 12 {
        return id.to_string();
    }
    let prefix: String = id.chars().take(12).collect();
    format!("{prefix}…")
}

fn build_system_prompt(model: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let extras = load_prompt_extras(&cwd);
    Ok(load_system_prompt_with_extras(
        cwd,
        DEFAULT_DATE,
        env::consts::OS,
        "unknown",
        model_family_identity_for(model),
        extras,
    )?)
}

/// Load optional system prompt extras (persistent memory + repository map).
///
/// Both are best-effort: if the memory file does not exist or the RepoMap
/// fails to render, we silently fall back to no extras rather than blocking
/// startup. The persistent memory is loaded-and-frozen so its snapshot stays
/// byte-stable for the session (preserving the prompt-cache prefix).
///
/// **性能守卫**：当 cwd 是"宽泛目录"（用户主目录、Windows 系统目录、驱动器根等）
/// 时跳过 RepoMap 扫描。RepoMap 会递归扫描整个 cwd 子树为每个源文件计算符号树，
/// 在用户主目录下会遍历 AppData / .cargo / .rustup 等数十万文件，耗时 30+ 秒。
/// persistent_memory 不受影响（只是读一个 JSON 文件）。
fn load_prompt_extras(cwd: &Path) -> SystemPromptExtras {
    let t0 = std::time::Instant::now();
    let persistent_memory = {
        let memory_path = cwd.join(".claw").join("memory.json");
        if memory_path.exists() {
            Some(runtime::PersistentMemory::load_and_freeze(&memory_path))
        } else {
            None
        }
    };
    let t_mem = t0.elapsed();

    // 宽泛目录守卫：用户主目录、Windows 系统目录、驱动器根等。
    // 在这些目录下 RepoMap 扫描会遍历数十万文件，耗时 30+ 秒，
    // 且对用户实际工作毫无价值（用户主目录不是代码仓库）。
    let is_broad_cwd = is_broad_working_directory(cwd);
    let repomap = if is_broad_cwd {
        eprintln!(
            "[load_prompt_extras] skipping RepoMap: cwd is a broad directory ({})",
            cwd.display()
        );
        None
    } else {
        let mut map = RepoMap::new(cwd).with_max_tokens(1024);
        let rendered = map.render();
        if rendered.trim().is_empty() {
            None
        } else {
            Some(rendered)
        }
    };
    let t_map = t0.elapsed();
    eprintln!(
        "[timing] load_prompt_extras: memory={:?} repomap={:?} broad_cwd={} (cwd={})",
        t_mem, t_map, is_broad_cwd, cwd.display()
    );
    SystemPromptExtras {
        persistent_memory,
        repomap,
    }
}

/// 判断 cwd 是否是"宽泛目录"——即不应触发 RepoMap 全量扫描的目录。
///
/// 判定规则：
/// 1. Windows: cwd == %USERPROFILE%（用户主目录，如 `C:\Users\38225`）
/// 2. Windows: cwd == 系统盘根（如 `C:\`、`D:\`）
/// 3. Windows: cwd 在 `C:\Windows` 下
/// 4. Unix: cwd == `/` 或 `~`（home 目录）
/// 5. cwd 没有 `.git` 子目录且没有 `Cargo.toml` / `package.json` / `pyproject.toml`
///    等典型项目标识文件（这是宽泛的"不像项目目录"启发式）
fn is_broad_working_directory(cwd: &Path) -> bool {
    use std::path::PathBuf;

    // 规则 1-4: 与用户主目录、系统根目录比较
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if cwd == home.as_path() {
            return true;
        }
    }
    if let Some(userprofile) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        if cwd == userprofile.as_path() {
            return true;
        }
    }

    // 规则 2: 驱动器根目录（Windows `C:\` / `D:\`）或 Unix `/`
    let cwd_str = cwd.to_string_lossy();
    if cwd_str.len() <= 3 && cwd_str.ends_with('\\') {
        return true; // 如 `C:\` `D:\`
    }
    if cwd == std::path::Path::new("/") {
        return true;
    }

    // 规则 3: cwd 在 Windows 系统目录下
    if let Ok(windir) = std::env::var("WINDIR") {
        let windir = PathBuf::from(windir);
        if cwd.starts_with(&windir) {
            return true;
        }
    }

    // 规则 5: 没有任何项目标识文件 → 不像代码仓库 → 跳过 RepoMap
    const PROJECT_MARKERS: &[&str] = &[
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "setup.py",
        ".svn",
        "Makefile",
    ];
    let has_project_marker = PROJECT_MARKERS
        .iter()
        .any(|marker| cwd.join(marker).exists());
    !has_project_marker
}

struct PluginsCommandPayload {
    message: String,
    reload_runtime: bool,
    status: &'static str,
    config_load_error: Option<String>,
    plugins: Vec<Value>,
    load_failures: Vec<Value>,
}

fn plugins_command_payload_for(
    cwd: &Path,
    action: Option<&str>,
    target: Option<&str>,
) -> Result<PluginsCommandPayload, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let (runtime_config, config_load_error) = match loader.load() {
        Ok(runtime_config) => (runtime_config, None),
        Err(error) => (runtime::RuntimeConfig::empty(), Some(error.to_string())),
    };
    let mut manager = build_plugin_manager(cwd, &loader, &runtime_config);
    let result = handle_plugins_slash_command(action, target, &mut manager)?;
    let report = manager.installed_plugin_registry_report()?;
    Ok(plugins_command_payload_from_result(
        result,
        config_load_error,
        &report,
    ))
}

fn plugins_command_payload_from_result(
    result: PluginsCommandResult,
    config_load_error: Option<String>,
    report: &plugins::PluginRegistryReport,
) -> PluginsCommandPayload {
    let failures = report.failures();
    let status = if config_load_error.is_some() || !failures.is_empty() {
        "degraded"
    } else {
        "ok"
    };
    let message = match config_load_error.as_deref() {
        Some(error) => format!(
            "Config load error\n  Status           fail\n  Summary          runtime config failed to load; reporting partial plugins view\n  Details          {error}\n  Hint             `claw doctor` classifies config parse errors; fix the listed field and rerun\n\n{}",
            result.message
        ),
        None => result.message,
    };
    PluginsCommandPayload {
        message,
        reload_runtime: result.reload_runtime,
        status,
        config_load_error,
        plugins: report.summaries().iter().map(plugin_summary_json).collect(),
        load_failures: failures.iter().map(plugin_load_failure_json).collect(),
    }
}

fn build_runtime_plugin_state() -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load()?;
    build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
}

fn build_runtime_plugin_state_with_loader(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let plugin_manager = build_plugin_manager(cwd, loader, runtime_config);
    let plugin_registry = plugin_manager.plugin_registry()?;
    let plugin_hook_config =
        runtime_hook_config_from_plugin_hooks(plugin_registry.aggregated_hooks()?);
    let feature_config = runtime_config
        .feature_config()
        .clone()
        .with_hooks(runtime_config.hooks().merged(&plugin_hook_config));
    let (mcp_state, mut runtime_tools) = build_runtime_mcp_state(runtime_config)?;
    // Register the session_search tool. Its execution is intercepted by
    // ConversationRuntime::run_turn (routed to HistoryIndex), not handled by
    // CliToolExecutor — but the spec must be in the registry so the model
    // knows the tool exists and can call it.
    runtime_tools.push(RuntimeToolDefinition {
        name: "session_search".to_string(),
        description: Some(
            "Search the conversation history using full-text search. Use this \
             to recall specific past discussions, decisions, or file references \
             that may not be in the current context window."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Full-text search query. Supports FTS5 syntax."
                },
                "top_k": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 10).",
                    "default": 10
                }
            },
            "required": ["query"]
        }),
        required_permission: PermissionMode::ReadOnly,
    });
    let tool_registry = GlobalToolRegistry::with_plugin_tools(plugin_registry.aggregated_tools()?)?
        .with_runtime_tools(runtime_tools)?;
    Ok(RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        mcp_state,
    })
}

fn build_plugin_manager(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> PluginManager {
    let plugin_settings = runtime_config.plugins();
    let mut plugin_config = PluginManagerConfig::new(loader.config_home().to_path_buf());
    plugin_config.enabled_plugins = plugin_settings.enabled_plugins().clone();
    plugin_config.external_dirs = plugin_settings
        .external_directories()
        .iter()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path))
        .collect();
    plugin_config.install_root = plugin_settings
        .install_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.registry_path = plugin_settings
        .registry_path()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.bundled_root = plugin_settings
        .bundled_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    PluginManager::new(plugin_config)
}

fn resolve_plugin_path(cwd: &Path, config_home: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if value.starts_with('.') {
        cwd.join(path)
    } else {
        config_home.join(path)
    }
}

fn runtime_hook_config_from_plugin_hooks(hooks: PluginHooks) -> runtime::RuntimeHookConfig {
    runtime::RuntimeHookConfig::new(
        hooks.pre_tool_use,
        hooks.post_tool_use,
        hooks.post_tool_use_failure,
    )
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn build_runtime(
    session: Session,
    session_id: &str,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let runtime_plugin_state = build_runtime_plugin_state()?;
    build_runtime_with_plugin_state(
        session,
        session_id,
        model,
        system_prompt,
        enable_tools,
        emit_output,
        allowed_tools,
        permission_mode,
        progress_reporter,
        runtime_plugin_state,
    )
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn build_runtime_with_plugin_state(
    mut session: Session,
    session_id: &str,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
    runtime_plugin_state: RuntimePluginState,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    // Persist the model in session metadata so resumed sessions can report it.
    if session.model.is_none() {
        session.model = Some(model.clone());
    }
    // 从 session 提取工作区根白名单（主 cwd 根 + `--add-dir` 额外根），
    // 注入到 tool_registry，让 `classify_*_permission_with_roots` 在工具执行路径生效。
    let workspace_roots = session.workspace_roots();
    let RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        mcp_state,
    } = runtime_plugin_state;
    let tool_registry = tool_registry.with_workspace_roots(workspace_roots);
    plugin_registry.initialize()?;
    let policy = permission_policy(permission_mode, &feature_config, &tool_registry)
        .map_err(std::io::Error::other)?;
    let mut runtime = ConversationRuntime::new_with_features(
        session,
        AnthropicRuntimeClient::new(
            session_id,
            model,
            enable_tools,
            emit_output,
            allowed_tools.clone(),
            tool_registry.clone(),
            progress_reporter,
        )?,
        CliToolExecutor::new(
            allowed_tools.clone(),
            emit_output,
            tool_registry.clone(),
            mcp_state.clone(),
        ),
        policy,
        system_prompt,
        &feature_config,
    );
    if emit_output {
        runtime = runtime.with_hook_progress_reporter(Box::new(CliHookProgressReporter));
    }
    // Attach persistent memory for nudge curation. Loaded-and-frozen so the
    // nudge layer can write new entries to disk while the prompt's frozen
    // snapshot (loaded separately in load_prompt_extras) stays byte-stable
    // for the session — preserving the prompt-cache prefix.
    if let Ok(cwd) = env::current_dir() {
        let memory_path = cwd.join(".claw").join("memory.json");
        if memory_path.exists() {
            let memory = runtime::PersistentMemory::load_and_freeze(&memory_path);
            runtime = runtime.with_persistent_memory(memory);
        }
    }
    Ok(BuiltRuntime::new(runtime, plugin_registry, mcp_state))
}

struct CliHookProgressReporter;

impl runtime::HookProgressReporter for CliHookProgressReporter {
    fn on_event(&mut self, event: &runtime::HookProgressEvent) {
        match event {
            runtime::HookProgressEvent::Started {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
            runtime::HookProgressEvent::Completed {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook done {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
            runtime::HookProgressEvent::Cancelled {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook cancelled {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
        }
    }
}

struct CliPermissionPrompter {
    current_mode: PermissionMode,
}

impl CliPermissionPrompter {
    fn new(current_mode: PermissionMode) -> Self {
        Self { current_mode }
    }
}

impl runtime::PermissionPrompter for CliPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        println!();
        println!("Permission approval required");
        println!("  Tool             {}", request.tool_name);
        println!("  Current mode     {}", self.current_mode.as_str());
        println!("  Required mode    {}", request.required_mode.as_str());
        if let Some(reason) = &request.reason {
            println!("  Reason           {reason}");
        }
        println!("  Input            {}", request.input);
        print!("Approve this tool call? [y/N]: ");
        let _ = io::stdout().flush();

        let mut response = String::new();
        match io::stdin().read_line(&mut response) {
            Ok(_) => {
                let normalized = response.trim().to_ascii_lowercase();
                if matches!(normalized.as_str(), "y" | "yes") {
                    runtime::PermissionPromptDecision::Allow
                } else {
                    runtime::PermissionPromptDecision::Deny {
                        reason: format!(
                            "tool '{}' denied by user approval prompt",
                            request.tool_name
                        ),
                    }
                }
            }
            Err(error) => runtime::PermissionPromptDecision::Deny {
                reason: format!("permission approval failed: {error}"),
            },
        }
    }
}

// NOTE: Despite the historical name `AnthropicRuntimeClient`, this struct
// now holds an `ApiProviderClient` which dispatches to Anthropic, xAI,
// OpenAI, or DashScope at construction time based on
// `detect_provider_kind(&model)`. The struct name is kept to avoid
// churning `BuiltRuntime` and every Deref/DerefMut site that references
// it. See ROADMAP #29 for the provider-dispatch routing fix.
struct AnthropicRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: ApiProviderClient,
    session_id: String,
    model: String,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    progress_reporter: Option<InternalPromptProgressReporter>,
    reasoning_effort: Option<String>,
}

impl AnthropicRuntimeClient {
    fn new(
        session_id: &str,
        model: String,
        enable_tools: bool,
        emit_output: bool,
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        progress_reporter: Option<InternalPromptProgressReporter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Dispatch to the correct provider at construction time.
        // `ApiProviderClient` (exposed by the api crate as
        // `ProviderClient`) is an enum over Anthropic / xAI / OpenAI
        // variants, where xAI and OpenAI both use the OpenAI-compat
        // wire format under the hood. We consult
        // `detect_provider_kind(&resolved_model)` so model-name prefix
        // routing (`openai/`, `gpt-`, `grok`, `qwen/`) wins over
        // env-var presence.
        //
        // For Anthropic we build the client directly instead of going
        // through `ApiProviderClient::from_model_with_anthropic_auth`
        // so we can explicitly apply `api::read_base_url()` — that
        // reads `ANTHROPIC_BASE_URL` and is required for the local
        // mock-server test harness
        // (`crates/rusty-claude-cli/tests/compact_output.rs`) to point
        // claw at its fake Anthropic endpoint. We also attach a
        // session-scoped prompt cache on the Anthropic path; the
        // prompt cache is Anthropic-only so non-Anthropic variants
        // skip it.
        let resolved_model = api::resolve_model_alias(&model);
        let client = match detect_provider_kind(&resolved_model) {
            ProviderKind::Anthropic => {
                let auth = resolve_cli_auth_source()?;
                let inner = AnthropicClient::from_auth(auth)
                    .with_base_url(api::read_base_url())
                    .with_prompt_cache(PromptCache::new(session_id));
                ApiProviderClient::Anthropic(inner)
            }
            ProviderKind::Xai | ProviderKind::OpenAi => {
                // The api crate's `ProviderClient::from_model_with_anthropic_auth`
                // with `None` for the anthropic auth routes via
                // `detect_provider_kind` and builds an
                // `OpenAiCompatClient::from_env` with the matching
                // `OpenAiCompatConfig` (openai / xai / dashscope).
                // That reads the correct API-key env var and BASE_URL
                // override internally, so this one call covers OpenAI,
                // OpenRouter, xAI, DashScope, Ollama, and any other
                // OpenAI-compat endpoint users configure via
                // `OPENAI_BASE_URL` / `XAI_BASE_URL` / `DASHSCOPE_BASE_URL`.
                ApiProviderClient::from_model_with_anthropic_auth(&resolved_model, None)?
            }
        };
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            session_id: session_id.to_string(),
            model,
            enable_tools,
            emit_output,
            allowed_tools,
            tool_registry,
            progress_reporter,
            reasoning_effort: None,
        })
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }
}

fn resolve_cli_auth_source() -> Result<AuthSource, Box<dyn std::error::Error>> {
    Ok(resolve_cli_auth_source_for_cwd()?)
}

#[allow(clippy::result_large_err)]
fn resolve_cli_auth_source_for_cwd() -> Result<AuthSource, api::ApiError> {
    resolve_startup_auth_source(|| Ok(None))
}

/// Convert a [`SystemPromptSplit`] into an Anthropic-compatible
/// [`SystemContent`] with prompt-caching markers.
///
/// The static (stable) sections are emitted as text blocks with
/// `cache_control: {type: "ephemeral"}` on the **last** static block,
/// marking the cache prefix boundary. Dynamic sections are emitted as
/// plain text blocks (no cache marker) so they re-flow every turn.
///
/// Returns `None` if both static and dynamic sections are empty, so
/// `MessageRequest.system` serializes to absent rather than `null`/`[]`.
fn build_system_blocks(split: &SystemPromptSplit) -> Option<SystemContent> {
    let mut blocks: Vec<SystemBlock> = Vec::new();

    // Static sections: mark the last one with cache_control.
    let static_len = split.static_sections.len();
    for (index, section) in split.static_sections.iter().enumerate() {
        let mut block = SystemBlock::new(section.clone());
        if index == static_len.saturating_sub(1) && static_len > 0 {
            block = block.with_cache_control(CacheControl::ephemeral());
        }
        blocks.push(block);
    }

    // Dynamic sections: no cache marker.
    for section in &split.dynamic_sections {
        blocks.push(SystemBlock::new(section.clone()));
    }

    if blocks.is_empty() {
        None
    } else {
        Some(SystemContent::from_blocks(blocks))
    }
}

/// Mark the last tool in a list with `cache_control: {type: "ephemeral"}` so
/// the Anthropic API caches the tools array prefix. No-op for empty lists.
///
/// Tools definitions are large (JSON schemas) and stable across turns within
/// a session, so caching them yields significant input-token savings.
fn mark_last_tool_with_cache_control(tools: &mut [ToolDefinition]) {
    if let Some(last) = tools.last_mut() {
        last.cache_control = Some(CacheControl::ephemeral());
    }
}

impl ApiClient for AnthropicRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        let is_post_tool = request_ends_with_tool_result(&request);

        // Extract system-role messages so they route through MessageRequest.system
        // (eligible for prompt caching) instead of being flattened into messages.
        let (system_text, filtered_messages) = extract_system_messages(&request.messages);
        let mut split = request.system_prompt;
        if !system_text.is_empty() {
            split.dynamic_sections.push(system_text);
        }

        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model),
            messages: convert_messages(&filtered_messages, &self.model),
            system: build_system_blocks(&split),
            tools: self.enable_tools.then(|| {
                let mut tools = filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref());
                mark_last_tool_with_cache_control(&mut tools);
                tools
            }),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            reasoning_effort: self.reasoning_effort.clone(),
            ..Default::default()
        };

        self.runtime.block_on(async {
            // Single attempt: re-sending the full request on stall doubles token
            // usage for no reliability gain (the stall is typically a transient
            // server/network issue that a retry of the identical request is
            // unlikely to fix). Let the caller handle the error.
            self.consume_stream(&message_request, is_post_tool).await
        })
    }
}

impl AnthropicRuntimeClient {
    /// Consume a single streaming response, optionally applying a stall
    /// timeout on the first event for post-tool continuations.
    #[allow(clippy::too_many_lines)]
    async fn consume_stream(
        &self,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut stream = self
            .client
            .stream_message(message_request)
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut stdout = io::stdout();
        let mut sink = io::sink();
        let out: &mut dyn Write = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        let renderer = TerminalRenderer::new();
        let mut markdown_stream = MarkdownStreamState::default();
        let mut events = Vec::new();
        let mut pending_tool: Option<(String, String, String)> = None;
        let mut block_has_thinking_summary = false;
        let mut saw_stop = false;
        let mut received_any_event = false;

        loop {
            let next = if apply_stall_timeout && !received_any_event {
                match tokio::time::timeout(POST_TOOL_STALL_TIMEOUT, stream.next_event()).await {
                    Ok(inner) => inner.map_err(|error| {
                        RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                    })?,
                    Err(_elapsed) => {
                        return Err(RuntimeError::new(
                            "post-tool stall: model did not respond within timeout",
                        ));
                    }
                }
            } else {
                stream.next_event().await.map_err(|error| {
                    RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                })?
            };

            let Some(event) = next else {
                break;
            };
            received_any_event = true;

            match event {
                ApiStreamEvent::MessageStart(start) => {
                    for block in start.message.content {
                        push_output_block(
                            block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                            &mut block_has_thinking_summary,
                        )?;
                    }
                }
                ApiStreamEvent::ContentBlockStart(start) => {
                    push_output_block(
                        start.content_block,
                        out,
                        &mut events,
                        &mut pending_tool,
                        true,
                        &mut block_has_thinking_summary,
                    )?;
                }
                ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                    ContentBlockDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            if let Some(progress_reporter) = &self.progress_reporter {
                                progress_reporter.mark_text_phase(&text);
                            }
                            if let Some(rendered) = markdown_stream.push(&renderer, &text) {
                                write!(out, "{rendered}")
                                    .and_then(|()| out.flush())
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                            }
                            events.push(AssistantEvent::TextDelta(text));
                        }
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        if let Some((_, _, input)) = &mut pending_tool {
                            input.push_str(&partial_json);
                        }
                    }
                    ContentBlockDelta::ThinkingDelta { .. } => {
                        if !block_has_thinking_summary {
                            render_thinking_block_summary(out, None, false)?;
                            block_has_thinking_summary = true;
                        }
                    }
                    ContentBlockDelta::SignatureDelta { .. } => {}
                },
                ApiStreamEvent::ContentBlockStop(_) => {
                    block_has_thinking_summary = false;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    if let Some((id, name, input)) = pending_tool.take() {
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_tool_phase(&name, &input);
                        }
                        // Display tool call now that input is fully accumulated
                        writeln!(out, "\n{}", format_tool_call_start(&name, &input))
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        events.push(AssistantEvent::ToolUse { id, name, input });
                    }
                }
                ApiStreamEvent::MessageDelta(delta) => {
                    events.push(AssistantEvent::Usage(delta.usage.token_usage()));
                }
                ApiStreamEvent::MessageStop(_) => {
                    saw_stop = true;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    events.push(AssistantEvent::MessageStop);
                }
            }
        }

        push_prompt_cache_record(&self.client, &mut events);

        if !saw_stop
            && events.iter().any(|event| {
                matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                    || matches!(event, AssistantEvent::ToolUse { .. })
            })
        {
            events.push(AssistantEvent::MessageStop);
        }

        if events
            .iter()
            .any(|event| matches!(event, AssistantEvent::MessageStop))
        {
            return Ok(events);
        }

        let response = self
            .client
            .send_message(&MessageRequest {
                stream: false,
                ..message_request.clone()
            })
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut events = response_to_events(response, out)?;
        push_prompt_cache_record(&self.client, &mut events);
        Ok(events)
    }
}

/// Returns `true` when the conversation ends with a tool-result message,
/// meaning the model is expected to continue after tool execution.
fn request_ends_with_tool_result(request: &ApiRequest) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Tool)
}

fn format_user_visible_api_error(session_id: &str, error: &api::ApiError) -> String {
    if error.is_context_window_failure() {
        format_context_window_blocked_error(session_id, error)
    } else if error.is_generic_fatal_wrapper() {
        let mut qualifiers = vec![format!("session {session_id}")];
        if let Some(request_id) = error.request_id() {
            qualifiers.push(format!("trace {request_id}"));
        }
        format!(
            "{} ({}): {}",
            error.safe_failure_class(),
            qualifiers.join(", "),
            error
        )
    } else {
        error.to_string()
    }
}

fn format_context_window_blocked_error(session_id: &str, error: &api::ApiError) -> String {
    let mut lines = vec![
        "Context window blocked".to_string(),
        "  Failure class    context_window_blocked".to_string(),
        format!("  Session          {session_id}"),
    ];

    if let Some(request_id) = error.request_id() {
        lines.push(format!("  Trace            {request_id}"));
    }

    match error {
        api::ApiError::ContextWindowExceeded {
            model,
            estimated_input_tokens,
            requested_output_tokens,
            estimated_total_tokens,
            context_window_tokens,
        } => {
            lines.push(format!("  Model            {model}"));
            lines.push(format!(
                "  Input estimate   ~{estimated_input_tokens} tokens (heuristic)"
            ));
            lines.push(format!(
                "  Requested output {requested_output_tokens} tokens"
            ));
            lines.push(format!(
                "  Total estimate   ~{estimated_total_tokens} tokens (heuristic)"
            ));
            lines.push(format!("  Context window   {context_window_tokens} tokens"));
        }
        api::ApiError::Api { message, body, .. } => {
            let detail = message.as_deref().unwrap_or(body).trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        api::ApiError::RetriesExhausted { last_error, .. } => {
            let detail = match last_error.as_ref() {
                api::ApiError::Api { message, body, .. } => message.as_deref().unwrap_or(body),
                other => return format_context_window_blocked_error(session_id, other),
            }
            .trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        _ => {}
    }

    lines.push(String::new());
    lines.push("Recovery".to_string());
    lines.push("  Compact          /compact".to_string());
    lines.push(format!(
        "  Resume compact   claw --resume {session_id} /compact"
    ));
    lines.push("  Fresh session    /clear --confirm".to_string());
    lines.push(
        "  Reduce scope     remove large pasted context/files or ask for a smaller slice"
            .to_string(),
    );
    lines.push("  Retry            rerun after compacting or reducing the request".to_string());

    lines.join("\n")
}

fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn collect_tool_uses(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .assistant_messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

fn collect_tool_results(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .tool_results
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => Some(json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "output": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

fn collect_prompt_cache_events(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .prompt_cache_events
        .iter()
        .map(|event| {
            json!({
                "unexpected": event.unexpected,
                "reason": event.reason,
                "previous_cache_read_input_tokens": event.previous_cache_read_input_tokens,
                "current_cache_read_input_tokens": event.current_cache_read_input_tokens,
                "token_drop": event.token_drop,
            })
        })
        .collect()
}

/// Slash commands that are registered in the spec list but not yet implemented
/// in this build. Used to filter both REPL completions and help output so the
/// discovery surface only shows commands that actually work (ROADMAP #39).
const STUB_COMMANDS: &[&str] = &[
    "login",
    "logout",
    "vim",
    "upgrade",
    "share",
    "feedback",
    "files",
    "fast",
    "exit",
    "summary",
    "desktop",
    "brief",
    "advisor",
    "stickers",
    "insights",
    "thinkback",
    "release-notes",
    "security-review",
    "keybindings",
    "privacy-settings",
    "plan",
    "review",
    "tasks",
    "theme",
    "voice",
    "usage",
    "rename",
    "copy",
    "hooks",
    "context",
    "color",
    "effort",
    "branch",
    "rewind",
    "ide",
    "tag",
    "output-style",
    "add-dir",
    // Spec entries with no parse arm — produce circular "Did you mean" error
    // without this guard. Adding here routes them to the proper unsupported
    // message and excludes them from REPL completions / help.
    // NOTE: do NOT add "stats", "tokens", "cache" — they are implemented.
    "allowed-tools",
    "bookmarks",
    "workspace",
    "reasoning",
    "budget",
    "rate-limit",
    "changelog",
    "diagnostics",
    "metrics",
    "tool-details",
    "focus",
    "unfocus",
    "pin",
    "unpin",
    "language",
    "profile",
    "max-tokens",
    "temperature",
    "system-prompt",
    "notifications",
    "telemetry",
    "env",
    "project",
    "terminal-setup",
    "api-key",
    "reset",
    "undo",
    "stop",
    "retry",
    "paste",
    "screenshot",
    "image",
    "search",
    "listen",
    "speak",
    "format",
    "test",
    "lint",
    "build",
    "run",
    "git",
    "stash",
    "blame",
    "log",
    "cron",
    "team",
    "benchmark",
    "migrate",
    "templates",
    "explain",
    "refactor",
    "docs",
    "fix",
    "perf",
    "chat",
    "web",
    "map",
    "symbols",
    "references",
    "definition",
    "hover",
    "autofix",
    "multi",
    "macro",
    "alias",
    "parallel",
    "subagent",
    "agent",
];

fn slash_command_completion_candidates_with_sessions(
    model: &str,
    active_session_id: Option<&str>,
    recent_session_ids: Vec<String>,
) -> Vec<String> {
    let mut completions = BTreeSet::new();

    for spec in slash_command_specs() {
        if STUB_COMMANDS.contains(&spec.name) {
            continue;
        }
        completions.insert(format!("/{}", spec.name));
        for alias in spec.aliases {
            if !STUB_COMMANDS.contains(alias) {
                completions.insert(format!("/{alias}"));
            }
        }
    }

    for candidate in [
        "/bughunter ",
        "/clear --confirm",
        "/config ",
        "/config env",
        "/config hooks",
        "/config model",
        "/config plugins",
        "/mcp ",
        "/mcp list",
        "/mcp show ",
        "/export ",
        "/issue ",
        "/model ",
        "/model opus",
        "/model sonnet",
        "/model haiku",
        "/permissions ",
        "/permissions read-only",
        "/permissions workspace-write",
        "/permissions danger-full-access",
        "/plugin list",
        "/plugin install ",
        "/plugin enable ",
        "/plugin disable ",
        "/plugin uninstall ",
        "/plugin update ",
        "/plugins list",
        "/pr ",
        "/resume ",
        "/session list",
        "/session switch ",
        "/session fork ",
        "/teleport ",
        "/ultraplan ",
        "/agents help",
        "/mcp help",
        "/skills help",
    ] {
        completions.insert(candidate.to_string());
    }

    if !model.trim().is_empty() {
        completions.insert(format!("/model {}", resolve_model_alias(model)));
        completions.insert(format!("/model {model}"));
    }

    if let Some(active_session_id) = active_session_id.filter(|value| !value.trim().is_empty()) {
        completions.insert(format!("/resume {active_session_id}"));
        completions.insert(format!("/session switch {active_session_id}"));
    }

    for session_id in recent_session_ids
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .take(10)
    {
        completions.insert(format!("/resume {session_id}"));
        completions.insert(format!("/session switch {session_id}"));
    }

    completions.into_iter().collect()
}

/// 工具卡片左边框（ANSI 245 灰色）。用于卡片体内每行前缀，视觉上把
/// "调用详情"和"工具结果"框在同一个卡片容器里。
const TOOL_CARD_PREFIX: &str = "\x1b[38;5;245m│\x1b[0m ";

/// 把多行字符串的每一行都加上 `TOOL_CARD_PREFIX` 前缀。
/// 空行也保留前缀，保持卡片左边框连续。
fn indent_with_card_prefix(content: &str) -> String {
    content
        .lines()
        .map(|line| format!("{TOOL_CARD_PREFIX}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 渲染工具卡片的开头部分（顶部边框 + 标题行 + 详情行）。
/// 不含底部闭合边框 `╰─╯`——底部边框由 `format_tool_result_card_close` 补上，
/// 这样调用详情和工具结果在同一卡片内。
fn format_tool_call_start(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));

    let detail = match name {
        "bash" | "Bash" => format_bash_call(&parsed),
        "read_file" | "Read" => {
            let path = extract_tool_path(&parsed);
            format!("\x1b[2m📄 Reading {path}…\x1b[0m")
        }
        "write_file" | "Write" => {
            let path = extract_tool_path(&parsed);
            let lines = parsed
                .get("content")
                .and_then(|value| value.as_str())
                .map_or(0, |content| content.lines().count());
            format!("\x1b[1;32m✏️ Writing {path}\x1b[0m \x1b[2m({lines} lines)\x1b[0m")
        }
        "edit_file" | "Edit" => {
            let path = extract_tool_path(&parsed);
            let old_value = parsed
                .get("old_string")
                .or_else(|| parsed.get("oldString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let new_value = parsed
                .get("new_string")
                .or_else(|| parsed.get("newString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            format!(
                "\x1b[1;33m📝 Editing {path}\x1b[0m{}",
                format_patch_preview(old_value, new_value)
                    .map(|preview| format!("\n{preview}"))
                    .unwrap_or_default()
            )
        }
        "glob_search" | "Glob" => format_search_start("🔎 Glob", &parsed),
        "grep_search" | "Grep" => format_search_start("🔎 Grep", &parsed),
        "PowerShell" | "powershell" => {
            let command = parsed
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if command.is_empty() {
                summarize_tool_payload(input)
            } else {
                format!("\x1b[1;34m🖥️ PowerShell\x1b[0m \x1b[2m{command}\x1b[0m")
            }
        }
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("?")
            .to_string(),
        _ => summarize_tool_payload(input),
    };

    // 顶部边框宽度对齐 name + 4 (两侧空格) + 4 (装饰 ╭─ ─╮)
    let border = "─".repeat(name.len() + 8);
    let indented_detail = indent_with_card_prefix(&detail);
    format!(
        "\x1b[38;5;245m╭─ \x1b[1;36m{name}\x1b[0;38;5;245m ─╮\x1b[0m\n{indented_detail}"
    )
}

/// 渲染工具卡片的闭合部分：工具结果（带左边框对齐）+ 底部边框。
/// 在 Full 模式下被调用，把工具结果嵌入到与调用详情同一个卡片容器内。
/// 内层复用 `format_tool_result` 的解析逻辑，外层加卡片左边框对齐 + 底部闭合边框。
fn format_tool_result_card_close(name: &str, output: &str, is_error: bool) -> String {
    let inner = format_tool_result(name, output, is_error, OutputVerbosity::Full);
    let border = "─".repeat(name.len() + 8);
    // 在详情和结果之间加一个空行（带左边框），视觉分隔调用与结果
    format!(
        "\n{TOOL_CARD_PREFIX}\n{}\n\x1b[38;5;245m╰{border}╯\x1b[0m",
        indent_with_card_prefix(&inner)
    )
}

/// User 消息卡片前缀（ANSI 111 浅蓝色），与工具卡片的灰色前缀视觉区分。
const USER_CARD_PREFIX: &str = "\x1b[38;5;111m│\x1b[0m ";

/// 渲染 user 消息卡片：顶部边框 + 用户输入（每行加 `│` 前缀）+ 底部边框。
/// 颜色用浅蓝（38;5;111）与工具卡片灰色（38;5;245）区分。
/// 调用方需在打印卡片前清除 rustyline 的 echo 行（单行输入时），否则会重复显示。
fn format_user_message_card(input: &str) -> String {
    let label = "you";
    let border = "─".repeat(label.len() + 8);
    let indented: String = input
        .lines()
        .map(|line| format!("{USER_CARD_PREFIX}{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "\x1b[38;5;111m╭─ \x1b[1;37m{label}\x1b[0;38;5;111m ─╮\x1b[0m\n{indented}\n\x1b[38;5;111m╰{border}╯\x1b[0m"
    )
}

/// 打印 user 消息卡片。
/// 单行输入时先清除 rustyline 的 echo 行（避免重复显示）。
/// 清除行数根据终端宽度和输入显示宽度计算，覆盖长输入换行的情况。
/// 多行输入时不清除 echo（多行清除复杂且易出错），保留 echo + 卡片视觉冗余。
/// P3 多行粘贴折叠实现后，多行粘贴会被折叠为单行占位符，统一走单行清除路径。
fn print_user_card(input: &str) {
    let line_count = input.lines().count();
    if line_count <= 1 {
        clear_rustyline_echo(input);
    }
    println!("{}", format_user_message_card(input.trim()));
}

/// 清除 rustyline 的回显行。
/// rustyline prompt "> " 占 2 列，长输入在窄终端会换行多行。
/// 根据终端宽度和输入显示宽度计算换行数，逐行上移并清除整行。
/// 注意：调用此函数前，光标必须紧跟在 rustyline 回显之后（无其他输出插入），
/// 否则清除会错位。CJK 字符按 2 列估算（粗略近似，不引入 unicode-width 依赖）。
fn clear_rustyline_echo(input: &str) {
    let cols = match crossterm::terminal::size() {
        Ok((c, _)) if c > 0 => c as usize,
        _ => 80,
    };
    let prompt_width = 2; // "> "
    let input_width = estimate_display_width(input);
    let total_width = prompt_width + input_width;
    let wrapped_lines = ((total_width + cols - 1) / cols).max(1);
    let mut stdout = io::stdout();
    for _ in 0..wrapped_lines {
        // \x1b[A = 光标上移一行, \x1b[2K = 清除整行
        let _ = write!(stdout, "\x1b[A\x1b[2K");
    }
    let _ = stdout.flush();
}

/// 估算字符串在终端的显示宽度（列数）。
/// ASCII 及窄字符算 1 列，CJK、全角、emoji 等宽字符算 2 列。
/// 粗略近似，不处理零宽字符、组合字符和 emoji ZWJ 序列。
fn estimate_display_width(s: &str) -> usize {
    s.chars()
        .map(|c| if is_wide_char(c as u32) { 2 } else { 1 })
        .sum()
}

/// 判断码点是否属于宽字符区间（CJK、全角、emoji 等）。
/// 参考 East Asian Width 的粗略近似，覆盖常见 CJK 和 emoji 区间。
fn is_wide_char(code: u32) -> bool {
    matches!(
        code,
        0x1100..=0x115F     // Hangul Jamo
        | 0x2E80..=0x303E   // CJK Radicals / Kangxi
        | 0x3040..=0x33BF   // Hiragana / Katakana / CJK Compat
        | 0x3400..=0x4DBF   // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0xA000..=0xA4CF   // Yi Syllables / Radicals
        | 0xAC00..=0xD7AF   // Hangul Syllables
        | 0xF900..=0xFAFF   // CJK Compatibility Ideographs
        | 0xFE30..=0xFE4F   // CJK Compatibility Forms
        | 0xFF00..=0xFF60   // Fullwidth Forms
        | 0xFFE0..=0xFFE6   // Fullwidth signs
        | 0x1F300..=0x1FAFF // Emoji & pictographs
        | 0x20000..=0x3FFFD // CJK Extensions B-F
    )
}

/// P2 富状态栏：渲染单行紧凑状态信息。
/// 格式：`model | 📁 cwd | 🔢 tokens | 💰 cost`
/// 在每次回合完成后打印，作为本回合的"尾部状态摘要"。
/// 不持久占用屏幕底部（持久底部栏需要 ratatui 全屏模式，与 rustyline 冲突）。
///
/// `cwd` 传入已缩短的显示路径（避免过长路径撑爆状态栏）。
/// `usage` 是累计 usage（cumulative across turns）。
///
/// Tier S #3 穷鬼模式：激活时追加 `🪙 poor` 标记，提醒用户非核心特性被跳过。

/// Tier S #1 Goal 持续驱动：网络错误关键词列表。
/// 在 `run_turn` 错误路径检测，命中时自动 `goal_manager.pause("network error")`。
/// 选用小写匹配（error.to_string().to_ascii_lowercase() 后比较）。
const NETWORK_ERROR_KEYWORDS: &[&str] = &[
    "timeout",
    "timed out",
    "connection reset",
    "connection refused",
    "connection aborted",
    "network is unreachable",
    "network error",
    "broken pipe",
    "dns resolution failed",
    "name resolution",
    "connect error",
    "hyper: error",
    "reqwest: error",
    "eof before",
    "stream closed",
];

/// Tier S #1 Goal 持续驱动：处理 `/goal` 命令的 args 参数，返回用户可见消息。
///
/// 支持的 args：
/// - `None` / `"status"`：查询当前 goal 状态
/// - `"set <text>"`：设置新 goal（覆盖已有 goal）
/// - `"clear"`：清除当前 goal
/// - `"pause"`：暂停当前 goal（不注入 prompt 前缀，但保留状态）
/// - `"resume"`：恢复暂停的 goal
/// - `"budget <n>"`：设置 token budget（仅当有 active goal 时生效）
/// - 其他：返回 usage 提示
///
/// `manager` 由调用方持有（LiveCli 的 `self.goal_manager` 或 resume 模式的临时 manager）。
/// 函数内部会调用 manager 的 set/clear/pause/resume 等方法，自动持久化到 goal.json。
fn handle_goal_command(
    manager: &mut runtime::GoalManager,
    args: Option<&str>,
) -> String {
    let trimmed = args.unwrap_or("").trim();
    let (subcommand, rest) = split_first_word(trimmed);
    match subcommand {
        "" | "status" | "show" | "?" => render_goal_status(manager),
        "set" => {
            let text = rest.trim();
            if text.is_empty() {
                return "Usage: /goal set <text>\n\x1b[2mExample: /goal set Refactor the auth module to use OAuth 2.0\x1b[0m".to_string();
            }
            // 解析可选的 budget 后缀：`<text> --budget 10000`
            let (clean_text, budget) = parse_goal_budget(text);
            match manager.set(clean_text, budget) {
                Ok(()) => format!(
                    "🎯 Goal set: {clean_text}\n\x1b[2mThe goal will be injected into every turn until cleared or blocked 3 times.\x1b[0m{}",
                    budget.map(|b| format!("\n\x1b[2mToken budget: {b}\x1b[0m")).unwrap_or_default()
                ),
                Err(error) => format!("❌ Failed to set goal: {error}"),
            }
        }
        "clear" | "stop" => {
            let reason = if rest.is_empty() { "user cleared" } else { rest };
            match manager.clear(reason) {
                Ok(()) => "🎯 Goal cleared.\n\x1b[2mPrompt injection disabled.\x1b[0m".to_string(),
                Err(error) => format!("❌ Failed to clear goal: {error}"),
            }
        }
        "pause" => {
            let reason = if rest.is_empty() { "user paused" } else { rest };
            match manager.pause(reason) {
                Ok(()) => "🎯 Goal paused.\n\x1b[2mPrompt injection disabled. Use /goal resume to reactivate.\x1b[0m".to_string(),
                Err(error) => format!("❌ Failed to pause goal: {error}"),
            }
        }
        "resume" => match manager.resume() {
            Ok(()) => "🎯 Goal resumed.\n\x1b[2mPrompt injection re-enabled.\x1b[0m".to_string(),
            Err(error) => format!("❌ Failed to resume goal: {error}"),
        },
        other => format!(
            "Unknown goal subcommand: '{other}'.\n\x1b[2mUsage: /goal [set <text>|clear|pause|resume|status]\x1b[0m"
        ),
    }
}

/// 渲染当前 goal 状态为用户可见的多行字符串。
fn render_goal_status(manager: &runtime::GoalManager) -> String {
    let Some(goal) = manager.active() else {
        return "🎯 Goal: inactive\n\x1b[2mSet a goal with: /goal set <text>\x1b[0m".to_string();
    };
    let state_label = match &goal.state {
        runtime::GoalState::Active => "active".to_string(),
        runtime::GoalState::Paused { reason, .. } => format!("paused ({reason})"),
        runtime::GoalState::Blocked { reason, .. } => format!("blocked ({reason})"),
    };
    let budget_str = match goal.token_budget {
        Some(b) => format!("{}/{} tokens", goal.tokens_used, b),
        None => format!("{} tokens", goal.tokens_used),
    };
    let created_str = format_timestamp_ms(goal.created_at_ms);
    format!(
        "🎯 Goal: {state_label}\n  \x1b[2mText\x1b[0m:          {}\n  \x1b[2mCreated\x1b[0m:        {created_str}\n  \x1b[2mBlocked\x1b[0m:        {}/3\n  \x1b[2mTokens used\x1b[0m:    {budget_str}",
        goal.text, goal.blocked_count
    )
}

/// 把毫秒时间戳渲染为 `YYYY-MM-DD HH:MM:SS` 格式（本地时区）。
fn format_timestamp_ms(ms: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let diff_secs = now.saturating_sub(ms) / 1000;
    if diff_secs < 60 {
        return format!("{diff_secs}s ago");
    }
    if diff_secs < 3600 {
        return format!("{}m ago", diff_secs / 60);
    }
    if diff_secs < 86400 {
        return format!("{}h ago", diff_secs / 3600);
    }
    format!("{}d ago", diff_secs / 86400)
}

/// 把输入字符串拆分为 `(first_word, remainder)`。first_word 是第一个空白分隔的 token，
/// remainder 是剩余部分（已 trim）。空输入返回 `("", "")`。
fn split_first_word(input: &str) -> (&str, &str) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return ("", "");
    }
    match trimmed.find(|c: char| c.is_whitespace()) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim()),
        None => (trimmed, ""),
    }
}

/// 从 goal text 中解析可选的 `--budget <n>` 后缀。
/// 返回 `(clean_text, Option<u64>)`。clean_text 已移除 budget 标记。
fn parse_goal_budget(text: &str) -> (&str, Option<u64>) {
    if let Some(idx) = text.find("--budget") {
        let clean = text[..idx].trim();
        let budget_part = text[idx + "--budget".len()..].trim();
        let budget = budget_part
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok());
        (clean, budget)
    } else {
        (text, None)
    }
}

/// 处理 `/poor` 命令的 action 参数，返回 `(新状态, 用户可见消息)`。
///
/// 支持的 action：
/// - `None` / `"toggle"`：切换状态
/// - `"on"` / `"true"` / `"1"` / `"yes"`：开启
/// - `"off"` / `"false"` / `"0"` / `"no"`：关闭
/// - `"status"` / `"?"` / `"show"`：仅查询当前状态
/// - 其他：返回错误提示，状态不变
///
/// 切换仅影响进程级 `runtime::poor_mode` 全局 AtomicBool，
/// 不写回 settings.json；下次启动会重新从 `settings.poorMode` 读取初始值。
fn handle_poor_mode_action(action: Option<&str>) -> (bool, String) {
    let current = runtime::poor_mode::is_active();
    let normalized = action.map(|s| s.trim().to_ascii_lowercase());
    match normalized.as_deref() {
        None | Some("toggle") => {
            let new_state = runtime::poor_mode::toggle();
            let verb = if new_state { "enabled" } else { "disabled" };
            let icon = if new_state { "🪙" } else { "💸" };
            (
                new_state,
                format!(
                    "{icon} Poor mode {verb}.\n\
                     \x1b[2mNon-essential token usage (nudge, etc.) is {}.\x1b[0m",
                    if new_state { "skipped" } else { "restored" }
                ),
            )
        }
        Some("on") | Some("true") | Some("1") | Some("yes") => {
            runtime::poor_mode::set_active(true);
            (
                true,
                "🪙 Poor mode enabled.\n\x1b[2mNon-essential token usage (nudge, etc.) is skipped.\x1b[0m"
                    .to_string(),
            )
        }
        Some("off") | Some("false") | Some("0") | Some("no") => {
            runtime::poor_mode::set_active(false);
            (
                false,
                "💸 Poor mode disabled.\n\x1b[2mNon-essential token usage restored.\x1b[0m"
                    .to_string(),
            )
        }
        Some("status") | Some("?") | Some("show") => {
            let icon = if current { "🪙" } else { "💸" };
            let state = if current { "ACTIVE" } else { "inactive" };
            (
                current,
                format!("{icon} Poor mode: {state}\n\x1b[2mToggle with /poor, or /poor on|off\x1b[0m"),
            )
        }
        Some(other) => (
            current,
            format!(
                "Unknown poor action: '{other}'.\n\x1b[2mUsage: /poor [on|off|status|toggle]\x1b[0m"
            ),
        ),
    }
}

/// Tier S #2 后台会话：处理 `/bg` slash 命令。
///
/// 支持的 args：
/// - `None` / `"ps"` / `"list"`：列出所有后台会话（含存活状态刷新）
/// - `"logs <pid> [N]"`：读取 pid 的最后 N 行日志（默认 50）
/// - `"kill <pid>"`：终止 pid 对应的后台进程
/// - `"purge <pid>"`：删除已退出/被 kill 的 pid 的状态文件和 log
/// - `"spawn <prompt>"`：启动新后台 claw 会话（`claw -p "<prompt>"`）
/// - 其他：返回 usage 提示
///
/// 返回 `(用户可见消息, JSON 结构化输出)`。JSON 用于 resume 模式编程式访问。
fn handle_bg_command(args: Option<&str>, cwd: &Path) -> (String, serde_json::Value) {
    let trimmed = args.unwrap_or("").trim();
    let (subcommand, rest) = split_first_word(trimmed);
    match subcommand {
        "" | "ps" | "list" => {
            let records = runtime::bg::list(cwd);
            if records.is_empty() {
                return (
                    "🚀 No background sessions.\n\x1b[2mStart one with: /bg spawn <prompt>\x1b[0m"
                        .to_string(),
                    serde_json::json!({"kind": "bg", "action": "ps", "records": []}),
                );
            }
            let mut lines = Vec::new();
            lines.push(format!(
                "🚀 {} background session(s):",
                records.len()
            ));
            for record in &records {
                let status_icon = match &record.status {
                    runtime::BgStatus::Running => "🟢",
                    runtime::BgStatus::Exited { .. } => "⚫",
                    runtime::BgStatus::Killed { .. } => "🔴",
                };
                let status_label = match &record.status {
                    runtime::BgStatus::Running => "running".to_string(),
                    runtime::BgStatus::Exited { at_ms } => {
                        format!("exited {}", format_timestamp_ms(*at_ms))
                    }
                    runtime::BgStatus::Killed { at_ms } => {
                        format!("killed {}", format_timestamp_ms(*at_ms))
                    }
                };
                let session_str = record
                    .session_id
                    .as_ref()
                    .map(|s| format!("  \x1b[2msession\x1b[0m: {s}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "  {status_icon} PID {}\n     \x1b[2mstatus\x1b[0m:    {status_label}\n     \x1b[2mstarted\x1b[0m:   {}\n     \x1b[2mcommand\x1b[0m:   {}\n     \x1b[2mlog\x1b[0m:       {}{}",
                    record.pid,
                    format_timestamp_ms(record.started_at_ms),
                    record.command,
                    record.log_path,
                    if session_str.is_empty() {
                        String::new()
                    } else {
                        format!("\n    {session_str}")
                    }
                ));
            }
            (
                lines.join("\n"),
                serde_json::json!({
                    "kind": "bg",
                    "action": "ps",
                    "records": records.iter().map(|r| serde_json::json!({
                        "pid": r.pid,
                        "started_at_ms": r.started_at_ms,
                        "command": r.command,
                        "status": match &r.status {
                            runtime::BgStatus::Running => "running",
                            runtime::BgStatus::Exited { .. } => "exited",
                            runtime::BgStatus::Killed { .. } => "killed",
                        },
                        "session_id": r.session_id,
                    })).collect::<Vec<_>>(),
                }),
            )
        }
        "logs" => {
            let (pid_str, lines_str) = split_first_word(rest);
            let pid = match pid_str.parse::<u32>() {
                Ok(p) => p,
                Err(_) => {
                    return (
                        "Usage: /bg logs <pid> [N]\n\x1b[2mExample: /bg logs 1234 50\x1b[0m"
                            .to_string(),
                        serde_json::json!({"kind": "bg", "action": "logs", "error": "invalid_pid"}),
                    );
                }
            };
            let n = lines_str
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(50);
            match runtime::bg::read_log_tail(cwd, pid, n) {
                Ok(content) => {
                    if content.is_empty() {
                        (
                            format!("📄 Log for PID {pid} is empty."),
                            serde_json::json!({"kind": "bg", "action": "logs", "pid": pid, "empty": true}),
                        )
                    } else {
                        (
                            format!("📄 Last {n} lines for PID {pid}:\n\n{content}"),
                            serde_json::json!({"kind": "bg", "action": "logs", "pid": pid, "lines": n}),
                        )
                    }
                }
                Err(e) => (
                    format!("❌ Failed to read log for PID {pid}: {e}"),
                    serde_json::json!({"kind": "bg", "action": "logs", "pid": pid, "error": e.to_string()}),
                ),
            }
        }
        "kill" => {
            let pid_str = rest.trim();
            let pid = match pid_str.parse::<u32>() {
                Ok(p) => p,
                Err(_) => {
                    return (
                        "Usage: /bg kill <pid>\n\x1b[2mExample: /bg kill 1234\x1b[0m".to_string(),
                        serde_json::json!({"kind": "bg", "action": "kill", "error": "invalid_pid"}),
                    );
                }
            };
            match runtime::bg::kill(cwd, pid) {
                Ok(()) => (
                    format!("🔴 Killed background session {pid}."),
                    serde_json::json!({"kind": "bg", "action": "kill", "pid": pid, "ok": true}),
                ),
                Err(e) => (
                    format!("❌ Failed to kill PID {pid}: {e}"),
                    serde_json::json!({"kind": "bg", "action": "kill", "pid": pid, "error": e.to_string()}),
                ),
            }
        }
        "purge" => {
            let pid_str = rest.trim();
            let pid = match pid_str.parse::<u32>() {
                Ok(p) => p,
                Err(_) => {
                    return (
                        "Usage: /bg purge <pid>\n\x1b[2mExample: /bg purge 1234\x1b[0m".to_string(),
                        serde_json::json!({"kind": "bg", "action": "purge", "error": "invalid_pid"}),
                    );
                }
            };
            match runtime::bg::purge(cwd, pid) {
                Ok(()) => (
                    format!("🧹 Purged record for PID {pid}."),
                    serde_json::json!({"kind": "bg", "action": "purge", "pid": pid, "ok": true}),
                ),
                Err(e) => (
                    format!("❌ Failed to purge PID {pid}: {e}"),
                    serde_json::json!({"kind": "bg", "action": "purge", "pid": pid, "error": e.to_string()}),
                ),
            }
        }
        "spawn" => {
            let prompt = rest.trim();
            if prompt.is_empty() {
                return (
                    "Usage: /bg spawn <prompt>\n\x1b[2mExample: /bg spawn Refactor the auth module to use OAuth 2.0\x1b[0m".to_string(),
                    serde_json::json!({"kind": "bg", "action": "spawn", "error": "empty_prompt"}),
                );
            }
            // 构造 claw -p "<prompt>" 命令行。
            // --allow-broad-cwd：后台会话继承父 claw 的 cwd，可能落在宽目录
            // （如 C:\Users\<user>）。用户显式 /bg spawn 视为已授权，跳过保护检查。
            let command_args = vec![
                "--allow-broad-cwd".to_string(),
                "-p".to_string(),
                prompt.to_string(),
            ];
            match runtime::bg::spawn(&command_args, cwd, None) {
                Ok(record) => (
                    format!(
                        "🚀 Spawned background session.\n  \x1b[2mPID\x1b[0m:     {}\n  \x1b[2mCommand\x1b[0m: {}\n  \x1b[2mLog\x1b[0m:      {}\n\x1b[2mTrack with /bg ps, view output with /bg logs {} [N]\x1b[0m",
                        record.pid, record.command, record.log_path, record.pid
                    ),
                    serde_json::json!({
                        "kind": "bg",
                        "action": "spawn",
                        "pid": record.pid,
                        "log_path": record.log_path,
                        "ok": true,
                    }),
                ),
                Err(e) => (
                    format!("❌ Failed to spawn background session: {e}"),
                    serde_json::json!({"kind": "bg", "action": "spawn", "error": e.to_string()}),
                ),
            }
        }
        other => (
            format!(
                "Unknown bg subcommand: '{other}'.\n\x1b[2mUsage: /bg [ps|logs <pid>|kill <pid>|purge <pid>|spawn <prompt>]\x1b[0m"
            ),
            serde_json::json!({"kind": "bg", "error": format!("unknown subcommand: {other}")}),
        ),
    }
}

/// 把毫秒时间戳渲染为人类可读的相对时长（如 "3m 25s"、"1h 5m"、"2d 4h"）。
#[allow(dead_code)]
fn format_age_ms(started_at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let delta_secs = now.saturating_sub(started_at_ms) / 1000;
    let days = delta_secs / 86_400;
    let hours = (delta_secs % 86_400) / 3600;
    let minutes = (delta_secs % 3600) / 60;
    let seconds = delta_secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_status_bar(model: &str, cwd: &str, usage: runtime::TokenUsage) -> String {
    let provider = provider_label(detect_provider_kind(model));
    // 成本用模型对应的 pricing 估算；未知模型走 sonnet 默认档。
    let pricing = runtime::pricing_for_model(model);
    let cost = pricing.map_or_else(
        || usage.estimate_cost_usd(),
        |p| usage.estimate_cost_usd_with_pricing(p),
    );
    let total_cost = cost.total_cost_usd();
    let total_tokens = usage.total_tokens();
    // Tier S #3 穷鬼模式激活时显示 🪙 标记（黄色），提醒用户非核心特性被跳过。
    // Tier S #1 Goal 持续驱动：goal 徽章由 LiveCli::print_status_bar 追加（需访问 goal_manager）。
    let poor_badge = if runtime::poor_mode::is_active() {
        "\x1b[38;5;240m │ \x1b[33m🪙 poor\x1b[0;38;5;240m"
    } else {
        ""
    };
    // 状态栏配色：整体暗灰（38;5;240）+ 分隔符浅灰（38;5;245），
    // 关键数字用强调色（model=青色, tokens=黄色, cost=绿色）。
    format!(
        "\x1b[38;5;240m│ \x1b[1;36m{model}\x1b[0;38;5;240m via \x1b[3;36m{provider}\x1b[0;38;5;240m \
         │ \x1b[2m📁\x1b[0m {cwd} \
         │ \x1b[2m🔢\x1b[0m \x1b[33m{total_tokens}\x1b[0;38;5;240m tokens \
         │ \x1b[2m💰\x1b[0m \x1b[32m${total_cost:.4}\x1b[0;38;5;240m \
         {poor_badge}\
         │\x1b[0m"
    )
}

/// 把 cwd 缩短为状态栏友好的显示路径：
/// - 家目录前缀替换为 `~`
/// - 仅保留最后 2 级目录（避免长路径撑爆状态栏）
fn shorten_cwd_for_statusbar(cwd: &Path) -> String {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let display = if let Some(home) = home.as_ref() {
        if let Ok(stripped) = cwd.strip_prefix(home) {
            if stripped.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", stripped.display())
            }
        } else {
            cwd.display().to_string()
        }
    } else {
        cwd.display().to_string()
    };
    // 仅保留最后 2 级目录（以 `/` 或 `\` 分隔）
    let sep = if display.contains('\\') { '\\' } else { '/' };
    let parts: Vec<&str> = display.split(sep).collect();
    if parts.len() <= 3 {
        display
    } else {
        let tail = &parts[parts.len() - 2..];
        format!("…{sep}{}", tail.join(&sep.to_string()))
    }
}

fn format_tool_result(
    name: &str,
    output: &str,
    is_error: bool,
    verbosity: OutputVerbosity,
) -> String {
    // 成功路径仅在 Full 模式被调用（见 CliToolExecutor::execute 的门控），
    // 错误路径在 Full/Compact/Minimal 模式都被调用（Silent 由 show_tool_errors() 抑制）。
    // verbosity 参数保留供未来按级别裁剪错误详情，当前错误一律走完整渲染。
    let _ = verbosity;
    let icon = if is_error {
        "\x1b[1;31m✗\x1b[0m"
    } else {
        "\x1b[1;32m✓\x1b[0m"
    };
    if is_error {
        let summary = truncate_for_summary(output.trim(), 160);
        return if summary.is_empty() {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
        } else {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n\x1b[38;5;203m{summary}\x1b[0m")
        };
    }

    let parsed: serde_json::Value =
        serde_json::from_str(output).unwrap_or(serde_json::Value::String(output.to_string()));
    match name {
        "bash" | "Bash" => format_bash_result(icon, &parsed),
        "read_file" | "Read" => format_read_result(icon, &parsed),
        "write_file" | "Write" => format_write_result(icon, &parsed),
        "edit_file" | "Edit" => format_edit_result(icon, &parsed),
        "glob_search" | "Glob" => format_glob_result(icon, &parsed),
        "grep_search" | "Grep" => format_grep_result(icon, &parsed),
        _ => format_generic_tool_result(icon, name, &parsed),
    }
}

/// Compact/Minimal 模式下的工具成功结果摘要。返回空字符串表示该工具不显示摘要
/// （Minimal 模式下非关键工具静默）；返回非空字符串则由调用方 stream_markdown 打印。
fn format_tool_result_compact(name: &str) -> String {
    // Minimal: 仅关键工具显示摘要，其他工具完全静默。
    if !matches!(
        name,
        "read_file" | "write_file" | "edit_file" | "bash" | "bash_command"
            | "glob_search" | "grep_search"
    ) {
        return String::new();
    }
    format!("\x1b[2m○ {name} ok\x1b[0m")
}

const DISPLAY_TRUNCATION_NOTICE: &str =
    "\x1b[2m… output truncated for display; full result preserved in session.\x1b[0m";
const READ_DISPLAY_MAX_LINES: usize = 80;
const READ_DISPLAY_MAX_CHARS: usize = 6_000;
const TOOL_OUTPUT_DISPLAY_MAX_LINES: usize = 60;
const TOOL_OUTPUT_DISPLAY_MAX_CHARS: usize = 4_000;

pub(crate) fn extract_tool_path(parsed: &serde_json::Value) -> String {
    parsed
        .get("file_path")
        .or_else(|| parsed.get("filePath"))
        .or_else(|| parsed.get("path"))
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string()
}

fn format_search_start(label: &str, parsed: &serde_json::Value) -> String {
    let pattern = parsed
        .get("pattern")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let scope = parsed
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or(".");
    format!("{label} {pattern}\n\x1b[2min {scope}\x1b[0m")
}

fn format_patch_preview(old_value: &str, new_value: &str) -> Option<String> {
    if old_value.is_empty() && new_value.is_empty() {
        return None;
    }
    Some(format!(
        "\x1b[38;5;203m- {}\x1b[0m\n\x1b[38;5;70m+ {}\x1b[0m",
        truncate_for_summary(first_visible_line(old_value), 72),
        truncate_for_summary(first_visible_line(new_value), 72)
    ))
}

fn format_bash_call(parsed: &serde_json::Value) -> String {
    let command = parsed
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if command.is_empty() {
        String::new()
    } else {
        format!(
            "\x1b[48;5;236;38;5;255m $ {} \x1b[0m",
            truncate_for_summary(command, 160)
        )
    }
}

pub(crate) fn first_visible_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
}

fn format_bash_result(icon: &str, parsed: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    let mut lines = vec![format!("{icon} \x1b[38;5;245mbash\x1b[0m")];
    if let Some(task_id) = parsed
        .get("backgroundTaskId")
        .and_then(|value| value.as_str())
    {
        write!(&mut lines[0], " backgrounded ({task_id})").expect("write to string");
    } else if let Some(status) = parsed
        .get("returnCodeInterpretation")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
    {
        write!(&mut lines[0], " {status}").expect("write to string");
    }

    if let Some(stdout) = parsed.get("stdout").and_then(|value| value.as_str()) {
        if !stdout.trim().is_empty() {
            lines.push(truncate_output_for_display(
                stdout,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            ));
        }
    }
    if let Some(stderr) = parsed.get("stderr").and_then(|value| value.as_str()) {
        if !stderr.trim().is_empty() {
            lines.push(format!(
                "\x1b[38;5;203m{}\x1b[0m",
                truncate_output_for_display(
                    stderr,
                    TOOL_OUTPUT_DISPLAY_MAX_LINES,
                    TOOL_OUTPUT_DISPLAY_MAX_CHARS,
                )
            ));
        }
    }

    lines.join("\n\n")
}

fn format_read_result(icon: &str, parsed: &serde_json::Value) -> String {
    let file = parsed.get("file").unwrap_or(parsed);
    let path = extract_tool_path(file);
    let start_line = file
        .get("startLine")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let num_lines = file
        .get("numLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_lines = file
        .get("totalLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(num_lines);
    let content = file
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let end_line = start_line.saturating_add(num_lines.saturating_sub(1));

    format!(
        "{icon} \x1b[2m📄 Read {path} (lines {}-{} of {})\x1b[0m\n{}",
        start_line,
        end_line.max(start_line),
        total_lines,
        truncate_output_for_display(content, READ_DISPLAY_MAX_LINES, READ_DISPLAY_MAX_CHARS)
    )
}

fn format_write_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let kind = parsed
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("write");
    let line_count = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .map_or(0, |content| content.lines().count());
    format!(
        "{icon} \x1b[1;32m✏️ {} {path}\x1b[0m \x1b[2m({line_count} lines)\x1b[0m",
        if kind == "create" { "Wrote" } else { "Updated" },
    )
}

fn format_structured_patch_preview(parsed: &serde_json::Value) -> Option<String> {
    let hunks = parsed.get("structuredPatch")?.as_array()?;
    let mut preview = Vec::new();
    for hunk in hunks.iter().take(2) {
        let lines = hunk.get("lines")?.as_array()?;
        for line in lines.iter().filter_map(|value| value.as_str()).take(6) {
            match line.chars().next() {
                Some('+') => preview.push(format!("\x1b[38;5;70m{line}\x1b[0m")),
                Some('-') => preview.push(format!("\x1b[38;5;203m{line}\x1b[0m")),
                _ => preview.push(line.to_string()),
            }
        }
    }
    if preview.is_empty() {
        None
    } else {
        Some(preview.join("\n"))
    }
}

fn format_edit_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let suffix = if parsed
        .get("replaceAll")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        " (replace all)"
    } else {
        ""
    };
    let preview = format_structured_patch_preview(parsed).or_else(|| {
        let old_value = parsed
            .get("oldString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let new_value = parsed
            .get("newString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        format_patch_preview(old_value, new_value)
    });

    match preview {
        Some(preview) => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m\n{preview}"),
        None => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m"),
    }
}

fn format_glob_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if filenames.is_empty() {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files")
    } else {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files\n{filenames}")
    }
}

fn format_grep_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_matches = parsed
        .get("numMatches")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let content = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let summary = format!(
        "{icon} \x1b[38;5;245mgrep_search\x1b[0m {num_matches} matches across {num_files} files"
    );
    if !content.trim().is_empty() {
        format!(
            "{summary}\n{}",
            truncate_output_for_display(
                content,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            )
        )
    } else if !filenames.is_empty() {
        format!("{summary}\n{filenames}")
    } else {
        summary
    }
}

fn format_generic_tool_result(icon: &str, name: &str, parsed: &serde_json::Value) -> String {
    let rendered_output = match parsed {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string_pretty(parsed).unwrap_or_else(|_| parsed.to_string())
        }
        _ => parsed.to_string(),
    };
    let preview = truncate_output_for_display(
        &rendered_output,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );

    if preview.is_empty() {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
    } else if preview.contains('\n') {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n{preview}")
    } else {
        format!("{icon} \x1b[38;5;245m{name}:\x1b[0m {preview}")
    }
}

pub(crate) fn summarize_tool_payload(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.trim().to_string(),
    };
    truncate_for_summary(&compact, 96)
}

pub(crate) fn truncate_for_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn truncate_output_for_display(content: &str, max_lines: usize, max_chars: usize) -> String {
    let original = content.trim_end_matches('\n');
    if original.is_empty() {
        return String::new();
    }

    let mut preview_lines = Vec::new();
    let mut used_chars = 0usize;
    let mut truncated = false;

    for (index, line) in original.lines().enumerate() {
        if index >= max_lines {
            truncated = true;
            break;
        }

        let newline_cost = usize::from(!preview_lines.is_empty());
        let available = max_chars.saturating_sub(used_chars + newline_cost);
        if available == 0 {
            truncated = true;
            break;
        }

        let line_chars = line.chars().count();
        if line_chars > available {
            preview_lines.push(line.chars().take(available).collect::<String>());
            truncated = true;
            break;
        }

        preview_lines.push(line.to_string());
        used_chars += newline_cost + line_chars;
    }

    let mut preview = preview_lines.join("\n");
    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(DISPLAY_TRUNCATION_NOTICE);
    }
    preview
}

fn render_thinking_block_summary(
    out: &mut (impl Write + ?Sized),
    char_count: Option<usize>,
    redacted: bool,
) -> Result<(), RuntimeError> {
    let summary = if redacted {
        "\n▶ Thinking block hidden by provider\n".to_string()
    } else if let Some(char_count) = char_count {
        format!("\n▶ Thinking ({char_count} chars hidden)\n")
    } else {
        "\n▶ Thinking hidden\n".to_string()
    };
    write!(out, "{summary}")
        .and_then(|()| out.flush())
        .map_err(|error| RuntimeError::new(error.to_string()))
}

fn push_output_block(
    block: OutputContentBlock,
    out: &mut (impl Write + ?Sized),
    events: &mut Vec<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String)>,
    streaming_tool_input: bool,
    block_has_thinking_summary: &mut bool,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                let rendered = TerminalRenderer::new().markdown_to_ansi(&text);
                write!(out, "{rendered}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            // During streaming, the initial content_block_start has an empty input ({}).
            // The real input arrives via input_json_delta events. In
            // non-streaming responses, preserve a legitimate empty object.
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            *pending_tool = Some((id, name, initial_input));
        }
        OutputContentBlock::Thinking { thinking, .. } => {
            render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
            *block_has_thinking_summary = true;
        }
        OutputContentBlock::RedactedThinking { .. } => {
            render_thinking_block_summary(out, None, true)?;
            *block_has_thinking_summary = true;
        }
    }
    Ok(())
}

fn response_to_events(
    response: MessageResponse,
    out: &mut (impl Write + ?Sized),
) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tool = None;

    for block in response.content {
        let mut block_has_thinking_summary = false;
        push_output_block(
            block,
            out,
            &mut events,
            &mut pending_tool,
            false,
            &mut block_has_thinking_summary,
        )?;
        if let Some((id, name, input)) = pending_tool.take() {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

fn push_prompt_cache_record(client: &ApiProviderClient, events: &mut Vec<AssistantEvent>) {
    // `ApiProviderClient::take_last_prompt_cache_record` is a pass-through
    // to the Anthropic variant and returns `None` for OpenAI-compat /
    // xAI variants, which do not have a prompt cache. So this helper
    // remains a no-op on non-Anthropic providers without any extra
    // branching here.
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    output_verbosity: OutputVerbosity,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

impl CliToolExecutor {
    fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            output_verbosity: OutputVerbosity::default(),
            allowed_tools,
            tool_registry,
            mcp_state,
        }
    }

    fn with_verbosity(mut self, verbosity: OutputVerbosity) -> Self {
        self.output_verbosity = verbosity;
        self
    }

    fn execute_search_tool(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        serde_json::to_string_pretty(&self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        ))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute(tool_name, &value)
                .map_err(ToolError::new)
        };
        match result {
            Ok(output) => {
                if self.emit_output && self.output_verbosity.show_tool_results() {
                    // Full 模式：用卡片闭合（带左边框对齐 + 底部边框），
                    // 把工具结果嵌入到 format_tool_call_start 开的卡片容器内。
                    let markdown = format_tool_result_card_close(
                        tool_name,
                        &output,
                        false,
                    );
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                } else if self.emit_output && self.output_verbosity.show_tool_errors() {
                    // Compact / Minimal: show a one-line success marker for key tools
                    let summary = format_tool_result_compact(tool_name);
                    if !summary.is_empty() {
                        self.renderer
                            .stream_markdown(&summary, &mut io::stdout())
                            .map_err(|error| ToolError::new(error.to_string()))?;
                    }
                }
                Ok(output)
            }
            Err(error) => {
                if self.emit_output && self.output_verbosity.show_tool_errors() {
                    // Full 模式：错误结果也嵌入卡片容器（闭合）。
                    // Compact/Minimal 模式：format_tool_result 会输出完整错误（错误不折叠）。
                    let markdown = if self.output_verbosity.show_tool_results() {
                        format_tool_result_card_close(
                            tool_name,
                            &error.to_string(),
                            true,
                        )
                    } else {
                        format_tool_result(
                            tool_name,
                            &error.to_string(),
                            true,
                            self.output_verbosity,
                        )
                    };
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                }
                Err(error)
            }
        }
    }
}

fn permission_policy(
    mode: PermissionMode,
    feature_config: &runtime::RuntimeFeatureConfig,
    tool_registry: &GlobalToolRegistry,
) -> Result<PermissionPolicy, String> {
    Ok(tool_registry.permission_specs(None)?.into_iter().fold(
        PermissionPolicy::new(mode).with_permission_rules(feature_config.permission_rules()),
        |policy, (name, required_permission)| {
            policy.with_tool_requirement(name, required_permission)
        },
    ))
}

/// Extract system-role messages from a conversation and merge their text
/// content into a single string. Returns `(system_text, non_system_messages)`.
///
/// The system text is joined with `\n\n` separators. Non-system messages
/// (User/Assistant/Tool) are returned in their original order, with ownership
/// transferred.
///
/// This lets `stream()` route system content through `MessageRequest.system`
/// (where it can be prompt-cached) instead of flattening it into the messages
/// array as a fake "user" turn.
fn extract_system_messages(messages: &[ConversationMessage]) -> (String, Vec<ConversationMessage>) {
    let mut system_texts: Vec<String> = Vec::new();
    let mut rest: Vec<ConversationMessage> = Vec::new();
    for message in messages {
        if message.role == MessageRole::System {
            for block in &message.blocks {
                if let ContentBlock::Text { text } = block {
                    if !text.is_empty() {
                        system_texts.push(text.clone());
                    }
                }
            }
        } else {
            rest.push(message.clone());
        }
    }
    let system_text = system_texts.join("\n\n");
    (system_text, rest)
}

/// 在送入模型前压缩工具结果的 JSON 外壳，节省 context tokens。
///
/// `edit_file` / `write_file` 等工具会返回包含 `originalFile`、`structuredPatch`
/// 和全文件内容的完整 JSON，回传给模型会浪费上下文预算（一次 500 KB 文件编辑
/// ≈ 135 k tokens）。本函数把这些状态变更型工具的输出剥离为简短人类可读消息；
/// 完整结构化数据仍保留在 session 中用于展示与回放。
///
/// 行为镜像 upstream Claude Code（TS 版）的 `mapToolResultToToolResultBlockParam`。
fn compact_tool_output_for_model(tool_name: &str, output: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(output)
        .unwrap_or(serde_json::Value::String(output.to_string()));

    match tool_name {
        "edit_file" | "Edit" => {
            let path = parsed
                .get("filePath")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let user_modified = parsed
                .get("userModified")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let replace_all = parsed
                .get("replaceAll")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let modified_note = if user_modified {
                ". The user modified your proposed changes before accepting them."
            } else {
                ""
            };
            if replace_all {
                format!(
                    "The file {path} has been updated{modified_note}. All occurrences were successfully replaced."
                )
            } else {
                format!("The file {path} has been updated successfully{modified_note}.")
            }
        }
        "write_file" | "Write" => {
            let path = parsed
                .get("filePath")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let kind = parsed
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("update");
            match kind {
                "create" => format!("File created successfully at: {path}"),
                _ => format!("The file {path} has been updated successfully."),
            }
        }
        "NotebookEdit" => {
            let path = parsed
                .get("notebookPath")
                .or_else(|| parsed.get("notebook_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let edit_mode = parsed
                .get("editMode")
                .and_then(|v| v.as_str())
                .unwrap_or("replace");
            match edit_mode {
                "insert" => format!("Cell inserted successfully in {path}."),
                "delete" => format!("Cell deleted successfully in {path}."),
                _ => format!("The notebook {path} has been updated successfully."),
            }
        }
        "WebSearch" => {
            // WebSearch 结果是一个对象数组：第一条是 commentary 字符串，
            // 后续是结构化命中对象。这里只保留 commentary。
            let results = parsed.get("results").and_then(|v| v.as_array());
            let commentary = results.and_then(|arr| arr.iter().find_map(|item| item.as_str()));
            commentary.map_or_else(|| output.to_string(), |c| c.to_string())
        }
        "WebFetch" => {
            // WebFetch 返回 { bytes, code, result, ... }，仅保留 AI 摘要 `result`。
            let result = parsed.get("result").and_then(|v| v.as_str());
            result.map_or_else(|| output.to_string(), |r| r.to_string())
        }
        _ => output.to_string(),
    }
}

fn convert_messages(
    messages: &[ConversationMessage],
    model: &str,
) -> Vec<InputMessage> {
    let keep_thinking = model_requires_reasoning_content_in_history(model);
    let mut result: Vec<InputMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
            MessageRole::Assistant => "assistant",
        };
        let content = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => {
                    Some(InputContentBlock::Text { text: text.clone() })
                }
                ContentBlock::Thinking { .. } => {
                    if keep_thinking {
                        Some(InputContentBlock::Thinking {
                            thinking: String::new(),
                            signature: None,
                        })
                    } else {
                        None
                    }
                }
                ContentBlock::ToolUse { id, name, input } => Some(InputContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::from_str(input)
                        .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                }),
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => Some(InputContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: vec![ToolResultContentBlock::Text {
                        text: compact_tool_output_for_model(tool_name, output),
                    }],
                    is_error: *is_error,
                }),
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            continue;
        }

        // Anthropic API 严格要求：一个 assistant 消息里的所有 tool_use 必须在
        // **下一条** user 消息里都有对应的 tool_result。如果 runtime 把多个
        // tool_result push 成多条独立的 tool 消息（每条 1 个 tool_result），
        // API 会认为只有第一个 tool_result 在 "next message"，后续的 tool_use
        // 缺少 tool_result，返回 400 Bad Request。
        //
        // 修复：如果当前消息是 tool 消息（role=Tool，只含 ToolResult blocks），
        // 且前一条已生成的 InputMessage 也是 user 消息且只含 ToolResult blocks，
        // 则把当前的 ToolResult blocks 合并到前一条 InputMessage 里。
        if role == "user" && message.role == MessageRole::Tool {
            let current_is_tool_only =
                content.iter().all(|b| matches!(b, InputContentBlock::ToolResult { .. }));
            if current_is_tool_only {
                if let Some(last) = result.last_mut() {
                    let last_is_user = last.role == "user";
                    let last_is_tool_only = last
                        .content
                        .iter()
                        .all(|b| matches!(b, InputContentBlock::ToolResult { .. }));
                    if last_is_user && last_is_tool_only {
                        last.content.extend(content);
                        continue;
                    }
                }
            }
        }

        result.push(InputMessage {
            role: role.to_string(),
            content,
        });
    }
    result
}

#[allow(clippy::too_many_lines)]
fn print_help_to(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "claw v{VERSION}")?;
    writeln!(out)?;
    writeln!(out, "Usage:")?;
    writeln!(
        out,
        "  claw [--model MODEL] [--allowedTools TOOL[,TOOL...]]"
    )?;
    writeln!(out, "      Start the interactive REPL")?;
    writeln!(
        out,
        "  claw [--model MODEL] [--output-format text|json] prompt TEXT"
    )?;
    writeln!(out, "      Send one prompt and exit")?;
    writeln!(
        out,
        "  claw [--model MODEL] [--output-format text|json] TEXT"
    )?;
    writeln!(out, "      Shorthand non-interactive prompt mode")?;
    writeln!(
        out,
        "  claw --resume [SESSION.jsonl|session-id|latest] [/status] [/compact] [...]"
    )?;
    writeln!(
        out,
        "      Inspect or maintain a saved session without entering the REPL"
    )?;
    writeln!(out, "  claw help")?;
    writeln!(out, "      Alias for --help")?;
    writeln!(out, "  claw version")?;
    writeln!(out, "      Alias for --version")?;
    writeln!(out, "  claw status")?;
    writeln!(
        out,
        "      Show the current local workspace status snapshot"
    )?;
    writeln!(out, "  claw sandbox")?;
    writeln!(out, "      Show the current sandbox isolation snapshot")?;
    writeln!(out, "  claw doctor")?;
    writeln!(
        out,
        "      Diagnose local auth, config, workspace, and sandbox health"
    )?;
    writeln!(out, "  claw acp [serve]")?;
    writeln!(
        out,
        "      Show ACP/Zed editor integration status (currently unsupported; aliases: --acp, -acp)"
    )?;
    writeln!(out, "      Source of truth: {OFFICIAL_REPO_SLUG}")?;
    writeln!(
        out,
        "      Warning: do not `{DEPRECATED_INSTALL_COMMAND}` (deprecated stub)"
    )?;
    writeln!(out, "  claw dump-manifests [--manifests-dir PATH]")?;
    writeln!(out, "  claw bootstrap-plan")?;
    writeln!(out, "  claw agents")?;
    writeln!(out, "  claw mcp")?;
    writeln!(out, "  claw skills")?;
    writeln!(out, "  claw system-prompt [--cwd PATH] [--date YYYY-MM-DD]")?;
    writeln!(out, "  claw init")?;
    writeln!(
        out,
        "  claw export [PATH] [--session SESSION] [--output PATH]"
    )?;
    writeln!(
        out,
        "      Dump the latest (or named) session as markdown; writes to PATH or stdout"
    )?;
    writeln!(out)?;
    writeln!(out, "Flags:")?;
    writeln!(
        out,
        "  --model MODEL              Override the active model"
    )?;
    writeln!(
        out,
        "  --output-format FORMAT     Non-interactive output format: text or json"
    )?;
    writeln!(
        out,
        "  --compact                  Strip tool call details; print only the final assistant text (text mode only; useful for piping)"
    )?;
    writeln!(
        out,
        "  --permission-mode MODE     Set read-only, workspace-write, or danger-full-access"
    )?;
    writeln!(
        out,
        "  --dangerously-skip-permissions  Skip all permission checks"
    )?;
    writeln!(out, "  --allowedTools TOOLS       Restrict enabled tools (repeatable; comma-separated aliases supported)")?;
    writeln!(
        out,
        "  --version, -V              Print version and build information locally"
    )?;
    writeln!(out)?;
    writeln!(out, "Interactive slash commands:")?;
    writeln!(out, "{}", render_slash_command_help_filtered(STUB_COMMANDS))?;
    writeln!(out)?;
    let resume_commands = resume_supported_slash_commands()
        .into_iter()
        .filter(|spec| !STUB_COMMANDS.contains(&spec.name))
        .map(|spec| match spec.argument_hint {
            Some(argument_hint) => format!("/{} {}", spec.name, argument_hint),
            None => format!("/{}", spec.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Resume-safe commands: {resume_commands}")?;
    writeln!(out)?;
    writeln!(out, "Session shortcuts:")?;
    writeln!(
        out,
        "  REPL turns auto-save to .claw/sessions/<session-id>.{PRIMARY_SESSION_EXTENSION}"
    )?;
    writeln!(
        out,
        "  Use `{LATEST_SESSION_REFERENCE}` with --resume, /resume, or /session switch to target the newest saved session"
    )?;
    writeln!(
        out,
        "  Use /session list in the REPL to browse managed sessions"
    )?;
    writeln!(out, "Examples:")?;
    writeln!(out, "  claw --model claude-opus \"summarize this repo\"")?;
    writeln!(
        out,
        "  claw --output-format json prompt \"explain src/main.rs\""
    )?;
    writeln!(out, "  claw --compact \"summarize Cargo.toml\" | wc -l")?;
    writeln!(
        out,
        "  claw --allowedTools read,glob \"summarize Cargo.toml\""
    )?;
    writeln!(out, "  claw --resume {LATEST_SESSION_REFERENCE}")?;
    writeln!(
        out,
        "  claw --resume {LATEST_SESSION_REFERENCE} /status /diff /export notes.txt"
    )?;
    writeln!(out, "  claw agents")?;
    writeln!(out, "  claw mcp show my-server")?;
    writeln!(out, "  claw /skills")?;
    writeln!(out, "  claw doctor")?;
    writeln!(out, "  source of truth: {OFFICIAL_REPO_URL}")?;
    writeln!(
        out,
        "  do not run `{DEPRECATED_INSTALL_COMMAND}` — it installs a deprecated stub"
    )?;
    writeln!(out, "  claw init")?;
    writeln!(out, "  claw export")?;
    writeln!(out, "  claw export conversation.md")?;
    Ok(())
}

fn print_help(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    print_help_to(&mut buffer)?;
    let message = String::from_utf8(buffer)?;
    match output_format {
        CliOutputFormat::Text => print!("{message}"),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "help",
                "message": message,
            }))?
        ),
    }
    Ok(())
}

