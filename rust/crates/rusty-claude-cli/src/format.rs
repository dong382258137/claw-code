//! Output formatting: reports, JSON, status bar, help text, diff rendering.

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::init::initialize_repo;
use crate::session_mgr::{LATEST_SESSION_REFERENCE, PRIMARY_SESSION_EXTENSION};
use crate::suggestion::suggest_slash_commands;
use api::detect_provider_kind;
use commands::{render_slash_command_help_filtered, resume_supported_slash_commands};
use runtime::{
    canonicalize_report, format_usd, pricing_for_model, resolve_sandbox_status, CanonicalReportV1,
    ClaimKind, ConfigLoader, ConfigSource, ContentBlock, MessageRole, PermissionMode,
    ProjectContext, ReportClaim, ReportConfidence, ReportIdentity, RuntimeConfig, SensitivityClass,
    Session, TokenUsage, REPORT_SCHEMA_V1,
};

use crate::commands_handler::{
    omc_compatibility_note_for_unknown_slash_command, LocalHelpTopic, STUB_COMMANDS,
};
use crate::doctor::{
    build_boot_preflight_snapshot, classify_session_lifecycle_for, parse_git_status_metadata,
    parse_git_workspace_summary, BranchFreshness, GitWorkspaceSummary, StatusContext, StatusUsage,
};
use crate::{
    provider_label, stale_base_state_for, AllowedToolSet, CliOutputFormat, ModelProvenance,
    ModelSource, BUILD_TARGET, DEFAULT_DATE, DEPRECATED_INSTALL_COMMAND, GIT_SHA,
    OFFICIAL_REPO_SLUG, OFFICIAL_REPO_URL, VERSION,
};

#[cfg(test)]
pub(crate) fn format_unknown_slash_command_message(name: &str) -> String {
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

pub(crate) fn format_model_report(model: &str, message_count: usize, turns: u32) -> String {
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

pub(crate) fn format_model_switch_report(
    previous: &str,
    next: &str,
    message_count: usize,
) -> String {
    format!(
        "Model updated
  Previous         {previous}
  Current          {next}
  Preserved msgs   {message_count}"
    )
}

pub(crate) fn format_permissions_report(mode: &str) -> String {
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

pub(crate) fn format_permissions_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Permissions updated
  Result           mode switched
  Previous mode    {previous}
  Active mode      {next}
  Applies to       subsequent tool calls
  Usage            /permissions to inspect current mode"
    )
}

pub(crate) fn format_cost_report(usage: TokenUsage) -> String {
    let estimated_cost = usage.estimate_cost_usd();
    format!(
        "成本
  输入 tokens      {}
  输出 tokens      {}
  缓存创建         {}
  缓存读取         {}
  总 tokens        {}
  预估成本         {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        usage.total_tokens(),
        format_usd(estimated_cost.total_cost_usd()),
    )
}

pub(crate) fn format_resume_report(session_path: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Session resumed
  Session file     {session_path}
  Messages         {message_count}
  Turns            {turns}"
    )
}

pub(crate) fn render_resume_usage() -> String {
    format!(
        "Resume
  Usage            /resume <session-path|session-id|{LATEST_SESSION_REFERENCE}>
  Auto-save        .claw/sessions/<workspace-fingerprint>/<session-id>.{PRIMARY_SESSION_EXTENSION}
  Tip              use /session list to inspect saved sessions"
    )
}

pub(crate) fn format_compact_report(
    removed: usize,
    resulting_messages: usize,
    skipped: bool,
) -> String {
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

pub(crate) fn format_auto_compaction_notice(removed: usize) -> String {
    format!("[auto-compacted: removed {removed} messages]")
}

pub(crate) fn render_repl_help() -> String {
    [
        "REPL 交互模式".to_string(),
        "  /exit                退出 REPL".to_string(),
        "  /quit                退出 REPL".to_string(),
        "  ↑/↓                  浏览历史输入".to_string(),
        "  Ctrl-R               反向搜索历史输入".to_string(),
        "  Tab                  补全命令、模式和最近会话".to_string(),
        "  Ctrl-C               清空输入（空行时退出）".to_string(),
        "  Shift+Enter/Ctrl+J   插入换行".to_string(),
        "  自动保存             .claw/sessions/<workspace-fingerprint>/<session-id>.jsonl"
            .to_string(),
        "  恢复最近会话         /resume latest".to_string(),
        "  浏览所有会话         /session list".to_string(),
        "  查看输入历史         /history [数量]".to_string(),
        String::new(),
        render_slash_command_help_filtered(STUB_COMMANDS),
    ]
    .join(
        "
",
    )
}

pub(crate) fn print_status_snapshot(
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

pub(crate) fn status_json_value(
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
    // Epic 4:report_schema 接入 claw status --output-format json。
    // 在现有 status JSON 基础上追加 `canonical_report` 字段(不替换现有字段,
    // 保持向后兼容),让 report_schema 模块有生产输出路径。详见 plan.md §9.2 Epic 4。
    let canonical_report = build_status_canonical_report(model, permission_mode, context);
    let canonical_value = serde_json::to_value(&canonical_report).unwrap_or(Value::Null);
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
        },
        "canonical_report": canonical_value,
    })
}

