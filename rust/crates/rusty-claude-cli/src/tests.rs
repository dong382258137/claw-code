#![allow(dead_code, unused_imports, unused_variables)]

use crate::*;

use crate::{
    acp_status_json, build_runtime_plugin_state_with_loader, build_runtime_with_plugin_state,
    classify_error_kind, classify_session_lifecycle_from_panes, collect_session_prompt_history,
    create_managed_session_handle, describe_tool_progress, filter_tool_specs,
    format_bughunter_report, format_commit_preflight_report, format_commit_skipped_report,
    format_compact_report, format_connected_line, format_cost_report, format_history_timestamp,
    format_internal_prompt_progress_line, format_issue_report, format_model_report,
    format_model_switch_report, format_permissions_report, format_permissions_switch_report,
    format_pr_report, format_resume_report, format_status_report, format_tool_call_start,
    format_tool_result, format_ultraplan_report, format_unknown_slash_command,
    format_unknown_slash_command_message, format_user_visible_api_error, merge_prompt_with_stdin,
    normalize_permission_mode, parse_args, parse_export_args, parse_git_status_branch,
    parse_git_status_metadata_for, parse_git_workspace_summary, parse_history_count,
    permission_policy, print_help_to, push_output_block, render_config_report, render_diff_report,
    render_diff_report_for, render_help_topic, render_help_topic_json, render_memory_report,
    render_prompt_history_report, render_repl_help, render_resume_usage, render_session_list,
    render_session_markdown, resolve_model_alias, resolve_model_alias_with_config,
    resolve_repl_model, resolve_session_reference, response_to_events,
    resume_supported_slash_commands, run_resume_command, short_tool_id,
    slash_command_completion_candidates_with_sessions, split_error_hint, status_context,
    status_json_value, summarize_tool_payload_for_markdown, try_resolve_bare_skill_prompt,
    validate_no_args, CliAction, CliOutputFormat, CliToolExecutor, GitWorkspaceSummary,
    InternalPromptProgressEvent, InternalPromptProgressState, LiveCli, LocalHelpTopic,
    OutputVerbosity, PromptHistoryEntry, SessionLifecycleKind, SessionLifecycleSummary,
    SlashCommand, StatusUsage, TmuxPaneSnapshot, DEFAULT_MODEL, LATEST_SESSION_REFERENCE,
    STUB_COMMANDS,
};
use api::{ApiError, MessageResponse, OutputContentBlock, Usage};
use plugins::{
    PluginManager, PluginManagerConfig, PluginTool, PluginToolDefinition, PluginToolPermission,
};
use runtime::{
    load_oauth_credentials, save_oauth_credentials, AssistantEvent, ConfigLoader, ContentBlock,
    ConversationMessage, MessageRole, OAuthConfig, PermissionMode, Session, ToolExecutor,
};
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tools::GlobalToolRegistry;

fn registry_with_plugin_tool() -> GlobalToolRegistry {
    GlobalToolRegistry::with_plugin_tools(vec![PluginTool::new(
        "plugin-demo@external",
        "plugin-demo",
        PluginToolDefinition {
            name: "plugin_echo".to_string(),
            description: Some("Echo plugin payload".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        },
        "echo".to_string(),
        Vec::new(),
        PluginToolPermission::WorkspaceWrite,
        None,
    )])
    .expect("plugin tool registry should build")
}

#[test]
fn opaque_provider_wrapper_surfaces_failure_class_session_and_trace() {
    let error = ApiError::Api {
        status: "500".parse().expect("status"),
        error_type: Some("api_error".to_string()),
        message: Some(
            "Something went wrong while processing your request. Please try again, or use /new to start a fresh session."
                .to_string(),
        ),
        request_id: Some("req_jobdori_789".to_string()),
        body: String::new(),
        retryable: true,
        suggested_action: None,
        retry_after: None,
    };

    let rendered = format_user_visible_api_error("session-issue-22", &error);
    assert!(rendered.contains("provider_internal"));
    assert!(rendered.contains("session session-issue-22"));
    assert!(rendered.contains("trace req_jobdori_789"));
}

#[test]
fn retry_exhaustion_uses_retry_failure_class_for_generic_provider_wrapper() {
    let error = ApiError::RetriesExhausted {
        attempts: 3,
        last_error: Box::new(ApiError::Api {
            status: "502".parse().expect("status"),
            error_type: Some("api_error".to_string()),
            message: Some(
                "Something went wrong while processing your request. Please try again, or use /new to start a fresh session."
                    .to_string(),
            ),
            request_id: Some("req_jobdori_790".to_string()),
            body: String::new(),
            retryable: true,
            suggested_action: None,
            retry_after: None,
        }),
    };

    let rendered = format_user_visible_api_error("session-issue-22", &error);
    assert!(rendered.contains("provider_retry_exhausted"), "{rendered}");
    assert!(rendered.contains("session session-issue-22"));
    assert!(rendered.contains("trace req_jobdori_790"));
}

#[test]
fn context_window_preflight_errors_render_recovery_steps() {
    let error = ApiError::ContextWindowExceeded {
        model: "claude-sonnet-4-6".to_string(),
        estimated_input_tokens: 182_000,
        requested_output_tokens: 64_000,
        estimated_total_tokens: 246_000,
        context_window_tokens: 200_000,
    };

    let rendered = format_user_visible_api_error("session-issue-32", &error);
    assert!(rendered.contains("Context window blocked"), "{rendered}");
    assert!(rendered.contains("context_window_blocked"), "{rendered}");
    assert!(
        rendered.contains("Session          session-issue-32"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Model            claude-sonnet-4-6"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Input estimate   ~182000 tokens (heuristic)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Total estimate   ~246000 tokens (heuristic)"),
        "{rendered}"
    );
    assert!(rendered.contains("Compact          /compact"), "{rendered}");
    assert!(
        rendered.contains("Resume compact   claw --resume session-issue-32 /compact"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Fresh session    /clear --confirm"),
        "{rendered}"
    );
    assert!(rendered.contains("Reduce scope"), "{rendered}");
    assert!(rendered.contains("Retry            rerun"), "{rendered}");
}

#[test]
fn provider_context_window_errors_are_reframed_with_same_guidance() {
    let error = ApiError::Api {
        status: "400".parse().expect("status"),
        error_type: Some("invalid_request_error".to_string()),
        message: Some(
            "This model's maximum context length is 200000 tokens, but your request used 230000 tokens."
                .to_string(),
        ),
        request_id: Some("req_ctx_456".to_string()),
        body: String::new(),
        retryable: false,
        suggested_action: None,
        retry_after: None,
    };

    let rendered = format_user_visible_api_error("session-issue-32", &error);
    assert!(rendered.contains("context_window_blocked"), "{rendered}");
    assert!(
        rendered.contains("Trace            req_ctx_456"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Detail           This model's maximum context length is 200000 tokens"),
        "{rendered}"
    );
    assert!(rendered.contains("Compact          /compact"), "{rendered}");
    assert!(
        rendered.contains("Fresh session    /clear --confirm"),
        "{rendered}"
    );
}

#[test]
fn openai_configured_limit_errors_are_rendered_as_context_window_guidance() {
    let error = ApiError::Api {
        status: "400".parse().expect("status"),
        error_type: Some("invalid_request_error".to_string()),
        message: Some(
            "Input tokens exceed the configured limit of 922000 tokens. Your messages resulted in 1860900 tokens. Please reduce the length of the messages."
                .to_string(),
        ),
        request_id: Some("req_ctx_openai_456".to_string()),
        body: String::new(),
        retryable: false,
        suggested_action: None,
        retry_after: None,
    };

    let rendered = format_user_visible_api_error("session-issue-32", &error);
    assert!(rendered.contains("Context window blocked"), "{rendered}");
    assert!(rendered.contains("context_window_blocked"), "{rendered}");
    assert!(
        rendered.contains("Trace            req_ctx_openai_456"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "Detail           Input tokens exceed the configured limit of 922000 tokens."
        ),
        "{rendered}"
    );
    assert!(rendered.contains("Compact          /compact"), "{rendered}");
    assert!(
        rendered.contains("Fresh session    /clear --confirm"),
        "{rendered}"
    );
}

#[test]
fn retry_wrapped_context_window_errors_keep_recovery_guidance() {
    let error = ApiError::RetriesExhausted {
        attempts: 2,
        last_error: Box::new(ApiError::Api {
            status: "413".parse().expect("status"),
            error_type: Some("invalid_request_error".to_string()),
            message: Some("Request is too large for this model's context window.".to_string()),
            request_id: Some("req_ctx_retry_789".to_string()),
            body: String::new(),
            retryable: false,
            suggested_action: None,
            retry_after: None,
        }),
    };

    let rendered = format_user_visible_api_error("session-issue-32", &error);
    assert!(rendered.contains("Context window blocked"), "{rendered}");
    assert!(rendered.contains("context_window_blocked"), "{rendered}");
    assert!(
        rendered.contains("Trace            req_ctx_retry_789"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Detail           Request is too large for this model's context window."),
        "{rendered}"
    );
    assert!(rendered.contains("Compact          /compact"), "{rendered}");
    assert!(
        rendered.contains("Resume compact   claw --resume session-issue-32 /compact"),
        "{rendered}"
    );
}

fn temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rusty-claude-cli-{nanos}-{unique}"))
}

fn git(args: &[&str], cwd: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git command should run");
    assert!(
        status.success(),
        "git command failed: git {}",
        args.join(" ")
    );
}

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn with_current_dir<T>(cwd: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = cwd_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = std::env::current_dir().expect("cwd should load");
    std::env::set_current_dir(cwd).expect("cwd should change");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::env::set_current_dir(previous).expect("cwd should restore");
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn write_skill_fixture(root: &Path, name: &str, description: &str) {
    let skill_dir = root.join(name);
    fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
    )
    .expect("skill file should write");
}

fn write_plugin_fixture(root: &Path, name: &str, include_hooks: bool, include_lifecycle: bool) {
    fs::create_dir_all(root.join(".claude-plugin")).expect("manifest dir");
    if include_hooks {
        fs::create_dir_all(root.join("hooks")).expect("hooks dir");
        fs::write(
            root.join("hooks").join("pre.sh"),
            "#!/bin/sh\nprintf 'plugin pre hook'\n",
        )
        .expect("write hook");
    }
    if include_lifecycle {
        fs::create_dir_all(root.join("lifecycle")).expect("lifecycle dir");
        fs::write(
            root.join("lifecycle").join("init.sh"),
            "#!/bin/sh\nprintf 'init\\n' >> lifecycle.log\n",
        )
        .expect("write init lifecycle");
        fs::write(
            root.join("lifecycle").join("shutdown.sh"),
            "#!/bin/sh\nprintf 'shutdown\\n' >> lifecycle.log\n",
        )
        .expect("write shutdown lifecycle");
    }

    let hooks = if include_hooks {
        ",\n  \"hooks\": {\n    \"PreToolUse\": [\"./hooks/pre.sh\"]\n  }"
    } else {
        ""
    };
    let lifecycle = if include_lifecycle {
        ",\n  \"lifecycle\": {\n    \"Init\": [\"./lifecycle/init.sh\"],\n    \"Shutdown\": [\"./lifecycle/shutdown.sh\"]\n  }"
    } else {
        ""
    };
    fs::write(
        root.join(".claude-plugin").join("plugin.json"),
        format!(
            "{{\n  \"name\": \"{name}\",\n  \"version\": \"1.0.0\",\n  \"description\": \"runtime plugin fixture\"{hooks}{lifecycle}\n}}"
        ),
    )
    .expect("write plugin manifest");
}
#[test]
fn defaults_to_repl_when_no_args() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    assert_eq!(
        parse_args(&[]).expect("args should parse"),
        CliAction::Repl {
            model: DEFAULT_MODEL.to_string(),
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
            tui: true,
            enable_plan_mode: false,
            enable_policy_engine: false,
        }
    );
}

#[test]
fn default_permission_mode_uses_project_config_when_env_is_unset() {
    let _guard = env_lock();
    let root = temp_dir();
    let cwd = root.join("project");
    let config_home = root.join("config-home");
    std::fs::create_dir_all(cwd.join(".claw")).expect("project config dir should exist");
    std::fs::create_dir_all(&config_home).expect("config home should exist");
    std::fs::write(
        cwd.join(".claw").join("settings.json"),
        r#"{"permissionMode":"acceptEdits"}"#,
    )
    .expect("project config should write");

    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_permission_mode = std::env::var("RUSTY_CLAUDE_PERMISSION_MODE").ok();
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");

    let resolved = with_current_dir(&cwd, super::default_permission_mode);

    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_permission_mode {
        Some(value) => std::env::set_var("RUSTY_CLAUDE_PERMISSION_MODE", value),
        None => std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE"),
    }
    std::fs::remove_dir_all(root).expect("temp config root should clean up");

    assert_eq!(resolved, PermissionMode::WorkspaceWrite);
}

#[test]
fn env_permission_mode_overrides_project_config_default() {
    let _guard = env_lock();
    let root = temp_dir();
    let cwd = root.join("project");
    let config_home = root.join("config-home");
    std::fs::create_dir_all(cwd.join(".claw")).expect("project config dir should exist");
    std::fs::create_dir_all(&config_home).expect("config home should exist");
    std::fs::write(
        cwd.join(".claw").join("settings.json"),
        r#"{"permissionMode":"acceptEdits"}"#,
    )
    .expect("project config should write");

    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_permission_mode = std::env::var("RUSTY_CLAUDE_PERMISSION_MODE").ok();
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::set_var("RUSTY_CLAUDE_PERMISSION_MODE", "read-only");

    let resolved = with_current_dir(&cwd, super::default_permission_mode);

    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_permission_mode {
        Some(value) => std::env::set_var("RUSTY_CLAUDE_PERMISSION_MODE", value),
        None => std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE"),
    }
    std::fs::remove_dir_all(root).expect("temp config root should clean up");

    assert_eq!(resolved, PermissionMode::ReadOnly);
}

#[test]
fn resolve_cli_auth_source_ignores_saved_oauth_credentials() {
    let _guard = env_lock();
    let config_home = temp_dir();
    std::fs::create_dir_all(&config_home).expect("config home should exist");

    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let original_auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::remove_var("ANTHROPIC_AUTH_TOKEN");

    save_oauth_credentials(&runtime::OAuthTokenSet {
        access_token: "expired-access-token".to_string(),
        refresh_token: Some("refresh-token".to_string()),
        expires_at: Some(0),
        scopes: vec!["org:create_api_key".to_string(), "user:profile".to_string()],
    })
    .expect("save expired oauth credentials");

    let error = super::resolve_cli_auth_source_for_cwd()
        .expect_err("saved oauth should be ignored without env auth");

    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_api_key {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
    match original_auth_token {
        Some(value) => std::env::set_var("ANTHROPIC_AUTH_TOKEN", value),
        None => std::env::remove_var("ANTHROPIC_AUTH_TOKEN"),
    }
    std::fs::remove_dir_all(config_home).expect("temp config home should clean up");

    assert!(error.to_string().contains("ANTHROPIC_API_KEY"));
}

#[test]
fn parses_prompt_subcommand() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "prompt".to_string(),
        "hello".to_string(),
        "world".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::Prompt {
            prompt: "hello world".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn merge_prompt_with_stdin_returns_prompt_unchanged_when_no_pipe() {
    // given
    let prompt = "Review this";

    // when
    let merged = merge_prompt_with_stdin(prompt, None);

    // then
    assert_eq!(merged, "Review this");
}

#[test]
fn merge_prompt_with_stdin_ignores_whitespace_only_pipe() {
    // given
    let prompt = "Review this";
    let piped = "   \n\t\n  ";

    // when
    let merged = merge_prompt_with_stdin(prompt, Some(piped));

    // then
    assert_eq!(merged, "Review this");
}

#[test]
fn merge_prompt_with_stdin_appends_piped_content_as_context() {
    // given
    let prompt = "Review this";
    let piped = "fn main() { println!(\"hi\"); }\n";

    // when
    let merged = merge_prompt_with_stdin(prompt, Some(piped));

    // then
    assert_eq!(merged, "Review this\n\nfn main() { println!(\"hi\"); }");
}

#[test]
fn merge_prompt_with_stdin_trims_surrounding_whitespace_on_pipe() {
    // given
    let prompt = "Summarize";
    let piped = "\n\n  some notes  \n\n";

    // when
    let merged = merge_prompt_with_stdin(prompt, Some(piped));

    // then
    assert_eq!(merged, "Summarize\n\nsome notes");
}

#[test]
fn merge_prompt_with_stdin_returns_pipe_when_prompt_is_empty() {
    // given
    let prompt = "";
    let piped = "standalone body";

    // when
    let merged = merge_prompt_with_stdin(prompt, Some(piped));

    // then
    assert_eq!(merged, "standalone body");
}

#[test]
fn parses_bare_prompt_and_json_output_flag() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--output-format=json".to_string(),
        "--model".to_string(),
        "opus".to_string(),
        "explain".to_string(),
        "this".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::Prompt {
            prompt: "explain this".to_string(),
            model: "claude-opus-4-6".to_string(),
            output_format: CliOutputFormat::Json,
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn parses_quiet_flag_sets_compact_verbosity() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--quiet".to_string(),
        "explain".to_string(),
        "this".to_string(),
    ];
    let parsed = parse_args(&args).expect("args should parse");
    match parsed {
        CliAction::Prompt {
            output_verbosity, ..
        } => {
            assert_eq!(output_verbosity, OutputVerbosity::Compact);
        }
        other => panic!("expected Prompt, got {other:?}"),
    }
}

#[test]
fn parses_silent_flag_sets_silent_verbosity() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--silent".to_string(),
        "explain".to_string(),
        "this".to_string(),
    ];
    let parsed = parse_args(&args).expect("args should parse");
    match parsed {
        CliAction::Prompt {
            output_verbosity, ..
        } => {
            assert_eq!(output_verbosity, OutputVerbosity::Silent);
        }
        other => panic!("expected Prompt, got {other:?}"),
    }
}

#[test]
fn verbose_overrides_quiet_when_applied_after() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--quiet".to_string(),
        "--verbose".to_string(),
        "explain".to_string(),
        "this".to_string(),
    ];
    let parsed = parse_args(&args).expect("args should parse");
    match parsed {
        CliAction::Prompt {
            output_verbosity, ..
        } => {
            assert_eq!(output_verbosity, OutputVerbosity::Full);
        }
        other => panic!("expected Prompt, got {other:?}"),
    }
}

#[test]
fn parses_output_verbosity_eq_flag() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--output-verbosity=minimal".to_string(),
        "explain".to_string(),
        "this".to_string(),
    ];
    let parsed = parse_args(&args).expect("args should parse");
    match parsed {
        CliAction::Prompt {
            output_verbosity, ..
        } => {
            assert_eq!(output_verbosity, OutputVerbosity::Minimal);
        }
        other => panic!("expected Prompt, got {other:?}"),
    }
}

#[test]
fn rejects_invalid_output_verbosity_value() {
    let _guard = env_lock();
    let err = parse_args(&["--output-verbosity=loud".to_string(), "hi".to_string()])
        .expect_err("invalid verbosity should be rejected");
    assert!(err.contains("invalid value for --output-verbosity"));
    assert!(err.contains("loud"));
}

