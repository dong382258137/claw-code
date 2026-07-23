//! CLI argument parsing and slash command handler helpers.

use std::collections::BTreeSet;
use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static CLI_MAX_TURNS: AtomicU32 = AtomicU32::new(0);
pub(crate) fn take_max_turns() -> Option<u32> {
    let v = CLI_MAX_TURNS.swap(0, Ordering::Relaxed);
    if v == 0 { None } else { Some(v) }
}

use crate::render::OutputVerbosity;
use api::model_family_identity_for;
use commands::{
    classify_skills_slash_command, resolve_skill_invocation, slash_command_specs,
    SkillSlashDispatch, SlashCommand,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use runtime::{load_system_prompt, PermissionMode};
use serde_json::json;

use crate::session_mgr::LATEST_SESSION_REFERENCE;
use crate::suggestion::{
    looks_like_subcommand_typo, render_suggestion_line, suggest_closest_term,
    suggest_similar_subcommand, suggest_slash_commands, CLI_OPTION_SUGGESTIONS,
};
use crate::{
    config_alias_for_current_dir, config_model_for_current_dir,
    config_permission_mode_for_current_dir, current_tool_registry, default_permission_mode,
    format_connected_line, normalize_allowed_tools, parse_dump_manifests_args, parse_export_args,
    parse_permission_mode_arg, parse_resume_args, parse_system_prompt_args,
    permission_mode_from_label, permission_mode_from_resolved, provider_label,
    render_version_report, resolve_model_alias, resolve_model_alias_with_config,
    resolve_repl_model, validate_model_syntax, AllowedToolSet, CliOutputFormat, BUILD_TARGET,
    DEFAULT_DATE, DEFAULT_MODEL, GIT_SHA, VERSION,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CliAction {
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
        /// `claw doctor --cache-stats`:仅输出 Cache Aligner 监控指标
        /// (按原因分类的 cache break 计数 + completion 缓存命中统计),
        /// 跳过常规健康检查。底层读取 `~/.claude/cache/prompt-cache/*/stats.json`
        /// 并跨 session 汇总。
        cache_stats: bool,
    },
    Acp {
        output_format: CliOutputFormat,
    },
    /// `claw acp serve`:启动 stdio ACP 服务器,通过 stdin/stdout 与外部
    /// ACP 客户端(Zed / VS Code 等)通信。阻塞调用,直到 stdin EOF 或 cancel。
    AcpServe {
        model: String,
        permission_mode: PermissionMode,
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
    ForkSession {
        session_id: String,
        output_format: CliOutputFormat,
    },
    ListSessions {
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
        /// 启用 full-tui 模式：使用 ratatui 全屏 TUI 替代 inline REPL。
        /// 仅当 `full-tui` Cargo feature 启用时生效；否则报错。
        tui: bool,
        /// 启用 Plan/Execute/Review 三段循环(Step 2.1)。
        /// 启用后,复杂用户输入(>200 字符或含 "refactor"/"多文件" 等关键词)
        /// 会触发 PlanArtifact 创建,末尾追加到 system_prompt 的变动区。
        /// 详见 `docs/harness-engineering-optimization-plan.md` Step 2.1 与 §5.2。
        /// 预期 DeepSeek V4 PRO 缓存命中率从 95% 降至 88-92%。
        enable_plan_mode: bool,
        /// P1-1:启用 PolicyEngine 策略引擎。
        /// 启用后,lane 完成时会调用 PolicyEngine::evaluate 产出策略动作
        /// (CloseoutLane/CleanupSession 等),发布到 lane_events。
        /// 默认关闭,向后兼容。
        enable_policy_engine: bool,
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
pub(crate) enum LocalHelpTopic {
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

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_args(args: &[String]) -> Result<CliAction, String> {
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
    // TUI 模式默认启用（default = ["full-tui"] 编译时）。
    // `--no-tui` 显式回退到旧 rustyline inline REPL。
    // `--tui` 仍然支持（冗余但便于脚本明确意图）。
    let mut tui = true;
    // `--enable-plan-mode`：启用 Plan/Execute/Review 三段循环(Step 2.1)。
    // 默认关闭。详见 `docs/harness-engineering-optimization-plan.md` Step 2.1。
    let mut enable_plan_mode = false;
    // P1-1:`--enable-policy-engine` — 启用 PolicyEngine 策略引擎。
    // 默认关闭。启用后 lane 完成时调用 PolicyEngine::evaluate。
    let mut enable_policy_engine = false;
    // `--cache-stats`:仅对 `claw doctor` 生效,切换到 Cache Aligner 监控视图。
    let mut cache_stats = false;
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
            "--cache-stats" => {
                // `claw doctor --cache-stats`:切换到 Cache Aligner 监控视图。
                // 仅 `doctor` 子命令消费此 flag;其他子命令会忽略它。
                cache_stats = true;
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
                    return Err(format!("--add-dir path does not exist: {}", path.display()));
                }
                additional_workspace_roots.push(path);
                index += 2;
            }
            flag if flag.starts_with("--add-dir=") => {
                let path = PathBuf::from(&flag[10..]);
                if !path.exists() {
                    return Err(format!("--add-dir path does not exist: {}", path.display()));
                }
                additional_workspace_roots.push(path);
                index += 1;
            }
            // 冗度控制：后覆盖先。`--verbose` 显式回到 Full（覆盖之前的 --quiet）。
            "--verbose" => {
                output_verbosity = OutputVerbosity::Full;
                index += 1;
            }
            "--tui" => {
                tui = true;
                index += 1;
            }
            "--no-tui" => {
                tui = false;
                index += 1;
            }
            "--enable-plan-mode" => {
                enable_plan_mode = true;
                index += 1;
            }
            "--enable-policy-engine" => {
                enable_policy_engine = true;
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
                    output_verbosity,
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
                    output_verbosity,
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
            output_verbosity,
            tui,
            enable_plan_mode,
            enable_policy_engine,
        });
    }
    if rest.first().map(String::as_str) == Some("--fork-session") {
        let session_id = rest.get(1).cloned().unwrap_or_default();
        return Ok(CliAction::ForkSession { session_id, output_format });
    }
    if rest.first().map(String::as_str) == Some("--list-sessions") {
        return Ok(CliAction::ListSessions { output_format });
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
        cache_stats,
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
                    output_verbosity,
                }),
                SkillSlashDispatch::Local => Ok(CliAction::Skills {
                    args,
                    output_format,
                }),
            }
        }
        "system-prompt" => parse_system_prompt_args(&rest[1..], model, output_format),
        "acp" => parse_acp_args(
            &rest[1..],
            model.clone(),
            permission_mode_override,
            output_format,
        ),
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
                output_verbosity,
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
            output_verbosity,
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
                output_verbosity,
            })
        }
    }
}