/// Epic 4:为 `claw status` 构造一个 `CanonicalReportV1`,作为 report_schema 模块
/// 的生产输出载体。报告包含 model / permission / workspace 三个 ObservedFact claim,
/// 经 `canonicalize_report` 自动填充 schema_version / report_id / content_hash。
///
/// 当前不填充 negative_evidence / field_deltas(留待后续 lane 事件流接入)。
fn build_status_canonical_report(
    model: Option<&str>,
    permission_mode: &str,
    context: &StatusContext,
) -> CanonicalReportV1 {
    let mut claims = Vec::new();
    if let Some(model) = model {
        claims.push(ReportClaim {
            id: "claim-model".to_string(),
            kind: ClaimKind::ObservedFact,
            text: format!("active model: {model}"),
            confidence: ReportConfidence::High,
            evidence: Vec::new(),
            sensitivity: SensitivityClass::Internal,
        });
    }
    claims.push(ReportClaim {
        id: "claim-permission".to_string(),
        kind: ClaimKind::ObservedFact,
        text: format!("permission mode: {permission_mode}"),
        confidence: ReportConfidence::High,
        evidence: Vec::new(),
        sensitivity: SensitivityClass::Internal,
    });
    claims.push(ReportClaim {
        id: "claim-workspace".to_string(),
        kind: ClaimKind::ObservedFact,
        text: format!("cwd: {}", context.cwd.display()),
        confidence: ReportConfidence::High,
        evidence: Vec::new(),
        sensitivity: SensitivityClass::Internal,
    });
    let report = CanonicalReportV1 {
        schema_version: String::new(),
        identity: ReportIdentity {
            report_id: String::new(),
            content_hash: String::new(),
        },
        generated_at: crate::DEFAULT_DATE.to_string(),
        producer: "claw-status".to_string(),
        claims,
        negative_evidence: Vec::new(),
        field_deltas: Vec::new(),
    };
    let _ = REPORT_SCHEMA_V1; // 确保常量被引用(canonicalize 内部会用)
    canonicalize_report(report)
}

