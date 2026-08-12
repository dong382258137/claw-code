//! Session CRUD, history, export, resume.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use commands::{
    classify_skills_slash_command, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command, handle_mcp_slash_command_json, handle_skills_slash_command,
    handle_skills_slash_command_json, slash_command_specs, SkillSlashDispatch, SlashCommand,
};
use runtime::{
    build_embedding_provider, format_usd, resolve_sandbox_status, CompactionConfig, ConfigLoader,
    ContentBlock, HistoryIndex, MessageRole, Session, SessionStore, TokenUsage, UsageTracker,
};
use serde_json::{json, Value};

use crate::plugin_state::plugins_command_payload_for;
use crate::tool_display::{short_tool_id, truncate_for_summary};
use crate::{
    classify_error_kind, classify_session_lifecycle_for, default_permission_mode,
    format_compact_report, format_cost_report, format_sandbox_report, format_status_report,
    format_unknown_slash_command, handle_bg_command, handle_bus_command, handle_goal_command,
    handle_im_command, handle_poor_mode_action, init_json_value, render_config_json, render_config_report,
    render_diff_json_for, render_diff_report_for, render_doctor_report, render_export_text,
    render_memory_json, render_memory_report, render_repl_help, render_version_report,
    sandbox_json_value, split_error_hint, status_context, status_json_value, version_json_value,
    CliOutputFormat, StatusUsage, STUB_COMMANDS,
};

pub(crate) const PRIMARY_SESSION_EXTENSION: &str = "jsonl";
pub(crate) const LEGACY_SESSION_EXTENSION: &str = "json";
pub(crate) const LATEST_SESSION_REFERENCE: &str = "latest";
pub(crate) const SESSION_REFERENCE_ALIASES: &[&str] = &[LATEST_SESSION_REFERENCE, "last", "recent"];
pub(crate) const DEFAULT_HISTORY_LIMIT: usize = 20;
pub(crate) const SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT: usize = 280;

