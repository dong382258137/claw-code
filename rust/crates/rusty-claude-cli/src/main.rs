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
mod streaming;
mod suggestion;
mod ultraplan;
mod tool_display;
mod plugin_state;
mod session_mgr;
mod doctor;
mod commands_handler;
mod format;
mod app;

#[cfg(feature = "full-tui")]
mod tui;

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
use tool_display::{
    CliToolExecutor, ToolSearchRequest, McpToolRequest, ListMcpResourcesRequest,
    ReadMcpResourceRequest,
    format_tool_call_start, format_tool_result_card_close, format_tool_result,
    format_tool_result_compact, format_user_message_card, print_user_card,
    clear_rustyline_echo, estimate_display_width, is_wide_char, indent_with_card_prefix,
    extract_tool_path, format_search_start, format_patch_preview, format_bash_call,
    first_visible_line, format_bash_result, format_read_result, format_write_result,
    format_structured_patch_preview, format_edit_result, format_glob_result,
    format_grep_result, format_generic_tool_result, summarize_tool_payload,
    truncate_for_summary, truncate_output_for_display, short_tool_id,
    TOOL_CARD_PREFIX, USER_CARD_PREFIX, DISPLAY_TRUNCATION_NOTICE,
    READ_DISPLAY_MAX_LINES, READ_DISPLAY_MAX_CHARS,
    TOOL_OUTPUT_DISPLAY_MAX_LINES, TOOL_OUTPUT_DISPLAY_MAX_CHARS,
};
use plugin_state::{
    RuntimePluginState, RuntimeMcpState, RuntimePluginStateBuildOutput,
    build_runtime_mcp_state, mcp_runtime_tool_definition, mcp_wrapper_tool_definitions,
    permission_mode_for_mcp_tool, mcp_annotation_flag,
    plugins_command_payload_for, plugins_command_payload_from_result,
    build_runtime_plugin_state, build_runtime_plugin_state_with_loader,
    build_plugin_manager, resolve_plugin_path, runtime_hook_config_from_plugin_hooks,
};
use streaming::{
    AnthropicRuntimeClient, HookAbortMonitor,
    resolve_cli_auth_source, resolve_cli_auth_source_for_cwd,
    build_system_blocks, mark_last_tool_with_cache_control,
    request_ends_with_tool_result, format_user_visible_api_error,
    format_context_window_blocked_error, final_assistant_text,
    collect_tool_uses, collect_tool_results, collect_prompt_cache_events,
    render_thinking_block_summary, push_output_block, response_to_events,
    push_prompt_cache_record, prompt_cache_record_to_runtime_event,
    permission_policy, extract_system_messages,
    compact_tool_output_for_model, convert_messages,
    POST_TOOL_STALL_TIMEOUT, NETWORK_ERROR_KEYWORDS,
};
use session_mgr::{
    civil_from_days, collect_session_prompt_history, confirm_session_deletion,
    create_managed_session_handle, current_session_store, default_export_filename,
    delete_managed_session, format_history_timestamp, format_session_modified_age,
    latest_managed_session, list_managed_sessions, load_session_reference,
    looks_like_slash_command_token, new_cli_session, new_cli_session_with_roots,
    parse_history_count, recent_user_context, render_prompt_history_report,
    render_session_list, render_session_markdown, resume_command_can_absorb_token,
    resume_session, resolve_export_path, resolve_managed_session_path,
    resolve_session_reference, run_export, run_resumed_session_command,
    run_resume_command, session_clear_backup_path, session_details_json,
    session_exists_json, session_reference_exists, sessions_dir,
    summarize_tool_payload_for_markdown, write_session_clear_backup,
    DEFAULT_HISTORY_LIMIT, LEGACY_SESSION_EXTENSION, LATEST_SESSION_REFERENCE,
    PRIMARY_SESSION_EXTENSION, SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT,
    SESSION_REFERENCE_ALIASES, ManagedSessionSummary, PromptHistoryEntry,
    ResumeCommandOutcome, SessionHandle, SessionLifecycleKind, SessionLifecycleSummary,
};
use doctor::*;
use commands_handler::*;
use format::*;
use app::*;