pub(crate) fn status_context(
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

pub(crate) fn format_status_report(
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
        "状态 (降级)"
    } else {
        "状态"
    };
    let mut blocks: Vec<String> = Vec::new();
    if let Some(err) = context.config_load_error.as_deref() {
        blocks.push(format!(
            "配置加载错误\n  状态             失败\n  摘要             运行时配置加载失败;仅报告部分状态\n  详情             {err}\n  提示             `claw doctor` 会分类配置解析错误;修复列出的字段后重新运行"
        ));
    }
    // #148:在 Model 之后渲染 Model source 行,显示字符串来源
    // (flag / env / config / default)以及原始输入(如有)。
    let model_source_line = provenance
        .map(|p| match &p.raw {
            Some(raw) if raw != model => {
                format!("\n  模型来源         {} (原始: {raw})", p.source.as_str())
            }
            Some(_) => format!("\n  模型来源         {}", p.source.as_str()),
            None => format!("\n  模型来源         {}", p.source.as_str()),
        })
        .unwrap_or_default();
    blocks.extend([
        format!(
            "{status_line}
  模型             {model}{model_source_line}
  权限模式         {permission_mode}
  消息数           {}
  轮次             {}
  预估 tokens      {}",
            usage.message_count, usage.turns, usage.estimated_tokens,
        ),
        format!(
            "用量
  本次总量         {}
  累计输入         {}
  累计输出         {}
  缓存创建         {}
  缓存读取         {}
  累计总量         {}
  预估成本         {}",
            usage.latest.total_tokens(),
            usage.cumulative.input_tokens,
            usage.cumulative.output_tokens,
            usage.cumulative.cache_creation_input_tokens,
            usage.cumulative.cache_read_input_tokens,
            usage.cumulative.total_tokens(),
            format_usd(usage.cumulative.estimate_cost_usd().total_cost_usd()),
        ),
        format!(
            "工作区
  当前目录         {}
  项目根目录       {}
  Git 分支         {}
  Git 状态         {}
  已更改文件       {}
  已暂存           {}
  未暂存           {}
  未跟踪           {}
  会话             {}
  生命周期         {}
  分支最新         {}
  启动预检         {}
  配置文件         已加载 {}/{}
  Memory 文件      {}
  建议流程         /status → /diff → /commit",
            context.cwd.display(),
            context
                .project_root
                .as_ref()
                .map_or_else(|| "未知".to_string(), |path| path.display().to_string()),
            context.git_branch.as_deref().unwrap_or("未知"),
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
                .map(|fresh| if fresh { "是" } else { "落后" })
                .unwrap_or("无上游"),
            context.boot_preflight.summary(),
            context.loaded_config_files,
            context.discovered_config_files,
            context.memory_file_count,
        ),
        format_sandbox_report(&context.sandbox_status),
    ]);
    blocks.join("\n\n")
}

pub(crate) fn format_sandbox_report(status: &runtime::SandboxStatus) -> String {
    format!(
        "Sandbox 沙箱
  已启用            {}
  已激活            {}
  受支持            {}
  在容器中          {}
  请求的命名空间    {}
  激活的命名空间    {}
  请求的网络隔离    {}
  激活的网络        {}
  文件系统模式      {}
  文件系统已激活    {}
  允许的挂载        {}
  容器标记          {}
  降级原因          {}",
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
            "<无>".to_string()
        } else {
            status.allowed_mounts.join(", ")
        },
        if status.container_markers.is_empty() {
            "<无>".to_string()
        } else {
            status.container_markers.join(", ")
        },
        status
            .fallback_reason
            .clone()
            .unwrap_or_else(|| "<无>".to_string()),
    )
}