#[derive(Debug, Clone)]
pub(crate) struct ResumeCommandOutcome {
    pub(crate) session: Session,
    pub(crate) message: Option<String>,
    pub(crate) json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLifecycleKind {
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
pub(crate) struct SessionLifecycleSummary {
    pub(crate) kind: SessionLifecycleKind,
    pub(crate) pane_id: Option<String>,
    pub(crate) pane_command: Option<String>,
    pub(crate) pane_path: Option<PathBuf>,
    pub(crate) workspace_dirty: bool,
    pub(crate) abandoned: bool,
}

impl SessionLifecycleSummary {
    pub(crate) fn signal(&self) -> String {
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

    pub(crate) fn json_value(&self) -> serde_json::Value {
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

#[derive(Debug, Clone)]
pub(crate) struct SessionHandle {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedSessionSummary {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) updated_at_ms: u64,
    pub(crate) modified_epoch_millis: u128,
    pub(crate) message_count: usize,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) branch_name: Option<String>,
    pub(crate) lifecycle: SessionLifecycleSummary,
}

#[derive(Debug, Clone)]
pub(crate) struct PromptHistoryEntry {
    pub(crate) timestamp_ms: u64,
    pub(crate) text: String,
}

pub(crate) fn resume_command_can_absorb_token(current_command: &str, token: &str) -> bool {
    matches!(
        SlashCommand::parse(current_command),
        Ok(Some(SlashCommand::Export { path: None }))
    ) && !looks_like_slash_command_token(token)
}

pub(crate) fn looks_like_slash_command_token(token: &str) -> bool {
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

#[allow(clippy::too_many_lines)]
pub(crate) fn resume_session(
    session_path: &Path,
    commands: &[String],
    output_format: CliOutputFormat,
) {
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

#[allow(clippy::too_many_lines)]
pub(crate) fn run_resume_command(
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
            let report = crate::init::initialize_repo(&cwd, false)?;
            let message = report.render();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message.clone()),
                json: Some(init_json_value(&report, &message)),
            })
        }
        SlashCommand::InitForce => {
            // `/init-force`: 覆盖现有 CLAUDE.md（等价于 `claw init --force`）。
            let cwd = env::current_dir()?;
            let report = crate::init::initialize_repo(&cwd, true)?;
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
        SlashCommand::Search { query } => {
            let q = query.as_deref().unwrap_or("");
            let results = search_session_history(session, q);
            let message = if results.is_empty() {
                format!("Search\n  Query           {q}\n  Result           no matches found")
            } else {
                let mut msg = format!(
                    "Search\n  Query           {q}\n  Matches          {}\n\n",
                    results.len()
                );
                for (i, (idx, preview)) in results.iter().take(20).enumerate() {
                    msg.push_str(&format!("  {}. [msg {idx}] {preview}\n", i + 1));
                }
                if results.len() > 20 {
                    msg.push_str(&format!(
                        "\n  ... and {} more matches\n",
                        results.len() - 20
                    ));
                }
                msg
            };
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: None,
            })
        }
        SlashCommand::Undo => {
            // Undo not supported in resume (non-interactive) mode
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some("Undo\n  Result           not available in resumed mode\n  Detail           start an interactive session to use /undo".to_string()),
                json: None,
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
        SlashCommand::Summary => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_session_summary_text(session)),
            json: Some(serde_json::json!({
                "kind": "summary",
                "messages": session.messages.len(),
            })),
        }),
        SlashCommand::Context { .. } => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_context_report(session)),
            json: Some(serde_json::json!({
                "kind": "context",
                "messages": session.messages.len(),
                "estimated_tokens": runtime::compact::estimate_session_tokens(session),
            })),
        }),
        SlashCommand::Usage { .. } => {
            let tracker = UsageTracker::from_session(session);
            let cum = tracker.cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_usage_report(
                    None,
                    cum,
                    tracker.current_turn_usage(),
                    tracker.turns(),
                )),
                json: Some(serde_json::json!({
                    "kind": "usage",
                    "turns": tracker.turns(),
                    "cumulative": {
                        "input_tokens": cum.input_tokens,
                        "output_tokens": cum.output_tokens,
                        "cache_creation_input_tokens": cum.cache_creation_input_tokens,
                        "cache_read_input_tokens": cum.cache_read_input_tokens,
                    },
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
        SlashCommand::Im { args } => {
            // IM Bridge: status/config/start via resume mode.
            let (message, json_value) = handle_im_command(args.as_deref());
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json_value),
            })
        }
        SlashCommand::Bus { args } => {
            // Session Bus: list/send/watch peers via resume mode.
            let message = handle_bus_command(args.as_deref(), &session.session_id);
            let json_text = message.clone();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(serde_json::json!({ "kind": "bus", "text": json_text })),
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
        | SlashCommand::DetectionStrategy { .. }
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
        | SlashCommand::Effort { .. }
        | SlashCommand::Branch { .. }
        | SlashCommand::Rewind { .. }
        | SlashCommand::Ide { .. }
        | SlashCommand::Tag { .. }
        | SlashCommand::OutputStyle { .. }
        | SlashCommand::AddDir { .. } => Err("unsupported resumed slash command".into()),
    }
}

/// Search the conversation history for a (case-insensitive) substring.
///
/// Returns a list of `(message_index, preview)` tuples for messages whose
/// text content (Text/Thinking/ToolUse input/ToolResult output) contains the
/// query. An empty query matches every message. Previews are truncated to
/// 80 characters and collapse newlines so they render nicely on a single
/// line in the result list.
pub(crate) fn search_session_history(session: &Session, query: &str) -> Vec<(usize, String)> {
    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    for (idx, msg) in session.messages.iter().enumerate() {
        let role_label = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
            MessageRole::System => "system",
        };
        for block in &msg.blocks {
            let candidate: Option<String> = match block {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::Thinking { thinking, .. } => Some(thinking.clone()),
                ContentBlock::ToolUse { name, input, .. } => Some(format!("[{name}] {input}")),
                ContentBlock::ToolResult {
                    tool_name, output, ..
                } => Some(format!("[{tool_name} result] {output}")),
            };
            let Some(text) = candidate else { continue };
            if needle.is_empty() || text.to_lowercase().contains(&needle) {
                let preview = build_search_preview(role_label, &text);
                hits.push((idx, preview));
                break; // one preview per message
            }
        }
    }
    hits
}