pub(crate) const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// #148: Model provenance for `claw status` JSON/text output. Records where
/// the resolved model string came from so claws don't have to re-read argv
/// to audit whether their `--model` flag was honored vs falling back to env
/// or config or default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelSource {
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
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            ModelSource::Flag => "flag",
            ModelSource::Env => "env",
            ModelSource::Config => "config",
            ModelSource::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelProvenance {
    /// Resolved model string (after alias expansion).
    pub(crate) resolved: String,
    /// Raw user input before alias resolution. None when source is Default.
    pub(crate) raw: Option<String>,
    /// Where the resolved model string originated.
    pub(crate) source: ModelSource,
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

    pub(crate) fn from_env_or_config_or_default(cli_model: &str) -> Self {
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

pub(crate) fn max_tokens_for_model(model: &str) -> u32 {
    api::max_tokens_for_model(model)
}
// Build-time constants injected by build.rs (fall back to static values when
// build.rs hasn't run, e.g. in doc-test or unusual toolchain environments).
pub(crate) const DEFAULT_DATE: &str = match option_env!("BUILD_DATE") {
    Some(d) => d,
    None => "unknown",
};
const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 4545;
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const BUILD_TARGET: Option<&str> = option_env!("TARGET");
pub(crate) const GIT_SHA: Option<&str> = option_env!("GIT_SHA");
pub(crate) const OFFICIAL_REPO_URL: &str = "https://github.com/ultraworkers/claw-code";
pub(crate) const OFFICIAL_REPO_SLUG: &str = "ultraworkers/claw-code";
pub(crate) const DEPRECATED_INSTALL_COMMAND: &str = "cargo install claw-code";

pub(crate) type AllowedToolSet = BTreeSet<String>;

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
pub(crate) fn classify_error_kind(message: &str) -> &'static str {
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
pub(crate) fn split_error_hint(message: &str) -> (String, Option<String>) {
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
pub(crate) fn read_piped_stdin() -> Option<String> {
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
pub(crate) fn merge_prompt_with_stdin(prompt: &str, stdin_content: Option<&str>) -> String {
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

pub(crate) fn plugin_command_json(
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

pub(crate) fn plugin_summary_json(plugin: &plugins::PluginSummary) -> Value {
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

pub(crate) fn plugin_load_failure_json(failure: &plugins::PluginLoadFailure) -> Value {
    json!({
        "plugin_root": failure.plugin_root.display().to_string(),
        "kind": failure.kind.to_string(),
        "source": &failure.source,
        "lifecycle_state": "load_failed",
        "error": failure.error().to_string(),
    })
}

/// Entry point for `claw --tui`: construct LiveCli via the shared helper and
/// hand off to `tui::run_tui_repl`. Only compiled when `full-tui` feature is on;
/// without the feature, the dispatch in `run()` prints an error and exits.
#[cfg(feature = "full-tui")]
fn run_tui_repl_entry(
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
    let cli = build_live_cli_for_repl(
        model,
        allowed_tools,
        permission_mode,
        additional_workspace_roots,
        output_verbosity,
        reasoning_effort,
    )?;
    tui::app::run_tui_repl(cli)
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
            tui,
        } => {
            if tui {
                #[cfg(feature = "full-tui")]
                {
                    return run_tui_repl_entry(
                        model,
                        allowed_tools,
                        permission_mode,
                        base_commit,
                        reasoning_effort,
                        allow_broad_cwd,
                        additional_workspace_roots,
                        output_verbosity,
                    );
                }
                #[cfg(not(feature = "full-tui"))]
                {
                    eprintln!(
                        "error: --tui requires the `full-tui` Cargo feature.\n\
                         Rebuild with: cargo build --release --features full-tui"
                    );
                    std::process::exit(1);
                }
            }
            run_repl(
                model,
                allowed_tools,
                permission_mode,
                base_commit,
                reasoning_effort,
                allow_broad_cwd,
                additional_workspace_roots,
                output_verbosity,
            )?
        }
        CliAction::HelpTopic {
            topic,
            output_format,
        } => print_help_topic(topic, output_format)?,
        CliAction::Help { output_format } => print_help(output_format)?,
    }
    Ok(())
}



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliOutputFormat {
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


pub(crate) fn resolve_model_alias(model: &str) -> &str {
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
pub(crate) fn resolve_model_alias_with_config(model: &str) -> String {
    let trimmed = model.trim();
    if let Some(resolved) = config_alias_for_current_dir(trimmed) {
        return resolve_model_alias(&resolved).to_string();
    }
    resolve_model_alias(trimmed).to_string()
}

/// Validate model syntax at parse time.
/// Accepts: known aliases (opus, sonnet, haiku) or provider/model pattern.
/// Rejects: empty, whitespace-only, strings with spaces, or invalid chars.
pub(crate) fn validate_model_syntax(model: &str) -> Result<(), String> {
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

pub(crate) fn config_alias_for_current_dir(alias: &str) -> Option<String> {
    if alias.is_empty() {
        return None;
    }
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    let config = loader.load().ok()?;
    config.aliases().get(alias).cloned()
}

pub(crate) fn normalize_allowed_tools(values: &[String]) -> Result<Option<AllowedToolSet>, String> {
    if values.is_empty() {
        return Ok(None);
    }
    current_tool_registry()?.normalize_allowed_tools(values)
}

pub(crate) fn current_tool_registry() -> Result<GlobalToolRegistry, String> {
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

pub(crate) fn parse_permission_mode_arg(value: &str) -> Result<PermissionMode, String> {
    normalize_permission_mode(value)
        .ok_or_else(|| {
            format!(
                "unsupported permission mode '{value}'. Use read-only, workspace-write, or danger-full-access."
            )
        })
        .map(permission_mode_from_label)
}

pub(crate) fn permission_mode_from_label(mode: &str) -> PermissionMode {
    match mode {
        "read-only" => PermissionMode::ReadOnly,
        "workspace-write" => PermissionMode::WorkspaceWrite,
        "danger-full-access" => PermissionMode::DangerFullAccess,
        other => panic!("unsupported permission mode label: {other}"),
    }
}

pub(crate) fn permission_mode_from_resolved(mode: ResolvedPermissionMode) -> PermissionMode {
    match mode {
        ResolvedPermissionMode::ReadOnly => PermissionMode::ReadOnly,
        ResolvedPermissionMode::WorkspaceWrite => PermissionMode::WorkspaceWrite,
        ResolvedPermissionMode::DangerFullAccess => PermissionMode::DangerFullAccess,
    }
}

pub(crate) fn default_permission_mode() -> PermissionMode {
    env::var("RUSTY_CLAUDE_PERMISSION_MODE")
        .ok()
        .as_deref()
        .and_then(normalize_permission_mode)
        .map(permission_mode_from_label)
        .or_else(config_permission_mode_for_current_dir)
        .unwrap_or(PermissionMode::DangerFullAccess)
}

pub(crate) fn config_permission_mode_for_current_dir() -> Option<PermissionMode> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader
        .load()
        .ok()?
        .permission_mode()
        .map(permission_mode_from_resolved)
}

pub(crate) fn config_model_for_current_dir() -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader.load().ok()?.model().map(ToOwned::to_owned)
}

pub(crate) fn resolve_repl_model(cli_model: String) -> String {
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

pub(crate) fn provider_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Xai => "xai",
        ProviderKind::OpenAi => "openai",
    }
}

pub(crate) fn format_connected_line(model: &str) -> String {
    let provider = provider_label(detect_provider_kind(model));
    format!("Connected: {model} via {provider}")
}

pub(crate) fn filter_tool_specs(
    tool_registry: &GlobalToolRegistry,
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<ToolDefinition> {
    tool_registry.definitions(allowed_tools)
}

pub(crate) fn parse_system_prompt_args(
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

pub(crate) fn parse_export_args(args: &[String], output_format: CliOutputFormat) -> Result<CliAction, String> {
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

pub(crate) fn parse_dump_manifests_args(
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

pub(crate) fn parse_resume_args(args: &[String], output_format: CliOutputFormat) -> Result<CliAction, String> {
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

pub(crate) fn git_output(args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
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

pub(crate) fn git_status_ok(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
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

pub(crate) fn command_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(crate) fn write_temp_text_file(
    filename: &str,
    contents: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = env::temp_dir().join(filename);
    fs::write(&path, contents)?;
    Ok(path)
}


pub(crate) struct PluginsCommandPayload {
    pub(crate) message: String,
    pub(crate) reload_runtime: bool,
    pub(crate) status: &'static str,
    pub(crate) config_load_error: Option<String>,
    pub(crate) plugins: Vec<Value>,
    pub(crate) load_failures: Vec<Value>,
}