pub(crate) fn format_commit_preflight_report(
    branch: Option<&str>,
    summary: GitWorkspaceSummary,
) -> String {
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

pub(crate) fn format_commit_skipped_report() -> String {
    "Commit
  Result           skipped
  Reason           no workspace changes
  Action           create a git commit from the current workspace changes
  Next             /status to inspect context · /diff to inspect repo changes"
        .to_string()
}

pub(crate) fn print_sandbox_status_snapshot(
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

pub(crate) fn sandbox_json_value(status: &runtime::SandboxStatus) -> serde_json::Value {
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

pub(crate) fn render_help_topic(topic: LocalHelpTopic) -> String {
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
        LocalHelpTopic::Acp => "ACP (Agent Client Protocol)
  Usage            claw acp [serve] [--output-format <format>]
  Aliases          claw --acp · claw -acp
  Purpose          `claw acp serve` starts a stdio ACP JSON-RPC server for editor integration
  Status           supported (stdio server, newline-delimited JSON-RPC over stdin/stdout)
  Connect          spawn `claw acp serve` as the agent process from ACP-compatible editors (Zed, etc.)
  Formats          text (default), json
  Related          claw status · claw doctor · claw --help"
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

pub(crate) fn local_help_topic_command(topic: LocalHelpTopic) -> &'static str {
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

pub(crate) fn render_export_help_json() -> serde_json::Value {
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

pub(crate) fn render_help_topic_json(topic: LocalHelpTopic) -> serde_json::Value {
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

pub(crate) fn print_help_topic(
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

pub(crate) fn acp_status_message() -> &'static str {
    "`claw acp serve` launches a stdio ACP (Agent Client Protocol) JSON-RPC server. \
     Connect from ACP-compatible editors (Zed, VS Code extensions, etc.) by spawning \
     `claw acp serve` as the editor's agent process; it speaks newline-delimited JSON-RPC \
     over stdin/stdout and supports initialize/authenticate/new_session/prompt. \
     `claw acp` (without `serve`) prints this status surface only."
}

pub(crate) fn acp_status_json() -> serde_json::Value {
    json!({
        "schema_version": "1.1",
        "kind": "acp",
        "status": "supported",
        "phase": "stdio_server",
        "supported": true,
        "exit_code": 0,
        "serve_alias_only": false,
        "message": acp_status_message(),
        "launch_command": "claw acp serve",
        "protocol": {
            "name": "ACP",
            "version": 1,
            "json_rpc": true,
            "transport": "newline_delimited_json",
            "daemon": false,
            "endpoint": "stdio",
            "serve_starts_daemon": false
        },
        "contracts": {
            "stable_status_surface": "claw acp [serve] --output-format json",
            "unsupported_invocation_kind": "unsupported_acp_invocation",
            "serve_subcommand": "claw acp serve"
        },
        "aliases": ["acp", "--acp", "-acp"],
        "tracking": "ROADMAP #76 / #3033 / #3004",
        "recommended_workflows": [
            "claw acp serve",
            "claw prompt TEXT",
            "claw",
            "claw doctor"
        ],
    })
}

pub(crate) fn print_acp_status(
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => {
            println!(
                "ACP (Agent Client Protocol)\n  Status           supported (stdio server)\n  Transport        newline-delimited JSON-RPC over stdin/stdout\n  Protocol version 1\n  Launch           `claw acp serve` starts the stdio ACP server\n  Status surface   `claw acp` / `claw --acp` / `claw -acp` print this report\n  Connect          spawn `claw acp serve` from ACP-compatible editors (Zed, etc.)\n  Today            use `claw prompt`, the REPL, or `claw doctor` for non-ACP workflows\n  Tracking         ROADMAP #76 / #3033 / #3004\n  Message          {}",
                acp_status_message()
            );
        }
        CliOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&acp_status_json())?);
        }
    }
    Ok(())
}

pub(crate) fn render_config_report(
    section: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let mut lines = vec![
        format!(
            "Config 配置
  工作目录         {}
  已加载文件       {}
  合并键数         {}",
            cwd.display(),
            runtime_config.loaded_entries().len(),
            runtime_config.merged().len()
        ),
        "发现的文件".to_string(),
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
            "已加载"
        } else {
            "缺失"
        };
        lines.push(format!(
            "  {source:<7} {status:<7} {}",
            entry.path.display()
        ));
    }

    if let Some(section) = section {
        lines.push(format!("合并的节: {section}"));
        let value = match section {
            "env" => runtime_config.get("env"),
            "hooks" => runtime_config.get("hooks"),
            "model" => runtime_config.get("model"),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins")),
            other => {
                lines.push(format!(
                    "  不支持的配置节 '{other}'。请使用 env、hooks、model 或 plugins。"
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
                None => "<未设置>".to_string(),
            }
        ));
        return Ok(lines.join(
            "
",
        ));
    }

    lines.push("合并的 JSON".to_string());
    lines.push(format!("  {}", runtime_config.as_json().render()));
    Ok(lines.join(
        "
",
    ))
}

pub(crate) fn render_config_json(
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
                    "error": format!("不支持的配置节 '{other}'。请使用 env、hooks、model 或 plugins。"),
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

pub(crate) fn render_memory_report() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, DEFAULT_DATE)?;
    let mut lines = vec![format!(
        "Memory 记忆
  工作目录         {}
  指令文件数       {}",
        cwd.display(),
        project_context.instruction_files.len()
    )];
    if project_context.instruction_files.is_empty() {
        lines.push("发现的文件".to_string());
        lines.push(
            "  在当前目录的祖先目录中未发现 CLAUDE 指令文件。"
                .to_string(),
        );
    } else {
        lines.push("发现的文件".to_string());
        for (index, file) in project_context.instruction_files.iter().enumerate() {
            let preview = file.content.lines().next().unwrap_or("").trim();
            let preview = if preview.is_empty() {
                "<空>"
            } else {
                preview
            };
            lines.push(format!("  {}. {}", index + 1, file.path.display(),));
            lines.push(format!(
                "     行数={} 预览={}",
                file.content.lines().count(),
                preview
            ));
        }
    }

    // 持久化记忆面(Persona / Human / Tasks 块 + 活跃条目)。
    // 加载并冻结,使渲染的快照与运行时注入当前会话系统提示的内容一致。
    // 当 memory.json 尚不存在时静默跳过 — 保持命令在全新工作区可用。
    let memory_path = cwd.join(".claw").join("memory.json");
    if memory_path.exists() {
        let memory = runtime::PersistentMemory::load_and_freeze(&memory_path);
        lines.push(String::new());
        lines.push("持久化记忆".to_string());
        lines.push(format!("  文件 {}", memory_path.display()));
        // 块摘要:标签 + 容量占比。
        for block in memory.blocks() {
            let cur = block.content().chars().count();
            let max = block.max_chars();
            let ratio = if max == 0 {
                0.0
            } else {
                cur as f64 / max as f64 * 100.0
            };
            let preview = block.content().lines().next().unwrap_or("").trim();
            let preview = if preview.is_empty() {
                "<空>"
            } else {
                preview
            };
            lines.push(format!(
                "  {} {}/{} 字符 ({:.0}%) 预览={}",
                block.label(),
                cur,
                max,
                ratio,
                preview
            ));
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let active = memory.active_entries(now_ms);
        lines.push(format!("  活跃条目         {}", active.len()));
        if !active.is_empty() {
            for entry in active.iter().take(10) {
                let marker = if entry.is_unverified(now_ms) {
                    "[未验证] "
                } else {
                    ""
                };
                let preview = entry.content.lines().next().unwrap_or("").trim();
                lines.push(format!("    - {marker}{preview}"));
            }
            if active.len() > 10 {
                lines.push(format!("    ... 还有 {} 条", active.len() - 10));
            }
        }
        lines.push(format!("  归档条目         {}", memory.archive().len()));
    }

    Ok(lines.join(
        "
",
    ))
}

pub(crate) fn render_memory_json() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
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

    // Surface persistent memory state alongside instruction files so the
    // JSON output matches the text report. Returns `null` for the
    // `persistent_memory` field when no memory.json exists yet.
    let memory_path = cwd.join(".claw").join("memory.json");
    let persistent_memory = if memory_path.exists() {
        let memory = runtime::PersistentMemory::load_and_freeze(&memory_path);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let active = memory.active_entries(now_ms);
        let blocks: Vec<_> = memory
            .blocks()
            .iter()
            .map(|b| {
                json!({
                    "label": b.label(),
                    "chars": b.content().chars().count(),
                    "max_chars": b.max_chars(),
                })
            })
            .collect();
        let active_entries: Vec<_> = active
            .iter()
            .map(|e| {
                json!({
                    "content": e.content,
                    "source": e.source,
                    "unverified": e.is_unverified(now_ms),
                })
            })
            .collect();
        Some(json!({
            "file": memory_path.display().to_string(),
            "blocks": blocks,
            "active_entries": active_entries,
            "active_count": active.len(),
            "archived_count": memory.archive().len(),
        }))
    } else {
        None
    };

    Ok(json!({
        "kind": "memory",
        "cwd": cwd.display().to_string(),
        "instruction_files": files.len(),
        "files": files,
        "persistent_memory": persistent_memory,
    }))
}

pub(crate) fn init_claude_md() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(initialize_repo(&cwd)?.render())
}

