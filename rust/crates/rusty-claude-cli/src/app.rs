//! LiveCli REPL core: REPL loop, runtime construction, broad-cwd policy,
//! stale-base preflight, system prompt assembly, hook/permission prompters.

#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    clippy::unneeded_struct_pattern,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]

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
use crate::init::initialize_repo;
use crate::input;
use plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use crate::render::{MarkdownStreamState, OutputVerbosity, Spinner, TerminalRenderer};
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
use crate::paste::{
    expand_paste_placeholders, fold_pasted_input, format_pasted_text_ref, paste_cache_path,
    paste_cache_root, pasted_text_ref_num_lines, read_clipboard_text, should_fold_paste,
    store_paste_and_make_placeholder, try_auto_expand_clipboard, PASTE_FOLD_CHAR_THRESHOLD,
    PASTE_FOLD_LINE_THRESHOLD,
};
use crate::suggestion::{
    common_prefix_len, levenshtein_distance, looks_like_subcommand_typo, ranked_suggestions,
    render_suggestion_line, suggest_closest_term, suggest_similar_subcommand,
    suggest_slash_commands, CLI_OPTION_SUGGESTIONS,
};
use crate::ultraplan::{
    describe_tool_progress, format_internal_prompt_progress_line,
    INTERNAL_PROGRESS_HEARTBEAT_INTERVAL, InternalPromptProgressEvent,
    InternalPromptProgressReporter, InternalPromptProgressRun, InternalPromptProgressShared,
    InternalPromptProgressState,
};
use crate::tool_display::{
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
use crate::plugin_state::{
    RuntimePluginState, RuntimeMcpState, RuntimePluginStateBuildOutput,
    build_runtime_mcp_state, mcp_runtime_tool_definition, mcp_wrapper_tool_definitions,
    permission_mode_for_mcp_tool, mcp_annotation_flag,
    plugins_command_payload_for, plugins_command_payload_from_result,
    build_runtime_plugin_state, build_runtime_plugin_state_with_loader,
    build_plugin_manager, resolve_plugin_path, runtime_hook_config_from_plugin_hooks,
};
use crate::streaming::{
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
use crate::session_mgr::{
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
use crate::doctor::*;
use crate::commands_handler::*;
use crate::format::*;

// 从 crate root 引入共享符号（CliOutputFormat、ModelProvenance、共享 helper 等）
use crate::{
    CliOutputFormat, ModelProvenance, ModelSource, DEFAULT_MODEL, DEFAULT_DATE, VERSION,
    BUILD_TARGET, GIT_SHA, OFFICIAL_REPO_URL, OFFICIAL_REPO_SLUG,
    DEPRECATED_INSTALL_COMMAND, AllowedToolSet,
    resolve_model_alias, resolve_model_alias_with_config, validate_model_syntax,
    config_alias_for_current_dir, normalize_allowed_tools, current_tool_registry,
    parse_permission_mode_arg, permission_mode_from_label, permission_mode_from_resolved,
    default_permission_mode, config_permission_mode_for_current_dir,
    config_model_for_current_dir, resolve_repl_model, provider_label,
    format_connected_line, filter_tool_specs, parse_system_prompt_args,
    parse_export_args, parse_dump_manifests_args, parse_resume_args,
    plugin_summary_json, plugin_load_failure_json,
    git_output, git_status_ok, command_exists, write_temp_text_file,
    classify_error_kind, split_error_hint, read_piped_stdin,
    merge_prompt_with_stdin, plugin_command_json,
};

// ===== Block A: detect_broad_cwd .. impl LiveCli (main.rs lines 987-2584) =====
/// Detect if the current working directory is "broad" (home directory or
/// filesystem root). Returns the cwd path if broad, None otherwise.
pub(crate) fn detect_broad_cwd() -> Option<PathBuf> {
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
pub(crate) fn enforce_broad_cwd_policy(
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

pub(crate) fn stale_base_state_for(cwd: &Path, flag_value: Option<&str>) -> BaseCommitState {
    let source = resolve_expected_base(flag_value, cwd);
    check_base_commit(cwd, source.as_ref())
}

pub(crate) fn stale_base_json_value(state: &BaseCommitState) -> serde_json::Value {
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

pub(crate) fn run_stale_base_preflight(flag_value: Option<&str>) {
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let state = stale_base_state_for(&cwd, flag_value);
    if let Some(warning) = format_stale_base_warning(&state) {
        eprintln!("{warning}");
    }
}

/// Shared LiveCli construction for both inline REPL and TUI modes.
/// Returns the constructed LiveCli with reasoning effort set.
pub(crate) fn build_live_cli_for_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    additional_workspace_roots: Vec<PathBuf>,
    output_verbosity: OutputVerbosity,
    reasoning_effort: Option<String>,
) -> Result<LiveCli, Box<dyn std::error::Error>> {
    let resolved_model = resolve_repl_model(model);
    let mut cli = LiveCli::new(
        resolved_model,
        true,
        allowed_tools,
        permission_mode,
        additional_workspace_roots,
        output_verbosity,
    )?;
    cli.set_reasoning_effort(reasoning_effort);
    Ok(cli)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_repl(
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
    let t0 = std::time::Instant::now();
    let mut cli = build_live_cli_for_repl(
        model,
        allowed_tools,
        permission_mode,
        additional_workspace_roots,
        output_verbosity,
        reasoning_effort,
    )?;
    let t_cli = t0.elapsed();
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

pub(crate) struct LiveCli {
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
    // Phase 2: feature-gated status_emitter holder. When set (by TuiApp
    // via set_status_emitter), prepare_turn_runtime injects it into the
    // freshly-constructed AnthropicRuntimeClient so streaming events drive
    // the TUI's StatusBarState + OutputView in real time.
    #[cfg(feature = "full-tui")]
    status_emitter: Option<crate::streaming::StatusEmitter>,
    /// Phase 2: When true, run_turn suppresses emit_output (consume_stream
    /// writes to io::sink instead of stdout). TUI captures content via the
    /// status_emitter's TextDelta callback. Set by TuiApp via set_tui_mode.
    #[cfg(feature = "full-tui")]
    tui_mode: bool,
}

pub(crate) struct BuiltRuntime {
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

    pub(crate) fn shutdown_plugins(&mut self) -> Result<(), Box<dyn std::error::Error>> {
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

impl LiveCli {
    pub(crate) fn new(
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
            #[cfg(feature = "full-tui")]
            status_emitter: None,
            #[cfg(feature = "full-tui")]
            tui_mode: false,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
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

    pub(crate) fn startup_banner(&self) -> String {
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
        // Phase 2: if a status_emitter is attached (TUI mode), inject it
        // into the freshly-built AnthropicRuntimeClient so streaming events
        // drive the TUI's StatusBarState + OutputView in real time.
        #[cfg(feature = "full-tui")]
        if let Some(emitter) = &self.status_emitter {
            if let Some(rt) = runtime.runtime.as_mut() {
                rt.api_client_mut().set_status_emitter(Arc::clone(emitter));
            }
        }
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal);

        Ok((runtime, hook_abort_monitor))
    }

    fn replace_runtime(&mut self, runtime: BuiltRuntime) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_plugins()?;
        self.runtime = runtime;
        Ok(())
    }

    pub(crate) fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Phase 2: in TUI mode, suppress emit_output so consume_stream writes
        // to io::sink instead of stdout — preventing duplicate output under
        // the TUI's alternate screen. Streaming content is captured via the
        // status_emitter's TextDelta callback.
        let emit_output = {
            #[cfg(feature = "full-tui")]
            { !self.tui_mode }
            #[cfg(not(feature = "full-tui"))]
            { true }
        };
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(emit_output)?;
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

    pub(crate) fn run_turn_with_output(
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

    pub(crate) fn print_agents(
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

    pub(crate) fn print_mcp(
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

    pub(crate) fn print_skills(
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

    pub(crate) fn print_plugins(
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

    // ===== TUI status bar snapshot accessors =====
    // These are read-only views into LiveCli state for the TUI StatusBar to
    // render. They are feature-gated to avoid dead-code warnings when full-tui
    // is disabled.

    #[cfg(feature = "full-tui")]
    pub(crate) fn model_snapshot(&self) -> &str {
        &self.model
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn cumulative_usage_snapshot(&self) -> runtime::TokenUsage {
        self.cumulative_usage
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn permission_mode_label(&self) -> &'static str {
        self.permission_mode.as_str()
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn git_branch_snapshot(&self) -> Option<String> {
        crate::format::status_context(None).ok().and_then(|c| c.git_branch)
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn goal_badge_snapshot(&self) -> Option<String> {
        // Return plain text (no ANSI codes) for TUI rendering.
        // ratatui applies its own styling via Span::styled.
        let goal = self.goal_manager.active()?;
        match &goal.state {
            runtime::GoalState::Active => Some("🎯 goal".to_string()),
            runtime::GoalState::Blocked { .. } => {
                Some(format!("⚠ goal ({}/3)", goal.blocked_count))
            }
            runtime::GoalState::Paused { .. } => None,
        }
    }

    #[cfg(feature = "full-tui")]
    pub(crate) fn session_id_snapshot(&self) -> &str {
        &self.session.id
    }

    /// Phase 2: Attach a StatusEmitter that will be injected into every
    /// subsequently-built AnthropicRuntimeClient via prepare_turn_runtime.
    /// The emitter receives streaming events (TextDelta, Usage, MessageStop,
    /// StreamStart, ToolUse) and should update the caller's shared state
    /// (e.g., TuiApp's OutputView + StatusBarState).
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_status_emitter(&mut self, emitter: crate::streaming::StatusEmitter) {
        self.status_emitter = Some(emitter);
    }

    /// Phase 2: Detach any previously-attached status emitter. Useful for
    /// cleanup or switching emitters between sessions.
    #[cfg(feature = "full-tui")]
    pub(crate) fn clear_status_emitter(&mut self) {
        self.status_emitter = None;
    }

    /// Phase 2: Toggle TUI mode. When on, run_turn calls prepare_turn_runtime
    /// with emit_output=false so consume_stream's `out` goes to io::sink()
    /// instead of stdout — preventing duplicate output in alternate screen.
    /// Streaming content is captured via the status_emitter's TextDelta callback.
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_tui_mode(&mut self, on: bool) {
        self.tui_mode = on;
    }
}


// ===== Block B: build_system_prompt / load_prompt_extras / is_broad_working_directory (main.rs lines 2626-2750) =====

pub(crate) fn build_system_prompt(model: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
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
pub(crate) fn load_prompt_extras(cwd: &Path) -> SystemPromptExtras {
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
pub(crate) fn is_broad_working_directory(cwd: &Path) -> bool {
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


// ===== Block C: build_runtime / build_runtime_with_plugin_state / CliHookProgressReporter / CliPermissionPrompter (main.rs lines 2761-2938) =====

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_runtime(
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
pub(crate) fn build_runtime_with_plugin_state(
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

pub(crate) struct CliHookProgressReporter;

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

pub(crate) struct CliPermissionPrompter {
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