fn build_search_preview(role_label: &str, text: &str) -> String {
    let collapsed: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = collapsed.trim();
    const MAX_PREVIEW: usize = 80;
    let truncated: String = trimmed.chars().take(MAX_PREVIEW).collect();
    let suffix = if trimmed.chars().count() > MAX_PREVIEW {
        "…"
    } else {
        ""
    };
    format!("[{role_label}] {truncated}{suffix}")
}

/// 会话摘要渲染 — 供 `/summary` 命令使用。
///
/// 复用 runtime compaction 的 LLM 优先摘要(已注册 `CompactionSummarizerClient`
/// 时由模型生成,否则启发式规则摘要)。不压缩、不删除任何消息。
#[must_use]
pub(crate) fn render_session_summary_text(session: &Session) -> String {
    let summary = runtime::compact::render_session_summary(session);
    format!(
        "Summary\n  Messages         {}\n  Source           {}\n\n{}",
        session.messages.len(),
        llm_or_heuristic_label(),
        summary
    )
}

fn llm_or_heuristic_label() -> &'static str {
    if runtime::compact::is_compaction_summarizer_registered() {
        "LLM summary"
    } else {
        "heuristic summary"
    }
}

/// 上下文占用报告 — 供 `/context` 命令使用。
///
/// 统计会话规模(消息数、按角色分布、估算 token)与压缩状态,
/// 帮助用户判断是否需要 /compact 或清理。
#[must_use]
pub(crate) fn render_context_report(session: &Session) -> String {
    let total = session.messages.len();
    let count_by_role =
        |role: MessageRole| session.messages.iter().filter(|m| m.role == role).count();
    let user = count_by_role(MessageRole::User);
    let assistant = count_by_role(MessageRole::Assistant);
    let tool = count_by_role(MessageRole::Tool);
    let system = count_by_role(MessageRole::System);
    let estimated = runtime::compact::estimate_session_tokens(session);
    // 压缩边界检测:boundary 之后的活跃消息数。
    let active = runtime::compact::get_messages_after_compact_boundary(&session.messages).len();
    let compacted = if active < total {
        format!("yes ({} summarized below boundary)", total - active)
    } else {
        "no".to_string()
    };
    let summarizer = if runtime::compact::is_compaction_summarizer_registered() {
        "LLM"
    } else {
        "heuristic"
    };
    // 展示配置的 provider fallback 链(settings.json `providerFallbacks`)。
    // ProviderRuntimeClient 在 retryable 错误时按序切换链上模型重试。
    let fallbacks = std::env::current_dir()
        .ok()
        .and_then(|cwd| ConfigLoader::default_for(cwd).load().ok())
        .map(|config| config.provider_fallbacks().clone())
        .unwrap_or_default();
    let mut fallback_line = String::from("none");
    if !fallbacks.fallbacks().is_empty() {
        let chain: Vec<String> = std::iter::once(
            fallbacks
                .primary()
                .map_or_else(|| "primary".to_string(), str::to_string),
        )
        .chain(fallbacks.fallbacks().iter().cloned())
        .collect();
        fallback_line = chain.join(" → ");
    }
    format!(
        "Context\n  Messages         {total}\n    user           {user}\n    assistant      {assistant}\n    tool           {tool}\n    system         {system}\n  Estimated tokens {estimated}\n  Active after boundary {active}\n  Compacted       {compacted}\n  Summarizer      {summarizer}\n  Provider fallback {fallback_line}"
    )
}

/// 用量报告 — 供 `/usage` 命令使用(与 /stats 互补,展示最新/累计/轮次/成本)。
#[must_use]
pub(crate) fn render_usage_report(
    model: Option<&str>,
    cumulative: TokenUsage,
    latest: TokenUsage,
    turns: u32,
) -> String {
    let model_line = model.unwrap_or("(unknown)");
    let mut out = format!(
        "Usage\n  Model           {model_line}\n  Turns           {turns}\n\nLatest turn\n{}",
        crate::format::format_cost_report(latest)
    );
    out.push_str("\n\nCumulative\n");
    out.push_str(&crate::format::format_cost_report(cumulative));
    out
}