pub(crate) fn parse_local_help_action(
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

pub(crate) fn is_help_flag(value: &str) -> bool {
    matches!(value, "--help" | "-h")
}

pub(crate) fn parse_single_word_command_alias(
    rest: &[String],
    model: &str,
    // #148: raw --model flag input for status provenance. None = no flag.
    model_flag_raw: Option<&str>,
    permission_mode_override: Option<PermissionMode>,
    output_format: CliOutputFormat,
    allowed_tools: Option<AllowedToolSet>,
    // `--cache-stats` flag,仅对 `doctor` 子命令生效。其他诊断动词忽略。
    cache_stats: bool,
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
        // `doctor --cache-stats` 是合法形式。--cache-stats 在主解析循环中
        // 已被消费并设置 cache_stats=true,所以正常情况下 rest 里不会出现它。
        // 这里兜底处理:若用户输入 `doctor --cache-stats`(且未被主循环捕获),
        // 视为合法,继续走 doctor 分支。
        if verb == "doctor" && rest.len() == 2 && rest[1] == "--cache-stats" {
            // fall through 到下面的 doctor 分支
        } else {
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
        "doctor" => Some(Ok(CliAction::Doctor {
            output_format,
            cache_stats,
        })),
        "state" => Some(Ok(CliAction::State { output_format })),
        // #146: let `config` and `diff` fall through to parse_subcommand
        // where they are wired as pure-local introspection, instead of
        // producing the "is a slash command" guidance. Zero-arg cases
        // reach parse_subcommand too via this None.
        "config" | "diff" => None,
        other => bare_slash_command_guidance(other).map(Err),
    }
}

pub(crate) fn bare_slash_command_guidance(command_name: &str) -> Option<String> {
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

pub(crate) fn removed_auth_surface_error(command_name: &str) -> String {
    format!(
        "`claw {command_name}` has been removed. Set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN instead."
    )
}

pub(crate) fn parse_acp_args(
    args: &[String],
    model: String,
    permission_mode_override: Option<PermissionMode>,
    output_format: CliOutputFormat,
) -> Result<CliAction, String> {
    let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
    match args {
        // `claw acp` / `claw --acp` / `claw -acp`:仅打印状态(向后兼容)
        [] => Ok(CliAction::Acp { output_format }),
        // `claw acp serve`:启动 stdio ACP 服务器
        [subcommand] if subcommand == "serve" => Ok(CliAction::AcpServe {
            model,
            permission_mode,
            output_format,
        }),
        _ => Err(String::from(
            "unsupported ACP invocation. Use `claw acp`, `claw acp serve`, `claw --acp`, or `claw -acp`.",
        )),
    }
}

pub(crate) fn try_resolve_bare_skill_prompt(cwd: &Path, trimmed: &str) -> Option<String> {
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

pub(crate) fn join_optional_args(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub(crate) fn parse_direct_slash_cli_action(
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
                    output_verbosity,
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

pub(crate) fn format_unknown_option(option: &str) -> String {
    let mut message = format!("unknown option: {option}");
    if let Some(suggestion) = suggest_closest_term(option, CLI_OPTION_SUGGESTIONS) {
        message.push_str("\nDid you mean ");
        message.push_str(suggestion);
        message.push('?');
    }
    message.push_str("\nRun `claw --help` for usage.");
    message
}

pub(crate) fn format_unknown_direct_slash_command(name: &str) -> String {
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

pub(crate) fn format_unknown_slash_command(name: &str) -> String {
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

pub(crate) fn omc_compatibility_note_for_unknown_slash_command(name: &str) -> Option<&'static str> {
    name.starts_with("oh-my-claudecode:")
        .then_some(
            "Compatibility note: `/oh-my-claudecode:*` is a Claude Code/OMC plugin command. `claw` does not yet load plugin slash commands, Claude statusline stdin, or OMC session hooks.",
        )
}

pub(crate) fn dump_manifests(
    manifests_dir: Option<&Path>,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    dump_manifests_at_path(&workspace_dir, manifests_dir, output_format)
}

pub(crate) const DUMP_MANIFESTS_OVERRIDE_HINT: &str =
    "Hint: set CLAUDE_CODE_UPSTREAM=/path/to/upstream or pass `claw dump-manifests --manifests-dir /path/to/upstream`.";

// Internal function for testing that accepts a workspace directory path.
pub(crate) fn dump_manifests_at_path(
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

pub(crate) fn print_bootstrap_plan(
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(crate) fn print_system_prompt(
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

pub(crate) fn print_version(
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => println!("{}", render_version_report()),
        CliOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&version_json_value())?);
        }
    }
    Ok(())
}

pub(crate) fn version_json_value() -> serde_json::Value {
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

/// Slash commands that are registered in the spec list but not yet implemented
/// in this build. Used to filter both REPL completions and help output so the
/// discovery surface only shows commands that actually work (ROADMAP #39).
pub(crate) const STUB_COMMANDS: &[&str] = &[
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

pub(crate) fn slash_command_completion_candidates_with_sessions(
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
        "/search ",
        "/session list",
        "/session pick",
        "/session switch ",
        "/session exists ",
        "/session fork ",
        "/session fork",
        "/session delete ",
        "/teleport ",
        "/ultraplan ",
        "/undo",
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
        completions.insert(format!("/session pick {active_session_id}"));
        completions.insert(format!("/session exists {active_session_id}"));
    }

    // Generate per-session candidates for the most recent N sessions so
    // Tab-completion works for `/session switch <id>`, `/session pick <id>`,
    // `/session exists <id>`, `/session delete <id>`, and `/resume <id>`.
    // Cap at 10 to avoid flooding the completion menu on noisy workspaces.
    for session_id in recent_session_ids
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .take(10)
    {
        completions.insert(format!("/resume {session_id}"));
        completions.insert(format!("/session switch {session_id}"));
        completions.insert(format!("/session pick {session_id}"));
        completions.insert(format!("/session exists {session_id}"));
        completions.insert(format!("/session delete {session_id}"));
    }

    completions.into_iter().collect()
}

// P2 富状态栏：渲染单行紧凑状态信息。
// 格式：`model | 📁 cwd | 🔢 tokens | 💰 cost`
// 在每次回合完成后打印，作为本回合的"尾部状态摘要"。
// 不持久占用屏幕底部（持久底部栏需要 ratatui 全屏模式，与 rustyline 冲突）。
//
// `cwd` 传入已缩短的显示路径（避免过长路径撑爆状态栏）。
// `usage` 是累计 usage（cumulative across turns）。
//
// Tier S #3 穷鬼模式：激活时追加 `🪙 poor` 标记，提醒用户非核心特性被跳过。
// (注：原为 `///` doc comment，但对应 item 在 app.rs 中，悬空 doc 触发 clippy
//  `empty_line_after_doc_comment`，改为普通注释。)

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
pub(crate) fn handle_goal_command(
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
pub(crate) fn render_goal_status(manager: &runtime::GoalManager) -> String {
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
pub(crate) fn format_timestamp_ms(ms: u64) -> String {
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
pub(crate) fn split_first_word(input: &str) -> (&str, &str) {
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
pub(crate) fn parse_goal_budget(text: &str) -> (&str, Option<u64>) {
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
pub(crate) fn handle_poor_mode_action(action: Option<&str>) -> (bool, String) {
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
                format!(
                    "{icon} Poor mode: {state}\n\x1b[2mToggle with /poor, or /poor on|off\x1b[0m"
                ),
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
pub(crate) fn handle_bg_command(args: Option<&str>, cwd: &Path) -> (String, serde_json::Value) {
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