#[test]
fn parses_compact_flag_for_prompt_mode() {
    // given a bare prompt invocation that includes the --compact flag
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--compact".to_string(),
        "summarize".to_string(),
        "this".to_string(),
    ];

    // when parse_args interprets the flag
    let parsed = parse_args(&args).expect("args should parse");

    // then compact mode is propagated and other defaults stay unchanged
    assert_eq!(
        parsed,
        CliAction::Prompt {
            prompt: "summarize this".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            compact: true,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn prompt_subcommand_defaults_compact_to_false() {
    // given a `prompt` subcommand invocation without --compact
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec!["prompt".to_string(), "hello".to_string()];

    // when parse_args runs
    let parsed = parse_args(&args).expect("args should parse");

    // then compact stays false (opt-in flag)
    match parsed {
        CliAction::Prompt { compact, .. } => assert!(!compact),
        other => panic!("expected Prompt action, got {other:?}"),
    }
}

#[test]
fn resolves_model_aliases_in_args() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--model".to_string(),
        "opus".to_string(),
        "explain".to_string(),
        "this".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::Prompt {
            prompt: "explain this".to_string(),
            model: "claude-opus-4-6".to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn resolves_known_model_aliases() {
    assert_eq!(resolve_model_alias("opus"), "claude-opus-4-6");
    assert_eq!(resolve_model_alias("sonnet"), "claude-sonnet-4-6");
    assert_eq!(resolve_model_alias("haiku"), "claude-haiku-4-5-20251213");
    assert_eq!(resolve_model_alias("claude-opus"), "claude-opus");
}

#[test]
fn user_defined_aliases_resolve_before_provider_dispatch() {
    // given
    let _guard = env_lock();
    let root = temp_dir();
    let cwd = root.join("project");
    let config_home = root.join("config-home");
    std::fs::create_dir_all(cwd.join(".claw")).expect("project config dir should exist");
    std::fs::create_dir_all(&config_home).expect("config home should exist");
    std::fs::write(
        cwd.join(".claw").join("settings.json"),
        r#"{"aliases":{"fast":"claude-haiku-4-5-20251213","smart":"opus","cheap":"grok-3-mini"}}"#,
    )
    .expect("project config should write");

    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);

    // when
    let direct = with_current_dir(&cwd, || resolve_model_alias_with_config("fast"));
    let chained = with_current_dir(&cwd, || resolve_model_alias_with_config("smart"));
    let cross_provider = with_current_dir(&cwd, || resolve_model_alias_with_config("cheap"));
    let unknown = with_current_dir(&cwd, || resolve_model_alias_with_config("unknown-model"));
    let builtin = with_current_dir(&cwd, || resolve_model_alias_with_config("haiku"));

    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    std::fs::remove_dir_all(root).expect("temp config root should clean up");

    // then
    assert_eq!(direct, "claude-haiku-4-5-20251213");
    assert_eq!(chained, "claude-opus-4-6");
    assert_eq!(cross_provider, "grok-3-mini");
    assert_eq!(unknown, "unknown-model");
    assert_eq!(builtin, "claude-haiku-4-5-20251213");
}

#[test]
fn parses_version_flags_without_initializing_prompt_mode() {
    assert_eq!(
        parse_args(&["--version".to_string()]).expect("args should parse"),
        CliAction::Version {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["-V".to_string()]).expect("args should parse"),
        CliAction::Version {
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_permission_mode_flag() {
    let args = vec!["--permission-mode=read-only".to_string()];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::Repl {
            model: DEFAULT_MODEL.to_string(),
            allowed_tools: None,
            permission_mode: PermissionMode::ReadOnly,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
            tui: true,
            enable_plan_mode: false,
            enable_policy_engine: false,
        }
    );
}

#[test]
fn dangerously_skip_permissions_flag_forces_danger_full_access_in_repl() {
    let _guard = env_lock();
    std::env::set_var("RUSTY_CLAUDE_PERMISSION_MODE", "read-only");
    let args = vec!["--dangerously-skip-permissions".to_string()];
    let parsed = parse_args(&args).expect("args should parse");
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");

    assert_eq!(
        parsed,
        CliAction::Repl {
            model: DEFAULT_MODEL.to_string(),
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
            tui: true,
            enable_plan_mode: false,
            enable_policy_engine: false,
        }
    );
}

#[test]
fn dangerously_skip_permissions_flag_applies_to_prompt_subcommand() {
    let _guard = env_lock();
    std::env::set_var("RUSTY_CLAUDE_PERMISSION_MODE", "read-only");
    let args = vec![
        "--dangerously-skip-permissions".to_string(),
        "prompt".to_string(),
        "do".to_string(),
        "the".to_string(),
        "thing".to_string(),
    ];
    let parsed = parse_args(&args).expect("args should parse");
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");

    assert_eq!(
        parsed,
        CliAction::Prompt {
            prompt: "do the thing".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn parses_allowed_tools_flags_with_aliases_and_lists() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec![
        "--allowedTools".to_string(),
        "read,glob".to_string(),
        "--allowed-tools=write_file".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::Repl {
            model: DEFAULT_MODEL.to_string(),
            allowed_tools: Some(
                ["glob_search", "read_file", "write_file"]
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            ),
            permission_mode: PermissionMode::DangerFullAccess,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
            tui: true,
            enable_plan_mode: false,
            enable_policy_engine: false,
        }
    );
}

#[test]
fn rejects_unknown_allowed_tools() {
    let _guard = env_lock();
    let error = parse_args(&["--allowedTools".to_string(), "teleport".to_string()])
        .expect_err("tool should be rejected");
    assert!(error.contains("unsupported tool in --allowedTools: teleport"));
}

#[test]
fn rejects_empty_allowed_tools_flag() {
    let _guard = env_lock();
    for raw in ["", ",,"] {
        let error = parse_args(&["--allowedTools".to_string(), raw.to_string()])
            .expect_err("empty allowedTools should be rejected");
        assert!(
            error.contains("--allowedTools was provided with no usable tool names"),
            "unexpected error for {raw:?}: {error}"
        );
    }
}

#[test]
fn parses_system_prompt_options() {
    // given: system-prompt options for cwd and date
    let args = vec![
        "system-prompt".to_string(),
        "--cwd".to_string(),
        "/tmp/project".to_string(),
        "--date".to_string(),
        "2026-04-01".to_string(),
    ];

    // when: parsing the direct system-prompt command
    let action = parse_args(&args).expect("args should parse");

    // then: the action carries prompt options and default model
    assert_eq!(
        action,
        CliAction::PrintSystemPrompt {
            cwd: PathBuf::from("/tmp/project"),
            date: "2026-04-01".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_global_model_for_system_prompt() {
    // given: a global OpenAI-compatible model before system-prompt
    let args = vec![
        "--model".to_string(),
        "openai/gpt-4.1-mini".to_string(),
        "system-prompt".to_string(),
    ];

    // when: parsing the CLI arguments
    let action = parse_args(&args).expect("args should parse");

    // then: the system-prompt action carries the selected model
    match action {
        CliAction::PrintSystemPrompt { model, .. } => {
            assert_eq!(model, "openai/gpt-4.1-mini");
        }
        other => panic!("expected PrintSystemPrompt, got {other:?}"),
    }
}

#[test]
fn removed_login_and_logout_subcommands_error_helpfully() {
    let login = parse_args(&["login".to_string()]).expect_err("login should be removed");
    assert!(login.contains("ANTHROPIC_API_KEY"));
    let logout = parse_args(&["logout".to_string()]).expect_err("logout should be removed");
    assert!(logout.contains("ANTHROPIC_AUTH_TOKEN"));
    assert_eq!(
        parse_args(&["doctor".to_string()]).expect("doctor should parse"),
        CliAction::Doctor {
            output_format: CliOutputFormat::Text,
            cache_stats: false,
        }
    );
    assert_eq!(
        parse_args(&["state".to_string()]).expect("state should parse"),
        CliAction::State {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "state".to_string(),
            "--output-format".to_string(),
            "json".to_string()
        ])
        .expect("state --output-format json should parse"),
        CliAction::State {
            output_format: CliOutputFormat::Json,
        }
    );
    assert_eq!(
        parse_args(&["init".to_string()]).expect("init should parse"),
        CliAction::Init {
            output_format: CliOutputFormat::Text,
            force: false,
        }
    );
    assert_eq!(
        parse_args(&["init".to_string(), "--force".to_string()])
            .expect("init --force should parse"),
        CliAction::Init {
            output_format: CliOutputFormat::Text,
            force: true,
        }
    );
    assert_eq!(
        parse_args(&["init".to_string(), "-f".to_string()]).expect("init -f should parse"),
        CliAction::Init {
            output_format: CliOutputFormat::Text,
            force: true,
        }
    );
    assert_eq!(
        parse_args(&["agents".to_string()]).expect("agents should parse"),
        CliAction::Agents {
            args: None,
            output_format: CliOutputFormat::Text
        }
    );
    assert_eq!(
        parse_args(&["mcp".to_string()]).expect("mcp should parse"),
        CliAction::Mcp {
            args: None,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["skills".to_string()]).expect("skills should parse"),
        CliAction::Skills {
            args: None,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "skills".to_string(),
            "help".to_string(),
            "overview".to_string()
        ])
        .expect("skills help overview should invoke"),
        CliAction::Prompt {
            prompt: "$help overview".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: crate::default_permission_mode(),
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
    assert_eq!(
        parse_args(&["agents".to_string(), "--help".to_string()])
            .expect("agents help should parse"),
        CliAction::Agents {
            args: Some("--help".to_string()),
            output_format: CliOutputFormat::Text,
        }
    );
    // #145: `plugins` must parse as CliAction::Plugins (not fall through
    // to the prompt path, which would hit the Anthropic API for a purely
    // local introspection command).
    assert_eq!(
        parse_args(&["plugins".to_string()]).expect("plugins should parse"),
        CliAction::Plugins {
            action: None,
            target: None,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["plugins".to_string(), "list".to_string()])
            .expect("plugins list should parse"),
        CliAction::Plugins {
            action: Some("list".to_string()),
            target: None,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "plugins".to_string(),
            "enable".to_string(),
            "example-bundled".to_string(),
        ])
        .expect("plugins enable <target> should parse"),
        CliAction::Plugins {
            action: Some("enable".to_string()),
            target: Some("example-bundled".to_string()),
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "plugins".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ])
        .expect("plugins --output-format json should parse"),
        CliAction::Plugins {
            action: None,
            target: None,
            output_format: CliOutputFormat::Json,
        }
    );
    for alias in ["plugin", "marketplace"] {
        assert_eq!(
            parse_args(&[alias.to_string()]).expect("plugin alias should parse"),
            CliAction::Plugins {
                action: None,
                target: None,
                output_format: CliOutputFormat::Text,
            },
            "{alias} should route to local plugin handling, not Prompt"
        );
        assert_eq!(
            parse_args(&[alias.to_string(), "list".to_string()])
                .expect("plugin alias list should parse"),
            CliAction::Plugins {
                action: Some("list".to_string()),
                target: None,
                output_format: CliOutputFormat::Text,
            },
            "{alias} list should route to local plugin handling, not Prompt"
        );
        assert_eq!(
            parse_args(&[
                alias.to_string(),
                "install".to_string(),
                "./fixtures/plugin-demo".to_string(),
            ])
            .expect("plugin alias install should parse"),
            CliAction::Plugins {
                action: Some("install".to_string()),
                target: Some("./fixtures/plugin-demo".to_string()),
                output_format: CliOutputFormat::Text,
            },
            "{alias} install should route to local plugin handling, not Prompt"
        );
    }
    // #146: `config` and `diff` must parse as standalone CLI actions,
    // not fall through to the "is a slash command" error. Both are
    // pure-local read-only introspection.
    assert_eq!(
        parse_args(&["config".to_string()]).expect("config should parse"),
        CliAction::Config {
            section: None,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["config".to_string(), "env".to_string()]).expect("config env should parse"),
        CliAction::Config {
            section: Some("env".to_string()),
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "config".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ])
        .expect("config --output-format json should parse"),
        CliAction::Config {
            section: None,
            output_format: CliOutputFormat::Json,
        }
    );
    assert_eq!(
        parse_args(&["diff".to_string()]).expect("diff should parse"),
        CliAction::Diff {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "diff".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ])
        .expect("diff --output-format json should parse"),
        CliAction::Diff {
            output_format: CliOutputFormat::Json,
        }
    );
    // #147: empty / whitespace-only positional args must be rejected
    // with a specific error instead of falling through to the prompt
    // path (where they surface a misleading "missing Anthropic
    // credentials" error or burn API tokens on an empty prompt).
    let empty_err =
        parse_args(&["".to_string()]).expect_err("empty positional arg should be rejected");
    assert!(
        empty_err.starts_with("empty prompt:"),
        "empty-arg error should be specific, got: {empty_err}"
    );
    let whitespace_err = parse_args(&["   ".to_string()])
        .expect_err("whitespace-only positional arg should be rejected");
    assert!(
        whitespace_err.starts_with("empty prompt:"),
        "whitespace-only error should be specific, got: {whitespace_err}"
    );
    let multi_empty_err = parse_args(&["".to_string(), "".to_string()])
        .expect_err("multiple empty positional args should be rejected");
    assert!(
        multi_empty_err.starts_with("empty prompt:"),
        "multi-empty error should be specific, got: {multi_empty_err}"
    );
    // Typo guard from #108 must still take precedence for non-empty
    // single-word non-prompt-looking inputs.
    let typo_err = parse_args(&["sttaus".to_string()])
        .expect_err("typo'd subcommand should be caught by #108 guard");
    assert!(
        typo_err.starts_with("unknown subcommand:"),
        "typo guard should fire for 'sttaus', got: {typo_err}"
    );
    // #148: `--model` flag must be captured as model_flag_raw so status
    // JSON can report provenance (source: flag, raw: <user-input>).
    match parse_args(&[
        "--model".to_string(),
        "sonnet".to_string(),
        "status".to_string(),
    ])
    .expect("--model sonnet status should parse")
    {
        CliAction::Status {
            model,
            model_flag_raw,
            ..
        } => {
            assert_eq!(model, "claude-sonnet-4-6", "sonnet alias should resolve");
            assert_eq!(
                model_flag_raw.as_deref(),
                Some("sonnet"),
                "raw flag input should be preserved"
            );
        }
        other => panic!("expected CliAction::Status, got: {other:?}"),
    }
    // --model= form should also capture raw.
    match parse_args(&[
        "--model=anthropic/claude-opus-4-6".to_string(),
        "status".to_string(),
    ])
    .expect("--model=... status should parse")
    {
        CliAction::Status {
            model,
            model_flag_raw,
            ..
        } => {
            assert_eq!(model, "anthropic/claude-opus-4-6");
            assert_eq!(
                model_flag_raw.as_deref(),
                Some("anthropic/claude-opus-4-6"),
                "--model= form should also preserve raw input"
            );
        }
        other => panic!("expected CliAction::Status, got: {other:?}"),
    }
}

#[test]
fn dump_manifests_subcommand_accepts_explicit_manifest_dir() {
    assert_eq!(
        parse_args(&[
            "dump-manifests".to_string(),
            "--manifests-dir".to_string(),
            "/tmp/upstream".to_string(),
        ])
        .expect("dump-manifests should parse"),
        CliAction::DumpManifests {
            output_format: CliOutputFormat::Text,
            manifests_dir: Some(PathBuf::from("/tmp/upstream")),
        }
    );
    assert_eq!(
        parse_args(&[
            "dump-manifests".to_string(),
            "--manifests-dir=/tmp/upstream".to_string()
        ])
        .expect("inline dump-manifests flag should parse"),
        CliAction::DumpManifests {
            output_format: CliOutputFormat::Text,
            manifests_dir: Some(PathBuf::from("/tmp/upstream")),
        }
    );
}

#[test]
fn parses_acp_command_surfaces() {
    assert_eq!(
        parse_args(&["acp".to_string()]).expect("acp should parse"),
        CliAction::Acp {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["acp".to_string(), "serve".to_string()]).expect("acp serve should parse"),
        CliAction::AcpServe {
            model: DEFAULT_MODEL.to_string(),
            permission_mode: default_permission_mode(),
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["--acp".to_string()]).expect("--acp should parse"),
        CliAction::Acp {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["-acp".to_string()]).expect("-acp should parse"),
        CliAction::Acp {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "acp".to_string(),
            "serve".to_string(),
            "--output-format".to_string(),
            "json".to_string()
        ])
        .expect("acp serve json should parse"),
        CliAction::AcpServe {
            model: DEFAULT_MODEL.to_string(),
            permission_mode: default_permission_mode(),
            output_format: CliOutputFormat::Json,
        }
    );
    let unsupported = parse_args(&["acp".to_string(), "start".to_string()])
        .expect_err("unknown ACP subcommand should fail with a typed contract");
    assert!(unsupported.contains("unsupported ACP invocation"));
}

#[test]
fn acp_status_json_reflects_stdio_server_contract() {
    let value = acp_status_json();
    assert_eq!(value["schema_version"], "1.1");
    assert_eq!(value["kind"], "acp");
    assert_eq!(value["status"], "supported");
    assert_eq!(value["phase"], "stdio_server");
    assert_eq!(value["supported"], true);
    assert_eq!(value["exit_code"], 0);
    assert_eq!(value["serve_alias_only"], false);
    assert_eq!(value["launch_command"], "claw acp serve");
    assert_eq!(value["protocol"]["json_rpc"], true);
    assert_eq!(value["protocol"]["transport"], "newline_delimited_json");
    assert_eq!(value["protocol"]["daemon"], false);
    assert_eq!(value["protocol"]["endpoint"], "stdio");
    assert_eq!(value["protocol"]["serve_starts_daemon"], false);
    assert_eq!(
        value["contracts"]["unsupported_invocation_kind"],
        "unsupported_acp_invocation"
    );
    assert_eq!(value["contracts"]["serve_subcommand"], "claw acp serve");
}

#[test]
fn local_command_help_flags_stay_on_the_local_parser_path() {
    assert_eq!(
        parse_args(&["status".to_string(), "--help".to_string()])
            .expect("status help should parse"),
        CliAction::HelpTopic {
            topic: LocalHelpTopic::Status,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["sandbox".to_string(), "-h".to_string()]).expect("sandbox help should parse"),
        CliAction::HelpTopic {
            topic: LocalHelpTopic::Sandbox,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["doctor".to_string(), "--help".to_string()])
            .expect("doctor help should parse"),
        CliAction::HelpTopic {
            topic: LocalHelpTopic::Doctor,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["acp".to_string(), "--help".to_string()]).expect("acp help should parse"),
        CliAction::HelpTopic {
            topic: LocalHelpTopic::Acp,
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn subcommand_help_flag_has_one_contract_across_all_subcommands_141() {
    // #141: every documented subcommand must resolve `<subcommand> --help`
    // to a subcommand-specific help topic, never to global help, never to
    // an "unknown option" error, never to the subcommand's primary output.
    let cases: &[(&str, LocalHelpTopic)] = &[
        ("status", LocalHelpTopic::Status),
        ("sandbox", LocalHelpTopic::Sandbox),
        ("doctor", LocalHelpTopic::Doctor),
        ("acp", LocalHelpTopic::Acp),
        ("init", LocalHelpTopic::Init),
        ("state", LocalHelpTopic::State),
        ("export", LocalHelpTopic::Export),
        ("version", LocalHelpTopic::Version),
        ("system-prompt", LocalHelpTopic::SystemPrompt),
        ("dump-manifests", LocalHelpTopic::DumpManifests),
        ("bootstrap-plan", LocalHelpTopic::BootstrapPlan),
    ];
    for (subcommand, expected_topic) in cases {
        for flag in ["--help", "-h"] {
            let parsed =
                parse_args(&[subcommand.to_string(), flag.to_string()]).unwrap_or_else(|error| {
                    panic!("`{subcommand} {flag}` should parse as help but errored: {error}")
                });
            assert_eq!(
                parsed,
                CliAction::HelpTopic {
                    topic: *expected_topic,
                    output_format: CliOutputFormat::Text,
                },
                "`{subcommand} {flag}` should resolve to HelpTopic({expected_topic:?})"
            );
        }
        let json_parsed = parse_args(&[
            subcommand.to_string(),
            "--help".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ])
        .unwrap_or_else(|error| {
            panic!("`{subcommand} --help --output-format json` should parse: {error}")
        });
        assert_eq!(
            json_parsed,
            CliAction::HelpTopic {
                topic: *expected_topic,
                output_format: CliOutputFormat::Json,
            },
            "`{subcommand} --help --output-format json` should preserve json output format"
        );
        // And the rendered help must actually mention the subcommand name
        // (or its canonical title) so users know they got the right help.
        let rendered = render_help_topic(*expected_topic);
        assert!(
            !rendered.is_empty(),
            "{subcommand} help text should not be empty"
        );
        assert!(
            rendered.contains("Usage"),
            "{subcommand} help text should contain a Usage line"
        );
    }
}

#[test]
fn export_help_json_is_bounded_and_parseable_384() {
    let value = render_help_topic_json(LocalHelpTopic::Export);
    assert_eq!(value["kind"], "help");
    assert_eq!(value["topic"], "export");
    assert_eq!(value["command"], "export");
    assert_eq!(
        value["usage"],
        "claw export [--session <id|latest>] [--output <path>] [--output-format <format>]"
    );
    assert_eq!(value["defaults"]["session"], LATEST_SESSION_REFERENCE);
    assert!(value["options"].as_array().expect("options array").len() >= 4);
    assert!(
        value.get("message").is_none(),
        "export help json should be a bounded envelope, not plaintext help wrapped in json"
    );
}

#[test]
fn plugins_degrades_gracefully_on_malformed_mcp_config() {
    // Keep the plugins surface consistent with status/doctor/mcp: a bad
    // MCP entry should not make local plugin introspection unusable.
    let _guard = env_lock();
    let root = temp_dir();
    let cwd = root.join("project-with-malformed-mcp-for-plugins");
    let config_home = root.join("config-home");
    std::fs::create_dir_all(&cwd).expect("project dir should exist");
    std::fs::create_dir_all(&config_home).expect("config home should exist");
    std::fs::write(
        cwd.join(".claw.json"),
        r#"{
  "mcpServers": {
"missing-command": {"args": ["arg-only-no-command"]}
  }
}
"#,
    )
    .expect("write malformed .claw.json");

    let previous_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    let payload = super::plugins_command_payload_for(&cwd, None, None)
        .expect("plugins list should not hard-fail on malformed MCP config");
    match previous_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }

    assert_eq!(payload.status, "degraded");
    let err = payload
        .config_load_error
        .as_deref()
        .expect("config_load_error should be populated");
    assert!(
        err.contains("mcpServers.missing-command"),
        "config_load_error should name the malformed MCP field: {err}"
    );
    assert!(payload.message.contains("Config load error"));
    assert!(payload.message.contains("partial plugins view"));
    assert!(payload.message.contains("Plugins"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn status_degrades_gracefully_on_malformed_mcp_config_143() {
    // #143: previously `claw status` hard-failed on any config parse error,
    // taking down the entire health surface for one malformed MCP entry.
    // `claw doctor` already degrades gracefully; this test locks `status`
    // to the same contract.
    let _guard = env_lock();
    let root = temp_dir();
    let cwd = root.join("project-with-malformed-mcp");
    std::fs::create_dir_all(&cwd).expect("project dir should exist");
    // One valid server + one malformed entry missing `command`.
    std::fs::write(
        cwd.join(".claw.json"),
        r#"{
  "mcpServers": {
"everything": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"]},
"missing-command": {"args": ["arg-only-no-command"]}
  }
}
"#,
    )
    .expect("write malformed .claw.json");

    let context = with_current_dir(&cwd, || {
        super::status_context(None)
            .expect("status_context should not hard-fail on config parse errors (#143)")
    });

    // Phase 1 contract: config_load_error is populated with the parse error.
    let err = context
        .config_load_error
        .as_ref()
        .expect("config_load_error should be Some when config parse fails");
    assert!(
        err.contains("mcpServers.missing-command"),
        "config_load_error should name the malformed field path: {err}"
    );
    assert!(
        err.contains("missing string field command"),
        "config_load_error should carry the underlying parse error: {err}"
    );

    // Phase 1 contract: workspace/git/sandbox fields are still populated
    // (independent of config parse). Sandbox falls back to defaults.
    // Note: status_context returns cwd without canonicalization (format.rs:404),
    // so the test must compare against the raw cwd, not canonicalize(). On
    // Windows canonicalize() prepends the `\\?\` UNC prefix which would
    // cause a spurious mismatch.
    assert_eq!(context.cwd, cwd);
    assert_eq!(
        context.loaded_config_files, 0,
        "loaded_config_files should be 0 when config parse fails"
    );
    assert!(
        context.discovered_config_files > 0,
        "discovered_config_files should still count the file that failed to parse"
    );

    // JSON output contract: top-level `status: "degraded"` + config_load_error field.
    let usage = super::StatusUsage {
        message_count: 0,
        turns: 0,
        latest: runtime::TokenUsage::default(),
        cumulative: runtime::TokenUsage::default(),
        estimated_tokens: 0,
    };
    let json = super::status_json_value(
        Some("test-model"),
        usage,
        "workspace-write",
        &context,
        None,
        None,
    );
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("degraded"),
        "top-level status marker should be 'degraded' when config parse failed: {json}"
    );
    assert!(
        json.get("config_load_error")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("mcpServers.missing-command")),
        "config_load_error should surface in JSON output: {json}"
    );
    // Independent fields still populated.
    assert_eq!(
        json.get("model").and_then(|v| v.as_str()),
        Some("test-model")
    );
    assert!(
        json.get("workspace").is_some(),
        "workspace field still reported"
    );
    assert_eq!(
        json.pointer("/lane_board/status_json_supported")
            .and_then(|v| v.as_bool()),
        Some(true),
        "status JSON should advertise lane board support: {json}"
    );
    assert_eq!(
        json.pointer("/lane_board/freshness_states/2")
            .and_then(|v| v.as_str()),
        Some("transport_dead"),
        "status JSON should advertise transport-dead freshness: {json}"
    );
    assert!(
        json.get("sandbox").is_some(),
        "sandbox field still reported"
    );
    assert_eq!(
        json.pointer("/allowed_tools/source")
            .and_then(|v| v.as_str()),
        Some("default"),
        "default status should expose unrestricted tool source: {json}"
    );
    assert_eq!(
        json.pointer("/allowed_tools/restricted")
            .and_then(|v| v.as_bool()),
        Some(false),
        "default status should expose unrestricted tool state: {json}"
    );

    let allowed: super::AllowedToolSet = ["read_file", "grep_search"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let restricted_json = super::status_json_value(
        Some("test-model"),
        usage,
        "workspace-write",
        &context,
        None,
        Some(&allowed),
    );
    assert_eq!(
        restricted_json
            .pointer("/allowed_tools/source")
            .and_then(|v| v.as_str()),
        Some("flag"),
        "flag status should expose allow-list source: {restricted_json}"
    );
    assert_eq!(
        restricted_json
            .pointer("/allowed_tools/entries")
            .and_then(|v| v.as_array())
            .map(Vec::len),
        Some(2),
        "flag status should expose allow-list entries: {restricted_json}"
    );

    // Clean path: no config error → status: "ok", config_load_error: null.
    let clean_cwd = root.join("project-with-clean-config");
    std::fs::create_dir_all(&clean_cwd).expect("clean project dir");
    let clean_context = with_current_dir(&clean_cwd, || {
        super::status_context(None).expect("clean status_context should succeed")
    });
    assert!(clean_context.config_load_error.is_none());
    let clean_json = super::status_json_value(
        Some("test-model"),
        usage,
        "workspace-write",
        &clean_context,
        None,
        None,
    );
    assert_eq!(
        clean_json.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "clean run should report status: 'ok'"
    );
}

#[test]
fn state_error_surfaces_actionable_worker_commands_139() {
    // #139: the error for missing `.claw/worker-state.json` must name
    // the concrete commands that produce worker state, otherwise claws
    // have no discoverable path from the error to a fix.
    let _guard = env_lock();
    let root = temp_dir();
    let cwd = root.join("project-with-no-state");
    std::fs::create_dir_all(&cwd).expect("project dir should exist");

    let error = with_current_dir(&cwd, || {
        super::run_worker_state(CliOutputFormat::Text).expect_err("missing state should error")
    });
    let message = error.to_string();

    // Keep the original locator so scripts grepping for it still work.
    assert!(
        message.contains("未找到 worker 状态文件:"),
        "error should keep the canonical prefix: {message}"
    );
    // New actionable hints — this is what #139 is fixing.
    assert!(
        message.contains("claw prompt"),
        "error should name `claw prompt <text>` as a producer: {message}"
    );
    assert!(
        message.contains("REPL"),
        "error should mention the interactive REPL as a producer: {message}"
    );
    assert!(
        message.contains("claw state"),
        "error should tell the user what to rerun once state exists: {message}"
    );
    // And the State --help topic must document the worker relationship
    // so claws can discover the contract without hitting the error first.
    let state_help = render_help_topic(LocalHelpTopic::State);
    assert!(
        state_help.contains("Produces state"),
        "state help must document how state is produced: {state_help}"
    );
    assert!(
        state_help.contains("claw prompt"),
        "state help must name `claw prompt <text>` as a producer: {state_help}"
    );
}

#[test]
fn parses_single_word_command_aliases_without_falling_back_to_prompt_mode() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    assert_eq!(
        parse_args(&["help".to_string()]).expect("help should parse"),
        CliAction::Help {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["version".to_string()]).expect("version should parse"),
        CliAction::Version {
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["status".to_string()]).expect("status should parse"),
        CliAction::Status {
            model: DEFAULT_MODEL.to_string(),
            model_flag_raw: None, // #148: no --model flag passed
            permission_mode: PermissionMode::DangerFullAccess,
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
        }
    );
    assert_eq!(
        parse_args(&["sandbox".to_string()]).expect("sandbox should parse"),
        CliAction::Sandbox {
            output_format: CliOutputFormat::Text,
        }
    );
    // #152: `--json` on diagnostic verbs should hint the correct flag.
    let err = parse_args(&["doctor".to_string(), "--json".to_string()])
        .expect_err("`doctor --json` should fail with hint");
    assert!(
        err.contains("unrecognized argument `--json` for subcommand `doctor`"),
        "error should name the verb: {err}"
    );
    assert!(
        err.contains("Did you mean `--output-format json`?"),
        "error should hint the correct flag: {err}"
    );
    // Other unrecognized args should NOT trigger the --json hint.
    let err_other = parse_args(&["doctor".to_string(), "garbage".to_string()])
        .expect_err("`doctor garbage` should fail without --json hint");
    assert!(
        !err_other.contains("--output-format json"),
        "unrelated args should not trigger --json hint: {err_other}"
    );
    // #154: GPT-4 / qwen-plus models now pass validation because
    // metadata_for_model recognizes their prefix (gpt- → OpenAI,
    // qwen- → DashScope). They no longer need the provider/ prefix.
    let action_gpt = parse_args(&[
        "prompt".to_string(),
        "test".to_string(),
        "--model".to_string(),
        "gpt-4".to_string(),
    ])
    .expect("`--model gpt-4` should succeed: metadata_for_model routes gpt- → OpenAI");
    assert!(
        matches!(action_gpt, CliAction::Prompt { ref model, .. } if model == "gpt-4"),
        "gpt-4 should be accepted as a bare model name: {action_gpt:?}"
    );
    let action_qwen = parse_args(&[
        "prompt".to_string(),
        "test".to_string(),
        "--model".to_string(),
        "qwen-plus".to_string(),
    ])
    .expect("`--model qwen-plus` should succeed: metadata_for_model routes qwen- → DashScope");
    assert!(
        matches!(action_qwen, CliAction::Prompt { ref model, .. } if model == "qwen-plus"),
        "qwen-plus should be accepted as a bare model name: {action_qwen:?}"
    );
    // Unrelated invalid model should NOT get a hint
    let err_garbage = parse_args(&[
        "prompt".to_string(),
        "test".to_string(),
        "--model".to_string(),
        "asdfgh".to_string(),
    ])
    .expect_err("`--model asdfgh` should fail");
    assert!(
        !err_garbage.contains("Did you mean"),
        "Unrelated model errors should not get a hint: {err_garbage}"
    );
}

#[test]
fn classify_error_kind_returns_correct_discriminants() {
    // #77: error kind classification for JSON error payloads
    assert_eq!(
        classify_error_kind("missing Anthropic credentials; export ..."),
        "missing_credentials"
    );
    assert_eq!(
        classify_error_kind("no worker state file found at /tmp/..."),
        "missing_worker_state"
    );
    assert_eq!(
        classify_error_kind("session not found: abc123"),
        "session_not_found"
    );
    assert_eq!(
        classify_error_kind("failed to restore session: no managed sessions found"),
        "session_load_failed"
    );
    assert_eq!(
        classify_error_kind("unrecognized argument `--foo` for subcommand `doctor`"),
        "cli_parse"
    );
    assert_eq!(
        classify_error_kind("unsupported ACP invocation. Use `claw acp`."),
        "unsupported_acp_invocation"
    );
    assert_eq!(
        classify_error_kind("invalid model syntax: 'gpt-4'. Expected ..."),
        "invalid_model_syntax"
    );
    assert_eq!(
        classify_error_kind("unsupported resumed command: /blargh"),
        "unsupported_resumed_command"
    );
    assert_eq!(
        classify_error_kind("api failed after 3 attempts: ..."),
        "api_http_error"
    );
    assert_eq!(
        classify_error_kind("something completely unknown"),
        "unknown"
    );
}

#[test]
fn split_error_hint_separates_reason_from_runbook() {
    // #77: short reason / hint separation for JSON error payloads
    let (short, hint) = split_error_hint("missing credentials\nHint: export ANTHROPIC_API_KEY");
    assert_eq!(short, "missing credentials");
    assert_eq!(hint, Some("Hint: export ANTHROPIC_API_KEY".to_string()));

    let (short, hint) = split_error_hint("simple error with no hint");
    assert_eq!(short, "simple error with no hint");
    assert_eq!(hint, None);
}

#[test]
fn parses_bare_export_subcommand_targeting_latest_session() {
    // given
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    let args = vec!["export".to_string()];

    // when
    let parsed = parse_args(&args).expect("bare export should parse");

    // then
    assert_eq!(
        parsed,
        CliAction::Export {
            session_reference: LATEST_SESSION_REFERENCE.to_string(),
            output_path: None,
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_export_subcommand_with_positional_output_path() {
    // given
    let args = vec!["export".to_string(), "conversation.md".to_string()];

    // when
    let parsed = parse_args(&args).expect("export with path should parse");

    // then
    assert_eq!(
        parsed,
        CliAction::Export {
            session_reference: LATEST_SESSION_REFERENCE.to_string(),
            output_path: Some(PathBuf::from("conversation.md")),
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_export_subcommand_with_session_and_output_flags() {
    // given
    let args = vec![
        "export".to_string(),
        "--session".to_string(),
        "session-alpha".to_string(),
        "--output".to_string(),
        "/tmp/share.md".to_string(),
    ];

    // when
    let parsed = parse_args(&args).expect("export flags should parse");

    // then
    assert_eq!(
        parsed,
        CliAction::Export {
            session_reference: "session-alpha".to_string(),
            output_path: Some(PathBuf::from("/tmp/share.md")),
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_export_subcommand_with_inline_flag_values() {
    // given
    let args = vec![
        "export".to_string(),
        "--session=session-beta".to_string(),
        "--output=/tmp/beta.md".to_string(),
    ];

    // when
    let parsed = parse_args(&args).expect("export inline flags should parse");

    // then
    assert_eq!(
        parsed,
        CliAction::Export {
            session_reference: "session-beta".to_string(),
            output_path: Some(PathBuf::from("/tmp/beta.md")),
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_export_subcommand_with_json_output_format() {
    // given
    let args = vec![
        "--output-format=json".to_string(),
        "export".to_string(),
        "/tmp/notes.md".to_string(),
    ];

    // when
    let parsed = parse_args(&args).expect("json export should parse");

    // then
    assert_eq!(
        parsed,
        CliAction::Export {
            session_reference: LATEST_SESSION_REFERENCE.to_string(),
            output_path: Some(PathBuf::from("/tmp/notes.md")),
            output_format: CliOutputFormat::Json,
        }
    );
}

#[test]
fn rejects_unknown_export_options_with_helpful_message() {
    // given
    let args = vec!["export".to_string(), "--bogus".to_string()];

    // when
    let error = parse_args(&args).expect_err("unknown export option should fail");

    // then
    assert!(error.contains("unknown export option: --bogus"));
}

#[test]
fn rejects_export_with_extra_positional_after_path() {
    // given
    let args = vec![
        "export".to_string(),
        "first.md".to_string(),
        "second.md".to_string(),
    ];

    // when
    let error = parse_args(&args).expect_err("multiple positionals should fail");

    // then
    assert!(error.contains("unexpected export argument: second.md"));
}

#[test]
fn parse_export_args_helper_defaults_to_latest_reference_and_no_output() {
    // given
    let args: Vec<String> = vec![];

    // when
    let parsed =
        parse_export_args(&args, CliOutputFormat::Text).expect("empty export args should parse");

    // then
    assert_eq!(
        parsed,
        CliAction::Export {
            session_reference: LATEST_SESSION_REFERENCE.to_string(),
            output_path: None,
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn render_session_markdown_includes_header_and_summarized_tool_calls() {
    // given
    let mut session = Session::new();
    session.session_id = "session-export-test".to_string();
    session.messages = vec![
        ConversationMessage::user_text("How do I list files?"),
        ConversationMessage::assistant(vec![
            ContentBlock::Text {
                text: "I'll run a tool.".to_string(),
            },
            ContentBlock::ToolUse {
                id: "toolu_abcdefghijklmnop".to_string(),
                name: "bash".to_string(),
                input: r#"{"command":"ls -la"}"#.to_string(),
            },
        ]),
        ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_abcdefghijklmnop".to_string(),
                tool_name: "bash".to_string(),
                output: "total 8\ndrwxr-xr-x  2 user staff   64 Apr  7 12:00 .".to_string(),
                is_error: false,
            }],
            usage: None,
        },
    ];

    // when
    let markdown = render_session_markdown(
        &session,
        "session-export-test",
        std::path::Path::new("/tmp/sessions/session-export-test.jsonl"),
    );

    // then
    assert!(markdown.starts_with("# Conversation Export"));
    assert!(markdown.contains("- **Session**: `session-export-test`"));
    assert!(markdown.contains("- **Messages**: 3"));
    assert!(markdown.contains("## 1. User"));
    assert!(markdown.contains("How do I list files?"));
    assert!(markdown.contains("## 2. Assistant"));
    assert!(markdown.contains("**Tool call** `bash`"));
    assert!(markdown.contains("toolu_abcdef…"));
    assert!(markdown.contains("ls -la"));
    assert!(markdown.contains("## 3. Tool"));
    assert!(markdown.contains("**Tool result** `bash`"));
    assert!(markdown.contains("ok"));
    assert!(markdown.contains("total 8"));
}

#[test]
fn render_session_markdown_marks_tool_errors_and_skips_empty_summaries() {
    // given
    let mut session = Session::new();
    session.session_id = "errs".to_string();
    session.messages = vec![ConversationMessage {
        role: MessageRole::Tool,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: "short".to_string(),
            tool_name: "read_file".to_string(),
            output: "   ".to_string(),
            is_error: true,
        }],
        usage: None,
    }];

    // when
    let markdown = render_session_markdown(&session, "errs", std::path::Path::new("errs.jsonl"));

    // then
    assert!(markdown.contains("**Tool result** `read_file` _(id `short`, error)_"));
    // an empty summary should not produce a stray blockquote line
    assert!(!markdown.contains("> \n"));
}

#[test]
fn summarize_tool_payload_for_markdown_compacts_json_and_truncates_overflow() {
    // given
    let json_payload = r#"{
        "command":   "ls -la",
        "cwd": "/tmp"
    }"#;
    let long_payload = "a".repeat(600);

    // when
    let compacted = summarize_tool_payload_for_markdown(json_payload);
    let truncated = summarize_tool_payload_for_markdown(&long_payload);

    // then
    assert_eq!(compacted, r#"{"command":"ls -la","cwd":"/tmp"}"#);
    assert!(truncated.ends_with('…'));
    assert!(truncated.chars().count() <= 281);
}

#[test]
fn short_tool_id_truncates_long_identifiers_with_ellipsis() {
    // given
    let long = "toolu_01ABCDEFGHIJKLMN";
    let short = "tool_1";

    // when
    let trimmed_long = short_tool_id(long);
    let trimmed_short = short_tool_id(short);

    // then
    assert_eq!(trimmed_long, "toolu_01ABCD…");
    assert_eq!(trimmed_short, "tool_1");
}

#[test]
fn parses_json_output_for_mcp_and_skills_commands() {
    assert_eq!(
        parse_args(&["--output-format=json".to_string(), "mcp".to_string()])
            .expect("json mcp should parse"),
        CliAction::Mcp {
            args: None,
            output_format: CliOutputFormat::Json,
        }
    );
    assert_eq!(
        parse_args(&[
            "--output-format=json".to_string(),
            "/skills".to_string(),
            "help".to_string(),
        ])
        .expect("json /skills help should parse"),
        CliAction::Skills {
            args: Some("help".to_string()),
            output_format: CliOutputFormat::Json,
        }
    );
}

#[test]
fn single_word_slash_command_names_return_guidance_instead_of_hitting_prompt_mode() {
    let error = parse_args(&["cost".to_string()]).expect_err("cost should return guidance");
    assert!(error.contains("slash command"));
    assert!(error.contains("/cost"));
}

#[test]
fn multi_word_prompt_still_uses_shorthand_prompt_mode() {
    let _guard = env_lock();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    // Input is ["--model", "opus", "please", "debug", "this"] so the joined
    // prompt shorthand must stay a normal multi-word prompt while still
    // honoring alias validation at parse time.
    assert_eq!(
        parse_args(&[
            "--model".to_string(),
            "opus".to_string(),
            "please".to_string(),
            "debug".to_string(),
            "this".to_string(),
        ])
        .expect("prompt shorthand should still work"),
        CliAction::Prompt {
            prompt: "please debug this".to_string(),
            model: "claude-opus-4-6".to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: crate::default_permission_mode(),
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn parses_direct_agents_mcp_and_skills_slash_commands() {
    let _guard = env_lock();
    let _cwd_guard = cwd_guard();
    std::env::remove_var("RUSTY_CLAUDE_PERMISSION_MODE");
    assert_eq!(
        parse_args(&["/agents".to_string()]).expect("/agents should parse"),
        CliAction::Agents {
            args: None,
            output_format: CliOutputFormat::Text
        }
    );
    assert_eq!(
        parse_args(&["/mcp".to_string(), "show".to_string(), "demo".to_string()])
            .expect("/mcp show demo should parse"),
        CliAction::Mcp {
            args: Some("show demo".to_string()),
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["/skills".to_string()]).expect("/skills should parse"),
        CliAction::Skills {
            args: None,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["/skill".to_string()]).expect("/skill should parse"),
        CliAction::Skills {
            args: None,
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["/skills".to_string(), "help".to_string()])
            .expect("/skills help should parse"),
        CliAction::Skills {
            args: Some("help".to_string()),
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["/skill".to_string(), "list".to_string()]).expect("/skill list should parse"),
        CliAction::Skills {
            args: Some("list".to_string()),
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&[
            "/skills".to_string(),
            "help".to_string(),
            "overview".to_string()
        ])
        .expect("/skills help overview should invoke"),
        CliAction::Prompt {
            prompt: "$help overview".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: crate::default_permission_mode(),
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
    assert_eq!(
        parse_args(&[
            "/skills".to_string(),
            "install".to_string(),
            "./fixtures/help-skill".to_string(),
        ])
        .expect("/skills install should parse"),
        CliAction::Skills {
            args: Some("install ./fixtures/help-skill".to_string()),
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["/skills".to_string(), "/test".to_string()])
            .expect("/skills /test should normalize to a single skill prompt prefix"),
        CliAction::Prompt {
            prompt: "$test".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: crate::default_permission_mode(),
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
    let error = parse_args(&["/status".to_string()])
        .expect_err("/status should remain REPL-only when invoked directly");
    assert!(error.contains("interactive-only"));
    assert!(error.contains("claw --resume SESSION.jsonl /status"));
}

#[test]
fn direct_slash_commands_surface_shared_validation_errors() {
    let compact_error = parse_args(&["/compact".to_string(), "now".to_string()])
        .expect_err("invalid /compact shape should be rejected");
    assert!(compact_error.contains("Unexpected arguments for /compact."));
    assert!(compact_error.contains("Usage            /compact"));

    let plugins_error = parse_args(&[
        "/plugins".to_string(),
        "list".to_string(),
        "extra".to_string(),
    ])
    .expect_err("invalid /plugins list shape should be rejected");
    assert!(plugins_error.contains("Usage: /plugin list"));
    assert!(plugins_error.contains("Aliases          /plugins, /marketplace"));

    for alias in ["/plugin", "/plugins", "/marketplace"] {
        let error = parse_args(&[alias.to_string()])
            .expect_err("valid plugin slash aliases are local/interactive, never prompts");
        assert!(
            error.contains("interactive-only"),
            "{alias} should reject as an interactive plugin command outside the REPL, got: {error}"
        );
    }
}

#[test]
fn formats_unknown_slash_command_with_suggestions() {
    let report = format_unknown_slash_command_message("statsu");
    assert!(report.contains("unknown slash command: /statsu"));
    assert!(report.contains("Did you mean"));
    assert!(report.contains("Use /help"));
}

#[test]
fn typoed_doctor_subcommand_returns_did_you_mean_error() {
    let error = parse_args(&["doctorr".to_string()]).expect_err("doctorr should error");
    assert!(error.contains("unknown subcommand: doctorr."));
    assert!(error.contains("Did you mean"));
    assert!(error.contains("doctor"));
}

#[test]
fn typoed_skills_subcommand_returns_did_you_mean_error() {
    let error = parse_args(&["skilsl".to_string()]).expect_err("skilsl should error");
    assert!(error.contains("unknown subcommand: skilsl."));
    assert!(error.contains("skills"));
}

#[test]
fn typoed_status_subcommand_returns_did_you_mean_error() {
    let error = parse_args(&["statuss".to_string()]).expect_err("statuss should error");
    assert!(error.contains("unknown subcommand: statuss."));
    assert!(error.contains("status"));
}

#[test]
fn typoed_export_subcommand_returns_did_you_mean_error() {
    let error = parse_args(&["exporrt".to_string()]).expect_err("exporrt should error");
    assert!(error.contains("unknown subcommand: exporrt."));
    assert!(error.contains("Did you mean"));
    assert!(error.contains("export"));
}

#[test]
fn typoed_mcp_subcommand_returns_did_you_mean_error() {
    let error = parse_args(&["mcpp".to_string()]).expect_err("mcpp should error");
    assert!(error.contains("unknown subcommand: mcpp."));
    assert!(error.contains("mcp"));
}

#[test]
fn multi_word_prompt_still_bypasses_subcommand_typo_guard() {
    assert_eq!(
        parse_args(&[
            "hello".to_string(),
            "world".to_string(),
            "this".to_string(),
            "is".to_string(),
            "a".to_string(),
            "prompt".to_string(),
        ])
        .expect("multi-word prompt should still parse"),
        CliAction::Prompt {
            prompt: "hello world this is a prompt".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: crate::default_permission_mode(),
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn prompt_subcommand_allows_literal_typo_word() {
    assert_eq!(
        parse_args(&["prompt".to_string(), "doctorr".to_string()])
            .expect("explicit prompt subcommand should allow literal typo word"),
        CliAction::Prompt {
            prompt: "doctorr".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn punctuation_bearing_single_token_still_dispatches_to_prompt() {
    // #140: Guard against test pollution — isolate cwd + env so this test
    // doesn't pick up a stale .claw/settings.json from other tests that
    // may have set `permissionMode: acceptEdits` in a shared cwd.
    let _guard = env_lock();
    let root = temp_dir();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).expect("project dir should exist");
    let result = with_current_dir(&cwd, || {
        parse_args(&["PARITY_SCENARIO:bash_permission_prompt_approved".to_string()])
            .expect("scenario token should still dispatch to prompt")
    });
    assert_eq!(
        result,
        CliAction::Prompt {
            prompt: "PARITY_SCENARIO:bash_permission_prompt_approved".to_string(),
            model: DEFAULT_MODEL.to_string(),
            output_format: CliOutputFormat::Text,
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            compact: false,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            additional_workspace_roots: Vec::new(),
            output_verbosity: OutputVerbosity::default(),
        }
    );
}

#[test]
fn formats_namespaced_omc_slash_command_with_contract_guidance() {
    let report = format_unknown_slash_command_message("oh-my-claudecode:hud");
    assert!(report.contains("unknown slash command: /oh-my-claudecode:hud"));
    assert!(report.contains("Claude Code/OMC plugin command"));
    assert!(report.contains("plugin slash commands"));
    assert!(report.contains("statusline"));
    assert!(report.contains("session hooks"));
}

#[test]
fn parses_resume_flag_with_slash_command() {
    let args = vec![
        "--resume".to_string(),
        "session.jsonl".to_string(),
        "/compact".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::ResumeSession {
            session_path: PathBuf::from("session.jsonl"),
            commands: vec!["/compact".to_string()],
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_resume_flag_without_path_as_latest_session() {
    assert_eq!(
        parse_args(&["--resume".to_string()]).expect("args should parse"),
        CliAction::ResumeSession {
            session_path: PathBuf::from("latest"),
            commands: vec![],
            output_format: CliOutputFormat::Text,
        }
    );
    assert_eq!(
        parse_args(&["--resume".to_string(), "/status".to_string()])
            .expect("resume shortcut should parse"),
        CliAction::ResumeSession {
            session_path: PathBuf::from("latest"),
            commands: vec!["/status".to_string()],
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_resume_flag_with_multiple_slash_commands() {
    let args = vec![
        "--resume".to_string(),
        "session.jsonl".to_string(),
        "/status".to_string(),
        "/compact".to_string(),
        "/cost".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::ResumeSession {
            session_path: PathBuf::from("session.jsonl"),
            commands: vec![
                "/status".to_string(),
                "/compact".to_string(),
                "/cost".to_string(),
            ],
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn rejects_unknown_options_with_helpful_guidance() {
    let error = parse_args(&["--resum".to_string()]).expect_err("unknown option should fail");
    assert!(error.contains("unknown option: --resum"));
    assert!(error.contains("Did you mean --resume?"));
    assert!(error.contains("claw --help"));
}

#[test]
fn parses_resume_flag_with_slash_command_arguments() {
    let args = vec![
        "--resume".to_string(),
        "session.jsonl".to_string(),
        "/export".to_string(),
        "notes.txt".to_string(),
        "/clear".to_string(),
        "--confirm".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::ResumeSession {
            session_path: PathBuf::from("session.jsonl"),
            commands: vec![
                "/export notes.txt".to_string(),
                "/clear --confirm".to_string(),
            ],
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn parses_resume_flag_with_absolute_export_path() {
    let args = vec![
        "--resume".to_string(),
        "session.jsonl".to_string(),
        "/export".to_string(),
        "/tmp/notes.txt".to_string(),
        "/status".to_string(),
    ];
    assert_eq!(
        parse_args(&args).expect("args should parse"),
        CliAction::ResumeSession {
            session_path: PathBuf::from("session.jsonl"),
            commands: vec!["/export /tmp/notes.txt".to_string(), "/status".to_string()],
            output_format: CliOutputFormat::Text,
        }
    );
}

#[test]
fn filtered_tool_specs_respect_allowlist() {
    let allowed = ["read_file", "grep_search"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let filtered = filter_tool_specs(&GlobalToolRegistry::builtin(), Some(&allowed));
    let names = filtered
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["read_file", "grep_search"]);
}

#[test]
fn filtered_tool_specs_include_plugin_tools() {
    let filtered = filter_tool_specs(&registry_with_plugin_tool(), None);
    let names = filtered
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"plugin_echo".to_string()));
}

#[test]
fn permission_policy_uses_plugin_tool_permissions() {
    let feature_config = runtime::RuntimeFeatureConfig::default();
    let policy = permission_policy(
        PermissionMode::ReadOnly,
        &feature_config,
        &registry_with_plugin_tool(),
    )
    .expect("permission policy should build");
    let required = policy.required_mode_for("plugin_echo");
    assert_eq!(required, PermissionMode::WorkspaceWrite);
}

#[test]
fn shared_help_uses_resume_annotation_copy() {
    let help = commands::render_slash_command_help();
    assert!(help.contains("斜杠命令"));
    assert!(help.contains("也支持 --resume SESSION.jsonl"));
}

#[test]
fn bare_skill_dispatch_resolves_known_project_skill_to_prompt() {
    let _guard = env_lock();
    let workspace = temp_dir();
    write_skill_fixture(
        &workspace.join(".codex").join("skills"),
        "caveman",
        "Project skill fixture",
    );

    let prompt = try_resolve_bare_skill_prompt(&workspace, "caveman sharpen club")
        .expect("known bare skill should dispatch");
    assert_eq!(prompt, "$caveman sharpen club");

    fs::remove_dir_all(workspace).expect("workspace should clean up");
}

#[test]
fn bare_skill_dispatch_ignores_unknown_or_non_skill_input() {
    let _guard = env_lock();
    let workspace = temp_dir();
    fs::create_dir_all(&workspace).expect("workspace should exist");

    assert_eq!(
        try_resolve_bare_skill_prompt(&workspace, "not-a-known-skill do thing"),
        None
    );
    assert_eq!(try_resolve_bare_skill_prompt(&workspace, "/status"), None);

    fs::remove_dir_all(workspace).expect("workspace should clean up");
}

#[test]
fn repl_help_includes_shared_commands_and_exit() {
    let help = render_repl_help();
    assert!(help.contains("REPL"));
    assert!(help.contains("/help"));
    assert!(help.contains("补全命令、模式和最近会话"));
    assert!(help.contains("/status"));
    assert!(help.contains("/sandbox"));
    assert!(help.contains("/model [model]"));
    assert!(help.contains("/permissions [read-only|workspace-write|danger-full-access]"));
    assert!(help.contains("/clear [--confirm]"));
    assert!(help.contains("/cost"));
    assert!(help.contains("/resume <session-path>"));
    assert!(help.contains("/config [env|hooks|model|plugins]"));
    assert!(help.contains("/mcp [list|show <server>|help]"));
    assert!(help.contains("/memory"));
    assert!(help.contains("/init"));
    assert!(help.contains("/diff"));
    assert!(help.contains("/version"));
    assert!(help.contains("/export [file]"));
    // Batch 5 added `/session delete`; match on the stable core rather than
    // the trailing bracket so future additions don't re-break this.
    assert!(help.contains("/session [list"));
    assert!(help.contains(
        "/plugin [list|install <path>|enable <name>|disable <name>|uninstall <id>|update <id>]"
    ));
    assert!(help.contains("aliases: /plugins, /marketplace"));
    assert!(help.contains("/agents"));
    assert!(help.contains("/skills"));
    assert!(help.contains("/exit"));
    assert!(help.contains(
        "自动保存             .claw/sessions/<workspace-fingerprint>/<session-id>.jsonl"
    ));
    assert!(help.contains("恢复最近会话         /resume latest"));
}

#[test]
fn completion_candidates_include_workflow_shortcuts_and_dynamic_sessions() {
    let completions = slash_command_completion_candidates_with_sessions(
        "sonnet",
        Some("session-current"),
        vec!["session-old".to_string()],
    );

    assert!(completions.contains(&"/model claude-sonnet-4-6".to_string()));
    assert!(completions.contains(&"/permissions workspace-write".to_string()));
    assert!(completions.contains(&"/session list".to_string()));
    assert!(completions.contains(&"/session switch session-current".to_string()));
    assert!(completions.contains(&"/resume session-old".to_string()));
    assert!(completions.contains(&"/mcp list".to_string()));
    assert!(completions.contains(&"/ultraplan ".to_string()));
}

#[test]
fn completion_candidates_include_new_search_undo_pick_subcommands() {
    let completions = slash_command_completion_candidates_with_sessions(
        "sonnet",
        Some("active"),
        vec!["recent-1".to_string(), "recent-2".to_string()],
    );

    // New top-level commands
    assert!(completions.contains(&"/search ".to_string()));
    assert!(completions.contains(&"/undo".to_string()));

    // /session pick is now a Tab-completable subcommand
    assert!(completions.contains(&"/session pick".to_string()));
    assert!(completions.contains(&"/session pick active".to_string()));
    assert!(completions.contains(&"/session pick recent-1".to_string()));
    assert!(completions.contains(&"/session pick recent-2".to_string()));

    // /session exists and /session delete also gain per-session candidates
    assert!(completions.contains(&"/session exists active".to_string()));
    assert!(completions.contains(&"/session exists recent-1".to_string()));
    assert!(completions.contains(&"/session delete recent-1".to_string()));
    assert!(completions.contains(&"/session delete recent-2".to_string()));

    // /session fork has both bare and arg-prefixed forms
    assert!(completions.contains(&"/session fork".to_string()));
    assert!(completions.contains(&"/session fork ".to_string()));
}

#[test]
fn startup_banner_mentions_workflow_completions() {
    let _guard = env_lock();
    // Inject dummy credentials so LiveCli can construct without real Anthropic key
    std::env::set_var("ANTHROPIC_API_KEY", "test-dummy-key-for-banner-test");
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");

    let banner = with_current_dir(&root, || {
        LiveCli::new(
            "claude-sonnet-4-6".to_string(),
            true,
            None,
            PermissionMode::DangerFullAccess,
            Vec::new(),
            OutputVerbosity::default(),
        )
        .expect("cli should initialize")
        .startup_banner()
    });

    assert!(banner.contains("Tab"));
    assert!(banner.contains("workflow completions"));

    fs::remove_dir_all(root).expect("cleanup temp dir");
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[test]
fn format_connected_line_renders_anthropic_provider_for_claude_model() {
    let model = "claude-sonnet-4-6";

    let line = format_connected_line(model);

    assert_eq!(line, "Connected: claude-sonnet-4-6 via anthropic");
}

#[test]
fn format_connected_line_renders_xai_provider_for_grok_model() {
    let model = "grok-3";

    let line = format_connected_line(model);

    assert_eq!(line, "Connected: grok-3 via xai");
}

#[test]
fn resolve_repl_model_returns_user_supplied_model_unchanged_when_explicit() {
    let user_model = "claude-sonnet-4-6".to_string();

    let resolved = resolve_repl_model(user_model);

    assert_eq!(resolved, "claude-sonnet-4-6");
}

#[test]
fn resolve_repl_model_falls_back_to_anthropic_model_env_when_default() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    let config_home = root.join("config");
    fs::create_dir_all(&config_home).expect("config home dir");
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("ANTHROPIC_MODEL");
    std::env::set_var("ANTHROPIC_MODEL", "sonnet");

    let resolved = with_current_dir(&root, || resolve_repl_model(DEFAULT_MODEL.to_string()));

    assert_eq!(resolved, "claude-sonnet-4-6");

    std::env::remove_var("ANTHROPIC_MODEL");
    std::env::remove_var("CLAW_CONFIG_HOME");
    fs::remove_dir_all(root).expect("cleanup temp dir");
}

// ── Auto-detect model tests ────────────────────────────────────────────────

#[test]
fn resolve_repl_model_auto_detects_deepseek_when_key_present() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    let config_home = root.join("config");
    fs::create_dir_all(&config_home).expect("config home dir");
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("ANTHROPIC_MODEL");

    // Clear Anthropic auth so DeepSeek is picked (priority 2)
    let orig_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    let orig_auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let orig_deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
    let orig_openai = std::env::var("OPENAI_API_KEY").ok();
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::set_var("DEEPSEEK_API_KEY", "sk-test-deepseek-key");

    let resolved = with_current_dir(&root, || resolve_repl_model(DEFAULT_MODEL.to_string()));

    assert_eq!(resolved, "deepseek-v4-pro");

    // Restore
    std::env::remove_var("DEEPSEEK_API_KEY");
    if let Some(v) = orig_anthropic { std::env::set_var("ANTHROPIC_API_KEY", v); }
    if let Some(v) = orig_auth_token { std::env::set_var("ANTHROPIC_AUTH_TOKEN", v); }
    if let Some(v) = orig_openai { std::env::set_var("OPENAI_API_KEY", v); }
    if let Some(v) = orig_deepseek { std::env::set_var("DEEPSEEK_API_KEY", v); }
    std::env::remove_var("CLAW_CONFIG_HOME");
    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn resolve_repl_model_auto_detects_openai_when_only_openai_key_present() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    let config_home = root.join("config");
    fs::create_dir_all(&config_home).expect("config home dir");
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("ANTHROPIC_MODEL");

    let orig_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    let orig_auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let orig_deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
    let orig_openai = std::env::var("OPENAI_API_KEY").ok();
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
    std::env::remove_var("DEEPSEEK_API_KEY");
    std::env::set_var("OPENAI_API_KEY", "sk-test-openai-key");

    let resolved = with_current_dir(&root, || resolve_repl_model(DEFAULT_MODEL.to_string()));

    assert_eq!(resolved, "openai/gpt-4.1-mini");

    // Restore
    std::env::remove_var("OPENAI_API_KEY");
    if let Some(v) = orig_anthropic { std::env::set_var("ANTHROPIC_API_KEY", v); }
    if let Some(v) = orig_auth_token { std::env::set_var("ANTHROPIC_AUTH_TOKEN", v); }
    if let Some(v) = orig_openai { std::env::set_var("OPENAI_API_KEY", v); }
    if let Some(v) = orig_deepseek { std::env::set_var("DEEPSEEK_API_KEY", v); }
    std::env::remove_var("CLAW_CONFIG_HOME");
    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn model_provenance_reports_auto_detect_when_no_config() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    let config_home = root.join("config");
    fs::create_dir_all(&config_home).expect("config home dir");
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("ANTHROPIC_MODEL");

    // Set DEEPSEEK_API_KEY only, clear Anthropic
    let orig_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    let orig_auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let orig_deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
    let orig_openai = std::env::var("OPENAI_API_KEY").ok();
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::set_var("DEEPSEEK_API_KEY", "sk-test-deepseek");

    let provenance =
        with_current_dir(&root, || ModelProvenance::from_env_or_config_or_default(DEFAULT_MODEL));

    assert_eq!(provenance.source, ModelSource::AutoDetect);
    assert_eq!(provenance.resolved, "deepseek-v4-pro");

    // Restore
    std::env::remove_var("DEEPSEEK_API_KEY");
    if let Some(v) = orig_anthropic { std::env::set_var("ANTHROPIC_API_KEY", v); }
    if let Some(v) = orig_auth_token { std::env::set_var("ANTHROPIC_AUTH_TOKEN", v); }
    if let Some(v) = orig_openai { std::env::set_var("OPENAI_API_KEY", v); }
    if let Some(v) = orig_deepseek { std::env::set_var("DEEPSEEK_API_KEY", v); }
    std::env::remove_var("CLAW_CONFIG_HOME");
    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn model_provenance_reports_default_when_no_keys_at_all() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    let config_home = root.join("config");
    fs::create_dir_all(&config_home).expect("config home dir");
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("ANTHROPIC_MODEL");

    // Clear ALL API keys
    let orig_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    let orig_auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let orig_deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
    let orig_openai = std::env::var("OPENAI_API_KEY").ok();
    let orig_xai = std::env::var("XAI_API_KEY").ok();
    let orig_dashscope = std::env::var("DASHSCOPE_API_KEY").ok();
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
    std::env::remove_var("DEEPSEEK_API_KEY");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("XAI_API_KEY");
    std::env::remove_var("DASHSCOPE_API_KEY");

    let provenance =
        with_current_dir(&root, || ModelProvenance::from_env_or_config_or_default(DEFAULT_MODEL));

    assert_eq!(provenance.source, ModelSource::Default);
    assert_eq!(provenance.resolved, DEFAULT_MODEL);

    // Restore
    if let Some(v) = orig_anthropic { std::env::set_var("ANTHROPIC_API_KEY", v); }
    if let Some(v) = orig_auth_token { std::env::set_var("ANTHROPIC_AUTH_TOKEN", v); }
    if let Some(v) = orig_openai { std::env::set_var("OPENAI_API_KEY", v); }
    if let Some(v) = orig_deepseek { std::env::set_var("DEEPSEEK_API_KEY", v); }
    if let Some(v) = orig_xai { std::env::set_var("XAI_API_KEY", v); }
    if let Some(v) = orig_dashscope { std::env::set_var("DASHSCOPE_API_KEY", v); }
    std::env::remove_var("CLAW_CONFIG_HOME");
    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn resolve_repl_model_returns_default_when_env_unset_and_no_config() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    let config_home = root.join("config");
    fs::create_dir_all(&config_home).expect("config home dir");
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);
    std::env::remove_var("ANTHROPIC_MODEL");

    let resolved = with_current_dir(&root, || resolve_repl_model(DEFAULT_MODEL.to_string()));

    assert_eq!(resolved, DEFAULT_MODEL);

    std::env::remove_var("CLAW_CONFIG_HOME");
    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn resume_supported_command_list_matches_expected_surface() {
    let names = resume_supported_slash_commands()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    // Now with 135+ slash commands, verify minimum resume support
    assert!(
        names.len() >= 39,
        "expected at least 39 resume-supported commands, got {}",
        names.len()
    );
    // Verify key resume commands still exist
    assert!(names.contains(&"help"));
    assert!(names.contains(&"status"));
    assert!(names.contains(&"compact"));
}

#[test]
fn session_exists_resume_command_reports_json_contract() {
    let session = Session::new();
    let path = PathBuf::from("missing-session.jsonl");
    let outcome = run_resume_command(
        &path,
        &session,
        &SlashCommand::Session {
            action: Some("exists".to_string()),
            target: Some("definitely-missing-session".to_string()),
        },
    )
    .expect("exists command should not fail for missing sessions");

    let json = outcome.json.expect("json contract");
    assert_eq!(json["kind"], "session_exists");
    assert_eq!(json["exists"], false);
    assert_eq!(json["session"], "definitely-missing-session");
}

#[test]
fn resume_report_uses_sectioned_layout() {
    let report = format_resume_report("session.jsonl", 14, 6);
    assert!(report.contains("Session resumed"));
    assert!(report.contains("Session file     session.jsonl"));
    assert!(report.contains("Messages         14"));
    assert!(report.contains("Turns            6"));
}

#[test]
fn compact_report_uses_structured_output() {
    let compacted = format_compact_report(8, 5, false);
    assert!(compacted.contains("Compact"));
    assert!(compacted.contains("Result           compacted"));
    assert!(compacted.contains("Messages removed 8"));
    let skipped = format_compact_report(0, 3, true);
    assert!(skipped.contains("Result           skipped"));
}

#[test]
fn cost_report_uses_sectioned_layout() {
    let report = format_cost_report(runtime::TokenUsage {
        input_tokens: 20,
        output_tokens: 8,
        cache_creation_input_tokens: 3,
        cache_read_input_tokens: 1,
    });
    assert!(report.contains("成本"));
    assert!(report.contains("输入 tokens      20"));
    assert!(report.contains("输出 tokens      8"));
    assert!(report.contains("缓存创建         3"));
    assert!(report.contains("缓存读取         1"));
    assert!(report.contains("总 tokens        32"));
    assert!(report.contains("预估成本"));
}

#[test]
fn permissions_report_uses_sectioned_layout() {
    let report = format_permissions_report("workspace-write");
    assert!(report.contains("Permissions"));
    assert!(report.contains("Active mode      workspace-write"));
    assert!(report.contains("Modes"));
    assert!(report.contains("read-only          ○ available Read/search tools only"));
    assert!(report.contains("workspace-write    ● current   Edit files inside the workspace"));
    assert!(report.contains("danger-full-access ○ available Unrestricted tool access"));
}

#[test]
fn permissions_switch_report_is_structured() {
    let report = format_permissions_switch_report("read-only", "workspace-write");
    assert!(report.contains("Permissions updated"));
    assert!(report.contains("Result           mode switched"));
    assert!(report.contains("Previous mode    read-only"));
    assert!(report.contains("Active mode      workspace-write"));
    assert!(report.contains("Applies to       subsequent tool calls"));
}

#[test]
fn init_help_mentions_direct_subcommand() {
    let mut help = Vec::new();
    print_help_to(&mut help).expect("help should render");
    let help = String::from_utf8(help).expect("help should be utf8");
    assert!(help.contains("claw help"));
    assert!(help.contains("claw version"));
    assert!(help.contains("claw status"));
    assert!(help.contains("claw sandbox"));
    assert!(help.contains("claw init"));
    assert!(help.contains("claw acp [serve]"));
    assert!(help.contains("claw agents"));
    assert!(help.contains("claw mcp"));
    assert!(help.contains("claw skills"));
    assert!(help.contains("claw /skills"));
    assert!(help.contains("dong382258137/claw-code"));
    assert!(help.contains("cargo install claw-code"));
    assert!(!help.contains("claw login"));
    assert!(!help.contains("claw logout"));
}

#[test]
fn model_report_uses_sectioned_layout() {
    let report = format_model_report("claude-sonnet", 12, 4);
    assert!(report.contains("Model"));
    assert!(report.contains("Current model    claude-sonnet"));
    assert!(report.contains("Session messages 12"));
    assert!(report.contains("Switch models with /model <name>"));
}

fn test_branch_freshness() -> super::BranchFreshness {
    super::BranchFreshness {
        upstream: Some("origin/main".to_string()),
        ahead: 0,
        behind: 0,
        fresh: Some(true),
    }
}

fn test_boot_preflight() -> super::BootPreflightSnapshot {
    super::BootPreflightSnapshot {
        repo_exists: true,
        worktree_exists: true,
        git_dir_exists: true,
        branch_freshness: test_branch_freshness(),
        trust_gate_allowed: Some(false),
        trusted_roots_count: 0,
        required_binaries: Vec::new(),
        control_sockets: Vec::new(),
        mcp_startup_eligible: true,
        mcp_servers_configured: 0,
        plugin_startup_eligible: true,
        plugins_configured: 0,
        last_failed_boot_reason: None,
    }
}

#[test]
fn model_switch_report_preserves_context_summary() {
    let report = format_model_switch_report("claude-sonnet", "claude-opus", 9);
    assert!(report.contains("Model updated"));
    assert!(report.contains("Previous         claude-sonnet"));
    assert!(report.contains("Current          claude-opus"));
    assert!(report.contains("Preserved msgs   9"));
}

#[test]
fn status_line_reports_model_and_token_totals() {
    let status = format_status_report(
        "claude-sonnet",
        StatusUsage {
            message_count: 7,
            turns: 3,
            latest: runtime::TokenUsage {
                input_tokens: 5,
                output_tokens: 4,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 0,
            },
            cumulative: runtime::TokenUsage {
                input_tokens: 20,
                output_tokens: 8,
                cache_creation_input_tokens: 2,
                cache_read_input_tokens: 1,
            },
            estimated_tokens: 128,
        },
        "workspace-write",
        &super::StatusContext {
            cwd: PathBuf::from("/tmp/project"),
            session_path: Some(PathBuf::from("session.jsonl")),
            loaded_config_files: 2,
            discovered_config_files: 3,
            memory_file_count: 4,
            project_root: Some(PathBuf::from("/tmp")),
            git_branch: Some("main".to_string()),
            git_summary: GitWorkspaceSummary {
                changed_files: 3,
                staged_files: 1,
                unstaged_files: 1,
                untracked_files: 1,
                conflicted_files: 0,
            },
            branch_freshness: test_branch_freshness(),
            stale_base_state: super::BaseCommitState::NoExpectedBase,
            session_lifecycle: SessionLifecycleSummary {
                kind: SessionLifecycleKind::IdleShell,
                pane_id: Some("%7".to_string()),
                pane_command: Some("zsh".to_string()),
                pane_path: Some(PathBuf::from("/tmp/project")),
                workspace_dirty: true,
                abandoned: true,
            },
            boot_preflight: test_boot_preflight(),
            sandbox_status: runtime::SandboxStatus::default(),
            config_load_error: None,
        },
        None, // #148
    );
    assert!(status.contains("状态"));
    assert!(status.contains("模型             claude-sonnet"));
    assert!(status.contains("权限模式         workspace-write"));
    assert!(status.contains("消息数           7"));
    assert!(status.contains("本次总量         10"));
    assert!(status.contains("缓存创建         2"));
    assert!(status.contains("缓存读取         1"));
    assert!(status.contains("累计总量         31"));
    assert!(status.contains("预估成本"));
    assert!(status.contains("当前目录         /tmp/project"));
    assert!(status.contains("项目根目录       /tmp"));
    assert!(status.contains("Git 分支         main"));
    assert!(status.contains("Git 状态         脏 · 3 个文件 · 1 已暂存, 1 未暂存, 1 未跟踪"));
    assert!(status.contains("已更改文件       3"));
    assert!(status.contains("已暂存           1"));
    assert!(status.contains("未暂存           1"));
    assert!(status.contains("未跟踪           1"));
    assert!(status.contains("会话             session.jsonl"));
    assert!(status.contains("生命周期         idle shell · dirty worktree · abandoned? · cmd=zsh"));
    assert!(status.contains("配置文件         已加载 2/3"));
    assert!(status.contains("Memory 文件      4"));
    assert!(status.contains("建议流程         /status → /diff → /commit"));
}

#[test]
fn session_lifecycle_prefers_running_process_over_idle_shell() {
    let workspace = PathBuf::from("/tmp/project");
    let lifecycle = classify_session_lifecycle_from_panes(
        &workspace,
        vec![
            TmuxPaneSnapshot {
                pane_id: "%1".to_string(),
                current_command: "zsh".to_string(),
                current_path: workspace.clone(),
            },
            TmuxPaneSnapshot {
                pane_id: "%2".to_string(),
                current_command: "claw".to_string(),
                current_path: workspace.join("rust"),
            },
        ],
    );

    assert_eq!(lifecycle.kind, SessionLifecycleKind::RunningProcess);
    assert_eq!(lifecycle.pane_id.as_deref(), Some("%2"));
    assert_eq!(lifecycle.pane_command.as_deref(), Some("claw"));
    assert!(!lifecycle.abandoned);
}

#[test]
fn session_lifecycle_marks_dirty_idle_shell_as_abandoned() {
    let _guard = env_lock();
    let workspace = temp_workspace("dirty-idle-shell");
    fs::create_dir_all(&workspace).expect("workspace should create");
    git(&["init", "--quiet"], &workspace);
    git(&["config", "user.email", "tests@example.com"], &workspace);
    git(&["config", "user.name", "Rusty Claude Tests"], &workspace);
    fs::write(workspace.join("tracked.txt"), "hello\n").expect("write tracked");
    git(&["add", "tracked.txt"], &workspace);
    git(&["commit", "-m", "init", "--quiet"], &workspace);
    fs::write(workspace.join("tracked.txt"), "hello\nchanged\n").expect("dirty tracked");

    let lifecycle = classify_session_lifecycle_from_panes(
        &workspace,
        vec![TmuxPaneSnapshot {
            pane_id: "%3".to_string(),
            current_command: "bash".to_string(),
            current_path: workspace.clone(),
        }],
    );

    assert_eq!(lifecycle.kind, SessionLifecycleKind::IdleShell);
    assert!(lifecycle.workspace_dirty);
    assert!(lifecycle.abandoned);

    fs::remove_dir_all(workspace).expect("cleanup temp dir");
}

#[test]
fn session_list_surfaces_saved_dirty_abandoned_lifecycle() {
    let _guard = cwd_guard();
    let workspace = temp_workspace("session-list-lifecycle");
    fs::create_dir_all(&workspace).expect("workspace should create");
    git(&["init", "--quiet"], &workspace);
    git(&["config", "user.email", "tests@example.com"], &workspace);
    git(&["config", "user.name", "Rusty Claude Tests"], &workspace);
    fs::write(workspace.join(".gitignore"), ".claw/\n").expect("write gitignore");
    fs::write(workspace.join("tracked.txt"), "hello\n").expect("write tracked");
    git(&["add", ".gitignore", "tracked.txt"], &workspace);
    git(&["commit", "-m", "init", "--quiet"], &workspace);

    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&workspace).expect("switch cwd");
    let handle = create_managed_session_handle("session-alpha").expect("session handle");
    Session::new()
        .with_workspace_root(workspace.clone())
        .with_persistence_path(handle.path.clone())
        .save_to_path(&handle.path)
        .expect("session should save");
    fs::write(workspace.join("tracked.txt"), "hello\nchanged\n").expect("dirty tracked");

    let report = render_session_list("session-alpha").expect("session list should render");

    assert!(report.contains("session-alpha"));
    assert!(report.contains("lifecycle=saved only · dirty worktree · abandoned?"));

    std::env::set_current_dir(previous).expect("restore cwd");
    fs::remove_dir_all(workspace).expect("cleanup temp dir");
}

#[test]
fn workspace_health_warns_when_stale_base_diverged() {
    let context = super::StatusContext {
        cwd: PathBuf::from("/tmp/project"),
        session_path: None,
        loaded_config_files: 0,
        discovered_config_files: 0,
        memory_file_count: 0,
        project_root: Some(PathBuf::from("/tmp/project")),
        git_branch: Some("feature/stale-base".to_string()),
        git_summary: GitWorkspaceSummary::default(),
        branch_freshness: test_branch_freshness(),
        stale_base_state: super::BaseCommitState::Diverged {
            expected: "base".to_string(),
            actual: "head".to_string(),
        },
        session_lifecycle: SessionLifecycleSummary {
            kind: SessionLifecycleKind::SavedOnly,
            pane_id: None,
            pane_command: None,
            pane_path: None,
            workspace_dirty: false,
            abandoned: false,
        },
        boot_preflight: test_boot_preflight(),
        sandbox_status: runtime::SandboxStatus::default(),
        config_load_error: None,
    };

    let check = super::check_workspace_health(&context);

    assert_eq!(check.level, super::DiagnosticLevel::Warn);
    assert_eq!(check.data["stale_base"]["status"], "diverged");
    assert_eq!(check.data["stale_base"]["fresh"], false);
    assert!(check
        .details
        .iter()
        .any(|detail| detail.contains("stale codebase")));
}

#[test]
fn status_json_surfaces_session_lifecycle_for_clawhip() {
    let context = super::StatusContext {
        cwd: PathBuf::from("/tmp/project"),
        session_path: None,
        loaded_config_files: 0,
        discovered_config_files: 0,
        memory_file_count: 0,
        project_root: Some(PathBuf::from("/tmp/project")),
        git_branch: Some("feature/session-lifecycle".to_string()),
        git_summary: GitWorkspaceSummary::default(),
        branch_freshness: test_branch_freshness(),
        stale_base_state: super::BaseCommitState::NoExpectedBase,
        session_lifecycle: SessionLifecycleSummary {
            kind: SessionLifecycleKind::RunningProcess,
            pane_id: Some("%9".to_string()),
            pane_command: Some("claw".to_string()),
            pane_path: Some(PathBuf::from("/tmp/project")),
            workspace_dirty: false,
            abandoned: false,
        },
        boot_preflight: test_boot_preflight(),
        sandbox_status: runtime::SandboxStatus::default(),
        config_load_error: None,
    };

    let value = status_json_value(
        Some("claude-sonnet"),
        StatusUsage {
            message_count: 0,
            turns: 0,
            latest: runtime::TokenUsage::default(),
            cumulative: runtime::TokenUsage::default(),
            estimated_tokens: 0,
        },
        "workspace-write",
        &context,
        None,
        None,
    );

    assert_eq!(
        value["workspace"]["session_lifecycle"]["kind"],
        "running_process"
    );
    assert_eq!(
        value["workspace"]["session_lifecycle"]["pane_command"],
        "claw"
    );
    assert_eq!(value["workspace"]["session_lifecycle"]["abandoned"], false);
    assert_eq!(value["workspace"]["branch_freshness"]["fresh"], true);
    assert_eq!(
        value["workspace"]["boot_preflight"]["repo"]["worktree_exists"],
        true
    );
    assert_eq!(
        value["workspace"]["boot_preflight"]["mcp_startup"]["eligible"],
        true
    );
    assert_eq!(
        value["workspace"]["boot_preflight"]["last_failed_boot_reason"],
        serde_json::Value::Null
    );
}

#[test]
fn branch_freshness_parses_ahead_behind_status_header() {
    let freshness = super::BranchFreshness::from_git_status(Some(
        "## feature/boot...origin/feature/boot [ahead 2, behind 3]\n M src/main.rs",
    ));

    assert_eq!(freshness.upstream.as_deref(), Some("origin/feature/boot"));
    assert_eq!(freshness.ahead, 2);
    assert_eq!(freshness.behind, 3);
    assert_eq!(freshness.fresh, Some(false));
}

#[test]
fn boot_preflight_snapshot_reports_machine_readable_contract_fields() {
    let _guard = env_lock();
    let workspace = temp_workspace("boot-preflight-json");
    fs::create_dir_all(&workspace).expect("workspace should create");
    git(&["init", "--quiet"], &workspace);
    git(&["config", "user.email", "tests@example.com"], &workspace);
    git(&["config", "user.name", "Rusty Claude Tests"], &workspace);
    fs::write(workspace.join("tracked.txt"), "hello\n").expect("write tracked");
    fs::write(workspace.join(".claw.json"), r#"{"trustedRoots": ["."]}"#).expect("write config");
    git(&["add", "tracked.txt"], &workspace);
    git(&["commit", "-m", "init", "--quiet"], &workspace);

    let loader = ConfigLoader::default_for(&workspace);
    let config = loader.load().expect("config should load");
    let status = super::run_git_capture_in(&workspace, &["status", "--short", "--branch"]);
    let snapshot = super::build_boot_preflight_snapshot(
        &workspace,
        Some(&workspace),
        status.as_deref(),
        Some(&config),
        None,
    );
    let json = snapshot.json_value();

    assert_eq!(json["repo"]["exists"], true);
    assert_eq!(json["repo"]["worktree_exists"], true);
    assert_eq!(json["trust_gate"]["allowlisted"], true);
    assert_eq!(json["mcp_startup"]["eligible"], true);
    assert!(json["required_binaries"]
        .as_array()
        .is_some_and(|items| { items.iter().any(|item| item["name"] == "git") }));
    fs::remove_dir_all(workspace).expect("cleanup temp dir");
}

#[test]
fn commit_reports_surface_workspace_context() {
    let summary = GitWorkspaceSummary {
        changed_files: 2,
        staged_files: 1,
        unstaged_files: 1,
        untracked_files: 0,
        conflicted_files: 0,
    };

    let preflight = format_commit_preflight_report(Some("feature/ux"), summary);
    assert!(preflight.contains("Result           ready"));
    assert!(preflight.contains("Branch           feature/ux"));
    assert!(preflight.contains("Workspace        脏 · 2 个文件 · 1 已暂存, 1 未暂存"));
    assert!(preflight
        .contains("Action           create a git commit from the current workspace changes"));
}

#[test]
fn commit_skipped_report_points_to_next_steps() {
    let report = format_commit_skipped_report();
    assert!(report.contains("Reason           no workspace changes"));
    assert!(
        report.contains("Action           create a git commit from the current workspace changes")
    );
    assert!(report.contains("/status to inspect context"));
    assert!(report.contains("/diff to inspect repo changes"));
}

#[test]
fn runtime_slash_reports_describe_command_behavior() {
    let bughunter = format_bughunter_report(Some("runtime"));
    assert!(bughunter.contains("Scope            runtime"));
    assert!(bughunter.contains("inspect the selected code for likely bugs"));

    let ultraplan = format_ultraplan_report(Some("ship the release"));
    assert!(ultraplan.contains("Task             ship the release"));
    assert!(ultraplan.contains("break work into a multi-step execution plan"));

    let pr = format_pr_report("feature/ux", Some("ready for review"));
    assert!(pr.contains("Branch           feature/ux"));
    assert!(pr.contains("draft or create a pull request"));

    let issue = format_issue_report(Some("flaky test"));
    assert!(issue.contains("Context          flaky test"));
    assert!(issue.contains("draft or create a GitHub issue"));
}

#[test]
fn no_arg_commands_reject_unexpected_arguments() {
    assert!(validate_no_args("/commit", None).is_ok());

    let error = validate_no_args("/commit", Some("now"))
        .expect_err("unexpected arguments should fail")
        .to_string();
    assert!(error.contains("/commit does not accept arguments"));
    assert!(error.contains("Received: now"));
}

#[test]
fn config_report_supports_section_views() {
    let report = render_config_report(Some("env")).expect("config report should render");
    assert!(report.contains("合并的节: env"));
    let plugins_report =
        render_config_report(Some("plugins")).expect("plugins config report should render");
    assert!(plugins_report.contains("合并的节: plugins"));
}

#[test]
fn memory_report_uses_sectioned_layout() {
    let report = render_memory_report().expect("memory report should render");
    assert!(report.contains("Memory"));
    assert!(report.contains("工作目录"));
    assert!(report.contains("指令文件数"));
    assert!(report.contains("发现的文件"));
}

#[test]
fn config_report_uses_sectioned_layout() {
    let report = render_config_report(None).expect("config report should render");
    assert!(report.contains("Config"));
    assert!(report.contains("发现的文件"));
    assert!(report.contains("合并的 JSON"));
}

#[test]
fn parses_git_status_metadata() {
    let _guard = env_lock();
    let temp_root = temp_dir();
    fs::create_dir_all(&temp_root).expect("root dir");
    let (project_root, branch) = parse_git_status_metadata_for(
        &temp_root,
        Some(
            "## rcc/cli...origin/rcc/cli
 M src/main.rs",
        ),
    );
    assert_eq!(branch.as_deref(), Some("rcc/cli"));
    assert!(project_root.is_none());
    fs::remove_dir_all(temp_root).expect("cleanup temp dir");
}

#[test]
fn parses_detached_head_from_status_snapshot() {
    let _guard = env_lock();
    assert_eq!(
        parse_git_status_branch(Some(
            "## HEAD (no branch)
 M src/main.rs"
        )),
        Some("detached HEAD".to_string())
    );
}

#[test]
fn parses_git_workspace_summary_counts() {
    let summary = parse_git_workspace_summary(Some(
        "## feature/ux
M  src/main.rs
 M README.md
?? notes.md
UU conflicted.rs",
    ));

    assert_eq!(
        summary,
        GitWorkspaceSummary {
            changed_files: 4,
            staged_files: 2,
            unstaged_files: 2,
            untracked_files: 1,
            conflicted_files: 1,
        }
    );
    assert_eq!(
        summary.headline(),
        "脏 · 4 个文件 · 2 已暂存, 2 未暂存, 1 未跟踪, 1 有冲突"
    );
}

#[test]
fn render_diff_report_shows_clean_tree_for_committed_repo() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    git(&["init", "--quiet"], &root);
    git(&["config", "user.email", "tests@example.com"], &root);
    git(&["config", "user.name", "Rusty Claude Tests"], &root);
    fs::write(root.join("tracked.txt"), "hello\n").expect("write file");
    git(&["add", "tracked.txt"], &root);
    git(&["commit", "-m", "init", "--quiet"], &root);

    let report = render_diff_report_for(&root).expect("diff report should render");
    assert!(report.contains("干净的工作树"));

    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn render_diff_report_includes_staged_and_unstaged_sections() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    git(&["init", "--quiet"], &root);
    git(&["config", "user.email", "tests@example.com"], &root);
    git(&["config", "user.name", "Rusty Claude Tests"], &root);
    fs::write(root.join("tracked.txt"), "hello\n").expect("write file");
    git(&["add", "tracked.txt"], &root);
    git(&["commit", "-m", "init", "--quiet"], &root);

    fs::write(root.join("tracked.txt"), "hello\nstaged\n").expect("update file");
    git(&["add", "tracked.txt"], &root);
    fs::write(root.join("tracked.txt"), "hello\nstaged\nunstaged\n").expect("update file twice");

    let report = render_diff_report_for(&root).expect("diff report should render");
    assert!(report.contains("已暂存的更改:"));
    assert!(report.contains("未暂存的更改:"));
    assert!(report.contains("tracked.txt"));

    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn render_diff_report_omits_ignored_files() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    git(&["init", "--quiet"], &root);
    git(&["config", "user.email", "tests@example.com"], &root);
    git(&["config", "user.name", "Rusty Claude Tests"], &root);
    fs::write(root.join(".gitignore"), ".omx/\nignored.txt\n").expect("write gitignore");
    fs::write(root.join("tracked.txt"), "hello\n").expect("write tracked");
    git(&["add", ".gitignore", "tracked.txt"], &root);
    git(&["commit", "-m", "init", "--quiet"], &root);
    fs::create_dir_all(root.join(".omx")).expect("write omx dir");
    fs::write(root.join(".omx").join("state.json"), "{}").expect("write ignored omx");
    fs::write(root.join("ignored.txt"), "secret\n").expect("write ignored file");
    fs::write(root.join("tracked.txt"), "hello\nworld\n").expect("write tracked change");

    let report = render_diff_report_for(&root).expect("diff report should render");
    assert!(report.contains("tracked.txt"));
    assert!(!report.contains("+++ b/ignored.txt"));
    assert!(!report.contains("+++ b/.omx/state.json"));

    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn resume_diff_command_renders_report_for_saved_session() {
    let _guard = env_lock();
    let root = temp_dir();
    fs::create_dir_all(&root).expect("root dir");
    git(&["init", "--quiet"], &root);
    git(&["config", "user.email", "tests@example.com"], &root);
    git(&["config", "user.name", "Rusty Claude Tests"], &root);
    fs::write(root.join("tracked.txt"), "hello\n").expect("write tracked");
    git(&["add", "tracked.txt"], &root);
    git(&["commit", "-m", "init", "--quiet"], &root);
    fs::write(root.join("tracked.txt"), "hello\nworld\n").expect("modify tracked");
    let session_path = root.join("session.json");
    Session::new()
        .save_to_path(&session_path)
        .expect("session should save");

    let session = Session::load_from_path(&session_path).expect("session should load");
    let outcome = with_current_dir(&root, || {
        run_resume_command(&session_path, &session, &SlashCommand::Diff)
            .expect("resume diff should work")
    });
    let message = outcome.message.expect("diff message should exist");
    assert!(message.contains("未暂存的更改:"));
    assert!(message.contains("tracked.txt"));

    fs::remove_dir_all(root).expect("cleanup temp dir");
}

#[test]
fn status_context_reads_real_workspace_metadata() {
    let context = status_context(None).expect("status context should load");
    assert!(context.cwd.is_absolute());
    assert!(context.discovered_config_files >= context.loaded_config_files);
    assert!(context.loaded_config_files <= context.discovered_config_files);
}

#[test]
fn normalizes_supported_permission_modes() {
    assert_eq!(normalize_permission_mode("read-only"), Some("read-only"));
    assert_eq!(
        normalize_permission_mode("workspace-write"),
        Some("workspace-write")
    );
    assert_eq!(
        normalize_permission_mode("danger-full-access"),
        Some("danger-full-access")
    );
    assert_eq!(normalize_permission_mode("unknown"), None);
}

#[test]
fn clear_command_requires_explicit_confirmation_flag() {
    assert_eq!(
        SlashCommand::parse("/clear"),
        Ok(Some(SlashCommand::Clear { confirm: false }))
    );
    assert_eq!(
        SlashCommand::parse("/clear --confirm"),
        Ok(Some(SlashCommand::Clear { confirm: true }))
    );
}

#[test]
fn parses_resume_and_config_slash_commands() {
    assert_eq!(
        SlashCommand::parse("/resume saved-session.jsonl"),
        Ok(Some(SlashCommand::Resume {
            session_path: Some("saved-session.jsonl".to_string())
        }))
    );
    assert_eq!(
        SlashCommand::parse("/clear --confirm"),
        Ok(Some(SlashCommand::Clear { confirm: true }))
    );
    assert_eq!(
        SlashCommand::parse("/config"),
        Ok(Some(SlashCommand::Config { section: None }))
    );
    assert_eq!(
        SlashCommand::parse("/config env"),
        Ok(Some(SlashCommand::Config {
            section: Some("env".to_string())
        }))
    );
    assert_eq!(
        SlashCommand::parse("/memory"),
        Ok(Some(SlashCommand::Memory))
    );
    assert_eq!(SlashCommand::parse("/init"), Ok(Some(SlashCommand::Init)));
    assert_eq!(
        SlashCommand::parse("/session fork incident-review"),
        Ok(Some(SlashCommand::Session {
            action: Some("fork".to_string()),
            target: Some("incident-review".to_string())
        }))
    );
}

#[test]
fn help_mentions_jsonl_resume_examples() {
    let mut help = Vec::new();
    print_help_to(&mut help).expect("help should render");
    let help = String::from_utf8(help).expect("help should be utf8");
    assert!(help.contains("claw --resume [SESSION.jsonl|session-id|latest]"));
    assert!(help.contains("Use `latest` with --resume, /resume, or /session switch"));
    assert!(help.contains("claw --resume latest"));
    assert!(help.contains("claw --resume latest /status /diff /export notes.txt"));
}

#[test]
fn managed_sessions_default_to_jsonl_and_resolve_legacy_json() {
    let _guard = cwd_guard();
    let workspace = temp_workspace("session-resolution");
    std::fs::create_dir_all(&workspace).expect("workspace should create");
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&workspace).expect("switch cwd");

    let handle = create_managed_session_handle("session-alpha").expect("jsonl handle");
    assert!(handle.path.ends_with("session-alpha.jsonl"));

    let legacy_path = workspace.join(".claw/sessions/legacy.json");
    std::fs::create_dir_all(
        legacy_path
            .parent()
            .expect("legacy path should have parent directory"),
    )
    .expect("session dir should exist");
    Session::new()
        .with_workspace_root(workspace.clone())
        .with_persistence_path(legacy_path.clone())
        .save_to_path(&legacy_path)
        .expect("legacy session should save");

    let resolved = resolve_session_reference("legacy").expect("legacy session should resolve");
    assert_eq!(
        resolved
            .path
            .canonicalize()
            .expect("resolved path should exist"),
        legacy_path
            .canonicalize()
            .expect("legacy path should exist")
    );

    std::env::set_current_dir(previous).expect("restore cwd");
    std::fs::remove_dir_all(workspace).expect("workspace should clean up");
}

#[test]
fn resumed_session_exists_and_delete_have_json_contracts() {
    let _env_guard = env_lock();
    let _guard = cwd_guard();
    let workspace = temp_workspace("resume-session-json-contracts");
    std::fs::create_dir_all(&workspace).expect("workspace should create");
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&workspace).expect("switch cwd");

    let active = create_managed_session_handle("session-active").expect("active handle");
    let active_session = Session::new()
        .with_workspace_root(workspace.clone())
        .with_persistence_path(active.path.clone());
    active_session
        .save_to_path(&active.path)
        .expect("active session should save");
    let saved = create_managed_session_handle("session-saved").expect("saved handle");
    Session::new()
        .with_workspace_root(workspace.clone())
        .with_persistence_path(saved.path.clone())
        .save_to_path(&saved.path)
        .expect("saved session should save");

    let exists_command = SlashCommand::parse("/session exists session-saved")
        .expect("parse should succeed")
        .expect("command should exist");
    let exists = run_resume_command(&active.path, &active_session, &exists_command)
        .expect("exists should run")
        .json
        .expect("exists should return json");
    assert_eq!(exists["kind"], "session_exists");
    assert_eq!(exists["session_id"], "session-saved");
    assert_eq!(exists["exists"], true);
    assert_eq!(exists["active"], false);
    assert!(exists["path"].as_str().is_some());

    let missing_command = SlashCommand::parse("/session exists missing-session")
        .expect("parse should succeed")
        .expect("command should exist");
    let missing = run_resume_command(&active.path, &active_session, &missing_command)
        .expect("missing exists should run")
        .json
        .expect("missing exists should return json");
    assert_eq!(missing["kind"], "session_exists");
    assert_eq!(missing["exists"], false);
    assert_eq!(missing["session_id"], "missing-session");
    assert!(missing["candidate_path"].as_str().is_some());

    let delete_command = SlashCommand::parse("/session delete session-saved --force")
        .expect("parse should succeed")
        .expect("command should exist");
    let deleted = run_resume_command(&active.path, &active_session, &delete_command)
        .expect("delete should run")
        .json
        .expect("delete should return json");
    assert_eq!(deleted["kind"], "session_delete");
    assert_eq!(deleted["deleted"], true);
    assert!(!saved.path.exists(), "saved session should be deleted");

    std::env::set_current_dir(previous).expect("restore cwd");
    std::fs::remove_dir_all(workspace).expect("workspace should clean up");
}

#[test]
fn latest_session_alias_resolves_most_recent_managed_session() {
    let _guard = cwd_guard();
    let workspace = temp_workspace("latest-session-alias");
    std::fs::create_dir_all(&workspace).expect("workspace should create");
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&workspace).expect("switch cwd");

    let older = create_managed_session_handle("session-older").expect("older handle");
    Session::new()
        .with_persistence_path(older.path.clone())
        .save_to_path(&older.path)
        .expect("older session should save");
    std::thread::sleep(Duration::from_millis(20));
    let newer = create_managed_session_handle("session-newer").expect("newer handle");
    Session::new()
        .with_persistence_path(newer.path.clone())
        .save_to_path(&newer.path)
        .expect("newer session should save");

    let resolved = resolve_session_reference("latest").expect("latest session should resolve");
    assert_eq!(
        resolved
            .path
            .canonicalize()
            .expect("resolved path should exist"),
        newer.path.canonicalize().expect("newer path should exist")
    );

    std::env::set_current_dir(previous).expect("restore cwd");
    std::fs::remove_dir_all(workspace).expect("workspace should clean up");
}

#[test]
fn load_session_reference_rejects_workspace_mismatch() {
    let _guard = cwd_guard();
    let workspace_a = temp_workspace("session-mismatch-a");
    let workspace_b = temp_workspace("session-mismatch-b");
    std::fs::create_dir_all(&workspace_a).expect("workspace a should create");
    std::fs::create_dir_all(&workspace_b).expect("workspace b should create");
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&workspace_b).expect("switch cwd");

    let session_path = workspace_a.join(".claw/sessions/legacy-cross.jsonl");
    std::fs::create_dir_all(
        session_path
            .parent()
            .expect("session path should have parent directory"),
    )
    .expect("session dir should exist");
    Session::new()
        .with_workspace_root(workspace_a.clone())
        .with_persistence_path(session_path.clone())
        .save_to_path(&session_path)
        .expect("session should save");

    let error = crate::load_session_reference(&session_path.display().to_string())
        .expect_err("mismatched workspace should fail");
    assert!(
        error.to_string().contains("session workspace mismatch"),
        "unexpected error: {error}"
    );
    assert!(
        error
            .to_string()
            .contains(&workspace_b.display().to_string()),
        "expected current workspace in error: {error}"
    );
    assert!(
        error
            .to_string()
            .contains(&workspace_a.display().to_string()),
        "expected originating workspace in error: {error}"
    );

    std::env::set_current_dir(previous).expect("restore cwd");
    std::fs::remove_dir_all(workspace_a).expect("workspace a should clean up");
    std::fs::remove_dir_all(workspace_b).expect("workspace b should clean up");
}

#[test]
fn unknown_slash_command_guidance_suggests_nearby_commands() {
    let message = format_unknown_slash_command("stats");
    assert!(message.contains("Unknown slash command: /stats"));
    assert!(message.contains("/status"));
    assert!(message.contains("/help"));
}

#[test]
fn unknown_omc_slash_command_guidance_explains_runtime_gap() {
    let message = format_unknown_slash_command("oh-my-claudecode:hud");
    assert!(message.contains("Unknown slash command: /oh-my-claudecode:hud"));
    assert!(message.contains("Claude Code/OMC plugin command"));
    assert!(message.contains("does not yet load plugin slash commands"));
}

#[test]
fn resume_usage_mentions_latest_shortcut() {
    let usage = render_resume_usage();
    assert!(usage.contains("/resume <session-path|session-id|latest>"));
    assert!(usage.contains(".claw/sessions/<workspace-fingerprint>/<session-id>.jsonl"));
    assert!(usage.contains("/session list"));
}

fn cwd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn cwd_guard() -> MutexGuard<'static, ()> {
    cwd_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn cwd_guard_recovers_after_poisoning() {
    let poisoned = std::thread::spawn(|| {
        let _guard = cwd_guard();
        panic!("poison cwd lock");
    })
    .join();
    assert!(poisoned.is_err(), "poisoning thread should panic");

    let _guard = cwd_guard();
}

fn temp_workspace(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("claw-cli-{label}-{nanos}"))
}

#[test]
fn init_template_mentions_detected_rust_workspace() {
    let _guard = cwd_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rendered = crate::init::render_init_claude_md(&workspace_root);
    assert!(rendered.contains("# CLAUDE.md"));
    assert!(rendered.contains("cargo clippy --workspace --all-targets -- -D warnings"));
}

#[test]
fn converts_tool_roundtrip_messages() {
    let messages = vec![
        ConversationMessage::user_text("hello"),
        ConversationMessage::assistant(vec![ContentBlock::ToolUse {
            id: "tool-1".to_string(),
            name: "bash".to_string(),
            input: "{\"command\":\"pwd\"}".to_string(),
        }]),
        ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                tool_name: "bash".to_string(),
                output: "ok".to_string(),
                is_error: false,
            }],
            usage: None,
        },
    ];

    let converted = super::convert_messages(&messages, "claude-sonnet-4-6");
    assert_eq!(converted.len(), 3);
    assert_eq!(converted[1].role, "assistant");
    assert_eq!(converted[2].role, "user");
}

#[test]
fn compact_tool_output_unwraps_web_search_json() {
    let web_search_json = serde_json::json!({
        "query": "rust async",
        "results": [
            "Search results for \"rust async\". Include a Sources section.\n- [Tokio](https://tokio.rs)\n- [async-std](https://async.rs)",
            {
                "tool_use_id": "web_search_1",
                "content": [
                    {"title": "Tokio", "url": "https://tokio.rs"},
                    {"title": "async-std", "url": "https://async.rs"}
                ]
            }
        ],
        "durationSeconds": 0.5
    })
    .to_string();

    let compacted = super::compact_tool_output_for_model("WebSearch", &web_search_json, false);
    assert!(
        compacted.contains("Tokio"),
        "should extract commentary containing search results, got: {compacted}"
    );
    assert!(
        !compacted.contains("durationSeconds"),
        "should strip JSON wrapper, got: {compacted}"
    );
    assert!(
        !compacted.contains("tool_use_id"),
        "should strip structured hit objects, got: {compacted}"
    );
}

#[test]
fn compact_tool_output_unwraps_web_fetch_json() {
    let web_fetch_json = serde_json::json!({
        "bytes": 1234,
        "code": 200,
        "code_text": "OK",
        "result": "This page is about Rust programming language features.",
        "duration_ms": 500,
        "url": "https://example.com/rust"
    })
    .to_string();

    let compacted = super::compact_tool_output_for_model("WebFetch", &web_fetch_json, false);
    assert_eq!(
        compacted,
        "This page is about Rust programming language features."
    );
}

#[test]
fn compact_tool_output_passes_through_unknown_tools() {
    let output = "some raw tool output";
    let compacted = super::compact_tool_output_for_model("Bash", output, false);
    assert_eq!(compacted, output);
}

#[test]
fn compact_tool_output_falls_back_on_invalid_json() {
    let output = "not valid json at all";
    let compacted = super::compact_tool_output_for_model("WebSearch", output, false);
    assert_eq!(compacted, output);
}

#[test]
fn compact_tool_output_preserves_edit_file_error() {
    // 当 edit_file 因为 old_string 不匹配等原因失败时，runtime 返回 is_error=true
    // 且 output 通常是一个 JSON 错误对象。我们绝不能把它包装成成功消息，也不能
    // 因为找不到 filePath 而把路径降级为 "unknown"。
    let error_output = serde_json::json!({
        "error": "old_string not found in file",
        "filePath": "D:\\claw-code-src\\test.txt"
    })
    .to_string();
    let compacted = super::compact_tool_output_for_model("edit_file", &error_output, true);
    assert!(
        compacted.contains("old_string not found in file"),
        "error message should be preserved, got: {compacted}"
    );
    assert!(
        !compacted.contains("has been updated successfully"),
        "should not claim success on error, got: {compacted}"
    );
    assert!(
        !compacted.contains("unknown"),
        "should not rewrite path to 'unknown', got: {compacted}"
    );
}

#[test]
fn repl_help_mentions_history_completion_and_multiline() {
    let help = render_repl_help();
    assert!(help.contains("↑/↓"));
    assert!(help.contains("Tab"));
    assert!(help.contains("Shift+Enter/Ctrl+J"));
    assert!(help.contains("Ctrl-R"));
    assert!(help.contains("反向搜索历史输入"));
    assert!(help.contains("/history [数量]"));
}

#[test]
fn parse_history_count_defaults_to_twenty_when_missing() {
    // given
    let raw: Option<&str> = None;

    // when
    let parsed = parse_history_count(raw);

    // then
    assert_eq!(parsed, Ok(20));
}

#[test]
fn parse_history_count_accepts_positive_integers() {
    // given
    let raw = Some("25");

    // when
    let parsed = parse_history_count(raw);

    // then
    assert_eq!(parsed, Ok(25));
}

#[test]
fn parse_history_count_rejects_zero() {
    // given
    let raw = Some("0");

    // when
    let parsed = parse_history_count(raw);

    // then
    assert!(parsed.is_err());
    assert!(parsed.unwrap_err().contains("greater than 0"));
}

#[test]
fn parse_history_count_rejects_non_numeric() {
    // given
    let raw = Some("abc");

    // when
    let parsed = parse_history_count(raw);

    // then
    assert!(parsed.is_err());
    assert!(parsed.unwrap_err().contains("invalid count 'abc'"));
}

#[test]
fn format_history_timestamp_renders_iso8601_utc() {
    // given
    // 2023-01-15T12:34:56.789Z -> 1673786096789 ms
    let timestamp_ms: u64 = 1_673_786_096_789;

    // when
    let formatted = format_history_timestamp(timestamp_ms);

    // then
    assert_eq!(formatted, "2023-01-15T12:34:56.789Z");
}

#[test]
fn format_history_timestamp_renders_unix_epoch_origin() {
    // given
    let timestamp_ms: u64 = 0;

    // when
    let formatted = format_history_timestamp(timestamp_ms);

    // then
    assert_eq!(formatted, "1970-01-01T00:00:00.000Z");
}

#[test]
fn render_prompt_history_report_lists_entries_with_timestamps() {
    // given
    let entries = vec![
        PromptHistoryEntry {
            timestamp_ms: 1_673_786_096_000,
            text: "first prompt".to_string(),
        },
        PromptHistoryEntry {
            timestamp_ms: 1_673_786_100_000,
            text: "second prompt".to_string(),
        },
    ];

    // when
    let rendered = render_prompt_history_report(&entries, 10);

    // then
    assert!(rendered.contains("Prompt history"));
    assert!(rendered.contains("Total            2"));
    assert!(rendered.contains("Showing          2 most recent"));
    assert!(rendered.contains("Reverse search   Ctrl-R in the REPL"));
    assert!(rendered.contains("2023-01-15T12:34:56.000Z"));
    assert!(rendered.contains("first prompt"));
    assert!(rendered.contains("second prompt"));
}

#[test]
fn render_prompt_history_report_truncates_to_limit_from_the_tail() {
    // given
    let entries = vec![
        PromptHistoryEntry {
            timestamp_ms: 1_000,
            text: "older".to_string(),
        },
        PromptHistoryEntry {
            timestamp_ms: 2_000,
            text: "middle".to_string(),
        },
        PromptHistoryEntry {
            timestamp_ms: 3_000,
            text: "latest".to_string(),
        },
    ];

    // when
    let rendered = render_prompt_history_report(&entries, 2);

    // then
    assert!(rendered.contains("Total            3"));
    assert!(rendered.contains("Showing          2 most recent"));
    assert!(!rendered.contains("older"));
    assert!(rendered.contains("middle"));
    assert!(rendered.contains("latest"));
}

#[test]
fn render_prompt_history_report_handles_empty_history() {
    // given
    let entries: Vec<PromptHistoryEntry> = Vec::new();

    // when
    let rendered = render_prompt_history_report(&entries, 10);

    // then
    assert!(rendered.contains("no prompts recorded yet"));
}

#[test]
fn collect_session_prompt_history_extracts_user_text_blocks() {
    // given
    let mut session = Session::new();
    session.push_user_text("hello").unwrap();
    session.push_user_text("world").unwrap();

    // when
    let entries = collect_session_prompt_history(&session);

    // then
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].text, "hello");
    assert_eq!(entries[1].text, "world");
}

#[test]
fn tool_rendering_helpers_compact_output() {
    let start = format_tool_call_start("read_file", r#"{"path":"src/main.rs"}"#);
    assert!(start.contains("read_file"));
    assert!(start.contains("src/main.rs"));

    let done = format_tool_result(
        "read_file",
        r#"{"file":{"filePath":"src/main.rs","content":"hello","numLines":1,"startLine":1,"totalLines":1}}"#,
        false,
        OutputVerbosity::Full,
    );
    assert!(done.contains("📄 Read src/main.rs"));
    assert!(done.contains("hello"));
}

#[test]
fn tool_rendering_truncates_large_read_output_for_display_only() {
    let content = (0..200)
        .map(|index| format!("line {index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = json!({
        "file": {
            "filePath": "src/main.rs",
            "content": content,
            "numLines": 200,
            "startLine": 1,
            "totalLines": 200
        }
    })
    .to_string();

    let rendered = format_tool_result("read_file", &output, false, OutputVerbosity::Full);

    assert!(rendered.contains("line 000"));
    assert!(rendered.contains("line 079"));
    assert!(!rendered.contains("line 199"));
    assert!(rendered.contains("full result preserved in session"));
    assert!(output.contains("line 199"));
}

#[test]
fn tool_rendering_truncates_large_bash_output_for_display_only() {
    let stdout = (0..120)
        .map(|index| format!("stdout {index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = json!({
        "stdout": stdout,
        "stderr": "",
        "returnCodeInterpretation": "completed successfully"
    })
    .to_string();

    let rendered = format_tool_result("bash", &output, false, OutputVerbosity::Full);

    assert!(rendered.contains("stdout 000"));
    assert!(rendered.contains("stdout 059"));
    assert!(!rendered.contains("stdout 119"));
    assert!(rendered.contains("full result preserved in session"));
    assert!(output.contains("stdout 119"));
}

#[test]
fn tool_rendering_truncates_generic_long_output_for_display_only() {
    let items = (0..120)
        .map(|index| format!("payload {index:03}"))
        .collect::<Vec<_>>();
    let output = json!({
        "summary": "plugin payload",
        "items": items,
    })
    .to_string();

    let rendered = format_tool_result("plugin_echo", &output, false, OutputVerbosity::Full);

    assert!(rendered.contains("plugin_echo"));
    assert!(rendered.contains("payload 000"));
    assert!(rendered.contains("payload 040"));
    assert!(!rendered.contains("payload 080"));
    assert!(!rendered.contains("payload 119"));
    assert!(rendered.contains("full result preserved in session"));
    assert!(output.contains("payload 119"));
}

#[test]
fn tool_rendering_truncates_raw_generic_output_for_display_only() {
    let output = (0..120)
        .map(|index| format!("raw {index:03}"))
        .collect::<Vec<_>>()
        .join("\n");

    let rendered = format_tool_result("plugin_echo", &output, false, OutputVerbosity::Full);

    assert!(rendered.contains("plugin_echo"));
    assert!(rendered.contains("raw 000"));
    assert!(rendered.contains("raw 059"));
    assert!(!rendered.contains("raw 119"));
    assert!(rendered.contains("full result preserved in session"));
    assert!(output.contains("raw 119"));
}

#[test]
fn ultraplan_progress_lines_include_phase_step_and_elapsed_status() {
    let snapshot = InternalPromptProgressState {
        command_label: "Ultraplan",
        task_label: "ship plugin progress".to_string(),
        step: 3,
        phase: "running read_file".to_string(),
        detail: Some("reading rust/crates/rusty-claude-cli/src/main.rs".to_string()),
        saw_final_text: false,
    };

    let started = format_internal_prompt_progress_line(
        InternalPromptProgressEvent::Started,
        &snapshot,
        Duration::from_secs(0),
        None,
    );
    let heartbeat = format_internal_prompt_progress_line(
        InternalPromptProgressEvent::Heartbeat,
        &snapshot,
        Duration::from_secs(9),
        None,
    );
    let completed = format_internal_prompt_progress_line(
        InternalPromptProgressEvent::Complete,
        &snapshot,
        Duration::from_secs(12),
        None,
    );
    let failed = format_internal_prompt_progress_line(
        InternalPromptProgressEvent::Failed,
        &snapshot,
        Duration::from_secs(12),
        Some("network timeout"),
    );

    assert!(started.contains("planning started"));
    assert!(started.contains("current step 3"));
    assert!(heartbeat.contains("heartbeat"));
    assert!(heartbeat.contains("9s elapsed"));
    assert!(heartbeat.contains("phase running read_file"));
    assert!(completed.contains("completed"));
    assert!(completed.contains("3 steps total"));
    assert!(failed.contains("failed"));
    assert!(failed.contains("network timeout"));
}

#[test]
fn describe_tool_progress_summarizes_known_tools() {
    assert_eq!(
        describe_tool_progress("read_file", r#"{"path":"src/main.rs"}"#),
        "reading src/main.rs"
    );
    assert!(
        describe_tool_progress("bash", r#"{"command":"cargo test -p rusty-claude-cli"}"#)
            .contains("cargo test -p rusty-claude-cli")
    );
    assert_eq!(
        describe_tool_progress("grep_search", r#"{"pattern":"ultraplan","path":"rust"}"#),
        "grep `ultraplan` in rust"
    );
}

#[test]
fn push_output_block_renders_markdown_text() {
    let mut out = Vec::new();
    let mut events = Vec::new();
    let mut pending_tool = None;
    let mut block_has_thinking_summary = false;
    let mut pending_thinking = None;

    push_output_block(
        OutputContentBlock::Text {
            text: "# Heading".to_string(),
        },
        &mut out,
        &mut events,
        &mut pending_tool,
        false,
        &mut block_has_thinking_summary,
        &mut pending_thinking,
    )
    .expect("text block should render");

    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("Heading"));
    assert!(rendered.contains('\u{1b}'));
}

#[test]
fn push_output_block_skips_empty_object_prefix_for_tool_streams() {
    let mut out = Vec::new();
    let mut events = Vec::new();
    let mut pending_tool = None;
    let mut block_has_thinking_summary = false;
    let mut pending_thinking = None;

    push_output_block(
        OutputContentBlock::ToolUse {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            input: json!({}),
        },
        &mut out,
        &mut events,
        &mut pending_tool,
        true,
        &mut block_has_thinking_summary,
        &mut pending_thinking,
    )
    .expect("tool block should accumulate");

    assert!(events.is_empty());
    assert_eq!(
        pending_tool,
        Some(("tool-1".to_string(), "read_file".to_string(), String::new(),))
    );
}

/// P0 修复回归测试：流式路径的 Thinking 块必须暂存到 pending_thinking，
/// 而不是直接 push 到 events。这是 DeepSeek thinking 模式不报 400 的关键 —
/// ContentBlockStop 时才会统一 emit AssistantEvent::Thinking，让 history
/// 里包含 reasoning_content 供下一轮请求回传。
#[test]
fn push_output_block_streaming_thinking_defers_to_pending() {
    let mut out = Vec::new();
    let mut events = Vec::new();
    let mut pending_tool = None;
    let mut block_has_thinking_summary = false;
    let mut pending_thinking = None;

    push_output_block(
        OutputContentBlock::Thinking {
            thinking: "initial".to_string(),
            signature: Some("sig_abc".to_string()),
        },
        &mut out,
        &mut events,
        &mut pending_tool,
        true, // streaming_tool_input=true：流式路径
        &mut block_has_thinking_summary,
        &mut pending_thinking,
    )
    .expect("thinking block should accumulate");

    // 流式路径下 events 必须为空（待 ContentBlockStop 时 emit）
    assert!(events.is_empty(), "streaming thinking must defer emit");
    // pending_thinking 必须暂存 thinking 内容与 signature
    assert_eq!(
        pending_thinking,
        Some(("initial".to_string(), Some("sig_abc".to_string())))
    );
    // 渲染摘要必须正常输出
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("▶ Thinking"));
}

/// P0 修复回归测试：非流式路径的 Thinking 块必须直接 emit 到 events。
/// 这条路径是 fallback（流式未收到 stop 时回退到 send_message），
/// 必须保持原有行为不变。
#[test]
fn push_output_block_nonstreaming_thinking_emits_directly() {
    let mut out = Vec::new();
    let mut events = Vec::new();
    let mut pending_tool = None;
    let mut block_has_thinking_summary = false;
    let mut pending_thinking = None;

    push_output_block(
        OutputContentBlock::Thinking {
            thinking: "step 1".to_string(),
            signature: Some("sig_xyz".to_string()),
        },
        &mut out,
        &mut events,
        &mut pending_tool,
        false, // streaming_tool_input=false：非流式回退路径
        &mut block_has_thinking_summary,
        &mut pending_thinking,
    )
    .expect("non-streaming thinking should emit directly");

    // 非流式路径必须直接 push 事件
    assert_eq!(events.len(), 1, "non-streaming thinking must emit directly");
    assert!(matches!(
        &events[0],
        AssistantEvent::Thinking { thinking, signature }
            if thinking == "step 1" && signature.as_deref() == Some("sig_xyz")
    ));
    // pending_thinking 必须保持空
    assert!(pending_thinking.is_none(), "non-streaming must not populate pending");
}

#[test]
fn response_to_events_preserves_empty_object_json_input_outside_streaming() {
    let mut out = Vec::new();
    let events = response_to_events(
        MessageResponse {
            id: "msg-1".to_string(),
            kind: "message".to_string(),
            model: "claude-opus-4-6".to_string(),
            role: "assistant".to_string(),
            content: vec![OutputContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                input: json!({}),
            }],
            stop_reason: Some("tool_use".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            request_id: None,
        },
        &mut out,
    )
    .expect("response conversion should succeed");

    assert!(matches!(
        &events[0],
        AssistantEvent::ToolUse { name, input, .. }
            if name == "read_file" && input == "{}"
    ));
}

#[test]
fn response_to_events_preserves_non_empty_json_input_outside_streaming() {
    let mut out = Vec::new();
    let events = response_to_events(
        MessageResponse {
            id: "msg-2".to_string(),
            kind: "message".to_string(),
            model: "claude-opus-4-6".to_string(),
            role: "assistant".to_string(),
            content: vec![OutputContentBlock::ToolUse {
                id: "tool-2".to_string(),
                name: "read_file".to_string(),
                input: json!({ "path": "rust/Cargo.toml" }),
            }],
            stop_reason: Some("tool_use".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            request_id: None,
        },
        &mut out,
    )
    .expect("response conversion should succeed");

    assert!(matches!(
        &events[0],
        AssistantEvent::ToolUse { name, input, .. }
            if name == "read_file" && input == "{\"path\":\"rust/Cargo.toml\"}"
    ));
}

#[test]
fn response_to_events_renders_collapsed_thinking_summary() {
    let mut out = Vec::new();
    let events = response_to_events(
        MessageResponse {
            id: "msg-3".to_string(),
            kind: "message".to_string(),
            model: "claude-opus-4-6".to_string(),
            role: "assistant".to_string(),
            content: vec![
                OutputContentBlock::Thinking {
                    thinking: "step 1".to_string(),
                    signature: Some("sig_123".to_string()),
                },
                OutputContentBlock::Text {
                    text: "Final answer".to_string(),
                },
            ],
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            request_id: None,
        },
        &mut out,
    )
    .expect("response conversion should succeed");

    // G10.5 fix: Thinking now emitted before Text in non-streaming path
    assert!(matches!(
        &events[0],
        AssistantEvent::Thinking { thinking, .. } if thinking == "step 1"
    ));
    assert!(matches!(
        &events[1],
        AssistantEvent::TextDelta(text) if text == "Final answer"
    ));
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("▶ Thinking (6 chars hidden)"));
    assert!(!rendered.contains("step 1"));
}

#[test]
fn build_runtime_plugin_state_merges_plugin_hooks_into_runtime_features() {
    let config_home = temp_dir();
    let workspace = temp_dir();
    let source_root = temp_dir();
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&source_root).expect("source root");
    write_plugin_fixture(&source_root, "hook-runtime-demo", true, false);

    let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
    manager
        .install(source_root.to_str().expect("utf8 source path"))
        .expect("plugin install should succeed");
    let loader = ConfigLoader::new(&workspace, &config_home);
    let runtime_config = loader.load().expect("runtime config should load");
    let state = build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
        .expect("plugin state should load");
    let pre_hooks = state.feature_config.hooks().pre_tool_use();
    assert_eq!(pre_hooks.len(), 1);
    assert!(
        pre_hooks[0].value.ends_with("hooks/pre.sh"),
        "expected installed plugin hook path, got {pre_hooks:?}"
    );

    let _ = fs::remove_dir_all(config_home);
    let _ = fs::remove_dir_all(workspace);
    let _ = fs::remove_dir_all(source_root);
}

// Windows-incompatible: fixture uses `python3` shebang which is not a standard
// alias on Windows (Python is installed as python.exe / py.exe). Unix-only.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn build_runtime_plugin_state_discovers_mcp_tools_and_surfaces_pending_servers() {
    let config_home = temp_dir();
    let workspace = temp_dir();
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&workspace).expect("workspace");
    let script_path = workspace.join("fixture-mcp.py");
    write_mcp_server_fixture(&script_path);
    fs::write(
        config_home.join("settings.json"),
        format!(
            r#"{{
              "mcpServers": {{
                "alpha": {{
                  "command": "python3",
                  "args": ["{}"]
                }},
                "broken": {{
                  "command": "python3",
                  "args": ["-c", "import sys; sys.exit(0)"]
                }}
              }}
            }}"#,
            script_path.to_string_lossy()
        ),
    )
    .expect("write mcp settings");

    let loader = ConfigLoader::new(&workspace, &config_home);
    let runtime_config = loader.load().expect("runtime config should load");
    let state = build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
        .expect("runtime plugin state should load");

    let allowed = state
        .tool_registry
        .normalize_allowed_tools(&["mcp__alpha__echo".to_string(), "MCPTool".to_string()])
        .expect("mcp tools should be allow-listable")
        .expect("allow-list should exist");
    assert!(allowed.contains("mcp__alpha__echo"));
    assert!(allowed.contains("MCPTool"));

    let mut executor = CliToolExecutor::new(
        None,
        false,
        state.tool_registry.clone(),
        state.mcp_state.clone(),
    );

    let tool_output = executor
        .execute("mcp__alpha__echo", r#"{"text":"hello"}"#)
        .expect("discovered mcp tool should execute");
    let tool_json: serde_json::Value =
        serde_json::from_str(&tool_output).expect("tool output should be json");
    assert_eq!(tool_json["structuredContent"]["echoed"], "hello");

    let wrapped_output = executor
        .execute(
            "MCPTool",
            r#"{"qualifiedName":"mcp__alpha__echo","arguments":{"text":"wrapped"}}"#,
        )
        .expect("generic mcp wrapper should execute");
    let wrapped_json: serde_json::Value =
        serde_json::from_str(&wrapped_output).expect("wrapped output should be json");
    assert_eq!(wrapped_json["structuredContent"]["echoed"], "wrapped");

    let search_output = executor
        .execute("ToolSearch", r#"{"query":"alpha echo","max_results":5}"#)
        .expect("tool search should execute");
    let search_json: serde_json::Value =
        serde_json::from_str(&search_output).expect("search output should be json");
    assert_eq!(search_json["matches"][0], "mcp__alpha__echo");
    assert_eq!(search_json["pending_mcp_servers"][0], "broken");
    assert_eq!(
        search_json["mcp_degraded"]["failed_servers"][0]["server_name"],
        "broken"
    );
    assert_eq!(
        search_json["mcp_degraded"]["failed_servers"][0]["phase"],
        "tool_discovery"
    );
    assert_eq!(
        search_json["mcp_degraded"]["available_tools"][0],
        "mcp__alpha__echo"
    );

    let listed = executor
        .execute("ListMcpResourcesTool", r#"{"server":"alpha"}"#)
        .expect("resources should list");
    let listed_json: serde_json::Value =
        serde_json::from_str(&listed).expect("resource output should be json");
    assert_eq!(listed_json["resources"][0]["uri"], "file://guide.txt");

    let read = executor
        .execute(
            "ReadMcpResourceTool",
            r#"{"server":"alpha","uri":"file://guide.txt"}"#,
        )
        .expect("resource should read");
    let read_json: serde_json::Value =
        serde_json::from_str(&read).expect("resource read output should be json");
    assert_eq!(
        read_json["contents"][0]["text"],
        "contents for file://guide.txt"
    );

    if let Some(mcp_state) = state.mcp_state {
        mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown()
            .expect("mcp shutdown should succeed");
    }

    let _ = fs::remove_dir_all(config_home);
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn build_runtime_plugin_state_surfaces_unsupported_mcp_servers_structurally() {
    let config_home = temp_dir();
    let workspace = temp_dir();
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(
        config_home.join("settings.json"),
        r#"{
          "mcpServers": {
            "remote": {
              "url": "https://example.test/mcp"
            }
          }
        }"#,
    )
    .expect("write mcp settings");

    let loader = ConfigLoader::new(&workspace, &config_home);
    let runtime_config = loader.load().expect("runtime config should load");
    let state = build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
        .expect("runtime plugin state should load");
    let mut executor = CliToolExecutor::new(
        None,
        false,
        state.tool_registry.clone(),
        state.mcp_state.clone(),
    );

    let search_output = executor
        .execute("ToolSearch", r#"{"query":"remote","max_results":5}"#)
        .expect("tool search should execute");
    let search_json: serde_json::Value =
        serde_json::from_str(&search_output).expect("search output should be json");
    assert_eq!(search_json["pending_mcp_servers"][0], "remote");
    assert_eq!(
        search_json["mcp_degraded"]["failed_servers"][0]["server_name"],
        "remote"
    );
    assert_eq!(
        search_json["mcp_degraded"]["failed_servers"][0]["phase"],
        "server_registration"
    );
    assert_eq!(
        search_json["mcp_degraded"]["failed_servers"][0]["error"]["context"]["transport"],
        "http"
    );

    let _ = fs::remove_dir_all(config_home);
    let _ = fs::remove_dir_all(workspace);
}

// Windows-incompatible: lifecycle fixtures are .sh scripts (`#!/bin/sh`),
// which Windows cannot natively execute. Unix-only.
#[cfg(unix)]
#[test]
fn build_runtime_runs_plugin_lifecycle_init_and_shutdown() {
    // Serialize access to process-wide env vars so parallel tests that
    // set/remove ANTHROPIC_API_KEY do not race with this test.
    let _guard = env_lock();
    let config_home = temp_dir();
    // Inject a dummy API key so runtime construction succeeds without real credentials.
    // This test only exercises plugin lifecycle (init/shutdown), never calls the API.
    std::env::set_var("ANTHROPIC_API_KEY", "test-dummy-key-for-plugin-lifecycle");
    let workspace = temp_dir();
    let source_root = temp_dir();
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&source_root).expect("source root");
    write_plugin_fixture(&source_root, "lifecycle-runtime-demo", false, true);

    let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
    let install = manager
        .install(source_root.to_str().expect("utf8 source path"))
        .expect("plugin install should succeed");
    let log_path = install.install_path.join("lifecycle.log");
    let loader = ConfigLoader::new(&workspace, &config_home);
    let runtime_config = loader.load().expect("runtime config should load");
    let runtime_plugin_state =
        build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
            .expect("plugin state should load");
    let mut runtime = build_runtime_with_plugin_state(
        Session::new(),
        "runtime-plugin-lifecycle",
        DEFAULT_MODEL.to_string(),
        vec!["test system prompt".to_string()],
        true,
        false,
        None,
        PermissionMode::DangerFullAccess,
        None,
        runtime_plugin_state,
    )
    .expect("runtime should build");

    assert_eq!(
        fs::read_to_string(&log_path).expect("init log should exist"),
        "init\n"
    );

    runtime
        .shutdown_plugins()
        .expect("plugin shutdown should succeed");

    assert_eq!(
        fs::read_to_string(&log_path).expect("shutdown log should exist"),
        "init\nshutdown\n"
    );

    let _ = fs::remove_dir_all(config_home);
    let _ = fs::remove_dir_all(workspace);
    let _ = fs::remove_dir_all(source_root);
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[test]
fn rejects_invalid_reasoning_effort_value() {
    let err = parse_args(&[
        "--reasoning-effort".to_string(),
        "turbo".to_string(),
        "prompt".to_string(),
        "hello".to_string(),
    ])
    .unwrap_err();
    assert!(
        err.contains("invalid value for --reasoning-effort"),
        "unexpected error: {err}"
    );
    assert!(err.contains("turbo"), "unexpected error: {err}");
}

#[test]
fn accepts_valid_reasoning_effort_values() {
    for value in ["low", "medium", "high"] {
        let result = parse_args(&[
            "--reasoning-effort".to_string(),
            value.to_string(),
            "prompt".to_string(),
            "hello".to_string(),
        ]);
        assert!(
            result.is_ok(),
            "--reasoning-effort {value} should be accepted, got: {result:?}"
        );
        if let Ok(CliAction::Prompt {
            reasoning_effort, ..
        }) = result
        {
            assert_eq!(reasoning_effort.as_deref(), Some(value));
        }
    }
}

#[test]
fn stub_commands_absent_from_repl_completions() {
    let candidates =
        slash_command_completion_candidates_with_sessions("claude-3-5-sonnet", None, vec![]);
    for stub in STUB_COMMANDS {
        let with_slash = format!("/{stub}");
        assert!(
            !candidates.contains(&with_slash),
            "stub command {with_slash} should not appear in REPL completions"
        );
    }
}

#[test]
fn stub_commands_absent_from_resume_safe_help() {
    let mut help = Vec::new();
    print_help_to(&mut help).expect("help should render");
    let help = String::from_utf8(help).expect("help should be utf8");
    let resume_line = help
        .lines()
        .find(|line| line.starts_with("Resume-safe commands:"))
        .expect("resume-safe command line should exist");
    let resume_roots = resume_line
        .trim_start_matches("Resume-safe commands:")
        .split(',')
        .filter_map(|entry| entry.trim().strip_prefix('/'))
        .filter_map(|entry| entry.split_whitespace().next())
        .collect::<Vec<_>>();

    for stub in STUB_COMMANDS {
        assert!(
            !resume_roots.contains(stub),
            "stub command /{stub} should not appear in resume-safe command list"
        );
    }

    assert!(resume_roots.contains(&"status"));
}

#[cfg(unix)]
fn write_mcp_server_fixture(script_path: &Path) {
    let script = [
            "#!/usr/bin/env python3",
            "import json, sys",
            "",
            "def read_message():",
            "    header = b''",
            r"    while not header.endswith(b'\r\n\r\n'):",
            "        chunk = sys.stdin.buffer.read(1)",
            "        if not chunk:",
            "            return None",
            "        header += chunk",
            "    length = 0",
            r"    for line in header.decode().split('\r\n'):",
            r"        if line.lower().startswith('content-length:'):",
            "            length = int(line.split(':', 1)[1].strip())",
            "    payload = sys.stdin.buffer.read(length)",
            "    return json.loads(payload.decode())",
            "",
            "def send_message(message):",
            "    payload = json.dumps(message).encode()",
            r"    sys.stdout.buffer.write(f'Content-Length: {len(payload)}\r\n\r\n'.encode() + payload)",
            "    sys.stdout.buffer.flush()",
            "",
            "while True:",
            "    request = read_message()",
            "    if request is None:",
            "        break",
            "    method = request['method']",
            "    if method == 'initialize':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'protocolVersion': request['params']['protocolVersion'],",
            "                'capabilities': {'tools': {}, 'resources': {}},",
            "                'serverInfo': {'name': 'fixture', 'version': '1.0.0'}",
            "            }",
            "        })",
            "    elif method == 'tools/list':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'tools': [",
            "                    {",
            "                        'name': 'echo',",
            "                        'description': 'Echo from MCP fixture',",
            "                        'inputSchema': {",
            "                            'type': 'object',",
            "                            'properties': {'text': {'type': 'string'}},",
            "                            'required': ['text'],",
            "                            'additionalProperties': False",
            "                        },",
            "                        'annotations': {'readOnlyHint': True}",
            "                    }",
            "                ]",
            "            }",
            "        })",
            "    elif method == 'tools/call':",
            "        args = request['params'].get('arguments') or {}",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'content': [{'type': 'text', 'text': f\"echo:{args.get('text', '')}\"}],",
            "                'structuredContent': {'echoed': args.get('text', '')},",
            "                'isError': False",
            "            }",
            "        })",
            "    elif method == 'resources/list':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'resources': [{'uri': 'file://guide.txt', 'name': 'guide', 'mimeType': 'text/plain'}]",
            "            }",
            "        })",
            "    elif method == 'resources/read':",
            "        uri = request['params']['uri']",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'contents': [{'uri': uri, 'mimeType': 'text/plain', 'text': f'contents for {uri}'}]",
            "            }",
            "        })",
            "    else:",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'error': {'code': -32601, 'message': method}",
            "        })",
            "",
        ]
        .join("\n");
    fs::write(script_path, script).expect("mcp fixture script should write");
}

#[cfg(test)]
mod sandbox_report_tests {
    use crate::{format_sandbox_report, HookAbortMonitor};
    use runtime::HookAbortSignal;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn sandbox_report_renders_expected_fields() {
        let report = format_sandbox_report(&runtime::SandboxStatus::default());
        assert!(report.contains("Sandbox"));
        assert!(report.contains("已启用"));
        assert!(report.contains("文件系统模式"));
        assert!(report.contains("降级原因"));
    }

    #[test]
    fn hook_abort_monitor_stops_without_aborting() {
        let abort_signal = HookAbortSignal::new();
        let (ready_tx, ready_rx) = mpsc::channel();
        let monitor = HookAbortMonitor::spawn_with_waiter(
            abort_signal.clone(),
            move |stop_rx, abort_signal| {
                ready_tx.send(()).expect("ready signal");
                let _ = stop_rx.recv();
                assert!(!abort_signal.is_aborted());
            },
        );

        ready_rx.recv().expect("waiter should be ready");
        monitor.stop();

        assert!(!abort_signal.is_aborted());
    }

    #[test]
    fn hook_abort_monitor_propagates_interrupt() {
        let abort_signal = HookAbortSignal::new();
        let (done_tx, done_rx) = mpsc::channel();
        let monitor = HookAbortMonitor::spawn_with_waiter(
            abort_signal.clone(),
            move |_stop_rx, abort_signal| {
                abort_signal.abort();
                done_tx.send(()).expect("done signal");
            },
        );

        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("interrupt should complete");
        monitor.stop();

        assert!(abort_signal.is_aborted());
    }
}

#[cfg(test)]
mod dump_manifests_tests {
    use crate::{dump_manifests_at_path, CliOutputFormat};
    use std::fs;

    #[test]
    fn dump_manifests_shows_helpful_error_when_manifests_missing() {
        let root = std::env::temp_dir().join(format!(
            "claw_test_missing_manifests_{}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("failed to create temp workspace");

        let result = dump_manifests_at_path(&workspace, None, CliOutputFormat::Text);
        assert!(
            result.is_err(),
            "expected an error when manifests are missing"
        );

        let error_msg = result.unwrap_err().to_string();

        assert!(
            error_msg.contains("Manifest source files are missing"),
            "error message should mention missing manifest sources: {error_msg}"
        );
        assert!(
            error_msg.contains(&root.display().to_string()),
            "error message should contain the resolved repo root path: {error_msg}"
        );
        assert!(
            error_msg.contains("src/commands.ts"),
            "error message should mention missing commands.ts: {error_msg}"
        );
        assert!(
            error_msg.contains("CLAUDE_CODE_UPSTREAM"),
            "error message should explain how to supply the upstream path: {error_msg}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dump_manifests_uses_explicit_manifest_dir() {
        let root = std::env::temp_dir().join(format!(
            "claw_test_explicit_manifest_dir_{}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let upstream = root.join("upstream");
        fs::create_dir_all(workspace.join("nested")).expect("workspace should exist");
        fs::create_dir_all(upstream.join("src/entrypoints"))
            .expect("upstream fixture should exist");
        fs::write(
            upstream.join("src/commands.ts"),
            "import FooCommand from './commands/foo'\n",
        )
        .expect("commands fixture should write");
        fs::write(
            upstream.join("src/tools.ts"),
            "import ReadTool from './tools/read'\n",
        )
        .expect("tools fixture should write");
        fs::write(
            upstream.join("src/entrypoints/cli.tsx"),
            "startupProfiler()\n",
        )
        .expect("cli fixture should write");

        let result = dump_manifests_at_path(&workspace, Some(&upstream), CliOutputFormat::Text);
        assert!(
            result.is_ok(),
            "explicit manifest dir should succeed: {result:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod system_block_tests {
    use crate::build_system_blocks;
    use api::SystemContent;
    use runtime::SystemPromptSplit;

    #[test]
    fn build_system_blocks_marks_last_static_with_cache_control() {
        let split = SystemPromptSplit {
            static_sections: vec!["static1".to_string(), "static2".to_string()],
            dynamic_sections: vec!["dynamic1".to_string()],
        };
        let content = build_system_blocks(&split).expect("non-empty");
        match content {
            SystemContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 3);
                // First static: no cache_control
                assert!(blocks[0].cache_control.is_none());
                // Last static: has cache_control
                let cc = blocks[1]
                    .cache_control
                    .as_ref()
                    .expect("last static has cache_control");
                assert_eq!(cc.cache_type, "ephemeral");
                // Dynamic: no cache_control
                assert!(blocks[2].cache_control.is_none());
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn build_system_blocks_returns_none_for_empty_split() {
        let split = SystemPromptSplit {
            static_sections: Vec::new(),
            dynamic_sections: Vec::new(),
        };
        assert!(build_system_blocks(&split).is_none());
    }

    #[test]
    fn build_system_blocks_handles_static_only() {
        let split = SystemPromptSplit {
            static_sections: vec!["only".to_string()],
            dynamic_sections: Vec::new(),
        };
        let content = build_system_blocks(&split).expect("non-empty");
        match content {
            SystemContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(blocks[0].cache_control.is_some());
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn build_system_blocks_handles_dynamic_only() {
        let split = SystemPromptSplit {
            static_sections: Vec::new(),
            dynamic_sections: vec!["dyn".to_string()],
        };
        let content = build_system_blocks(&split).expect("non-empty");
        match content {
            SystemContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(blocks[0].cache_control.is_none());
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn build_system_blocks_serializes_with_cache_control_in_json() {
        // Verify the wire format: the last static block should carry
        // "cache_control": {"type": "ephemeral"} when serialized to JSON.
        let split = SystemPromptSplit {
            static_sections: vec!["stable".to_string()],
            dynamic_sections: vec!["volatile".to_string()],
        };
        let content = build_system_blocks(&split).expect("non-empty");
        let json = serde_json::to_string(&content).expect("serialize");
        assert!(
            json.contains(r#""cache_control":{"type":"ephemeral"}"#),
            "JSON should contain cache_control marker: {json}"
        );
        // The static block (index 0) should be the one with cache_control,
        // appearing before the dynamic block's text.
        let cc_pos = json
            .find(r#""cache_control"#)
            .expect("cache_control position");
        let dyn_pos = json.find("volatile").expect("volatile position");
        assert!(
            cc_pos < dyn_pos,
            "cache_control should precede dynamic content"
        );
    }

    #[test]
    fn build_system_blocks_tiered_cache_breakpoints() {
        // Verify that tiered cache breakpoints are applied: sections at
        // instruction/snapshot/config tier boundaries get cache_control.
        let split = SystemPromptSplit {
            static_sections: vec![
                "# Intro".to_string(),
                "# System".to_string(),
                "# Persistent Memory".to_string(),
                "## Repository Map".to_string(),
                "# Environment context".to_string(),
                "# Runtime config".to_string(),
            ],
            dynamic_sections: vec!["dynamic".to_string()],
        };
        let content = build_system_blocks(&split).expect("non-empty");
        match content {
            SystemContent::Blocks(blocks) => {
                // 6 static + 1 dynamic = 7 blocks
                assert_eq!(blocks.len(), 7);
                // BP1 at index 1 (end of instructions)
                assert!(
                    blocks[1].cache_control.is_some(),
                    "instruction tier boundary should have cache_control"
                );
                // BP2 at index 3 (end of snapshot tier)
                assert!(
                    blocks[3].cache_control.is_some(),
                    "snapshot tier boundary should have cache_control"
                );
                // BP3 at index 5 (last static — config tier)
                assert!(
                    blocks[5].cache_control.is_some(),
                    "last static block should have cache_control"
                );
                // Non-breakpoint static blocks: no cache_control
                assert!(blocks[0].cache_control.is_none());
                assert!(blocks[2].cache_control.is_none());
                assert!(blocks[4].cache_control.is_none());
                // Dynamic block: no cache_control
                assert!(blocks[6].cache_control.is_none());
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tool_cache_tests {
    use crate::mark_last_tool_with_cache_control;
    use api::{CacheControl, ToolDefinition};

    #[test]
    fn marks_last_tool_with_cache_control() {
        let mut tools = vec![
            ToolDefinition {
                name: "tool_a".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                cache_control: None,
            },
            ToolDefinition {
                name: "tool_b".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                cache_control: None,
            },
        ];
        mark_last_tool_with_cache_control(&mut tools);
        assert!(
            tools[0].cache_control.is_none(),
            "first tool must not be marked"
        );
        let cc = tools[1]
            .cache_control
            .as_ref()
            .expect("last tool must be marked");
        assert_eq!(cc.cache_type, "ephemeral");
    }

    #[test]
    fn handles_empty_tool_list() {
        let mut tools: Vec<ToolDefinition> = Vec::new();
        // Should not panic
        mark_last_tool_with_cache_control(&mut tools);
        assert!(tools.is_empty());
    }

    #[test]
    fn handles_single_tool() {
        let mut tools = vec![ToolDefinition {
            name: "only".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,
        }];
        mark_last_tool_with_cache_control(&mut tools);
        assert!(tools[0].cache_control.is_some());
    }

    #[test]
    fn overwrites_existing_cache_control_on_last_tool() {
        let mut tools = vec![ToolDefinition {
            name: "tool".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral()),
        }];
        // Idempotent — calling again should still leave it marked
        mark_last_tool_with_cache_control(&mut tools);
        assert!(tools[0].cache_control.is_some());
    }
}

#[cfg(test)]
mod system_extraction_tests {
    use crate::{convert_messages, extract_system_messages};
    use runtime::{ContentBlock, ConversationMessage, MessageRole};

    fn system_text(text: &str) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::System,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            usage: None,
        }
    }

    fn user_text(text: &str) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            usage: None,
        }
    }

    fn assistant_text(text: &str) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            usage: None,
        }
    }

    #[test]
    fn extracts_system_messages_and_returns_filtered_rest() {
        let messages = vec![
            system_text("system rule 1"),
            user_text("hello"),
            system_text("system rule 2"),
            assistant_text("hi"),
        ];
        let (system_text, rest) = extract_system_messages(&messages);
        assert!(system_text.contains("system rule 1"));
        assert!(system_text.contains("system rule 2"));
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].role, MessageRole::User);
        assert_eq!(rest[1].role, MessageRole::Assistant);
    }

    #[test]
    fn returns_empty_string_when_no_system_messages() {
        let messages = vec![user_text("hi"), assistant_text("hello")];
        let (system_text, rest) = extract_system_messages(&messages);
        assert!(system_text.is_empty());
        assert_eq!(rest.len(), 2);
    }

    #[test]
    fn handles_empty_input() {
        let messages: Vec<ConversationMessage> = Vec::new();
        let (system_text, rest) = extract_system_messages(&messages);
        assert!(system_text.is_empty());
        assert!(rest.is_empty());
    }

    #[test]
    fn preserves_order_of_non_system_messages() {
        let messages = vec![
            system_text("s1"),
            user_text("u1"),
            system_text("s2"),
            user_text("u2"),
            assistant_text("a1"),
        ];
        let (_, rest) = extract_system_messages(&messages);
        assert_eq!(rest.len(), 3);
        assert_eq!(rest[0].role, MessageRole::User);
        assert_eq!(rest[1].role, MessageRole::User);
        assert_eq!(rest[2].role, MessageRole::Assistant);
        // Verify text content preserved
        match &rest[0].blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "u1"),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn system_text_concatenates_multiple_system_messages() {
        let messages = vec![
            system_text("first rule"),
            user_text("hi"),
            system_text("second rule"),
        ];
        let (system_text, _) = extract_system_messages(&messages);
        // Both rules should be present, joined by some separator
        assert!(system_text.contains("first rule"));
        assert!(system_text.contains("second rule"));
    }

    #[test]
    fn extracted_messages_convert_without_system_role() {
        // After extraction, convert_messages should produce no "system" role entries.
        let messages = vec![system_text("s1"), user_text("u1"), assistant_text("a1")];
        let (_, rest) = extract_system_messages(&messages);
        let converted = convert_messages(&rest, "claude-sonnet-4-6");
        // All converted messages should have role "user" or "assistant", never "system"
        for msg in &converted {
            assert!(
                msg.role == "user" || msg.role == "assistant",
                "unexpected role: {}",
                msg.role
            );
        }
    }
}