/// Undo the most recent file-editing tool call in the session.
///
/// Walks the message history backwards looking for the latest ToolUse whose
/// name is `Edit` or `Write`, locates its ToolResult, parses the
/// `originalFile` field from the result envelope, and restores the file to
/// that pre-edit contents (or deletes it if `Write` created a new file).
///
/// Returns a human-readable status line describing what was undone.
pub(crate) fn undo_last_file_edit(session: &Session) -> String {
    // Walk blocks in reverse order across messages (newest first) to find
    // the most recent file-editing ToolUse.
    let mut target_tool_use_id: Option<String> = None;
    let mut target_tool_name: Option<String> = None;
    let mut target_input: Option<String> = None;
    'outer: for msg in session.messages.iter().rev() {
        for block in &msg.blocks {
            if let ContentBlock::ToolUse { id, name, input } = block {
                if name == "Edit" || name == "Write" {
                    target_tool_use_id = Some(id.clone());
                    target_tool_name = Some(name.clone());
                    target_input = Some(input.clone());
                    break 'outer;
                }
            }
        }
    }

    let (Some(tool_use_id), Some(tool_name), Some(input_json)) =
        (target_tool_use_id, target_tool_name, target_input)
    else {
        return "Undo\n  Result           nothing to undo\n  Detail           no file edits found in this session".to_string();
    };

    // Parse the input JSON for filePath.
    let input_value: Value = match serde_json::from_str(&input_json) {
        Ok(v) => v,
        Err(e) => {
            return format!(
                "Undo\n  Result           failed\n  Detail           could not parse tool input: {e}"
            );
        }
    };
    let file_path = input_value
        .get("filePath")
        .or_else(|| input_value.get("file_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if file_path.is_empty() {
        return "Undo\n  Result           failed\n  Detail           tool input missing filePath"
            .to_string();
    }

    // Find the matching ToolResult and parse originalFile from its output.
    let mut original_file: Option<Option<String>> = None; // outer Option = found; inner = had original
    for msg in &session.messages {
        for block in &msg.blocks {
            if let ContentBlock::ToolResult {
                tool_use_id: tuid,
                output,
                is_error,
                ..
            } = block
            {
                if tuid == &tool_use_id {
                    if *is_error {
                        return format!(
                            "Undo\n  Result           skipped\n  Detail           original tool call errored; nothing to undo\n  File             {file_path}"
                        );
                    }
                    let output_value: Value = serde_json::from_str(output).unwrap_or(Value::Null);
                    // EditFileOutput.original_file is always present (String).
                    // WriteFileOutput.original_file is Option<String> (null on create).
                    if let Some(orig) = output_value.get("originalFile").and_then(|v| v.as_str()) {
                        original_file = Some(Some(orig.to_string()));
                    } else if output_value.get("originalFile").is_some() {
                        // originalFile present but null → Write created a new file.
                        original_file = Some(None);
                    }
                    break;
                }
            }
        }
    }

    let Some(maybe_original) = original_file else {
        return format!(
            "Undo\n  Result           failed\n  Detail           no tool result recorded for {tool_name} call\n  File             {file_path}"
        );
    };

    match maybe_original {
        Some(content) => match fs::write(file_path, content) {
            Ok(()) => format!(
                "Undo\n  Result           restored\n  Tool             {tool_name}\n  File             {file_path}"
            ),
            Err(e) => format!(
                "Undo\n  Result           failed\n  Detail           could not write file: {e}\n  File             {file_path}"
            ),
        },
        None => {
            // Write created a new file → delete it.
            match fs::remove_file(file_path) {
                Ok(()) => format!(
                    "Undo\n  Result           deleted (was a new file)\n  Tool             {tool_name}\n  File             {file_path}"
                ),
                Err(e) => format!(
                    "Undo\n  Result           failed\n  Detail           could not delete file: {e}\n  File             {file_path}"
                ),
            }
        }
    }
}

pub(crate) fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(current_session_store()?.sessions_dir().to_path_buf())
}

pub(crate) fn current_session_store() -> Result<runtime::SessionStore, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    runtime::SessionStore::from_cwd(&cwd).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

pub(crate) fn new_cli_session() -> Result<Session, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let mut session = Session::new().with_workspace_root(cwd.clone());
    // Attach a SQLite FTS5 history index so the `session_search` tool can
    // recall past messages. Best-effort: if the DB cannot be opened (e.g.
    // permission denied), the session still works — only search is disabled.
    let db_path = cwd.join(".claw").join("history.db");
    if let Ok(mut index) = HistoryIndex::open(&db_path) {
        // v4:注入进程级共享 embedding provider(BGE-small 单例)启用稠密检索;
        // 未编译 embedding feature 时返回 None,索引保持纯 FTS5 行为。
        if let Some(provider) = runtime::build_embedding_provider() {
            index = index.with_embedder(provider);
        }
        session = session.with_history_index(std::sync::Arc::new(index));
    }
    Ok(session)
}