pub(crate) fn run_init(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
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
pub(crate) fn init_json_value(
    report: &crate::init::InitReport,
    message: &str,
) -> serde_json::Value {
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

pub(crate) fn normalize_permission_mode(mode: &str) -> Option<&'static str> {
    match mode.trim() {
        "read-only" => Some("read-only"),
        "workspace-write" => Some("workspace-write"),
        "danger-full-access" => Some("danger-full-access"),
        _ => None,
    }
}

pub(crate) fn render_diff_report() -> Result<String, Box<dyn std::error::Error>> {
    render_diff_report_for(&env::current_dir()?)
}

pub(crate) fn render_diff_report_for(cwd: &Path) -> Result<String, Box<dyn std::error::Error>> {
    // 在调用 `git diff` 之前先确认我们在 git 仓库内。
    // 在 git 仓库外运行 `git diff --cached` 会产生误导性的
    // "unknown option `cached`" 错误,因为 git 会回退到 --no-index 模式。
    let in_git_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_git_repo {
        return Ok(format!(
            "Diff\n  结果             无 git 仓库\n  详情             {} 不在 git 项目内",
            cwd.display()
        ));
    }
    let staged = run_git_diff_command_in(cwd, &["diff", "--cached"])?;
    let unstaged = run_git_diff_command_in(cwd, &["diff"])?;
    if staged.trim().is_empty() && unstaged.trim().is_empty() {
        return Ok(
            "Diff\n  结果             干净的工作树\n  详情             当前没有更改"
                .to_string(),
        );
    }

    let mut sections = Vec::new();
    if !staged.trim().is_empty() {
        sections.push(format!("已暂存的更改:\n{}", colorize_diff(&staged)));
    }
    if !unstaged.trim().is_empty() {
        sections.push(format!("未暂存的更改:\n{}", colorize_diff(&unstaged)));
    }

    Ok(format!("Diff\n\n{}", sections.join("\n\n")))
}

/// Colorize a git diff output: green for additions, red for deletions,
/// cyan for diff headers (diff --git, index, @@ hunk markers).
fn colorize_diff(diff: &str) -> String {
    let mut result = String::with_capacity(diff.len());
    for line in diff.lines() {
        if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("---")
            || line.starts_with("+++")
        {
            // File headers
            result.push_str(&format!("\x1b[36m{line}\x1b[0m\n"));
        } else if line.starts_with("@@") {
            // Hunk headers
            result.push_str(&format!("\x1b[1;36m{line}\x1b[0m\n"));
        } else if line.starts_with('+') {
            // Additions
            result.push_str(&format!("\x1b[32m{line}\x1b[0m\n"));
        } else if line.starts_with('-') {
            // Deletions
            result.push_str(&format!("\x1b[31m{line}\x1b[0m\n"));
        } else {
            // Context lines
            result.push_str(&format!("{line}\n"));
        }
    }
    result.trim_end().to_string()
}

/// Page long output through an external pager (`$PAGER` or `less`/`more`).
/// If output is short enough (fits in terminal height), prints directly.
/// Falls back to direct println on any pager failure.
pub(crate) fn page_long_output(content: &str) {
    // Get terminal height
    let term_height = crossterm::terminal::size()
        .map(|(_, h)| h as usize)
        .unwrap_or(24);

    let line_count = content.lines().count();

    // If content fits in terminal, print directly
    if line_count <= term_height.saturating_sub(2) {
        println!("{content}");
        return;
    }

    // Try external pager
    let pager = env::var("PAGER").ok().unwrap_or_else(|| "less".to_string());

    // For `less`, add flags: -R (raw control chars for colors), -F (quit if one screen), -X (no clear)
    let (cmd, args) = if pager == "less" {
        ("less", vec!["-R", "-F", "-X"])
    } else if pager == "more" {
        ("more", vec![])
    } else {
        // Custom pager: try to split on whitespace for command + args
        let parts: Vec<&str> = pager.split_whitespace().collect();
        if parts.is_empty() {
            println!("{content}");
            return;
        }
        (parts[0], parts[1..].to_vec())
    };

    let result = Command::new(cmd)
        .args(&args)
        .env("LESS", "RFX") // Ensure less respects colors
        .stdin(std::process::Stdio::piped())
        .spawn();

    match result {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(e) = stdin.write_all(content.as_bytes()) {
                    // Failed to write to pager, fall back to direct print
                    eprintln!("warning: pager write failed ({e}), printing directly");
                    println!("{content}");
                    return;
                }
            }
            match child.wait() {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("warning: pager wait failed ({e}), printing directly");
                    println!("{content}");
                }
            }
        }
        Err(_) => {
            // Pager not available, fall back to direct print
            println!("{content}");
        }
    }
}

