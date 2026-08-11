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

use crate::commands_handler::*;
use crate::doctor::*;
use crate::format::*;
use crate::init::initialize_repo;
use crate::input;
use crate::paste::{
    expand_paste_placeholders, fold_pasted_input, format_pasted_text_ref, paste_cache_path,
    paste_cache_root, pasted_text_ref_num_lines, read_clipboard_text, should_fold_paste,
    store_paste_and_make_placeholder, try_auto_expand_clipboard, PASTE_FOLD_CHAR_THRESHOLD,
    PASTE_FOLD_LINE_THRESHOLD,
};
use crate::plugin_state::{
    build_plugin_manager, build_runtime_mcp_state, build_runtime_plugin_state,
    build_runtime_plugin_state_with_loader, mcp_annotation_flag, mcp_runtime_tool_definition,
    mcp_wrapper_tool_definitions, permission_mode_for_mcp_tool, plugins_command_payload_for,
    plugins_command_payload_from_result, resolve_plugin_path,
    runtime_hook_config_from_plugin_hooks, RuntimeMcpState, RuntimePluginState,
    RuntimePluginStateBuildOutput,
};
use crate::render::{MarkdownStreamState, OutputVerbosity, Spinner, TerminalRenderer};
use crate::session_mgr::{
    civil_from_days, collect_session_prompt_history, confirm_session_deletion,
    create_managed_session_handle, current_session_store, default_export_filename,
    delete_managed_session, format_history_timestamp, format_session_modified_age,
    interactive_session_pick, latest_managed_session, list_managed_sessions,
    load_session_reference, looks_like_slash_command_token, new_cli_session,
    new_cli_session_with_roots, parse_history_count, recent_user_context,
    render_prompt_history_report, render_session_list, render_session_markdown,
    resolve_export_path, resolve_managed_session_path, resolve_session_reference,
    resume_command_can_absorb_token, resume_session, run_export, run_resume_command,
    run_resumed_session_command, session_clear_backup_path, session_details_json,
    session_exists_json, session_reference_exists, sessions_dir,
    summarize_tool_payload_for_markdown, write_session_clear_backup, ManagedSessionSummary,
    PromptHistoryEntry, ResumeCommandOutcome, SessionHandle, SessionLifecycleKind,
    SessionLifecycleSummary, DEFAULT_HISTORY_LIMIT, LATEST_SESSION_REFERENCE,
    LEGACY_SESSION_EXTENSION, PRIMARY_SESSION_EXTENSION, SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT,
    SESSION_REFERENCE_ALIASES,
};
use crate::streaming::{
    build_system_blocks, collect_prompt_cache_events, collect_tool_results, collect_tool_uses,
    compact_tool_output_for_model, convert_messages, extract_system_messages, final_assistant_text,
    format_context_window_blocked_error, format_user_visible_api_error,
    mark_last_tool_with_cache_control, permission_policy, push_output_block,
    render_thinking_block_summary, request_ends_with_tool_result, response_to_events,
    AnthropicRuntimeClient, HookAbortMonitor, NETWORK_ERROR_KEYWORDS, POST_TOOL_STALL_TIMEOUT,
};
use crate::suggestion::{
    common_prefix_len, levenshtein_distance, looks_like_subcommand_typo, ranked_suggestions,
    render_suggestion_line, suggest_closest_term, suggest_similar_subcommand,
    suggest_slash_commands, CLI_OPTION_SUGGESTIONS,
};
use crate::tool_display::{
    clear_rustyline_echo, estimate_display_width, extract_tool_path, first_visible_line,
    format_bash_call, format_bash_result, format_edit_result, format_generic_tool_result,
    format_glob_result, format_grep_result, format_patch_preview, format_read_result,
    format_search_start, format_structured_patch_preview, format_tool_call_start,
    format_tool_result, format_tool_result_card_close, format_tool_result_compact,
    format_user_message_card, format_write_result, indent_with_card_prefix, is_wide_char,
    print_user_card, short_tool_id, summarize_tool_payload, truncate_for_summary,
    truncate_output_for_display, CliToolExecutor, ListMcpResourcesRequest, McpToolRequest,
    ReadMcpResourceRequest, ToolSearchRequest, DISPLAY_TRUNCATION_NOTICE, READ_DISPLAY_MAX_CHARS,
    READ_DISPLAY_MAX_LINES, TOOL_CARD_PREFIX, TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    TOOL_OUTPUT_DISPLAY_MAX_LINES, USER_CARD_PREFIX,
};
use crate::ultraplan::{
    describe_tool_progress, format_internal_prompt_progress_line, InternalPromptProgressEvent,
    InternalPromptProgressReporter, InternalPromptProgressRun, InternalPromptProgressShared,
    InternalPromptProgressState, INTERNAL_PROGRESS_HEARTBEAT_INTERVAL,
};
use api::{
    detect_provider_kind, model_family_identity_for, model_requires_reasoning_content_in_history,
    model_token_limit, CacheControl, ContentBlockDelta, InputContentBlock, InputMessage,
    MessageRequest, MessageResponse, OutputContentBlock, ProviderClient as ApiProviderClient,
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
use plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use runtime::{
    check_base_commit, format_stale_base_warning, format_usd, load_system_prompt,
    load_system_prompt_with_extras, pricing_for_model, resolve_expected_base,
    resolve_sandbox_status, ApiClient, ApiRequest, AssistantEvent, BaseCommitState,
    CompactionConfig, ConfigLoader, ConfigSource, ContentBlock, ContextAssembler,
    ConversationMessage, ConversationRuntime, HistoryIndex, McpServer, McpServerManager,
    McpServerSpec, McpTool, MessageRole, ModelPricing, PermissionMode, PermissionPolicy,
    ProjectContext, PromptCacheEvent, RepoMap, ResolvedPermissionMode, RuntimeError, Session,
    SystemPromptExtras, SystemPromptSplit, TokenBudget, TokenUsage, ToolError, ToolExecutor,
    UsageTracker,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tools::{
    execute_tool, init_lsp_from_config, mvp_tool_specs, GlobalToolRegistry, RuntimeToolDefinition,
    ToolSearchOutput,
};

// ACP stdio server:把 ClawAgent 通过 stdin/stdout JSON-RPC 暴露给外部编辑器
// 协议版本由 feature 决定:默认 acp-0_10(0.10.4),acp-1_5(1.3) 需
// --no-default-features --features acp-1_5 编译。
#[cfg(all(feature = "acp-0_10", not(feature = "acp-1_5")))]
use claw_shell::{run_stdio_agent, ClawAgentBuilder};
#[cfg(feature = "acp-1_5")]
use claw_shell::{run_stdio_agent_v1_3, ClawAgentV13Builder};
use tokio_util::sync::CancellationToken;

// 从 crate root 引入共享符号（CliOutputFormat、ModelProvenance、共享 helper 等）
use crate::{
    classify_error_kind, command_exists, config_alias_for_current_dir,
    config_model_for_current_dir, config_permission_mode_for_current_dir, current_tool_registry,
    default_permission_mode, filter_tool_specs, format_connected_line, git_output, git_status_ok,
    merge_prompt_with_stdin, normalize_allowed_tools, parse_dump_manifests_args, parse_export_args,
    parse_permission_mode_arg, parse_resume_args, parse_system_prompt_args,
    permission_mode_from_label, permission_mode_from_resolved, plugin_command_json,
    plugin_load_failure_json, plugin_summary_json, provider_label, read_piped_stdin,
    resolve_model_alias, resolve_model_alias_with_config, resolve_repl_model, split_error_hint,
    validate_model_syntax, write_temp_text_file, AllowedToolSet, CliOutputFormat, ModelProvenance,
    ModelSource, BUILD_TARGET, DEFAULT_DATE, DEFAULT_MODEL, DEPRECATED_INSTALL_COMMAND, GIT_SHA,
    OFFICIAL_REPO_SLUG, OFFICIAL_REPO_URL, VERSION,
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

/// Detect if cwd is inside a `target/debug` or `target/release` directory
/// and walk up to the project root (parent of `target/`). This prevents
/// workspace-boundary errors when the binary is launched from its build output.
///
/// Returns the corrected path if a fix-up was applied, `None` otherwise.
pub(crate) fn correct_cwd_from_target_dir() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;

    // Check if the last two components are target/<profile>
    let components: Vec<&std::ffi::OsStr> = cwd
        .components()
        .map(std::path::Component::as_os_str)
        .collect();
    let n = components.len();
    if n < 2 {
        return None;
    }
    let parent = components[n - 2];
    let leaf = components[n - 1];
    if parent == std::ffi::OsStr::new("target")
        && (leaf == std::ffi::OsStr::new("debug") || leaf == std::ffi::OsStr::new("release"))
    {
        // Walk up to the directory containing `target/`
        let project_root = cwd
            .ancestors()
            .nth(2) // skip leaf + "target"
            .map(Path::to_path_buf);
        if let Some(ref root) = project_root {
            if root.join("Cargo.toml").exists() || root.join(".git").exists() {
                let _ = env::set_current_dir(root);
                return Some(root.clone());
            }
        }
    }
    None
}

/// 从 session 历史收集最近的项目目录(用于 project_picker)。
///
/// 遍历 managed sessions,加载每个 session 文件提取 `workspace_root`,
/// 去重后按 `updated_at_ms` 倒序排列,只保留通过 `is_project_dir` 检测的目录。
///
/// 返回 `(路径, 最后更新时间)` 列表,最多 5 条。
fn collect_recent_project_dirs() -> Vec<(PathBuf, chrono::DateTime<chrono::Utc>)> {
    let summaries = match list_managed_sessions() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[project_picker] failed to list sessions: {e}");
            return vec![];
        }
    };

    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut projects: Vec<(PathBuf, chrono::DateTime<chrono::Utc>)> = Vec::new();

    for summary in summaries {
        // 加载 session 文件读 workspace_root
        let session = match Session::load_from_path(&summary.path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(root) = session.workspace_root else {
            continue;
        };
        if !root.is_dir() {
            continue;
        }
        if !crate::tui_ports::project_picker::is_project_dir(&root) {
            continue;
        }
        if !seen.insert(root.clone()) {
            continue; // 去重
        }
        let ts = chrono::DateTime::from_timestamp_millis(summary.updated_at_ms as i64)
            .unwrap_or_else(chrono::Utc::now);
        projects.push((root, ts));
        if projects.len() >= 5 {
            break;
        }
    }

    // 按时间倒序
    projects.sort_by_key(|a| std::cmp::Reverse(a.1));
    projects
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
        // Interactive mode: 用 project_picker 让用户选择项目目录(如果有 recent)
        // 或确认继续(无 recent 时回退到原 y/N 确认)。
        let recent_dirs = collect_recent_project_dirs();

        if recent_dirs.is_empty() {
            // 无最近项目目录,回退到原 y/N 确认流程
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
            return Ok(());
        }

        // 有最近项目目录:显示 project_picker 菜单
        use crate::tui_ports::project_picker;
        let pq = project_picker::build_project_question(&recent_dirs, &cwd);
        print!("{}", project_picker::render_question_stdout(&pq));
        io::stdout().flush()?;

        loop {
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match project_picker::parse_choice(&pq, &input) {
                Some(Some(chosen_path)) => {
                    // 用户选择了一个项目目录,切换过去
                    if let Err(e) = env::set_current_dir(chosen_path) {
                        eprintln!("Failed to switch to {}: {e}", chosen_path.display());
                        std::process::exit(1);
                    }
                    println!("Switched to: {}", chosen_path.display());
                    break;
                }
                Some(None) => {
                    // "Don't ask me again" —— 继续在当前目录
                    // (TODO: 持久化偏好到 config.toml,后续启动跳过检测)
                    break;
                }
                None => {
                    // 无效输入,重新提示
                    print!("Invalid choice. Select [1-N]: ");
                    io::stdout().flush()?;
                }
            }
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
                eprintln!("{}", json_error_envelope(&message));
            }
            CliOutputFormat::Text => {}
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
#[allow(clippy::too_many_arguments)] // Stage 1 验收:9 参数超 clippy 默认上限 7,重构参数到 struct 待 Stage 2 处理。
pub(crate) fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    allow_broad_cwd: bool,
    additional_workspace_roots: Vec<PathBuf>,
    output_verbosity: OutputVerbosity,
    enable_plan_mode: bool,
    enable_policy_engine: bool,
    enable_auto_planner: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    enforce_broad_cwd_policy(allow_broad_cwd, CliOutputFormat::Text)?;
    correct_cwd_from_target_dir();
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
    // Harness O(编排)层:Plan/Execute/Review 三段循环接入(Step 2.1)。
    // `--enable-plan-mode` 时启用,默认关闭。详见
    // `docs/harness-engineering-optimization-plan.md` Step 2.1。
    // P3-1:settings.json `planMode: true` 也能启用(在 LiveCli::new 内部处理),
    // CLI flag 优先级更高(此处覆盖)。
    // workspace_root 对所有工具路径都需要(notebook_update/recall_full/subagent 等),
    // 不应仅绑定在 plan_mode 内。
    cli.runtime.set_workspace_root(std::env::current_dir()?);
    if enable_plan_mode {
        cli.runtime.set_plan_mode_enabled(true);
    }
    // P1-1:PolicyEngine 策略引擎 flag。
    // 当前 lane_completion 模块已实现 PolicyEngine 调用(tools/lane_completion.rs),
    // 但生产路径未接入。flag 用于控制后续 lane 完成时是否调用策略评估。
    // 实际接入在 P1-2/P1-3 中完成(green_contract 桥接 + g004 校验闭环)。
    if enable_policy_engine {
        eprintln!("[policy] PolicyEngine enabled (lane completion policy evaluation active)");
    }
    // PlannerAgent 自动拆解 flag — 启用后复杂输入自动拆解为子任务并并行派发。
    // 依赖 P0/P1 容错加固:retry + FailFast::Off + 限流 + validation gate。
    // 默认开启,用 `--no-auto-planner` 关闭。
    if enable_auto_planner {
        runtime::planner::set_auto_planner_enabled(true);
        eprintln!("[planner] Auto-planner enabled (complex inputs will be decomposed and spawned in parallel)");
    } else {
        runtime::planner::set_auto_planner_enabled(false);
        eprintln!("[planner] Auto-planner disabled (--no-auto-planner)");
    }
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
                    // P0-1 适配：try_auto_expand_clipboard 现在返回三元组
                    // (display, expanded, raw_clipboard)。CLI 路径不需要 raw_clipboard，
                    // 用 _ 丢弃（TUI 路径会复用以避免第二次 PowerShell 调用）。
                    try_auto_expand_clipboard(
                        &trimmed,
                        &cli.session.id,
                        &mut paste_id_gen,
                        &mut pending_paste_lines,
                    )
                    .map(|(display, expanded, _clipboard)| (display, expanded))
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
    /// TUI 本地命令输出捕获：当设置时，`tui_println` 会把内容追加到此
    /// buffer 而不是打印到 stdout（避免破坏 alternate screen）。
    /// 由 TuiApp 在执行斜杠命令前设置，执行后清除。
    #[cfg(feature = "full-tui")]
    tui_output: Option<std::sync::Arc<std::sync::Mutex<crate::tui::output_view::OutputBuffer>>>,
    /// TUI 中断支持：当前 turn 的 abort signal handle。
    /// run_turn 开始时设置，turn 结束（成功/失败/中断）时清空。
    /// TUI 层 Ctrl+C（busy 时）通过此 handle 取消当前 turn。
    #[cfg(feature = "full-tui")]
    current_abort_signal: Option<runtime::HookAbortSignal>,
    /// TUI 中断支持：外部注入的 abort signal。
    /// TUI 层在启动 worker thread 前设置（保留 clone 用于 Ctrl+C abort），
    /// prepare_turn_runtime 优先使用此 signal（而非创建新的），
    /// prepare_turn_runtime 优先使用此 signal（而非创建新的），
    /// 让 TUI 主线程能取消正在执行的 turn。
    #[cfg(feature = "full-tui")]
    external_abort_signal: Option<runtime::HookAbortSignal>,
    /// 细粒度诊断回调：在 run_turn 关键路径埋点，写入 claw-diag.log。
    #[cfg(feature = "full-tui")]
    diag_callback: Option<Box<dyn Fn(String) + Send>>,
    /// 工具完成回调（P-fix）：runtime 内置工具（log_decision 等）不经
    /// CliToolExecutor，不 emit ToolResult 事件，导致 TUI ToolCard 永久 ⏳。
    /// 此回调在 prepare_turn_runtime 注入 runtime 的 tool_result_callback，
    /// 转发为 StatusEvent::ToolResult 让 TUI 闭合卡片。
    #[cfg(feature = "full-tui")]
    tool_result_callback: Option<runtime::ToolResultCallback>,
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
        #[cfg(feature = "full-tui")]
        crate::diag_log(&format!("[LiveCli::new] start, model={model}"));
        let t0 = std::time::Instant::now();
        #[cfg(feature = "full-tui")]
        crate::diag_log("[LiveCli::new] calling build_system_prompt");
        let system_prompt = build_system_prompt(&model)?;
        #[cfg(feature = "full-tui")]
        crate::diag_log("[LiveCli::new] build_system_prompt OK");
        let t_sp = t0.elapsed();
        let session_state = new_cli_session_with_roots(additional_workspace_roots)?;
        #[cfg(feature = "full-tui")]
        crate::diag_log("[LiveCli::new] new_cli_session_with_roots OK");
        let t_sess = t0.elapsed();
        let session = create_managed_session_handle(&session_state.session_id)?;
        #[cfg(feature = "full-tui")]
        crate::diag_log("[LiveCli::new] create_managed_session_handle OK");
        let t_handle = t0.elapsed();
        #[cfg(feature = "full-tui")]
        crate::diag_log("[LiveCli::new] calling build_runtime");
        let mut runtime = build_runtime(
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
            // workspace_root 对所有工具路径都需要(notebook_update/recall_full/
            // subagent 等),且与 config 解析正交——config 解析失败时不应导致
            // workspace_root 缺失。因此无条件设置,不嵌套在 loader.load() 分支内。
            runtime.set_workspace_root(cwd.clone());
            let loader = ConfigLoader::default_for(&cwd);
            if let Ok(config) = loader.load() {
                if let Some(poor) = config.feature_config().poor_mode() {
                    runtime::poor_mode::set_active(poor);
                }
                // P3-1:从 settings.json `planMode` 控制 Plan/Execute/Review。
                // 默认启用(true),CLI flag `--enable-plan-mode` 在 run_repl 中覆盖。
                match config.feature_config().plan_mode() {
                    Some(true) => runtime.set_plan_mode_enabled(true),
                    Some(false) => runtime.set_plan_mode_enabled(false),
                    None => {} // 保持默认(true)
                }
                // SP4.2-B3:从配置初始化 LSP servers(best-effort,失败不阻断启动)
                let _ = init_lsp_from_config(&config, &cwd);
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
            tui_output: None,
            #[cfg(feature = "full-tui")]
            current_abort_signal: None,
            #[cfg(feature = "full-tui")]
            external_abort_signal: None,
            #[cfg(feature = "full-tui")]
            diag_callback: None,
            #[cfg(feature = "full-tui")]
            tool_result_callback: None,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_reasoning_effort(effort);
        }
    }

    /// 读取当前 reasoning_effort 设置（供 TUI 侧栏显示）。
    pub(crate) fn reasoning_effort(&self) -> Option<String> {
        if let Some(rt) = self.runtime.runtime.as_ref() {
            rt.api_client().reasoning_effort()
        } else {
            None
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
            runtime::GoalState::Active => {
                Some("\x1b[38;5;240m │ \x1b[32m🎯 goal\x1b[0;38;5;240m".to_string())
            }
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
        &mut self,
        emit_output: bool,
    ) -> Result<
        (BuiltRuntime, HookAbortMonitor, runtime::HookAbortSignal),
        Box<dyn std::error::Error>,
    > {
        // TUI 中断支持：优先使用外部注入的 abort signal（由 TUI 层在 spawn
        // worker thread 前设置），让 TUI 主线程能通过 Ctrl+C 取消当前 turn。
        // 非中断模式（CLI/JSON 等）创建新的 signal。
        #[cfg(feature = "full-tui")]
        let hook_abort_signal = self.external_abort_signal.clone().unwrap_or_default();
        #[cfg(not(feature = "full-tui"))]
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
                // Also inject into CliToolExecutor so ToolResult events are emitted
                rt.tool_executor_mut()
                    .set_status_emitter(Arc::clone(emitter));
            }
        }
        // 细粒度诊断：将 TUI 层的 diag callback 注入 runtime，在 run_turn
        // 关键路径埋点，写入 claw-diag.log 帮助定位"会话卡死"问题。
        #[cfg(feature = "full-tui")]
        if let Some(cb) = self.diag_callback.take() {
            if let Some(rt) = runtime.runtime.take() {
                runtime.runtime = Some(rt.with_diag_callback(cb));
            }
        }
        // P-fix:将 TUI 层的 tool_result_callback 注入 runtime,让内置工具
        // (log_decision 等)完成后补发 ToolResult 事件闭合 ToolCard。
        // 仅在有 status_emitter(TUI 模式)时注入,CLI/headless 无 UI 消费。
        #[cfg(feature = "full-tui")]
        if self.status_emitter.is_some() {
            if let Some(cb) = self.tool_result_callback.take() {
                if let Some(rt) = runtime.runtime.take() {
                    runtime.runtime = Some(rt.with_tool_result_callback(cb));
                }
            }
        }
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal.clone());

        Ok((runtime, hook_abort_monitor, hook_abort_signal))
    }

    fn replace_runtime(&mut self, runtime: BuiltRuntime) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_plugins()?;
        self.runtime = runtime;
        Ok(())
    }

    pub(crate) fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        // REPL 路径:emit_output=true,consume_stream 写入 stdout,
        // spinner/println/print_status_bar 直接输出到终端。
        // TUI 路径请用 run_turn_tui(分离自原 tui_mode gating)。
        let (mut runtime, hook_abort_monitor, _abort_signal) = self.prepare_turn_runtime(true)?;
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        spinner.tick(
            "🦀 Thinking...",
            TerminalRenderer::new().color_theme(),
            &mut stdout,
        )?;
        let mut permission_prompter: Box<dyn runtime::PermissionPrompter> =
            Box::new(CliPermissionPrompter::new(self.permission_mode));
        // Tier S #1 Goal 持续驱动：在调 runtime.run_turn 之前 prepend goal 前缀。
        // 前缀包含 goal 文本、状态（active/blocked）、blocked 计数、token 用量。
        // LLM 每轮都看到 goal 上下文，驱动持续工作。Paused 状态不注入。
        let goal_prefix = self.goal_manager.render_prompt_prefix();
        let full_input = match &goal_prefix {
            Some(prefix) => format!("{prefix}{input}"),
            None => input.to_string(),
        };
        let result = runtime.run_turn(&full_input, Some(&mut *permission_prompter));
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
                self.accumulate_usage(summary.usage);
                // Tier S #1 Goal 持续驱动：累加本次回合的 token 用量到 goal_manager。
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

    /// TUI 模式专用 turn 入口(Phase A Step A5:从原 `tui_mode` gating 分离)。
    ///
    /// 与 `run_turn` 的差异:
    /// - `emit_output=false`:consume_stream 写入 io::sink,流式内容通过
    ///   status_emitter 的 TextDelta 回调驱动 TUI 渲染,避免 alternate screen
    ///   下重复输出。
    /// - 抑制所有 stdout/stderr 写入(spinner/println/print_status_bar/eprintln),
    ///   因为它们绑定到 alternate-screen 终端,stray write 会破坏 TUI 渲染。
    ///   状态更新(accumulate_usage/goal_manager.record_tokens/persist_session/
    ///   replace_runtime)仍执行。
    /// - 使用 `TuiSilentPermissionPrompter`:crossterm event loop 拥有 stdin
    ///   (raw mode),CliPermissionPrompter 的阻塞 read_line 会挂起或读到垃圾,
    ///   非交互 prompter 自动 deny(带清晰原因)让 turn 快速失败而非 wedging TUI。
    /// - 保存 `current_abort_signal` handle,让 TUI 层 Ctrl+C 能取消当前 turn。
    #[cfg(feature = "full-tui")]
    pub(crate) fn run_turn_tui(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor, abort_signal) = self.prepare_turn_runtime(false)?;
        // TUI 中断支持：保存 abort signal handle，让 TUI 层 Ctrl+C 能取消当前 turn。
        self.current_abort_signal = Some(abort_signal.clone());
        let mut permission_prompter: Box<dyn runtime::PermissionPrompter> =
            Box::new(TuiSilentPermissionPrompter::new(self.permission_mode));
        let goal_prefix = self.goal_manager.render_prompt_prefix();
        let full_input = match &goal_prefix {
            Some(prefix) => format!("{prefix}{input}"),
            None => input.to_string(),
        };
        let result = runtime.run_turn(&full_input, Some(&mut *permission_prompter));
        hook_abort_monitor.stop();
        match result {
            Ok(summary) => {
                self.replace_runtime(runtime)?;
                self.accumulate_usage(summary.usage);
                let turn_tokens = u64::from(summary.usage.total_tokens());
                let _ = self.goal_manager.record_tokens(turn_tokens);
                self.persist_session()?;
                // TUI 中断支持：turn 成功结束后清空 abort signal handle。
                self.current_abort_signal = None;
                Ok(())
            }
            Err(error) => {
                runtime.shutdown_plugins()?;
                let error_str = error.to_string().to_ascii_lowercase();
                let is_network_error = NETWORK_ERROR_KEYWORDS
                    .iter()
                    .any(|kw| error_str.contains(kw));
                if is_network_error {
                    let _ = self.goal_manager.pause("network error");
                }
                // TUI 中断支持：turn 结束（含错误/中断）后清空 abort signal handle。
                self.current_abort_signal = None;
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
        let (mut runtime, hook_abort_monitor, _abort_signal) = self.prepare_turn_runtime(false)?;
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
        let (mut runtime, hook_abort_monitor, _abort_signal) = self.prepare_turn_runtime(false)?;
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
        let (mut runtime, hook_abort_monitor, _abort_signal) = self.prepare_turn_runtime(false)?;
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
    pub(crate) fn handle_repl_command(
        &mut self,
        command: SlashCommand,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(match command {
            SlashCommand::Help => {
                let help = render_repl_help();
                if !self.tui_println(&help) {
                    println!("{help}");
                }
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
                // Step 2.1:对接 runtime planner。
                // 返回 true 表示 plan mode 状态变更,需要 persist session。
                self.run_ultraplan(task.as_deref())?;
                true
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
                self.print_sandbox_status();
                false
            }
            SlashCommand::Compact => {
                self.compact()?;
                false
            }
            SlashCommand::Model { model } => self.set_model(model)?,
            SlashCommand::DetectionStrategy {
                strategy,
                dry_run,
                verify,
            } => self.set_detection_strategy(strategy, dry_run, verify)?,
            SlashCommand::Permissions { mode } => self.set_permissions(mode)?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => self.resume_session(session_path)?,
            SlashCommand::Config { section } => {
                self.print_config(section.as_deref())?;
                false
            }
            SlashCommand::Mcp { action, target } => {
                let args = match (action.as_deref(), target.as_deref()) {
                    (None, None) => None,
                    (Some(action), None) => Some(action.to_string()),
                    (Some(action), Some(target)) => Some(format!("{action} {target}")),
                    (None, Some(target)) => Some(target.to_string()),
                };
                self.print_mcp(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Memory => {
                self.print_memory()?;
                false
            }
            SlashCommand::Init => {
                let cwd = env::current_dir()?;
                let report = initialize_repo(&cwd, false)?;
                let message = report.render();
                if !self.tui_println(&message) {
                    println!("{message}");
                }
                false
            }
            SlashCommand::InitForce => {
                let cwd = env::current_dir()?;
                let report = initialize_repo(&cwd, true)?;
                let message = report.render();
                if !self.tui_println(&message) {
                    println!("{message}");
                }
                false
            }
            SlashCommand::Diff => {
                self.print_diff()?;
                false
            }
            SlashCommand::Search { query } => {
                let q = query.as_deref().unwrap_or("");
                let session = self.runtime.session();
                let results = crate::session_mgr::search_session_history(session, q);
                let output = if results.is_empty() {
                    format!("Search\n  Query           {q}\n  Result           no matches found")
                } else {
                    let mut s = format!(
                        "Search\n  Query           {q}\n  Matches          {}\n\n",
                        results.len()
                    );
                    for (i, (idx, preview)) in results.iter().take(20).enumerate() {
                        s.push_str(&format!("  {}. [msg {idx}] {preview}\n", i + 1));
                    }
                    if results.len() > 20 {
                        s.push_str(&format!("\n  ... and {} more matches", results.len() - 20));
                    }
                    s
                };
                if !self.tui_println(&output) {
                    println!("{output}");
                }
                false
            }
            SlashCommand::Undo => {
                let session = self.runtime.session();
                let message = crate::session_mgr::undo_last_file_edit(session);
                if !self.tui_println(&message) {
                    println!("{message}");
                }
                // Persist session so any subsequent re-loads see a consistent
                // state (the undo wrote to disk; nothing changes in the
                // session history itself, but saving keeps timestamps fresh).
                let _ = self.persist_session();
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
                self.print_agents(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Skills { args } => {
                match classify_skills_slash_command(args.as_deref()) {
                    SkillSlashDispatch::Invoke(prompt) => self.run_turn(&prompt)?,
                    SkillSlashDispatch::Local => {
                        self.print_skills(args.as_deref(), CliOutputFormat::Text)?;
                    }
                }
                false
            }
            SlashCommand::Doctor => {
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = render_doctor_report()?.render();
                if !self.tui_println(&content) {
                    println!("{content}");
                }
                false
            }
            SlashCommand::History { count } => {
                self.print_prompt_history(count.as_deref());
                false
            }
            SlashCommand::Stats => {
                let usage = UsageTracker::from_session(self.runtime.session()).cumulative_usage();
                let report = format_cost_report(usage);
                if !self.tui_println(&report) {
                    println!("{report}");
                }
                false
            }
            SlashCommand::Summary => {
                let report =
                    crate::session_mgr::render_session_summary_text(self.runtime.session());
                if !self.tui_println(&report) {
                    println!("{report}");
                }
                false
            }
            SlashCommand::Context { action } => {
                // /context [clear]:仅支持查看;clear 提示使用 /clear。
                if let Some(action) = action.as_deref() {
                    let msg = if action == "clear" {
                        "Context\n  Hint             use /clear to reset the session transcript"
                            .to_string()
                    } else {
                        format!("Context\n  Unsupported action {action}")
                    };
                    if !self.tui_println(&msg) {
                        println!("{msg}");
                    }
                    return Ok(false);
                }
                let report = crate::session_mgr::render_context_report(self.runtime.session());
                if !self.tui_println(&report) {
                    println!("{report}");
                }
                false
            }
            SlashCommand::Usage { .. } => {
                let usage = self.runtime.usage();
                let report = crate::session_mgr::render_usage_report(
                    Some(&self.model),
                    usage.cumulative_usage(),
                    usage.current_turn_usage(),
                    usage.turns(),
                );
                if !self.tui_println(&report) {
                    println!("{report}");
                }
                false
            }
            SlashCommand::Poor { action } => {
                let (_, message) = handle_poor_mode_action(action.as_deref());
                if !self.tui_println(&message) {
                    println!("{message}");
                }
                false
            }
            SlashCommand::Goal { args } => {
                let message = handle_goal_command(&mut self.goal_manager, args.as_deref());
                if !self.tui_println(&message) {
                    println!("{message}");
                }
                false
            }
            SlashCommand::Bg { args } => {
                // Tier S #2 后台会话：REPL 模式下查询/管理后台进程。
                // 与 resume 模式共用 handle_bg_command，通过文件系统通信。
                let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let (message, _json) = handle_bg_command(args.as_deref(), &cwd);
                if !self.tui_println(&message) {
                    println!("{message}");
                }
                false
            }
            SlashCommand::Im { args } => {
                // IM Bridge: show status, config, or startup instructions.
                let (message, _json) = handle_im_command(args.as_deref());
                if !self.tui_println(&message) {
                    println!("{message}");
                }
                false
            }
            // /effort [low|medium|high|off] — 运行时调整 reasoning effort。
            // 无参数：显示当前值；off：清除（恢复默认）；low/medium/high：设置。
            // 设置后立即生效于下一次 API 请求，TUI 侧栏会同步显示。
            SlashCommand::Effort { level } => {
                let msg = match level.as_deref().map(str::trim) {
                    None => {
                        let current = self
                            .reasoning_effort()
                            .map(|v| format!("当前思考强度: {v}"))
                            .unwrap_or_else(|| "当前思考强度: 默认（未设置）".to_string());
                        format!("{current}\n用法: /effort low|medium|high|off")
                    }
                    Some("") => {
                        let current = self
                            .reasoning_effort()
                            .map(|v| format!("当前思考强度: {v}"))
                            .unwrap_or_else(|| "当前思考强度: 默认（未设置）".to_string());
                        format!("{current}\n用法: /effort low|medium|high|off")
                    }
                    Some("off") | Some("default") | Some("none") => {
                        self.set_reasoning_effort(None);
                        "思考强度已清除（恢复默认）".to_string()
                    }
                    Some("low") | Some("medium") | Some("high") => {
                        self.set_reasoning_effort(level.clone());
                        format!(
                            "思考强度已设置为: {}",
                            level.as_ref().expect("match arm guarantees Some")
                        )
                    }
                    Some(other) => {
                        format!(
                            "无效的思考强度: '{other}'\n有效值: low | medium | high | off\n用法: /effort low|medium|high|off"
                        )
                    }
                };
                if !self.tui_println(&msg) {
                    println!("{msg}");
                }
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
            | SlashCommand::Rename { .. }
            | SlashCommand::Copy { .. }
            | SlashCommand::Hooks { .. }
            | SlashCommand::Color { .. }
            | SlashCommand::Branch { .. }
            | SlashCommand::Rewind { .. }
            | SlashCommand::Ide { .. }
            | SlashCommand::Tag { .. }
            | SlashCommand::AddDir { .. } => {
                let cmd_name = command.slash_name();
                let msg = format!("{cmd_name} is not yet implemented in this build.");
                if !self.tui_println(&msg) {
                    eprintln!("{msg}");
                }
                false
            }
            SlashCommand::OutputStyle { style } => {
                if let Some(verbosity) = style.as_deref().and_then(OutputVerbosity::from_style_arg)
                {
                    self.output_verbosity = verbosity;
                    let msg = format!("Output style set to: {}", verbosity.label());
                    if !self.tui_println(&msg) {
                        println!("{msg}");
                    }
                } else {
                    let current = self.output_verbosity.label();
                    let msg = format!(
                        "Current output style: {current}\nAvailable styles: full, compact, silent, minimal\nUsage: /output-style [style]"
                    );
                    if !self.tui_println(&msg) {
                        println!("Current output style: {current}");
                        println!(
                            "Available styles: full, compact, silent, minimal\nUsage: /output-style [style]"
                        );
                    }
                }
                false
            }
            SlashCommand::Unknown(name) => {
                let msg = format_unknown_slash_command(&name);
                if !self.tui_println(&msg) {
                    eprintln!("{msg}");
                }
                false
            }
        })
    }

    pub(crate) fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    /// 当前会话的持久化文件路径（session JSONL）。
    /// 供 TUI 历史回看从文件流式重放。
    pub(crate) fn session_file_path(&self) -> std::path::PathBuf {
        self.session.path.clone()
    }

    fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content = format_status_report(
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
        );
        if !self.tui_println(&content) {
            println!("{content}");
        }
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
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content = render_prompt_history_report(&entries, limit);
        if !self.tui_println(&content) {
            println!("{content}");
        }
    }

    fn print_sandbox_status(&self) {
        let cwd = env::current_dir().expect("current dir");
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader
            .load()
            .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content =
            format_sandbox_report(&resolve_sandbox_status(runtime_config.sandbox(), &cwd));
        if !self.tui_println(&content) {
            println!("{content}");
        }
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

    /// v3 §4.7:`/detection-strategy` 命令处理 — 运行时切换决策检测策略。
    ///
    /// - 无参数:打印当前策略
    /// - `heuristic`:切换为启发式(零成本)
    /// - `llm`:切换为 LLM 提取(使用默认 flash 模型)
    /// - `llm:<model>`:切换为 LLM 提取并指定模型
    /// - `--dry-run`:预览切换结果而不实际应用(可与 strategy 组合)
    /// - `--verify`:校验 DecisionExtractorClient 注册状态
    ///
    /// flag 组合行为见 `parse_detection_strategy_args` 文档。
    /// 采用方案 A(直接 setter),不重建 runtime,因 `detection_strategy` 是简单字段。
    /// 切换到 `LlmExtract` 但未注册 client 时,提取逻辑会自动 3 路降级为 Heuristic。
    fn set_detection_strategy(
        &mut self,
        strategy: Option<String>,
        dry_run: bool,
        verify: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(rt) = self.runtime.runtime.as_mut() else {
            return Err("runtime not initialized".into());
        };

        let current = rt.detection_strategy().clone();
        let client_registered = runtime::is_decision_extractor_client_registered();

        // --verify 单独或组合使用:打印 client 注册校验报告
        // 若同时指定了 strategy,verify 在 dry-run/实际切换前先报告
        if verify && strategy.is_none() && !dry_run {
            // 仅 --verify:只校验当前 client
            let report = format_detection_strategy_verify_report(&current, client_registered);
            if !self.tui_println(&report) {
                println!("{report}");
            }
            return Ok(false);
        }

        let Some(strategy_arg) = strategy else {
            // 无 strategy 且无 verify:打印当前策略
            if !verify {
                let report = format_detection_strategy_report(&current);
                if !self.tui_println(&report) {
                    println!("{report}");
                }
                return Ok(false);
            }
            // --verify + --dry-run 但无 strategy:打印当前策略 + verify
            let report = format_detection_strategy_report(&current);
            let verify_report =
                format_detection_strategy_verify_report(&current, client_registered);
            let combined = format!("{report}\n\n{verify_report}");
            if !self.tui_println(&combined) {
                println!("{combined}");
            }
            return Ok(false);
        };

        let new_strategy = parse_detection_strategy(&strategy_arg).ok_or_else(|| {
            format!(
                "unsupported detection strategy '{strategy_arg}'. Use heuristic, llm, or llm:<model>."
            )
        })?;

        // --dry-run:仅预览,不应用
        if dry_run {
            let report = format_detection_strategy_dry_run_report(
                &current,
                &new_strategy,
                client_registered,
            );
            // 若同时 --verify,追加 verify 报告
            let final_report = if verify {
                let verify_report =
                    format_detection_strategy_verify_report(&new_strategy, client_registered);
                format!("{report}\n\n{verify_report}")
            } else {
                report
            };
            if !self.tui_println(&final_report) {
                println!("{final_report}");
            }
            return Ok(false);
        }

        // 实际切换
        if new_strategy == current {
            let report = format_detection_strategy_report(&new_strategy);
            if !self.tui_println(&report) {
                println!("{report}");
            }
            return Ok(false);
        }

        rt.set_detection_strategy(new_strategy.clone());
        let switch_report = format_detection_strategy_switch_report(&current, &new_strategy);
        if !self.tui_println(&switch_report) {
            println!("{switch_report}");
        }
        Ok(true)
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
            let content =
                "clear: confirmation required; run /clear --confirm to start a fresh session.";
            if !self.tui_println(content) {
                println!("{content}");
            }
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
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content = format!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            self.model,
            self.permission_mode.as_str(),
            self.session.id,
            self.session.path.display(),
        );
        if !self.tui_println(&content) {
            println!("{content}");
        }
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content = format_cost_report(cumulative);
        if !self.tui_println(&content) {
            println!("{content}");
        }
    }

    fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            let usage = render_resume_usage();
            if !self.tui_println(&usage) {
                println!("{usage}");
            }
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
        let report = format_resume_report(
            &self.session.path.display().to_string(),
            message_count,
            self.runtime.usage().turns(),
        );
        if !self.tui_println(&report) {
            println!("{report}");
        }
        Ok(true)
    }

    fn print_config(&self, section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content = render_config_report(section)?;
        if !self.tui_println(&content) {
            println!("{content}");
        }
        Ok(())
    }

    fn print_memory(&self) -> Result<(), Box<dyn std::error::Error>> {
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content = render_memory_report()?;
        if !self.tui_println(&content) {
            println!("{content}");
        }
        Ok(())
    }

    pub(crate) fn print_agents(
        &self,
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => {
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = handle_agents_slash_command(args, &cwd)?;
                if !self.tui_println(&content) {
                    println!("{content}");
                }
            }
            CliOutputFormat::Json => {
                let content =
                    serde_json::to_string_pretty(&handle_agents_slash_command_json(args, &cwd)?)?;
                if !self.tui_println(&content) {
                    println!("{content}");
                }
            }
        }
        Ok(())
    }

    pub(crate) fn print_mcp(
        &self,
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
            CliOutputFormat::Text => {
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = handle_mcp_slash_command(args, &cwd)?;
                if !self.tui_println(&content) {
                    println!("{content}");
                }
            }
            CliOutputFormat::Json => {
                let value = handle_mcp_slash_command_json(args, &cwd)?;
                // Propagate ok:false → non-zero exit so automation callers
                // can rely on exit code instead of inspecting the envelope.
                // (#68: mcp error envelopes previously always exited 0.)
                let is_error = value.get("ok").and_then(|v| v.as_bool()) == Some(false);
                let content = serde_json::to_string_pretty(&value)?;
                if !self.tui_println(&content) {
                    println!("{content}");
                }
                if is_error {
                    std::process::exit(1);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn print_skills(
        &self,
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => {
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = handle_skills_slash_command(args, &cwd)?;
                if !self.tui_println(&content) {
                    println!("{content}");
                }
            }
            CliOutputFormat::Json => {
                let content =
                    serde_json::to_string_pretty(&handle_skills_slash_command_json(args, &cwd)?)?;
                if !self.tui_println(&content) {
                    println!("{content}");
                }
            }
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

    fn print_diff(&self) -> Result<(), Box<dyn std::error::Error>> {
        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
        let content = render_diff_report()?;
        if !self.tui_println(&content) {
            println!("{content}");
        }
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
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = render_session_list(&self.session.id)?;
                if !self.tui_println(&content) {
                    println!("{content}");
                }
                Ok(false)
            }
            Some("pick") => {
                // TUI 模式下不支持交互式 stdin 选择（会卡死 worker 线程），
                // 回退为显示会话列表，提示用户用 /session switch <id> 切换。
                #[cfg(feature = "full-tui")]
                let in_tui = self.tui_output.is_some();
                #[cfg(not(feature = "full-tui"))]
                let in_tui = false;

                if in_tui || !io::stdin().is_terminal() {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = render_session_list(&self.session.id)?;
                    if !self.tui_println(&content) {
                        println!("{content}");
                    }
                    if in_tui {
                        let hint = "\n提示：使用 /session switch <session-id> 切换到指定会话";
                        if !self.tui_println(hint) {
                            println!("{hint}");
                        }
                    }
                    return Ok(false);
                }
                match interactive_session_pick(&self.session.id)? {
                    Some(picked) => {
                        let target_id = picked.id.clone();
                        let target_path = picked.path.clone();
                        // Reuse the existing switch logic by delegating to
                        // load_session_reference + replace_runtime.
                        let (handle, session) = load_session_reference(&target_id)?;
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
                        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                        let content = format!(
                            "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                            self.session.id,
                            target_path.display(),
                            message_count,
                        );
                        if !self.tui_println(&content) {
                            println!("{content}");
                        }
                        Ok(true)
                    }
                    None => {
                        // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                        let content = "Session pick cancelled.";
                        if !self.tui_println(content) {
                            println!("{content}");
                        }
                        Ok(false)
                    }
                }
            }
            Some("exists") => {
                let Some(target) = target else {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = "Usage: /session exists <session-id>";
                    if !self.tui_println(content) {
                        println!("{content}");
                    }
                    return Ok(false);
                };
                let exists = session_reference_exists(target)?;
                let handle = resolve_session_reference(target).ok();
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = format!(
                    "Session exists\n  Session          {target}\n  Exists           {exists}{}",
                    handle
                        .as_ref()
                        .map(|handle| format!("\n  File             {}", handle.path.display()))
                        .unwrap_or_default()
                );
                if !self.tui_println(&content) {
                    println!("{content}");
                }
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = "Usage: /session switch <session-id>";
                    if !self.tui_println(content) {
                        println!("{content}");
                    }
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
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = format!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                if !self.tui_println(&content) {
                    println!("{content}");
                }
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
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = format!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    self.session.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    self.session.path.display(),
                    message_count,
                );
                if !self.tui_println(&content) {
                    println!("{content}");
                }
                Ok(true)
            }
            Some("delete") => {
                let Some(target) = target else {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = "Usage: /session delete <session-id> [--force]";
                    if !self.tui_println(content) {
                        println!("{content}");
                    }
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = format!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    if !self.tui_println(&content) {
                        println!("{content}");
                    }
                    return Ok(false);
                }
                // TUI 模式下跳过阻塞式 stdin 确认（会卡死 worker 线程），
                // 提示用户使用 --force 标志直接删除。
                #[cfg(feature = "full-tui")]
                let in_tui = self.tui_output.is_some();
                #[cfg(not(feature = "full-tui"))]
                let in_tui = false;

                if in_tui {
                    let content = format!(
                        "TUI 模式下不支持交互式确认。如需删除会话，请使用：\n  /session delete {} --force",
                        handle.id
                    );
                    if !self.tui_println(&content) {
                        println!("{content}");
                    }
                    return Ok(false);
                }
                if !confirm_session_deletion(&handle.id) {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = "delete: cancelled.";
                    if !self.tui_println(content) {
                        println!("{content}");
                    }
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = format!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                if !self.tui_println(&content) {
                    println!("{content}");
                }
                Ok(false)
            }
            Some("delete-force") => {
                let Some(target) = target else {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = "Usage: /session delete <session-id> [--force]";
                    if !self.tui_println(content) {
                        println!("{content}");
                    }
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                    let content = format!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    if !self.tui_println(&content) {
                        println!("{content}");
                    }
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = format!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                if !self.tui_println(&content) {
                    println!("{content}");
                }
                Ok(false)
            }
            Some(other) => {
                // 走 tui_println 以避免在 TUI 模式下破坏 alternate screen
                let content = format!(
                    "Unknown /session action '{other}'. Use /session list, /session exists <session-id>, /session switch <session-id>, /session fork [branch-name], or /session delete <session-id> [--force]."
                );
                if !self.tui_println(&content) {
                    println!("{content}");
                }
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

    /// Step 2.1:对接 `/ultraplan` CLI 命令到 runtime planner。
    ///
    /// 行为:
    /// - 启用 plan mode(本会话内生效,无需 `--enable-plan-mode` CLI flag)。
    /// - 设置 workspace_root(若未设置),用于 PlanArtifact 持久化到
    ///   `<workspace>/.claw/plans/<id>.json`。
    /// - 若提供 `task` → 立即触发 `run_turn(task)`,让 runtime 内部的
    ///   `assess_complexity` 自动检测为 Complex 并创建 PlanArtifact。
    ///   PlanArtifact 通过末尾追加到 dynamic_sections 注入,不污染缓存
    ///   绝对/半稳定区(§5.2 缓存保护)。
    /// - 若未提供 `task` → 仅打印提示信息,等用户后续输入触发 plan。
    ///
    /// 与 `--enable-plan-mode` CLI flag 的区别:
    /// - `--enable-plan-mode`:整个会话启用 plan mode,所有复杂任务都触发。
    /// - `/ultraplan`:本会话启用 plan mode,且若提供 task 则立即触发一次。
    fn run_ultraplan(&mut self, task: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        // 启用 plan mode(若已启用则幂等)。
        self.runtime.set_plan_mode_enabled(true);
        // 设置 workspace_root(若未设置)— 用于 PlanArtifact 持久化。
        if self.runtime.active_plan().is_none() {
            // 只有 runtime 没有活跃 plan 时才设置,避免覆盖已有 workspace。
            // 通过检查 workspace_root 是否已设置决定。
            // 注意:runtime 没有公开 workspace_root() getter,这里保守地
            // 总是设置(若已设置会被相同值覆盖,无副作用)。
            let cwd = std::env::current_dir()?;
            self.runtime.set_workspace_root(cwd);
        }

        let plan_enabled_msg = "Plan mode enabled. Complex tasks (>200 chars or matching keywords) will trigger Plan→Execute→Review cycle.";
        if !self.tui_println(plan_enabled_msg) {
            println!("{plan_enabled_msg}");
        }

        if let Some(task) = task.map(str::trim).filter(|s| !s.is_empty()) {
            // 有 task → 立即触发 run_turn,让 runtime 自动通过 assess_complexity
            // 检测并创建 PlanArtifact。run_turn 会处理 plan 的整个生命周期
            // (Plan → Execute → Review → Replan/AllPassed/Failed)。
            self.run_turn(task)?;
        } else {
            // 无 task → 仅启用 plan mode,提示用户后续输入。
            let hint = "Now enter your task. The runtime will auto-detect complexity and create a PlanArtifact for complex tasks.";
            if !self.tui_println(hint) {
                println!("{hint}");
            }
        }
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
        crate::format::status_context(None)
            .ok()
            .and_then(|c| c.git_branch)
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

    /// TUI 侧栏轮次统计：返回累计 AI 思考轮次（每个 turn +1）。
    /// 底层由 `runtime::UsageTracker::turns()` 维护。
    #[cfg(feature = "full-tui")]
    pub(crate) fn turns_snapshot(&self) -> u32 {
        self.runtime.usage().turns()
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

    /// TUI 中断支持：取消当前正在执行的 turn。
    /// 通过 abort hook_abort_signal，agent loop 在下一次迭代顶部检测到
    /// 后退出。正在进行的 API 流式请求无法中断（阻塞 IO），但可以阻止
    /// 下一轮迭代（不再发起新请求、不再执行新工具）。
    /// 返回 true 表示已发送取消信号，false 表示当前没有正在执行的 turn。
    #[cfg(feature = "full-tui")]
    pub(crate) fn abort_current_turn(&mut self) -> bool {
        if let Some(signal) = &self.current_abort_signal {
            signal.abort();
            true
        } else {
            false
        }
    }

    /// TUI 中断支持：设置外部 abort signal。
    /// TUI 层在启动 worker thread 前调用此方法，传入 abort signal 的 clone。
    /// prepare_turn_runtime 会优先使用此 signal，让 TUI 主线程能通过保留的
    /// clone 取消当前 turn。turn 结束后由 clear_external_abort_signal 清空。
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_external_abort_signal(&mut self, signal: runtime::HookAbortSignal) {
        self.external_abort_signal = Some(signal);
    }

    /// TUI 中断支持：清空外部 abort signal（turn 结束后调用）。
    #[cfg(feature = "full-tui")]
    pub(crate) fn clear_external_abort_signal(&mut self) {
        self.external_abort_signal = None;
    }

    /// 细粒度诊断支持：设置 diag callback，在 run_turn 关键路径埋点。
    /// callback 接收 `[diag] ...` 格式的消息，应写入 claw-diag.log。
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_diag_callback(&mut self, cb: Box<dyn Fn(String) + Send>) {
        self.diag_callback = Some(cb);
    }

    /// 细粒度诊断支持：清空 diag callback（turn 结束后调用）。
    #[cfg(feature = "full-tui")]
    pub(crate) fn clear_diag_callback(&mut self) {
        self.diag_callback = None;
    }

    /// 工具完成回调支持：设置 tool_result_callback，供 TUI 闭合
    /// runtime 内置工具（log_decision 等）的 ToolCard。参数
    /// (tool_use_id, tool_name, output, is_error)。
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_tool_result_callback(&mut self, cb: runtime::ToolResultCallback) {
        self.tool_result_callback = Some(cb);
    }

    /// 工具完成回调支持：清空 tool_result_callback（turn 结束后调用）。
    #[cfg(feature = "full-tui")]
    pub(crate) fn clear_tool_result_callback(&mut self) {
        self.tool_result_callback = None;
    }

    /// TUI 模式下设置本地命令输出捕获 buffer。设置后，`tui_println` 会把
    /// 内容追加到此 buffer 而非 stdout，避免破坏 alternate screen。
    /// 由 TuiApp 在执行斜杠命令前调用。
    #[cfg(feature = "full-tui")]
    pub(crate) fn set_tui_output(
        &mut self,
        handle: std::sync::Arc<std::sync::Mutex<crate::tui::output_view::OutputBuffer>>,
    ) {
        self.tui_output = Some(handle);
    }

    /// 清除 TUI 输出捕获 buffer，后续 `tui_println` 回退到 stdout。
    #[cfg(feature = "full-tui")]
    pub(crate) fn clear_tui_output(&mut self) {
        self.tui_output = None;
    }

    /// TUI 感知的 println：若设置了 tui_output，把 msg + '\n' 追加到 buffer
    /// 并返回 true；否则返回 false，调用方应 fallback 到 `println!`。
    /// 用于 `handle_repl_command` 中简单 println 分支，让斜杠命令在 TUI
    /// 模式下能把输出显示在输出区而非破坏 alternate screen。
    #[cfg(feature = "full-tui")]
    fn tui_println(&self, msg: &str) -> bool {
        if let Some(handle) = &self.tui_output {
            if let Ok(mut buf) = handle.lock() {
                buf.append(msg);
                buf.append("\n");
            }
            true
        } else {
            false
        }
    }

    #[cfg(not(feature = "full-tui"))]
    fn tui_println(&self, _msg: &str) -> bool {
        false
    }
}

// ===== Block B: build_system_prompt / load_prompt_extras / is_broad_working_directory (main.rs lines 2626-2750) =====

pub(crate) fn build_system_prompt(model: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    #[cfg(feature = "full-tui")]
    crate::diag_log(&format!("[build_system_prompt] start, model={model}"));
    let cwd = env::current_dir()?;
    #[cfg(feature = "full-tui")]
    crate::diag_log(&format!("[build_system_prompt] cwd={}", cwd.display()));

    // Step 4.3:在 repomap 渲染前初始化 LSP server。
    // build_system_prompt 先于 build_runtime 执行,若不在此提前 spawn,
    // load_prompt_extras 里的 is_server_spawned("rust") 恒为 false,
    // render_with_lsp(跨模块语义重要性)永远不会生效。
    // best-effort:未配置 lspServers 或 spawn 失败时不阻断启动。
    // 注:build_runtime 阶段会再次调用 init_lsp_from_config,
    // 对已 spawn 的语言返回 "already spawned" 错误,被忽略,无害。
    let loader = ConfigLoader::default_for(&cwd);
    if let Ok(config) = loader.load() {
        let _ = init_lsp_from_config(&config, &cwd);
    }

    let extras = load_prompt_extras(&cwd);
    #[cfg(feature = "full-tui")]
    crate::diag_log(
        "[build_system_prompt] load_prompt_extras OK, calling load_system_prompt_with_extras",
    );
    let result = load_system_prompt_with_extras(
        cwd,
        DEFAULT_DATE,
        env::consts::OS,
        "unknown",
        model_family_identity_for(model),
        extras,
    )?;
    #[cfg(feature = "full-tui")]
    crate::diag_log("[build_system_prompt] load_system_prompt_with_extras OK");
    Ok(result)
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
    // load_and_freeze 内部已处理文件不存在的情况（返回空记忆），
    // 始终构造 PersistentMemory 以启用 nudge → add_entry → persist 链路，
    // 否则 memory.json 永远不会被创建（chicken-and-egg deadlock）。
    let persistent_memory = {
        let memory_path = cwd.join(".claw").join("memory.json");
        Some(runtime::PersistentMemory::load_and_freeze(&memory_path))
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
        // Step 4.3:若 rust-analyzer 已 spawn,用 render_with_lsp 获得 LSP
        // references 语义重要性(regex 子串匹配在 monorepo 跨模块定位不准)。
        // 仅在已 spawn 时启用,避免 prompt 组装阶段意外触发 slow auto-start。
        let registry = tools::global_lsp_registry();
        let rendered = if registry.is_server_spawned("rust") {
            map.render_with_lsp(registry)
        } else {
            map.render()
        };
        if rendered.trim().is_empty() {
            None
        } else {
            Some(rendered)
        }
    };
    let t_map = t0.elapsed();

    // Skill catalog: load all active skills (project + user + builtin) and
    // render a compact one-line-per-skill summary. Injected into the dynamic
    // region of the system prompt so the model can discover skills without
    // loading each SKILL.md.
    //
    // Best-effort: on filesystem errors we silently fall back to no catalog
    // rather than blocking startup. The catalog is session-stable — captured
    // once at startup, not refreshed per-turn.
    let skill_catalog = load_skill_catalog(cwd);
    let t_catalog = t0.elapsed();

    eprintln!(
        "[timing] load_prompt_extras: memory={:?} repomap={:?} catalog={:?} broad_cwd={} (cwd={})",
        t_mem,
        t_map,
        t_catalog,
        is_broad_cwd,
        cwd.display()
    );
    SystemPromptExtras {
        persistent_memory,
        repomap,
        skill_catalog,
    }
}

/// Load active skill summaries from all roots (project + user + builtin
/// shipped) and render them as a compact catalog string.
///
/// Returns `None` when:
/// - The skills catalog feature is disabled via settings (`skillsCatalogEnabled: false`)
/// - No skills are found (empty catalog would only waste tokens)
/// - A filesystem error occurs during discovery (best-effort fallback)
fn load_skill_catalog(cwd: &Path) -> Option<String> {
    // Respect the settings toggle. We need to load config first to check.
    // ConfigLoader::default_for is the same loader used by the main prompt
    // builder, so the toggle semantics match exactly.
    let config = runtime::ConfigLoader::default_for(cwd).load().ok()?;
    if !config.feature_config().skills_catalog_enabled_or_default() {
        return None;
    }

    // Load all active skills (shadowed ones are filtered out by
    // `list_skill_summaries`).
    let skills = commands::list_skill_summaries(cwd).ok()?;
    if skills.is_empty() {
        return None;
    }

    let catalog = commands::render_skill_catalog(&skills);
    if catalog.trim().is_empty() {
        None
    } else {
        Some(catalog)
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

/// design-gaps #5:从 `GlobalToolRegistry` 生成子 agent 工具签名目录。
///
/// 能力白名单使用**规范名**(read_file/grep_search/...,见
/// `SubagentCapability::allowed_tools()`),与注册表一致,无需映射;
/// `## Available Tools` 层展示的可调用名与 guard / API 工具定义全链路统一。
/// 描述取自注册表(`mvp_tool_specs` 单源真值,避免静态表描述漂移)。
///
/// `ConversationRuntime::build_subagent_context` 随后再按 capability 过滤
/// (ReadOnly 取只读子集,Execute 取全量)。`repomap` / `lsp_diagnostics` 未
/// 在注册表注册,自然不会出现在目录中。
fn subagent_tool_catalog(registry: &GlobalToolRegistry) -> Vec<runtime::ToolSummary> {
    let allowed: BTreeSet<String> = runtime::multi_agent::SubagentCapability::Execute
        .allowed_tools()
        .iter()
        .map(|s| s.to_string())
        .collect();
    registry
        .definitions(Some(&allowed))
        .into_iter()
        .map(|definition| runtime::ToolSummary {
            name: definition.name,
            description: definition.description.unwrap_or_default(),
        })
        .collect()
}

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
    // 根据模型 context window 提前获取 compaction 阈值,
    // 在 model 被 move 到 AnthropicRuntimeClient 之前完成。
    let context_window = model_token_limit(&model).map(|limit| limit.context_window_tokens);
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
    // Phase 4：提前创建 ProjectTopology 实例，供 CliToolExecutor 和 ConversationRuntime 共享。
    // 之前 CliToolExecutor 和 ConversationRuntime 各自独立，topology 只在 runtime 构造后注入，
    // 导致 CliToolExecutor 调用 refactor_algorithm_topo 时没有 topology 实例可用。
    // 现在提前创建，共享同一个 Arc。
    let shared_topology = env::current_dir()
        .ok()
        .map(|cwd| std::sync::Arc::new(runtime::project_topology::ProjectTopology::new(cwd)));
    // v0.2 生产接入:在 model 被 move 进 AnthropicRuntimeClient 之前先 clone 一份,
    // 稍后用于构造独立的 subagent api_client(DAG dispatch 用)。
    // AnthropicRuntimeClient 不是 Clone(内含 tokio::runtime::Runtime),只能重建。
    let model_for_subagent = model.clone();
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
        )
        .with_project_topology(shared_topology.clone()),
        policy,
        system_prompt,
        &feature_config,
    );
    if emit_output {
        runtime = runtime.with_hook_progress_reporter(Box::new(CliHookProgressReporter));
    }
    // design-gaps #5:注入子 agent 工具签名目录。
    // 从 GlobalToolRegistry 生成(描述与 mvp_tool_specs 单源一致),
    // build_subagent_context 按 capability 白名单过滤后注入 Available Tools 层。
    runtime = runtime.with_tool_catalog(subagent_tool_catalog(&tool_registry));
    // design-gaps #1:启用 hooks 配置热重载 — 每 turn 检查配置源
    // (settings.json / .claw.json 等)的 mtime,变化则原子替换 hooks 配置,
    // 无需重启会话。加载失败(如 cwd 不可用)时静默保持默认行为。
    if let Ok(cwd) = env::current_dir() {
        runtime = runtime.with_hooks_hot_reload(ConfigLoader::default_for(cwd));
    }
    // Harness C(Context Management)层接入:ContextAssembler 统一 prompt 注入。
    // 收集 Memory/Goal/Plan/remediation 等动态内容到 assembler,
    // 由 assemble() 按 7 级优先级栈排序,TokenBudget 控制各源上限。
    // 1M 模型(DeepSeek V4/GPT-5.4)使用缩放预算(480K 全局),
    // 200K 模型(Claude)使用标准预算(120K 全局)。
    // 详见 docs/harness-engineering-optimization-plan.md Step 2.3。
    {
        let budget = match context_window {
            Some(cw) => TokenBudget::for_context_window(cw),
            None => TokenBudget::default_claude(),
        };
        runtime = runtime.with_context_assembler(ContextAssembler::new(budget));
    }
    // P1-6 修复：注入 harness V(验证)层和 O(可观测性)层组件。
    // 之前 VerifierAgent / TraceAnalyzer 实现完整但从未注入主流程，
    // 导致 conversation.rs 中 `self.verifier_agent` / `self.trace_analyzer`
    // 永远为 None，相关代码分支永远走 else 路径，harness 层形同虚设。
    //
    // 现在无条件注入：
    // - VerifierAgent：内含 Rule/Visual/ModelJudge 三种 verifier。
    //   Rule 已实现，Visual/ModelJudge 是 placeholder（P0-2 修复后保守通过）。
    //   只在 `plan_mode_enabled && !plan.steps.is_empty()` 时被调用，
    //   未启用 plan mode 时不会有副作用。
    // - TraceAnalyzer：记录每次 turn 的 trace 数据（latency / failure_kind 等），
    //   未来可用于 CSV 导出和失败模式聚类。
    runtime = runtime
        .with_verifier_agent(runtime::VerifierAgent::new())
        .with_trace_analyzer(runtime::TraceAnalyzer::new());
    // design-gaps #2:注入 self-evolving harness archive。
    // 让会话运行时每 evolution_interval turn 自动执行 weakness mining +
    // 规则式 Proposer + 两重门控验证,失败教训跨会话沉淀。
    // 打开失败只静默跳过(archive 是可选增强,不应阻塞启动)。
    if let Ok(cwd) = env::current_dir() {
        runtime = runtime.with_harness_evolution(cwd);
    }
    // Epic 2:注入 MultiAgentCoordinator,启用 subagent-as-tool 路由。
    // 注入后,主 agent 可通过 dispatch_subagent tool 派发子 agent,
    // 通过 check_subagent tool 查询状态/结果。子 agent 走独立 LLM 请求 +
    // 独立 prompt cache,不污染主 agent 缓存(§5.2)。
    // 详见 plan.md §9.2 Epic 2。
    //
    // Epic 3:同时构造 TaskRegistry 并共享同一份 coordinator 引用,
    // 使 task 级元数据(状态/heartbeat/output)与 subagent 生命周期打通,
    // 为 LaneBoard 监控和后续 Epic 4 lane_events 提供数据源。
    // 详见 plan.md §9.2 Epic 3。
    let coordinator = runtime::MultiAgentCoordinator::new();

    // v2 Phase 2 Epic 4:启动恢复 — 扫描 .claw/checkpoints/*.json,
    // 把崩溃前未完成的 subagent 状态恢复到 registry,让 retry loop 接管。
    //
    // 语义边界:只恢复 subagent 注册表 + 元状态,不恢复 LLM 对话历史
    // (与 LangGraph/Temporal durable execution 一致)。
    // Running 状态会自动降级为 Created,允许重新 start() 调度。
    //
    // 失败处理:单个 checkpoint 恢复失败只 log,不中止整个启动
    // (避免一个损坏文件阻塞整个 CLI)。
    if let Ok(cwd) = env::current_dir() {
        let ckpt_dir = cwd.join(".claw").join("checkpoints");
        if let Ok(entries) = std::fs::read_dir(&ckpt_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    match coordinator.restore_from_checkpoint(&path) {
                        Ok(id) => {
                            eprintln!("[restore] subagent {id} restored from {}", path.display())
                        }
                        Err(e) => eprintln!("[restore] failed to restore {}: {e}", path.display()),
                    }
                }
            }
        }
    }

    // v2 §10.5 多 ValidationGate:注册 rust/npm/pytest gate。
    // 设计要点:
    // - gate 用 file_filter 正则隔离,Rust 子 agent 改 .rs 只触发 cargo-build,
    //   Node 子 agent 改 .ts/.tsx 只触发 npm-build,Python 同理 — 互不干扰
    // - 命令不存在时跳过注册(避免 retryable=false 中止 validation 链),
    //   借鉴 PATH 探测模式:有 cargo/npm/python 才注册对应 gate
    // - 无 workspace_root 时全部跳过(CommandValidationGate 需要 cwd)
    if let Ok(workspace_root) = env::current_dir() {
        if crate::command_exists("cargo") {
            coordinator.add_validation_gate(Box::new(
                runtime::multi_agent::validation::rust_compile_gate(workspace_root.clone()),
            ));
        }
        if crate::command_exists("npm") {
            coordinator.add_validation_gate(Box::new(
                runtime::multi_agent::validation::npm_build_gate(workspace_root.clone()),
            ));
        }
        if crate::command_exists("python") {
            coordinator.add_validation_gate(Box::new(
                runtime::multi_agent::validation::pytest_gate(workspace_root.clone()),
            ));
        }

        // v2 Phase 2 Epic 5:注册 LlmJudgeGate(诊断/架构任务的 LLM-as-judge 评分)。
        //
        // 设计要点:
        // - judge 模型用 subagent 同款模型(避免引入新配置项;用户可通过 --model 切换)
        // - max_tokens=1024(judge 只输出 0.0-1.0 分数 + 简短说明,1024 足够)
        // - 构造失败(无 API key / 模型名无效)时跳过注册,不阻断启动
        //   (降级为 MVP 行为:只有命令 gate,无 LLM judge)
        // - 注入后,LlmJudgeGate::validate 会在命令 gate 之后执行,
        //   对诊断/架构任务做四维评分(根因定位/方案可行性/完整性/副作用)
        match crate::llm_clients::DeepSeekJudgeClient::new(&model_for_subagent, Some(1024)) {
            Ok(judge_client) => {
                let judge: std::sync::Arc<dyn runtime::multi_agent::validation::JudgeClient> =
                    std::sync::Arc::new(judge_client);
                coordinator.add_validation_gate(Box::new(
                    runtime::multi_agent::validation::LlmJudgeGate::diagnostic_default(
                        &model_for_subagent,
                        workspace_root.clone(),
                    )
                    .with_client(judge),
                ));
            }
            Err(e) => {
                eprintln!("[startup] LlmJudgeGate skipped (judge client construction failed): {e}");
            }
        }
    }

    // v0.2 生产接入:保留一份 coordinator 的 Arc,用于构造 CoordinatorExecutor。
    // coordinator 是 Clone(内部全 Arc<Mutex>),clone 后再 move 给 with_multi_agent_coordinator。
    let coordinator_arc = std::sync::Arc::new(coordinator.clone());
    runtime = runtime.with_multi_agent_coordinator(coordinator);

    // v2 Phase 2 Epic 6:注入全局 DecisionExtractorClient。
    //
    // 注入后,context compaction 触发时 `extract_decisions_before_compaction` 的
    // `LlmExtract` 分支会真正调用 LLM 提取结构化决策点
    // (context/decision/rationale/alternatives),而非降级为 Heuristic。
    //
    // 设计要点:
    // - 用 subagent 同款模型(避免引入新配置项)
    // - max_tokens=2048(决策提取需输出 JSON 数组,2048 容纳多决策点)
    // - OnceLock 进程级单例,只能注册一次(重复调用静默忽略)
    // - 构造失败(无 API key / 模型名无效)时跳过,不阻断启动
    //   (降级为 Heuristic,保证不丢决策)
    // - 用 budget 模型降低成本(提取任务对推理能力要求低于 judge)
    match crate::llm_clients::DeepSeekDecisionExtractorClient::new(&model_for_subagent, Some(2048))
    {
        Ok(extractor) => {
            let extractor_client: std::sync::Arc<
                dyn runtime::decision_log::DecisionExtractorClient,
            > = std::sync::Arc::new(extractor);
            runtime::decision_log::set_global_decision_extractor_client(extractor_client);
            // v3 §4.7 端到端接入:client 注册成功后,把 runtime 的检测策略
            // 从默认的 Heuristic 升级为 LlmExtract。这样 maybe_auto_compact
            // 触发时会调用 LLM 提取结构化决策点(context/decision/rationale/
            // alternatives),而非仅做关键词匹配。
            //
            // 失败时(client 未注册 / LLM 调用失败 / JSON 解析失败)自动 3 路降级
            // 为 Heuristic,保证不阻塞 compaction(详见
            // decision_log::extract_decisions_with_llm)。
            runtime = runtime.with_detection_strategy(
                runtime::decision_log::DetectionStrategy::LlmExtract {
                    model: model_for_subagent.clone(),
                },
            );
        }
        Err(e) => {
            eprintln!("[startup] DecisionExtractorClient skipped (construction failed): {e}");
        }
    }

    // 知识新鲜度门控(Phase 1):注入全局 ResearchClient。
    //
    // 注入后,DAG 任务的 Novel 类(新版本/新论文)在派发前会触发联网调研
    // (WebSearch + WebFetch + LLM 摘要),摘要注入 system prompt,避免
    // 模型用过时参数知识自信地错答。Stable/Evolving 类不调研,零成本。
    //
    // 设计要点(镜像 DecisionExtractorClient 注入模式):
    // - 用 budget 模型(摘要任务对推理能力要求低,降成本)
    // - max_tokens=2048(摘要输出上限)
    // - OnceLock 进程级单例,重复注册静默忽略
    // - 构造失败(无 API key)时跳过,降级为不调研(不阻塞任务)
    // - 网络失败/超时由 gate_task 降级为 None(不阻塞任务)
    match crate::llm_clients::WebResearchClient::new(&model_for_subagent, Some(2048)) {
        Ok(researcher) => {
            let research_client: std::sync::Arc<dyn runtime::knowledge_freshness::ResearchClient> =
                std::sync::Arc::new(researcher);
            runtime::knowledge_freshness::set_global_research_client(research_client);
        }
        Err(e) => {
            eprintln!("[startup] ResearchClient skipped (construction failed): {e}");
        }
    }

    // 知识新鲜度门控(Phase 2.1):注入全局 FreshnessAssessor。
    //
    // 注入后,关键词评估为 Evolving(不确定)的任务会调 flash 模型做语义细化,
    // 区分"看似 Evolving 实则 Novel"(如冷门库新版本)和"确实 Stable"(如通用重构)。
    // Novel/Stable(强信号)不调 LLM,零额外成本。
    //
    // 设计要点:
    // - 用 budget 模型(评估任务对推理能力要求低,降成本)
    // - max_tokens=256(只返回 JSON,不需要长输出)
    // - OnceLock 进程级单例,重复注册静默忽略
    // - 构造失败(无 API key)时跳过,降级为纯关键词评估(不阻塞任务)
    match crate::llm_clients::DeepSeekFreshnessAssessor::new(&model_for_subagent, Some(256)) {
        Ok(assessor) => {
            let assessor_client: std::sync::Arc<
                dyn runtime::knowledge_freshness::FreshnessAssessor,
            > = std::sync::Arc::new(assessor);
            runtime::knowledge_freshness::set_global_freshness_assessor(assessor_client);
        }
        Err(e) => {
            eprintln!("[startup] FreshnessAssessor skipped (construction failed): {e}");
        }
    }

    // 知识新鲜度门控(Phase 2.3):注入全局 QueryBuilderClient。
    //
    // 注入后,Novel 任务(需调研)的搜索查询由 LLM 从任务文本提取关键实体构建,
    // 而非启发式截取前 200 字符。提升搜索命中率,调研摘要质量更高。
    // LLM 提取失败时降级回启发式 build_research_query。
    //
    // 设计要点:
    // - 用 budget 模型(提取任务对推理能力要求低,降成本)
    // - max_tokens=128(只返回关键词,不需要长输出)
    // - OnceLock 进程级单例,重复注册静默忽略
    // - 构造失败(无 API key)时跳过,降级为启发式查询(不阻塞任务)
    match crate::llm_clients::DeepSeekQueryBuilderClient::new(&model_for_subagent, Some(128)) {
        Ok(builder) => {
            let builder_client: std::sync::Arc<
                dyn runtime::knowledge_freshness::QueryBuilderClient,
            > = std::sync::Arc::new(builder);
            runtime::knowledge_freshness::set_global_query_builder(builder_client);
        }
        Err(e) => {
            eprintln!("[startup] QueryBuilderClient skipped (construction failed): {e}");
        }
    }

    // v1.0 LLM context editing:注入全局 CompactionSummarizerClient。
    //
    // 注入后,context compaction(`compact_session_with_trigger` →
    // `summarize_messages_with_llm`)会调用 LLM 生成模型摘要(语义压缩),
    // 而非纯启发式规则摘要;失败/未注入自动降级回启发式,不阻塞压缩。
    //
    // 设计要点:
    // - 用 budget 模型(与 decision extractor 同款),控制压缩成本
    // - max_tokens=2048(摘要输出通常足够)
    // - OnceLock 进程级单例,重复注册静默忽略
    // - 构造失败(无 API key)时跳过,启动不失败
    match crate::llm_clients::DeepSeekCompactionSummarizerClient::new(
        &model_for_subagent,
        Some(2048),
    ) {
        Ok(summarizer) => {
            let summarizer_client: std::sync::Arc<
                dyn runtime::compact::CompactionSummarizerClient,
            > = std::sync::Arc::new(summarizer);
            runtime::compact::set_global_compaction_summarizer_client(summarizer_client);
            eprintln!(
                "[startup] LLM compaction summarizer registered (model: {model_for_subagent})"
            );
        }
        Err(e) => {
            eprintln!("[startup] CompactionSummarizerClient skipped (construction failed): {e}");
        }
    }

    // D1.0 LLM-driven planning:注入全局 PlanGeneratorClient。
    //
    // 注入后,复杂任务(plan_mode + assess_complexity==Complex)由模型生成
    // 计划步骤(JSON),失败/未注入自动回退启发式 decompose_task。
    //
    // 设计要点:
    // - 复用主模型(与 decision extractor 同款策略,避免引入新配置项)
    // - max_tokens=2048(计划 JSON 通常足够)
    // - OnceLock 进程级单例,构造失败跳过,不阻断启动
    match crate::llm_clients::DeepSeekPlanGeneratorClient::new(&model_for_subagent, Some(2048)) {
        Ok(planner_client) => {
            let planner_client: std::sync::Arc<dyn runtime::planner::PlanGeneratorClient> =
                std::sync::Arc::new(planner_client);
            runtime::planner::set_global_plan_generator_client(planner_client);
            eprintln!("[startup] LLM plan generator registered (model: {model_for_subagent})");
        }
        Err(e) => {
            eprintln!("[startup] PlanGeneratorClient skipped (construction failed): {e}");
        }
    }

    // v0.2 生产接入:构造 CoordinatorExecutor 并装入 runtime,让 DAG 调度能真正
    // 执行子 agent turn(替代 v0.1 stub 路径)。
    //
    // 因 AnthropicRuntimeClient 不是 Clone(内含 tokio::runtime::Runtime),
    // 这里用之前 clone 的 model_for_subagent 重建一份独立的 client 给 subagent
    // dispatcher。subagent 走独立 LLM 请求 + 独立 prompt cache(§5.2 缓存保护),
    // enable_tools=false(subagent system prompt 明确不需要工具),
    // emit_output=false(避免子 agent 输出污染主 CLI),
    // progress_reporter=None(subagent 不需要进度回调)。
    if let Ok(workspace_root) = env::current_dir() {
        let subagent_api_client = AnthropicRuntimeClient::new(
            session_id,
            model_for_subagent,
            false, // enable_tools
            false, // emit_output
            None,  // allowed_tools
            tool_registry.clone(),
            None, // progress_reporter
        )?;
        runtime = runtime.with_dag_coordinator(
            coordinator_arc,
            subagent_api_client,
            workspace_root,
            None, // tool_executor — None=单轮无工具(向后兼容);Some(executor) 启用多轮 tool call
        );
    }

    // v0.2 生产接入:把 CoordinatorExecutor 注入 tools 层全局 registry,
    // 让 dag_run 工具的 "start" 分支能取出它构造 DagScheduler。
    // Arc<CoordinatorExecutor> 通过 unsizing 自动转换为
    // Arc<dyn SubagentExecutor + Send + Sync>。
    if let Some(executor) = runtime.coordinator_executor() {
        tools::set_coordinator_executor(executor.clone());
    }
    // 根据模型 context window 动态设置 compaction 阈值。
    // 1M 模型(DeepSeek V4/GPT-5.4)阈值 650K,200K 模型(Claude)阈值 130K,
    // 避免 100K 一刀切对长上下文模型过度激进压缩。
    if let Some(cw) = context_window {
        runtime = runtime.with_context_window(cw);
    }
    // Attach persistent memory for nudge curation. Loaded-and-frozen so the
    // nudge layer can write new entries to disk while the prompt's frozen
    // snapshot (loaded separately in load_prompt_extras) stays byte-stable
    // for the session — preserving the prompt-cache prefix.
    if let Ok(cwd) = env::current_dir() {
        let memory_path = cwd.join(".claw").join("memory.json");
        // load_and_freeze 内部已处理文件不存在的情况（返回空记忆），
        // 始终注入 PersistentMemory 以启用 nudge → add_entry → persist 链路，
        // 否则 memory.json 永远不会被创建（chicken-and-egg deadlock）。
        let memory = runtime::PersistentMemory::load_and_freeze(&memory_path);
        runtime = runtime.with_persistent_memory(memory);

        // Phase 4 认知外骨骼：注入 DecisionLog / ProjectTopology / RefactorTransaction。
        // 之前三个 with_* builder 已经定义但从未被调用，导致 LLM 调用 9 个新工具时
        // 全部返回 "not available" 降级字符串。现在无条件注入三个实例。
        // 详见 docs/agent-cognitive-exoskeleton-plan.md 第五章。
        //
        // DecisionLog：SQLite 决策库，失败时降级为 None（不阻断启动）。
        if let Ok(decision_log) = runtime::DecisionLog::open(&cwd) {
            runtime = runtime.with_decision_log(decision_log);
        }
        // ProjectTopology：复用提前创建的 shared_topology（与 CliToolExecutor 共享同一个 Arc）。
        if let Some(topo) = shared_topology.clone() {
            runtime = runtime.with_project_topology(topo);
        }
        // RefactorTransaction：非 git 仓库自动进入 Disabled 状态，安全无副作用。
        let tx = runtime::RefactorTransaction::new(cwd.clone());
        runtime = runtime.with_refactor_transaction(tx);
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
        // Enhanced permission prompt with box-drawing and colored tool name
        println!();
        println!("\x1b[33m┌─ ⚠ Permission approval required \x1b[0m");
        println!(
            "\x1b[33m│\x1b[0m Tool             \x1b[1;36m{}\x1b[0m",
            request.tool_name
        );
        println!(
            "\x1b[33m│\x1b[0m Current mode     {}",
            self.current_mode.as_str()
        );
        println!(
            "\x1b[33m│\x1b[0m Required mode    \x1b[1;31m{}\x1b[0m",
            request.required_mode.as_str()
        );
        if let Some(reason) = &request.reason {
            println!("\x1b[33m│\x1b[0m Reason           {reason}");
        }
        // Truncate very long inputs for display (UTF-8 safe)
        let input_display = if request.input.chars().count() > 200 {
            let truncated: String = request.input.chars().take(200).collect();
            format!("{truncated}… (truncated)")
        } else {
            request.input.clone()
        };
        println!("\x1b[33m│\x1b[0m Input            {input_display}");
        println!("\x1b[33m└─\x1b[0m Approve this tool call? [y/N]: ");
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

/// Non-interactive permission prompter for TUI mode.
///
/// BUG 2 fix: `CliPermissionPrompter::decide` calls `io::stdin().read_line`
/// which blocks waiting for line input. In TUI mode, crossterm's event loop
/// already owns stdin in raw mode, so a blocking `read_line` would either
/// hang the TUI forever or read raw-mode escape sequences as garbage. Instead
/// of fighting for stdin, this prompter auto-denies every request with a
/// clear reason pointing the user back to non-TUI mode for interactive
/// approval. This keeps the TUI responsive and the turn fails fast.
///
/// Recommended workflow: use `danger-full-access` permission mode in TUI if
/// you want tools to run without prompts, or run the same prompt in non-TUI
/// REPL mode when interactive approval is required.
pub(crate) struct TuiSilentPermissionPrompter {
    current_mode: PermissionMode,
}

impl TuiSilentPermissionPrompter {
    fn new(current_mode: PermissionMode) -> Self {
        Self { current_mode }
    }
}

impl runtime::PermissionPrompter for TuiSilentPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        // Never block on stdin. Surface a deny decision with an actionable
        // reason so the user knows to either switch permission mode or re-run
        // the prompt outside the TUI.
        let _ = self.current_mode; // suppress unused_field warning
        runtime::PermissionPromptDecision::Deny {
            reason: format!(
                "tool '{}' requires permission approval (mode '{}'), which is not available in TUI mode. Re-run `claw` without --tui to approve interactively, or use --permission-mode danger-full-access.",
                request.tool_name,
                request.required_mode.as_str()
            ),
        }
    }
}

// ===== ACP stdio server entrypoint =====
//
// `claw acp serve` 的入口:复用 AnthropicRuntimeClient + build_system_prompt +
// permission_policy 三件套构造 ClawAgentBuilder,然后交给 claw_shell::run_stdio_agent
// 在 current_thread + LocalSet 上跑 stdio ACP 服务器。
//
// 设计说明:
// - 不 emit CLI 输出(emit_output=false):stdout 被 JSON-RPC 流占用,任何 CLI 打印
//   都会污染协议
// - 不接 progress_reporter / status_emitter:stdio 模式下没有 TUI status bar 消费
// - session_id 用 UUID 生成:new_session 时 agent 会用 ACP 传入的 cwd 初始化 Session,
//   这里的 session_id 仅用于 AnthropicRuntimeClient 的 prompt cache 隔离
// - CancellationToken 暂不接 Ctrl+C:MVP 阶段靠 stdin EOF 退出;后续可加 signal handler

#[allow(clippy::needless_pass_by_value)]
pub fn run_acp_serve(
    model: String,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 生成 session_id(仅用于 prompt cache 隔离)
    let session_id = uuid::Uuid::new_v4().to_string();

    // 2. 构造 plugin state(feature_config + tool_registry + plugin_registry + mcp_state)
    let runtime_plugin_state = build_runtime_plugin_state()?;
    let RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        mcp_state: _mcp_state,
    } = runtime_plugin_state;
    // plugin_registry.initialize() 在 build_runtime_with_plugin_state 中被调用;
    // 这里 stdio agent 不直接用 plugin_registry(留给未来 hook 扩展),但仍 initialize
    // 以保持与 REPL 路径一致的副作用(注册 hooks 等)。
    plugin_registry.initialize()?;

    // 3. 构造 permission_policy(借用 tool_registry,不消耗)
    let policy = permission_policy(permission_mode, &feature_config, &tool_registry)
        .map_err(std::io::Error::other)?;

    // 4. 构造 api_client(clone tool_registry,因 AnthropicRuntimeClient 要持有)
    let api_client = AnthropicRuntimeClient::new(
        &session_id,
        model.clone(),
        true,  // enable_tools
        false, // emit_output(stdio 模式禁止 CLI 打印)
        None,  // allowed_tools
        tool_registry.clone(),
        None, // progress_reporter
    )?;

    // 5. 构造 system_prompt
    let system_prompt = build_system_prompt(&model)?;

    // 6. 按 feature 组装 builder 并启动 stdio ACP 服务器(阻塞直到 stdin EOF 或 cancel)
    //    默认 acp-0_10(0.10.4);acp-1_5(1.3) 需 --no-default-features --features acp-1_5 编译。
    //    MVP 阶段不接 Ctrl+C,靠 stdin EOF 退出。
    let cancel = CancellationToken::new();

    #[cfg(all(feature = "acp-0_10", not(feature = "acp-1_5")))]
    {
        let builder = ClawAgentBuilder::new(api_client, policy, system_prompt);
        run_stdio_agent(builder, cancel)?;
    }

    #[cfg(feature = "acp-1_5")]
    {
        // 1.3 路径:ClawAgentV13Builder 注入 api_client,turn 在命令循环中
        // 真实驱动 ConversationRuntime(Stage 3 接线)。
        let builder = ClawAgentV13Builder::new(api_client, policy, system_prompt);
        let agent = builder.build();
        run_stdio_agent_v1_3(agent, cancel)?;
    }

    Ok(())
}