/// 与 [`new_cli_session`] 类似，但额外附加 `--add-dir` 提供的工作区根。
/// 这些根与主工作区根一起构成多根校验集合（见 `WorkspacePathScope`）。
pub(crate) fn new_cli_session_with_roots(
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
    if let Ok(mut index) = HistoryIndex::open(&db_path) {
        // v4:注入进程级共享 embedding provider(BGE-small 单例)启用稠密检索;
        // 未编译 embedding feature 时返回 None,索引保持纯 FTS5 行为。
        if let Some(provider) = runtime::build_embedding_provider() {
            index = index.with_embedder(provider);
        }
        session = session.with_history_index(std::sync::Arc::new(index));
    }
    Ok(session)
}

pub(crate) fn create_managed_session_handle(
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let handle = current_session_store()?.create_handle(session_id);
    Ok(SessionHandle {
        id: handle.id,
        path: handle.path,
    })
}

pub(crate) fn resolve_session_reference(
    reference: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let handle = current_session_store()?
        .resolve_reference(reference)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(SessionHandle {
        id: handle.id,
        path: handle.path,
    })
}

pub(crate) fn session_reference_exists(
    reference: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(current_session_store()?.session_exists(reference))
}

pub(crate) fn resolve_managed_session_path(
    session_id: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    current_session_store()?
        .resolve_managed_path(session_id)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

pub(crate) fn list_managed_sessions(
) -> Result<Vec<ManagedSessionSummary>, Box<dyn std::error::Error>> {
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

pub(crate) fn latest_managed_session() -> Result<ManagedSessionSummary, Box<dyn std::error::Error>>
{
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

pub(crate) fn load_session_reference(
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

pub(crate) fn delete_managed_session(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("session file does not exist: {}", path.display()).into());
    }
    fs::remove_file(path)?;
    Ok(())
}

pub(crate) fn confirm_session_deletion(session_id: &str) -> bool {
    print!("Delete session '{session_id}'? This cannot be undone. [y/N]: ");
    io::stdout().flush().unwrap_or(());
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

/// Render a single session line for the interactive picker.
fn render_session_picker_line(
    idx: usize,
    session: &ManagedSessionSummary,
    is_active: bool,
) -> String {
    let marker = if is_active { "*" } else { " " };
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
    format!(
        "  {marker} [{idx:>3}] {id:<20} msgs={msgs:<4} modified={modified}{lineage}",
        id = session.id,
        msgs = session.message_count,
        modified = format_session_modified_age(session.modified_epoch_millis),
        lineage = lineage,
    )
}

/// Substring (case-insensitive) fuzzy match against the session id,
/// branch name, or parent session id.
fn session_matches_filter(session: &ManagedSessionSummary, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let needle = filter.to_lowercase();
    let haystacks = [
        session.id.as_str(),
        session.branch_name.as_deref().unwrap_or(""),
        session.parent_session_id.as_deref().unwrap_or(""),
    ];
    haystacks
        .iter()
        .any(|haystack| haystack.to_lowercase().contains(&needle))
}

/// Interactive session picker.
///
/// Lists managed sessions sorted by most-recently-modified first, then loops
/// reading user input:
///   - empty input / "q" / "quit" → cancel, return None
///   - a positive integer N       → pick filtered[N-1] and return it
///   - any other text             → use as new fuzzy filter and re-render
///
/// The active session id is highlighted with `*` so the user knows where
/// they currently are.
pub(crate) fn interactive_session_pick(
    active_session_id: &str,
) -> Result<Option<ManagedSessionSummary>, Box<dyn std::error::Error>> {
    let mut sessions = list_managed_sessions()?;
    // Sort newest-first by modified_epoch_millis.
    sessions.sort_by_key(|b| std::cmp::Reverse(b.modified_epoch_millis));

    if sessions.is_empty() {
        println!("No managed sessions saved yet.");
        return Ok(None);
    }

    let mut filter = String::new();
    loop {
        let filtered: Vec<&ManagedSessionSummary> = sessions
            .iter()
            .filter(|s| session_matches_filter(s, &filter))
            .collect();

        println!();
        println!(
            "Sessions  (filter: {:?}, {} of {} shown)",
            filter,
            filtered.len(),
            sessions.len()
        );
        if filtered.is_empty() {
            println!("  No sessions match the current filter.");
        } else {
            for (i, session) in filtered.iter().enumerate() {
                let is_active = session.id == active_session_id;
                println!("{}", render_session_picker_line(i + 1, session, is_active));
            }
        }
        println!();
        print!(
            "filter or pick [1-{}] (empty=cancel, q=cancel): ",
            filtered.len()
        );
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return Ok(None);
        }
        let trimmed = input.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("q")
            || trimmed.eq_ignore_ascii_case("quit")
        {
            return Ok(None);
        }
        // Numeric → pick
        if let Ok(n) = trimmed.parse::<usize>() {
            if n >= 1 && n <= filtered.len() {
                let picked = filtered[n - 1].clone();
                return Ok(Some(picked));
            }
            println!("  Index out of range (1-{}).", filtered.len());
            continue;
        }
        // Otherwise treat as new filter.
        filter = trimmed.to_string();
    }
}

pub(crate) fn session_details_json(sessions: &[ManagedSessionSummary]) -> Vec<serde_json::Value> {
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

pub(crate) fn session_exists_json(
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

pub(crate) fn run_resumed_session_command(
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

pub(crate) fn render_session_list(
    active_session_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
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

pub(crate) fn format_session_modified_age(modified_epoch_millis: u128) -> String {
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

pub(crate) fn write_session_clear_backup(
    session: &Session,
    session_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let backup_path = session_clear_backup_path(session_path);
    session.save_to_path(&backup_path)?;
    Ok(backup_path)
}

pub(crate) fn session_clear_backup_path(session_path: &Path) -> PathBuf {
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

pub(crate) fn parse_history_count(raw: Option<&str>) -> Result<usize, String> {
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

pub(crate) fn format_history_timestamp(timestamp_ms: u64) -> String {
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
pub(crate) fn civil_from_days(days: i64) -> (i32, u32, u32) {
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

pub(crate) fn render_prompt_history_report(entries: &[PromptHistoryEntry], limit: usize) -> String {
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

pub(crate) fn collect_session_prompt_history(session: &Session) -> Vec<PromptHistoryEntry> {
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

pub(crate) fn recent_user_context(session: &Session, limit: usize) -> String {
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

pub(crate) fn default_export_filename(session: &Session) -> String {
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

pub(crate) fn resolve_export_path(
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

pub(crate) fn summarize_tool_payload_for_markdown(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.split_whitespace().collect::<Vec<_>>().join(" "),
    };
    if compact.is_empty() {
        return String::new();
    }
    truncate_for_summary(&compact, SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT)
}

pub(crate) fn run_export(
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

pub(crate) fn render_session_markdown(
    session: &Session,
    session_id: &str,
    session_path: &Path,
) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ConversationMessage, Session};

    fn build_session_with_blocks(blocks: Vec<ContentBlock>, role: MessageRole) -> Session {
        let mut session = Session::new();
        session.messages.push(ConversationMessage {
            role,
            blocks,
            usage: None,
        });
        session
    }

    #[test]
    fn render_session_summary_text_works_without_llm() {
        let mut session = Session::new();
        session
            .messages
            .push(ConversationMessage::user_text("implement the auth module"));
        session
            .messages
            .push(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "I will add auth.rs".to_string(),
            }]));
        let text = render_session_summary_text(&session);
        assert!(text.contains("Summary"));
        assert!(text.contains("Messages"));
        // 未注册 LLM client 时走启发式,包含 Scope 统计。
        assert!(text.contains("Scope:"));
    }

    #[test]
    fn render_context_report_lists_roles_and_estimated_tokens() {
        let mut session = Session::new();
        session
            .messages
            .push(ConversationMessage::user_text("hello"));
        session
            .messages
            .push(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "hi".to_string(),
            }]));
        let text = render_context_report(&session);
        assert!(text.contains("Context"));
        assert!(text.contains("Messages         2"));
        assert!(text.contains("user"));
        assert!(text.contains("assistant"));
        assert!(text.contains("Estimated tokens"));
    }

    #[test]
    fn render_usage_report_shows_latest_and_cumulative() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 1,
        };
        let text = render_usage_report(Some("deepseek-v4-flash"), usage, usage, 3);
        assert!(text.contains("deepseek-v4-flash"));
        assert!(text.contains("Turns"));
        assert!(text.contains("3"));
        assert!(text.contains("Latest turn"));
        assert!(text.contains("Cumulative"));
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_string(),
        }
    }

    fn tool_use_block(id: &str, name: &str, input: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
        }
    }

    fn tool_result_block(id: &str, name: &str, output: &str, is_error: bool) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            output: output.to_string(),
            is_error,
        }
    }

    #[test]
    fn search_returns_empty_for_no_matches() {
        let session = build_session_with_blocks(vec![text_block("hello world")], MessageRole::User);
        let results = search_session_history(&session, "nonexistent");
        assert!(results.is_empty());
    }

    #[test]
    fn search_finds_case_insensitive_match() {
        let session = build_session_with_blocks(vec![text_block("Hello World")], MessageRole::User);
        let results = search_session_history(&session, "WORLD");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0);
        // Preview preserves original casing.
        assert!(results[0].1.contains("Hello World"));
    }

    #[test]
    fn search_empty_query_matches_everything() {
        let session =
            build_session_with_blocks(vec![text_block("anything goes")], MessageRole::Assistant);
        let results = search_session_history(&session, "");
        assert_eq!(results.len(), 1);
        assert!(results[0].1.starts_with("[assistant]"));
    }

    #[test]
    fn search_matches_tool_use_input() {
        let session = build_session_with_blocks(
            vec![tool_use_block(
                "tu_1",
                "Edit",
                r#"{"filePath":"/tmp/foo.rs","oldString":"a","newString":"b"}"#,
            )],
            MessageRole::Assistant,
        );
        let results = search_session_history(&session, "foo.rs");
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("[Edit]"));
    }

    #[test]
    fn search_one_preview_per_message_even_with_multiple_matches() {
        let session = build_session_with_blocks(
            vec![
                text_block("first match here"),
                text_block("second match here"),
            ],
            MessageRole::User,
        );
        let results = search_session_history(&session, "match");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_preview_truncates_long_text() {
        let long_text = "a".repeat(200);
        let session = build_session_with_blocks(vec![text_block(&long_text)], MessageRole::User);
        let results = search_session_history(&session, "a");
        assert_eq!(results.len(), 1);
        // preview = "[user] " (7 chars) + 80 'a' + "…" = 88 chars
        let preview = &results[0].1;
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), 88);
    }

    #[test]
    fn undo_nothing_to_undo_when_no_file_edits() {
        let session =
            build_session_with_blocks(vec![text_block("just chatting")], MessageRole::User);
        let message = undo_last_file_edit(&session);
        assert!(message.contains("nothing to undo"));
    }

    #[test]
    fn undo_skipped_when_tool_call_errored() {
        let session = build_session_with_blocks(
            vec![
                tool_use_block(
                    "tu_1",
                    "Edit",
                    r#"{"filePath":"/nonexistent/path.rs","oldString":"a","newString":"b"}"#,
                ),
                tool_result_block("tu_1", "Edit", "old_string not found in file", true),
            ],
            MessageRole::Assistant,
        );
        let message = undo_last_file_edit(&session);
        assert!(message.contains("skipped"));
        assert!(message.contains("errored"));
    }

    #[test]
    fn undo_finds_latest_edit_among_multiple_tools() {
        // Mix a Read (ignored) and two Edits; the latest Edit should be picked.
        let mut session = Session::new();
        session.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![
                tool_use_block("tu_1", "Read", r#"{"filePath":"/tmp/a.rs"}"#),
                tool_use_block(
                    "tu_2",
                    "Edit",
                    r#"{"filePath":"/tmp/older.rs","oldString":"a","newString":"b"}"#,
                ),
            ],
            usage: None,
        });
        session.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![tool_use_block(
                "tu_3",
                "Edit",
                r#"{"filePath":"/tmp/newer.rs","oldString":"x","newString":"y"}"#,
            )],
            usage: None,
        });
        // Don't actually run undo (file doesn't exist on disk); instead
        // verify we got past the "nothing to undo" guard and into the
        // "no tool result recorded" branch for the newest tool.
        let message = undo_last_file_edit(&session);
        assert!(
            message.contains("no tool result recorded") || message.contains("failed"),
            "got: {message}"
        );
        assert!(message.contains("/tmp/newer.rs"));
    }

    #[test]
    fn undo_missing_file_path_in_input_returns_failed() {
        let session = build_session_with_blocks(
            vec![
                tool_use_block("tu_1", "Edit", r#"{"oldString":"a","newString":"b"}"#),
                tool_result_block(
                    "tu_1",
                    "Edit",
                    r#"{"filePath":"/tmp/x","originalFile":"a"}"#,
                    false,
                ),
            ],
            MessageRole::Assistant,
        );
        let message = undo_last_file_edit(&session);
        assert!(message.contains("missing filePath"));
    }

    // ===== Session picker tests =====

    fn picker_session(
        id: &str,
        modified: u128,
        branch: Option<&str>,
        parent: Option<&str>,
    ) -> ManagedSessionSummary {
        ManagedSessionSummary {
            id: id.to_string(),
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            updated_at_ms: modified as u64,
            modified_epoch_millis: modified,
            message_count: 10,
            parent_session_id: parent.map(str::to_string),
            branch_name: branch.map(str::to_string),
            lifecycle: SessionLifecycleSummary {
                kind: SessionLifecycleKind::SavedOnly,
                pane_id: None,
                pane_command: None,
                pane_path: None,
                workspace_dirty: false,
                abandoned: false,
            },
        }
    }

    #[test]
    fn picker_filter_empty_matches_all() {
        let sessions = [
            picker_session("abc123", 1000, None, None),
            picker_session("xyz789", 2000, Some("feature"), None),
        ];
        let filtered: Vec<_> = sessions
            .iter()
            .filter(|s| session_matches_filter(s, ""))
            .collect();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn picker_filter_matches_id_case_insensitive() {
        let sessions = [
            picker_session("Abc123", 1000, None, None),
            picker_session("xyz789", 2000, None, None),
        ];
        let filtered: Vec<_> = sessions
            .iter()
            .filter(|s| session_matches_filter(s, "ABC"))
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "Abc123");
    }

    #[test]
    fn picker_filter_matches_branch_name() {
        let sessions = [
            picker_session("abc123", 1000, None, None),
            picker_session("xyz789", 2000, Some("feature-branch"), None),
        ];
        let filtered: Vec<_> = sessions
            .iter()
            .filter(|s| session_matches_filter(s, "feature"))
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "xyz789");
    }

    #[test]
    fn picker_filter_matches_parent_session_id() {
        let sessions = [
            picker_session("abc123", 1000, None, None),
            picker_session("fork-1", 2000, Some("dev"), Some("abc123")),
        ];
        let filtered: Vec<_> = sessions
            .iter()
            .filter(|s| session_matches_filter(s, "abc123"))
            .collect();
        // Both match: "abc123" by id, "fork-1" by parent_session_id.
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn picker_filter_no_matches_returns_empty() {
        let sessions = [
            picker_session("abc123", 1000, None, None),
            picker_session("xyz789", 2000, None, None),
        ];
        let filtered: Vec<_> = sessions
            .iter()
            .filter(|s| session_matches_filter(s, "nonexistent"))
            .collect();
        assert!(filtered.is_empty());
    }

    #[test]
    fn picker_render_line_marks_active_session() {
        let session = picker_session("abc123", 1000, None, None);
        let active_line = render_session_picker_line(1, &session, true);
        let inactive_line = render_session_picker_line(1, &session, false);
        // Active uses '*', inactive uses ' '.
        assert!(active_line.contains("* ["));
        assert!(inactive_line.contains("  [") && !inactive_line.contains('*'));
        assert!(active_line.contains("abc123"));
    }

    #[test]
    fn picker_render_line_includes_lineage() {
        let session = picker_session("fork-1", 1000, Some("dev"), Some("parent-1"));
        let line = render_session_picker_line(1, &session, false);
        assert!(line.contains("branch=dev"));
        assert!(line.contains("from=parent-1"));
    }
}