pub(crate) fn render_diff_json_for(
    cwd: &Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
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
            "detail": format!("{} 不在 git 项目内", cwd.display()),
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

pub(crate) fn run_git_diff_command_in(
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

pub(crate) fn render_teleport_report(target: &str) -> Result<String, Box<dyn std::error::Error>> {
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

pub(crate) fn render_last_tool_debug_report(
    session: &Session,
) -> Result<String, Box<dyn std::error::Error>> {
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

pub(crate) fn indent_block(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn validate_no_args(
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

pub(crate) fn format_bughunter_report(scope: Option<&str>) -> String {
    format!(
        "Bughunter
  Scope            {}
  Action           inspect the selected code for likely bugs and correctness issues
  Output           findings should include file paths, severity, and suggested fixes",
        scope.unwrap_or("the current repository")
    )
}

pub(crate) fn format_ultraplan_report(task: Option<&str>) -> String {
    format!(
        "Ultraplan
  Task             {}
  Action           break work into a multi-step execution plan
  Output           plan should cover goals, risks, sequencing, verification, and rollback",
        task.unwrap_or("the current repo work")
    )
}

pub(crate) fn format_pr_report(branch: &str, context: Option<&str>) -> String {
    format!(
        "PR
  Branch           {branch}
  Context          {}
  Action           draft or create a pull request for the current branch
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

pub(crate) fn format_issue_report(context: Option<&str>) -> String {
    format!(
        "Issue
  Context          {}
  Action           draft or create a GitHub issue from the current context
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

pub(crate) fn truncate_for_prompt(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.trim().to_string()
    } else {
        let truncated = value.chars().take(limit).collect::<String>();
        format!("{}\n…[truncated]", truncated.trim_end())
    }
}

pub(crate) fn sanitize_generated_message(value: &str) -> String {
    value.trim().trim_matches('`').trim().replace("\r\n", "\n")
}

pub(crate) fn parse_titled_body(value: &str) -> Option<(String, String)> {
    let normalized = sanitize_generated_message(value);
    let title = normalized
        .lines()
        .find_map(|line| line.strip_prefix("TITLE:").map(str::trim))?;
    let body_start = normalized.find("BODY:")?;
    let body = normalized[body_start + "BODY:".len()..].trim();
    Some((title.to_string(), body.to_string()))
}

pub(crate) fn render_version_report() -> String {
    let git_sha = GIT_SHA.unwrap_or("unknown");
    let target = BUILD_TARGET.unwrap_or("unknown");
    format!(
        "Claw Code\n  Version          {VERSION}\n  Git SHA          {git_sha}\n  Target           {target}\n  Build date       {DEFAULT_DATE}"
    )
}

pub(crate) fn render_export_text(session: &Session) -> String {
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

/// 把毫秒时间戳渲染为人类可读的相对时长（如 "3m 25s"、"1h 5m"、"2d 4h"）。
#[allow(dead_code)]
pub(crate) fn format_age_ms(started_at_ms: u64) -> String {
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

pub(crate) fn format_status_bar(model: &str, cwd: &str, usage: runtime::TokenUsage) -> String {
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
pub(crate) fn shorten_cwd_for_statusbar(cwd: &Path) -> String {
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

#[allow(clippy::too_many_lines)]
pub(crate) fn print_help_to(out: &mut impl Write) -> io::Result<()> {
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
        "      `claw acp serve` starts a stdio ACP JSON-RPC server for editor integration; aliases: --acp, -acp"
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

pub(crate) fn print_help(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
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

/// G1.22: Typed JSON error envelope contract.
///
/// Produces `{"type":"error","error":{"kind":...,"hint":...,"retryable":...}}`.
#[derive(serde::Serialize)]
pub(crate) struct TypedErrorEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub error: TypedErrorDetail,
}

#[derive(serde::Serialize)]
pub(crate) struct TypedErrorDetail {
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errno: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub retryable: bool,
}

#[must_use]
pub(crate) fn json_error_envelope(message: &str) -> String {
    serde_json::to_string(&TypedErrorEnvelope {
        envelope_type: "error".to_string(),
        error: TypedErrorDetail {
            kind: "execution".to_string(),
            message: message.to_string(),
            operation: None,
            target: None,
            errno: None,
            hint: None,
            retryable: false,
        },
    })
    .unwrap_or_else(|_| serde_json::json!({"type":"error","error":message}).to_string())
}

