//! Diagnostics, health checks, boot preflight.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use api::{cache_break_root, CacheBreakReasons, CacheBreakStats};
use runtime::{
    format_stale_base_warning, resolve_sandbox_status, BaseCommitState, ConfigLoader, McpServer,
    McpServerSpec, McpTool, ProjectContext, TokenUsage,
};
// Epic 3:policy_engine + green_contract 接入 doctor 作为 smoke test。
// - PolicyEngine/LaneContext/PolicyAction 等通过 runtime 顶层 re-export 拿到
//   (policy_engine 模块本身私有,但类型已 pub use)。
// - green_contract 是 pub mod,直接走完整路径;GreenLevel 在两个模块定义不同
//   (policy_engine 是 u8 别名,green_contract 是 enum),这里按需用 enum 版本。
use runtime::green_contract::{GreenContract, GreenLevel as ContractGreenLevel};
use runtime::{LaneContext, PolicyEngine};
// Epic 4:lane_events + g004_conformance + report_schema + branch_lock 接入。
// - lane_events:模块私有,但 try_publish/drain_lane_events + LaneEvent/LaneEventName/
//   LaneEventStatus 已通过 runtime 顶层 re-export(Epic 4 新增 try_publish/drain 导出)。
// - g004_conformance:pub mod,validate_g004_contract_bundle 走模块路径。
// - report_schema:模块私有,但全部类型通过 runtime 顶层 re-export。
// - branch_lock:pub mod,detect_branch_lock_collisions/BranchLockIntent 走模块路径或顶层均可。
use runtime::branch_lock::{detect_branch_lock_collisions, BranchLockIntent};
use runtime::g004_conformance::validate_g004_contract_bundle;
use runtime::{
    canonicalize_report, drain_lane_events, report_content_hash, try_publish, CanonicalReportV1,
    ClaimKind, DiscoveryResult, LaneEvent, LaneEventName, LaneEventStatus, McpConnectionStatus,
    McpResourceInfo, McpToolInfo, McpToolRegistry, PluginHealthcheck, PluginLifecycle, PluginState,
    ReportClaim, ReportConfidence, ReportIdentity, RuntimePluginConfig, SensitivityClass,
    REPORT_SCHEMA_V1,
};
// Epic 6:team_cron_registry smoke test 接入 — Team + Cron 两个 registry 的完整 API 验证。
use runtime::{CronEntry, CronRegistry, Team, TeamRegistry, TeamStatus};
use serde_json::{json, Map, Value};
use tools::{execute_tool, mvp_tool_specs};

use crate::session_mgr::{SessionLifecycleKind, SessionLifecycleSummary};
use crate::{
    stale_base_json_value, stale_base_state_for, CliOutputFormat, BUILD_TARGET, DEFAULT_DATE,
    DEPRECATED_INSTALL_COMMAND, GIT_SHA, OFFICIAL_REPO_SLUG, OFFICIAL_REPO_URL, VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosticLevel {
    Ok,
    Warn,
    Fail,
}

impl DiagnosticLevel {
    // 文本渲染用的中文标签;JSON 仍走 label() 以保持机器可读兼容性。
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    // 用户可见的中文状态标签,用于文本报告渲染。
    fn display_label(self) -> &'static str {
        match self {
            Self::Ok => "正常",
            Self::Warn => "警告",
            Self::Fail => "失败",
        }
    }

    fn is_failure(self) -> bool {
        matches!(self, Self::Fail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiagnosticCheck {
    pub(crate) name: &'static str,
    pub(crate) level: DiagnosticLevel,
    pub(crate) summary: String,
    pub(crate) details: Vec<String>,
    pub(crate) data: Map<String, Value>,
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
pub(crate) struct DoctorReport {
    pub(crate) checks: Vec<DiagnosticCheck>,
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

    pub(crate) fn render(&self) -> String {
        let (ok_count, warn_count, fail_count) = self.counts();
        let mut lines = vec![
            "Doctor 诊断报告".to_string(),
            format!(
                "摘要\n  正常             {ok_count}\n  警告             {warn_count}\n  失败             {fail_count}"
            ),
        ];
        lines.extend(self.checks.iter().map(render_diagnostic_check));
        lines.join("\n\n")
    }

    pub(crate) fn json_value(&self) -> Value {
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

pub(crate) fn render_diagnostic_check(check: &DiagnosticCheck) -> String {
    let mut lines = vec![format!(
        "{}\n  状态             {}\n  摘要             {}",
        check.name,
        check.level.display_label(),
        check.summary
    )];
    if !check.details.is_empty() {
        lines.push("  详情".to_string());
        lines.extend(check.details.iter().map(|detail| format!("    - {detail}")));
    }
    lines.join("\n")
}

pub(crate) fn render_doctor_report() -> Result<DoctorReport, Box<dyn std::error::Error>> {
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
            // Epic 3:policy_engine + green_contract smoke test。
            // 这两项验证两个模块在生产 binary 中可被调用(不再死代码),
            // 同时为未来 lane 事件流接入提供 baseline。
            check_policy_engine_health(),
            check_green_contract_health(),
            // Epic 4:lane_events + g004_conformance + report_schema + branch_lock smoke test。
            // 四项验证模块 API 可用,激活死代码。
            check_lane_events_health(),
            check_g004_conformance_health(),
            check_canonical_report_v1_health(),
            check_branch_lock_health(),
            // Epic 5:plugin_lifecycle + mcp_tool_bridge 打破死链 smoke test。
            check_plugin_lifecycle_health(),
            check_mcp_tool_bridge_health(),
            // Epic 6:team_cron_registry smoke test — Team + Cron registry API 验证。
            check_team_cron_registry_health(),
        ],
    })
}

pub(crate) fn run_doctor(
    output_format: CliOutputFormat,
    cache_stats: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if cache_stats {
        return run_doctor_cache_stats(output_format);
    }
    let report = render_doctor_report()?;
    let message = report.render();
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report.json_value())?);
        }
    }
    if report.has_failures() {
        return Err("doctor 发现失败的检查项".into());
    }
    Ok(())
}

/// `claw doctor --cache-stats`:服务端 prefix cache 命中率下降检测。
///
/// 汇总所有 session 的 cache break 统计,按根因分类显示。
/// 数据来源:`streaming.rs::record_cache_break` 每次请求后写入的
/// `~/.claude/cache/prompt-cache/<session>/stats.json`。
///
/// 主 agent 请求写入 `<session>/stats.json`,子智能体请求写入
/// `subagent-<session>/stats.json`。本命令将两类 session **分开聚合**:
/// 主汇总只统计非 `subagent-` 前缀的目录(不再被子智能体统计污染),
/// 子智能体在独立一节汇总展示,每个 session(含 `subagent-{session}`)
/// 在明细中独立成行,`last_response` 按 stats.json 修改时间取最新并标注
/// 所属 session 名(不再受 read_dir 顺序影响)。
fn run_doctor_cache_stats(
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = cache_break_root();
    let sessions = collect_session_cache_stats(&root);

    let mut main_sessions = Vec::new();
    let mut subagent_sessions = Vec::new();
    for session in sessions {
        if session.is_subagent {
            subagent_sessions.push(session);
        } else {
            main_sessions.push(session);
        }
    }

    let mut main_agg = CacheStatsAggregate::default();
    for session in &main_sessions {
        main_agg.push(&session.stats);
    }
    let mut subagent_agg = CacheStatsAggregate::default();
    for session in &subagent_sessions {
        subagent_agg.push(&session.stats);
    }

    match output_format {
        CliOutputFormat::Text => {
            if main_agg.session_count + subagent_agg.session_count == 0 {
                println!("Cache Break 监控:暂无 session stats 文件。");
                println!("  提示:运行一次 `claw` 对话后,stats 会在每次请求后持久化到:");
                println!("  {}", root.display());
                return Ok(());
            }
            println!(
                "{}",
                render_cache_stats_text(
                    &root,
                    &main_sessions,
                    &subagent_sessions,
                    &main_agg,
                    &subagent_agg,
                )
            );
        }
        CliOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&render_cache_stats_json(
                    &main_sessions,
                    &subagent_sessions,
                    &main_agg,
                    &subagent_agg,
                ))?
            );
        }
    }
    Ok(())
}

/// 单个 session 的缓存统计快照(doctor 展示用)。
#[derive(Debug, Clone)]
struct SessionCacheStatsSnapshot {
    name: String,
    is_subagent: bool,
    stats: CacheBreakStats,
    modified_at: Option<SystemTime>,
}

/// 一组 session 的聚合结果(主 / subagent 分开聚合)。
#[derive(Debug, Default)]
struct CacheStatsAggregate {
    session_count: u32,
    tracked_requests: u64,
    read_tokens: u64,
    creation_tokens: u64,
    unexpected_breaks: u64,
    expected_invalidations: u64,
    breaks: CacheBreakReasons,
}

impl CacheStatsAggregate {
    fn push(&mut self, stats: &CacheBreakStats) {
        self.session_count += 1;
        self.tracked_requests += stats.tracked_requests;
        self.read_tokens += stats.total_cache_read_input_tokens;
        self.creation_tokens += stats.total_cache_creation_input_tokens;
        self.unexpected_breaks += stats.unexpected_cache_breaks;
        self.expected_invalidations += stats.expected_invalidations;
        self.breaks.model_changed += stats.break_reasons.model_changed;
        self.breaks.system_prompt_changed += stats.break_reasons.system_prompt_changed;
        self.breaks.tool_definitions_changed += stats.break_reasons.tool_definitions_changed;
        self.breaks.message_payload_changed += stats.break_reasons.message_payload_changed;
        self.breaks.ttl_expiry += stats.break_reasons.ttl_expiry;
        self.breaks.unknown += stats.break_reasons.unknown;
    }

    fn hit_rate_pct(&self) -> u64 {
        hit_rate_pct(self.read_tokens, self.creation_tokens)
    }
}

fn hit_rate_pct(read_tokens: u64, creation_tokens: u64) -> u64 {
    let total = read_tokens + creation_tokens;
    if total == 0 {
        0
    } else {
        (read_tokens as f64 * 100.0 / total as f64).round() as u64
    }
}

/// 扫描 cache stats 根目录,收集所有含 stats.json 的 session 快照。
fn collect_session_cache_stats(root: &Path) -> Vec<SessionCacheStatsSnapshot> {
    let mut sessions = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let stats_path = entry.path().join("stats.json");
        if !stats_path.exists() {
            continue;
        }
        let Ok(raw) = fs::read(&stats_path) else {
            continue;
        };
        let Ok(stats) = serde_json::from_slice::<CacheBreakStats>(&raw) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_subagent = name.starts_with("subagent-");
        let modified_at = fs::metadata(&stats_path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        sessions.push(SessionCacheStatsSnapshot {
            name,
            is_subagent,
            stats,
            modified_at,
        });
    }
    sessions
}

/// 取一组 session 中 stats.json 修改时间最新的一条(`last_response` 归属)。
fn newest_session(sessions: &[SessionCacheStatsSnapshot]) -> Option<&SessionCacheStatsSnapshot> {
    sessions.iter().max_by_key(|session| session.modified_at)
}

fn render_cache_stats_text(
    root: &Path,
    main_sessions: &[SessionCacheStatsSnapshot],
    subagent_sessions: &[SessionCacheStatsSnapshot],
    main_agg: &CacheStatsAggregate,
    subagent_agg: &CacheStatsAggregate,
) -> String {
    let mut lines = vec![format!(
        "Cache Break 监控(主 session {} 个,subagent session {} 个;根目录 {})",
        main_agg.session_count,
        subagent_agg.session_count,
        root.display()
    )];
    lines.push(String::new());
    lines.push("== Cache Break 原因分布(主 session,排除 subagent)==".to_string());
    lines.extend(render_break_reasons_lines(&main_agg.breaks));
    lines.push(String::new());
    lines.push("== Token 累计(主 session,排除 subagent)==".to_string());
    lines.extend(render_token_lines(main_agg));
    lines.push(String::new());
    lines.push("== 异常检测(主 session)==".to_string());
    lines.extend(render_anomaly_lines(
        main_agg,
        newest_session(main_sessions),
    ));
    if subagent_agg.session_count > 0 {
        lines.push(String::new());
        lines.push("== Subagent 汇总(subagent-session-* 独立统计)==".to_string());
        lines.extend(render_subagent_lines(
            subagent_agg,
            newest_session(subagent_sessions),
        ));
    }
    lines.push(String::new());
    lines.push("== Session 明细(按 stats.json 修改时间倒序;主/子独立成行)==".to_string());
    let mut all_sessions = main_sessions
        .iter()
        .chain(subagent_sessions.iter())
        .collect::<Vec<_>>();
    all_sessions.sort_by_key(|b| std::cmp::Reverse(b.modified_at));
    for session in all_sessions {
        lines.push(render_session_line(session));
    }
    lines.join("\n")
}

fn render_break_reasons_lines(breaks: &CacheBreakReasons) -> Vec<String> {
    let total = breaks.total();
    let mut lines = vec![format!("  总 break 事件:{total}")];
    if total > 0 {
        let pct = |n: u64| (n as f64 * 100.0 / total as f64).round() as u64;
        lines.push(format!(
            "    model_changed           : {:>5} ({:>3}%)",
            breaks.model_changed,
            pct(breaks.model_changed)
        ));
        lines.push(format!(
            "    system_prompt_changed   : {:>5} ({:>3}%)  ← 动态值泄漏到静态区",
            breaks.system_prompt_changed,
            pct(breaks.system_prompt_changed)
        ));
        lines.push(format!(
            "    tool_definitions_changed: {:>5} ({:>3}%)  ← 动态值泄漏到静态区",
            breaks.tool_definitions_changed,
            pct(breaks.tool_definitions_changed)
        ));
        lines.push(format!(
            "    message_payload_changed : {:>5} ({:>3}%)  (正常,每 turn 都变)",
            breaks.message_payload_changed,
            pct(breaks.message_payload_changed)
        ));
        lines.push(format!(
            "    ttl_expiry              : {:>5} ({:>3}%)  (provider 侧 TTL)",
            breaks.ttl_expiry,
            pct(breaks.ttl_expiry)
        ));
        lines.push(format!(
            "    unknown                 : {:>5} ({:>3}%)  ← 指纹未变但命中率下降",
            breaks.unknown,
            pct(breaks.unknown)
        ));
    }
    lines
}

fn render_token_lines(agg: &CacheStatsAggregate) -> Vec<String> {
    vec![
        format!("  tracked_requests          : {}", agg.tracked_requests),
        format!("  total_cache_read_tokens   : {} (命中)", agg.read_tokens),
        format!(
            "  total_cache_creation_tokens: {} (未命中写入)",
            agg.creation_tokens
        ),
        format!("  累计命中率:{}%", agg.hit_rate_pct()),
    ]
}

fn render_anomaly_lines(
    agg: &CacheStatsAggregate,
    newest: Option<&SessionCacheStatsSnapshot>,
) -> Vec<String> {
    let mut lines = vec![
        format!(
            "  unexpected_cache_breaks   : {} (指纹未变但命中率突降)",
            agg.unexpected_breaks
        ),
        format!(
            "  expected_invalidations    : {} (指纹变化导致的预期失效)",
            agg.expected_invalidations
        ),
    ];
    if let Some(session) = newest {
        if let Some(reason) = &session.stats.last_break_reason {
            lines.push(format!(
                "  last_break_reason         : {reason} [{}]",
                session.name
            ));
        }
        if let (Some(read), Some(creation)) = (
            session.stats.last_cache_read_input_tokens,
            session.stats.last_cache_creation_input_tokens,
        ) {
            lines.push(format!(
                "  last_response             : [{}] read={} creation={} hit_rate={}%",
                session.name,
                read,
                creation,
                hit_rate_pct(u64::from(read), u64::from(creation))
            ));
        }
    }
    lines
}

fn render_subagent_lines(
    agg: &CacheStatsAggregate,
    newest: Option<&SessionCacheStatsSnapshot>,
) -> Vec<String> {
    let mut lines = vec![
        format!("  子 session 数            : {}", agg.session_count),
        format!("  tracked_requests          : {}", agg.tracked_requests),
        format!("  total_cache_read_tokens   : {} (命中)", agg.read_tokens),
        format!(
            "  total_cache_creation_tokens: {} (未命中写入)",
            agg.creation_tokens
        ),
        format!("  累计命中率:{}%", agg.hit_rate_pct()),
        format!("  总 break 事件             : {}", agg.breaks.total()),
    ];
    if let Some(session) = newest {
        if let (Some(read), Some(creation)) = (
            session.stats.last_cache_read_input_tokens,
            session.stats.last_cache_creation_input_tokens,
        ) {
            lines.push(format!(
                "  last_response             : [{}] read={} creation={} hit_rate={}%",
                session.name,
                read,
                creation,
                hit_rate_pct(u64::from(read), u64::from(creation))
            ));
        }
    }
    lines
}

fn render_session_line(session: &SessionCacheStatsSnapshot) -> String {
    let kind = if session.is_subagent { "子" } else { "主" };
    format!(
        "  [{kind}] {:48} : tracked={} read={} creation={} hit={}% unexpected={} expected={} last_break={}",
        session.name,
        session.stats.tracked_requests,
        session.stats.total_cache_read_input_tokens,
        session.stats.total_cache_creation_input_tokens,
        hit_rate_pct(
            session.stats.total_cache_read_input_tokens,
            session.stats.total_cache_creation_input_tokens,
        ),
        session.stats.unexpected_cache_breaks,
        session.stats.expected_invalidations,
        session.stats.last_break_reason.as_deref().unwrap_or("<无>"),
    )
}

fn break_reasons_json(breaks: &CacheBreakReasons) -> Value {
    json!({
        "total": breaks.total(),
        "model_changed": breaks.model_changed,
        "system_prompt_changed": breaks.system_prompt_changed,
        "tool_definitions_changed": breaks.tool_definitions_changed,
        "message_payload_changed": breaks.message_payload_changed,
        "ttl_expiry": breaks.ttl_expiry,
        "unknown": breaks.unknown,
    })
}

fn render_cache_stats_json(
    main_sessions: &[SessionCacheStatsSnapshot],
    subagent_sessions: &[SessionCacheStatsSnapshot],
    main_agg: &CacheStatsAggregate,
    subagent_agg: &CacheStatsAggregate,
) -> Value {
    let last_response_json = |newest: Option<&SessionCacheStatsSnapshot>| {
        newest.map(|session| {
            json!({
                "session": session.name,
                "is_subagent": session.is_subagent,
                "read_input_tokens": session.stats.last_cache_read_input_tokens,
                "creation_input_tokens": session.stats.last_cache_creation_input_tokens,
                "hit_rate_pct": match (
                    session.stats.last_cache_read_input_tokens,
                    session.stats.last_cache_creation_input_tokens,
                ) {
                    (Some(read), Some(creation)) => {
                        hit_rate_pct(u64::from(read), u64::from(creation))
                    }
                    _ => 0,
                },
            })
        })
    };
    let group_json = |agg: &CacheStatsAggregate, newest: Option<&SessionCacheStatsSnapshot>| {
        json!({
            "session_count": agg.session_count,
            "break_reasons": break_reasons_json(&agg.breaks),
            "tokens": {
                "tracked_requests": agg.tracked_requests,
                "total_cache_read_input_tokens": agg.read_tokens,
                "total_cache_creation_input_tokens": agg.creation_tokens,
                "cumulative_hit_rate_pct": agg.hit_rate_pct(),
            },
            "anomalies": {
                "unexpected_cache_breaks": agg.unexpected_breaks,
                "expected_invalidations": agg.expected_invalidations,
                "last_break_reason": newest.and_then(|s| s.stats.last_break_reason.clone()),
                "last_response": last_response_json(newest),
            },
        })
    };

    let mut all_sessions = main_sessions
        .iter()
        .chain(subagent_sessions.iter())
        .collect::<Vec<_>>();
    all_sessions.sort_by_key(|b| std::cmp::Reverse(b.modified_at));
    let sessions_json = all_sessions
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "is_subagent": s.is_subagent,
                "modified_at_unix_secs": s
                    .modified_at
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs()),
                "tracked_requests": s.stats.tracked_requests,
                "total_cache_read_input_tokens": s.stats.total_cache_read_input_tokens,
                "total_cache_creation_input_tokens": s.stats.total_cache_creation_input_tokens,
                "hit_rate_pct": hit_rate_pct(
                    s.stats.total_cache_read_input_tokens,
                    s.stats.total_cache_creation_input_tokens,
                ),
                "unexpected_cache_breaks": s.stats.unexpected_cache_breaks,
                "expected_invalidations": s.stats.expected_invalidations,
                "last_break_reason": s.stats.last_break_reason,
            })
        })
        .collect::<Vec<_>>();

    json!({
        // 总 session 数(含 subagent),与旧版字段保持兼容;主/子拆分数见下文。
        "session_count": main_agg.session_count + subagent_agg.session_count,
        "main_session_count": main_agg.session_count,
        "subagent_session_count": subagent_agg.session_count,
        // 顶层 break_reasons/tokens/anomalies 只统计主 session(排除 subagent),
        // 修复子智能体统计污染主 session 汇总的问题。
        "break_reasons": break_reasons_json(&main_agg.breaks),
        "tokens": {
            "tracked_requests": main_agg.tracked_requests,
            "total_cache_read_input_tokens": main_agg.read_tokens,
            "total_cache_creation_input_tokens": main_agg.creation_tokens,
            "cumulative_hit_rate_pct": main_agg.hit_rate_pct(),
        },
        "anomalies": {
            "unexpected_cache_breaks": main_agg.unexpected_breaks,
            "expected_invalidations": main_agg.expected_invalidations,
            "last_break_reason": newest_session(main_sessions)
                .and_then(|s| s.stats.last_break_reason.clone()),
            "last_response": last_response_json(newest_session(main_sessions)),
        },
        "subagent": group_json(subagent_agg, newest_session(subagent_sessions)),
        "sessions": sessions_json,
    })
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
pub(crate) fn run_worker_state(
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
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
            "未找到 worker 状态文件:{path}\n  提示:worker 状态由交互式 REPL 或非交互式 prompt 写入。\n  运行:  claw               # 启动 REPL(首次对话时写入状态)\n  或:    claw prompt <text> # 运行一次非交互式对话\n  然后重试:claw state [--output-format json]",
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
pub(crate) fn run_mcp_serve() -> Result<(), Box<dyn std::error::Error>> {
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
pub(crate) fn check_auth_health() -> DiagnosticCheck {
    let api_key_present = env::var("DEEPSEEK_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let env_details = format!(
        "环境变量          DEEPSEEK_API_KEY={}",
        if api_key_present {
            "已配置"
        } else {
            "缺失"
        }
    );

    DiagnosticCheck::new(
        "Auth",
        if api_key_present {
            DiagnosticLevel::Ok
        } else {
            DiagnosticLevel::Warn
        },
        if api_key_present {
            "DEEPSEEK_API_KEY 已配置"
        } else {
            "未找到 DEEPSEEK_API_KEY 环境变量"
        },
    )
    .with_details(vec![
        env_details,
        "建议操作          设置 DEEPSEEK_API_KEY 环境变量".to_string(),
    ])
    .with_data(Map::from_iter([(
        "api_key_present".to_string(),
        json!(api_key_present),
    )]))
}

pub(crate) fn check_config_health(
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
                "配置文件          已加载 {}/{}",
                loaded_count, present_count
            )];
            if let Some(model) = runtime_config.model() {
                details.push(format!("解析的模型        {model}"));
            }
            details.push(format!(
                "MCP 服务器        {}",
                runtime_config.mcp().servers().len()
            ));
            if present_paths.is_empty() {
                details.push("发现的文件        <无>(使用默认配置)".to_string());
            } else {
                details.extend(
                    present_paths
                        .iter()
                        .map(|path| format!("发现的文件        {path}")),
                );
            }
            DiagnosticCheck::new(
                "Config",
                DiagnosticLevel::Ok,
                if present_count == 0 {
                    "没有配置文件;使用默认配置"
                } else {
                    "运行时配置加载成功"
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
            format!("运行时配置加载失败: {error}"),
        )
        .with_details(if discovered_paths.is_empty() {
            vec!["发现的文件        <无>".to_string()]
        } else {
            discovered_paths
                .iter()
                .map(|path| format!("发现的文件        {path}"))
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

pub(crate) fn check_install_source_health() -> DiagnosticCheck {
    DiagnosticCheck::new(
        "Install source",
        DiagnosticLevel::Ok,
        format!(
            "官方来源是 {OFFICIAL_REPO_SLUG};避免使用 `{DEPRECATED_INSTALL_COMMAND}`"
        ),
    )
    .with_details(vec![
        format!("官方仓库          {OFFICIAL_REPO_URL}"),
        "推荐路径          从本仓库构建或使用 README.md 中记录的上游二进制"
            .to_string(),
        format!(
            "已弃用的 crate    `{DEPRECATED_INSTALL_COMMAND}` 安装的是已弃用的占位包,不提供 `claw` 二进制"
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

pub(crate) fn check_workspace_health(context: &StatusContext) -> DiagnosticCheck {
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
                "在分支 {} 上检测到项目根目录",
                context.git_branch.as_deref().unwrap_or("未知")
            )
        } else {
            "当前目录不在 git 项目内".to_string()
        },
    )
    .with_details(vec![
        format!("当前目录          {}", context.cwd.display()),
        format!(
            "项目根目录        {}",
            context
                .project_root
                .as_ref()
                .map_or_else(|| "<无>".to_string(), |path| path.display().to_string())
        ),
        format!(
            "Git 分支          {}",
            context.git_branch.as_deref().unwrap_or("未知")
        ),
        format!("Git 状态          {}", context.git_summary.headline()),
        format!("已更改文件        {}", context.git_summary.changed_files),
        format!(
            "Memory 文件       {} · 配置文件已加载 {}/{}",
            context.memory_file_count, context.loaded_config_files, context.discovered_config_files
        ),
        format!(
            "Stale base        {}",
            stale_base_warning.as_deref().unwrap_or("正常")
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

pub(crate) fn check_boot_preflight_health(context: &StatusContext) -> DiagnosticCheck {
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
                "控制套接字        {} configured={} exists={} path={}",
                socket.name,
                socket.configured,
                socket.exists,
                socket.path.as_deref().unwrap_or("<无>")
            )
        })
        .collect::<Vec<_>>();
    let mut details = vec![
        format!("仓库存在          {}", preflight.repo_exists),
        format!("工作树存在        {}", preflight.worktree_exists),
        format!("Git 目录存在      {}", preflight.git_dir_exists),
        format!("分支落后          {}", preflight.branch_freshness.behind),
        format!("信任白名单        {:?}", preflight.trust_gate_allowed),
        format!("受信任根数        {}", preflight.trusted_roots_count),
        format!(
            "MCP 可启动        {} · 服务器 {}",
            preflight.mcp_startup_eligible, preflight.mcp_servers_configured
        ),
        format!(
            "插件可启动        {} · 已配置 {}",
            preflight.plugin_startup_eligible, preflight.plugins_configured
        ),
        format!(
            "上次启动失败原因  {}",
            preflight
                .last_failed_boot_reason
                .as_deref()
                .unwrap_or("<无>")
        ),
    ];
    details.extend(preflight.required_binaries.iter().map(|binary| {
        format!(
            "必需二进制        {} available={}",
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

pub(crate) fn check_sandbox_health(status: &runtime::SandboxStatus) -> DiagnosticCheck {
    let platform_supported = runtime::platform_sandbox_supported();
    let degraded = status.enabled && !status.active;
    let mut details = vec![
        format!("已启用            {}", status.enabled),
        format!("已激活            {}", status.active),
        format!("受支持            {}", status.supported),
        format!("文件系统模式      {}", status.filesystem_mode.as_str()),
        format!("文件系统已激活    {}", status.filesystem_active),
    ];
    if let Some(reason) = &status.fallback_reason {
        details.push(format!("降级原因          {reason}"));
    }

    // 级别判定:平台无任何沙箱机制时升级为 Fail;平台支持但隔离未激活保持 Warn
    let (level, message) = if status.enabled && status.active {
        (DiagnosticLevel::Ok, "沙箱保护已激活")
    } else if status.enabled && !platform_supported {
        (
            DiagnosticLevel::Fail,
            "已请求沙箱但当前平台无任何沙箱机制，命令将无隔离执行",
        )
    } else if status.enabled {
        // status.enabled && !status.active && platform_supported
        (DiagnosticLevel::Warn, "平台支持沙箱但请求的隔离类型未激活")
    } else {
        (DiagnosticLevel::Ok, "当前会话未激活沙箱")
    };

    DiagnosticCheck::new("Sandbox", level, message)
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

pub(crate) fn check_system_health(
    cwd: &Path,
    config: Option<&runtime::RuntimeConfig>,
) -> DiagnosticCheck {
    let default_model = config.and_then(runtime::RuntimeConfig::model);
    let mut details = vec![
        format!(
            "操作系统          {} {}",
            env::consts::OS,
            env::consts::ARCH
        ),
        format!("工作目录          {}", cwd.display()),
        format!("版本              {}", VERSION),
        format!("构建目标          {}", BUILD_TARGET.unwrap_or("<未知>")),
        format!("Git SHA           {}", GIT_SHA.unwrap_or("<未知>")),
    ];
    if let Some(model) = default_model {
        details.push(format!("默认模型          {model}"));
    }
    DiagnosticCheck::new("System", DiagnosticLevel::Ok, "已捕获本地运行时元数据")
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

/// Epic 3:policy_engine smoke test。
///
/// 构造一个空规则集的 PolicyEngine 和一个 reconciled LaneContext,
/// 验证 evaluate / evaluate_with_events API 在生产 binary 中可正常调用。
/// 这把 policy_engine 从"死代码"激活为"doctor 可观察的健康检查项",
/// 同时为未来 lane 事件流接入(Plan §9.2 Epic 3)提供 baseline。
///
/// 当前不加载任何 PolicyRule(规则集为空),因此 evaluate 总是返回空 Vec。
/// 真正的规则加载需要 `.claw/policy.json` 配置文件支持,留待后续 Epic。
pub(crate) fn check_policy_engine_health() -> DiagnosticCheck {
    let engine = PolicyEngine::new(Vec::new());
    let context = LaneContext::reconciled("doctor-smoke");
    let actions = engine.evaluate(&context);
    let evaluation = engine.evaluate_with_events(&context);

    DiagnosticCheck::new(
        "PolicyEngine",
        DiagnosticLevel::Ok,
        "policy 引擎 evaluate/evaluate_with_events 可调用",
    )
    .with_details(vec![
        format!("已配置规则数      {}", engine.rules().len()),
        format!("已发出动作数      {}", actions.len()),
        format!("决策事件数        {}", evaluation.events.len()),
        format!("smoke 上下文      lane_id={}", context.lane_id),
    ])
    .with_data(Map::from_iter([
        ("rules_count".to_string(), json!(engine.rules().len())),
        ("actions_count".to_string(), json!(actions.len())),
        ("events_count".to_string(), json!(evaluation.events.len())),
        ("smoke_lane_id".to_string(), json!(context.lane_id)),
    ]))
}

/// Epic 3:green_contract smoke test。
///
/// 构造 `GreenContract::merge_ready(Workspace)` 作为项目默认契约,
/// 验证 evaluate 在 satisfied/unsatisfied 两种输入下都能正常返回 outcome。
/// 这把 green_contract 从"死代码"激活为"doctor 可观察的健康检查项"。
///
/// 当前不读取项目配置文件,固定使用 merge_ready + Workspace 作为 smoke。
/// 真正的契约配置需要 `.claw/green-contract.toml` 支持,留待后续 Epic。
pub(crate) fn check_green_contract_health() -> DiagnosticCheck {
    let contract = GreenContract::merge_ready(ContractGreenLevel::Workspace);
    let satisfied_outcome = contract.evaluate(Some(ContractGreenLevel::Workspace));
    let unsatisfied_outcome = contract.evaluate(None);

    let level = if satisfied_outcome.is_satisfied() && !unsatisfied_outcome.is_satisfied() {
        DiagnosticLevel::Ok
    } else {
        DiagnosticLevel::Warn
    };

    DiagnosticCheck::new(
        "GreenContract",
        level,
        "green 契约 evaluate 可调用(merge_ready/Workspace 基线)",
    )
    .with_details(vec![
        format!("要求的等级        {}", ContractGreenLevel::Workspace),
        format!(
            "满足时结果        {}",
            if satisfied_outcome.is_satisfied() {
                "已满足"
            } else {
                "未满足"
            }
        ),
        format!(
            "不满足时结果      {}",
            if unsatisfied_outcome.is_satisfied() {
                "已满足"
            } else {
                "未满足"
            }
        ),
        format!("要求数量          {}", contract.requirements.len()),
    ])
    .with_data(Map::from_iter([
        ("required_level".to_string(), json!("workspace")),
        ("contract_kind".to_string(), json!("merge_ready")),
        (
            "satisfied_when_workspace".to_string(),
            json!(satisfied_outcome.is_satisfied()),
        ),
        (
            "unsatisfied_when_none".to_string(),
            json!(!unsatisfied_outcome.is_satisfied()),
        ),
    ]))
}

/// Epic 4:lane_events smoke test。
///
/// 构造一个 `LaneEvent::started` 事件,通过 `try_publish` 写入 process-wide sink,
/// 然后用 `drain_lane_events` 取回,验证 publish/consume 往返链路可用。
/// 这把 lane_events 从"仅 4 处生产发布点无消费者"激活为"doctor 可观察的健康检查项"。
///
/// 注意:try_publish/drain_lane_events 操作全局静态 sink,doctor 调用时会清空
/// 当前 sink 中累积的事件(这是 drain 语义)。doctor 本身不依赖 sink 状态,
/// 因此清空副作用可接受。
pub(crate) fn check_lane_events_health() -> DiagnosticCheck {
    // 先 drain 清空 sink,确保 smoke 事件能被干净取回。
    let _ = drain_lane_events();

    let event = LaneEvent::started("doctor-smoke");
    let published = try_publish(event);
    let drained = drain_lane_events();
    let smoke_event = drained
        .iter()
        .find(|e| e.event == LaneEventName::Started)
        .cloned();

    let level = if published && smoke_event.is_some() {
        DiagnosticLevel::Ok
    } else {
        DiagnosticLevel::Warn
    };

    DiagnosticCheck::new(
        "LaneEvents",
        level,
        "lane 事件 try_publish/drain_lane_events 往返链路可调用",
    )
    .with_details(vec![
        format!("已发布            {}", published),
        format!("已取出数量        {}", drained.len()),
        format!(
            "smoke 事件已找到  {}",
            if smoke_event.is_some() { "是" } else { "否" }
        ),
        format!("sink 容量         512 (进程级 OnceLock<Mutex<Vec>>)"),
    ])
    .with_data(Map::from_iter([
        ("published".to_string(), json!(published)),
        ("drained_count".to_string(), json!(drained.len())),
        (
            "smoke_event_found".to_string(),
            json!(smoke_event.is_some()),
        ),
        (
            "sink_kind".to_string(),
            json!("process_wide_oncelock_mutex_vec"),
        ),
    ]))
}

/// Epic 4:g004_conformance smoke test。
///
/// 构造一个合法的 G004 contract bundle(含一条满足所有必填字段的 laneEvent)
/// 和一个非法 bundle(缺失 /metadata/seq),分别调用 `validate_g004_contract_bundle`,
/// 验证校验器能正确区分合法/非法。这把 g004_conformance 从"仅测试用"激活为
/// "doctor 可观察的健康检查项"。
pub(crate) fn check_g004_conformance_health() -> DiagnosticCheck {
    let valid_bundle = json!({
        "schemaVersion": "g004.contract.bundle.v1",
        "laneEvents": [
            {
                "event": "lane.started",
                "status": "running",
                "emittedAt": "2026-07-22T00:00:00Z",
                "metadata": {
                    "provenance": "live_lane",
                    "emitterIdentity": "claw-doctor",
                    "environmentLabel": "smoke",
                    "seq": 1
                }
            }
        ],
        "reports": [],
        "approvalTokens": []
    });
    let invalid_bundle = json!({
        "schemaVersion": "g004.contract.bundle.v1",
        "laneEvents": [
            {
                "event": "lane.started",
                "status": "running",
                "emittedAt": "2026-07-22T00:00:00Z",
                "metadata": {
                    "provenance": "live_lane",
                    "emitterIdentity": "claw-doctor",
                    "environmentLabel": "smoke"
                    // 缺失 seq 字段,应触发校验错误
                }
            }
        ],
        "reports": [],
        "approvalTokens": []
    });

    let valid_errors = validate_g004_contract_bundle(&valid_bundle);
    let invalid_errors = validate_g004_contract_bundle(&invalid_bundle);

    let level = if valid_errors.is_empty() && !invalid_errors.is_empty() {
        DiagnosticLevel::Ok
    } else {
        DiagnosticLevel::Warn
    };

    DiagnosticCheck::new(
        "G004Conformance",
        level,
        "g004 契约包校验器可区分合法/非法 fixture",
    )
    .with_details(vec![
        format!("合法包错误数          {}", valid_errors.len()),
        format!("非法包错误数          {}", invalid_errors.len()),
        format!("Bundle schema 版本    g004.contract.bundle.v1"),
        format!("Report schema 版本    g004.report.v1"),
    ])
    .with_data(Map::from_iter([
        ("valid_bundle_errors".to_string(), json!(valid_errors.len())),
        (
            "invalid_bundle_errors".to_string(),
            json!(invalid_errors.len()),
        ),
        (
            "bundle_schema_version".to_string(),
            json!("g004.contract.bundle.v1"),
        ),
        ("report_schema_version".to_string(), json!("g004.report.v1")),
    ]))
}

/// Epic 4:report_schema smoke test。
///
/// 构造一个最小 `CanonicalReportV1`,通过 `canonicalize_report` 自动填充
/// `schema_version` / `report_id` / `content_hash`,并验证 `report_content_hash`
/// 能稳定计算。这把 report_schema 从"仅 lib.rs 重导出"激活为"doctor 可观察的
/// 健康检查项",同时为后续 `claw status` 输出 CanonicalReportV1 铺路。
pub(crate) fn check_canonical_report_v1_health() -> DiagnosticCheck {
    let report = CanonicalReportV1 {
        schema_version: String::new(),
        identity: ReportIdentity {
            report_id: String::new(),
            content_hash: String::new(),
        },
        generated_at: "2026-07-22T00:00:00Z".to_string(),
        producer: "claw-doctor".to_string(),
        claims: vec![ReportClaim {
            id: "claim-1".to_string(),
            kind: ClaimKind::ObservedFact,
            text: "doctor smoke test executed canonicalize_report".to_string(),
            confidence: ReportConfidence::High,
            evidence: Vec::new(),
            sensitivity: SensitivityClass::Public,
        }],
        negative_evidence: Vec::new(),
        field_deltas: Vec::new(),
    };
    let canonical = canonicalize_report(report);
    let hash = report_content_hash(&canonical);

    let level = if canonical.schema_version == REPORT_SCHEMA_V1
        && !canonical.identity.report_id.is_empty()
        && !canonical.identity.content_hash.is_empty()
        && !hash.is_empty()
    {
        DiagnosticLevel::Ok
    } else {
        DiagnosticLevel::Warn
    };

    DiagnosticCheck::new(
        "CanonicalReportV1",
        level,
        "canonicalize_report + report_content_hash 往返链路可调用",
    )
    .with_details(vec![
        format!("Schema 版本       {}", canonical.schema_version),
        format!("报告 ID           {}", canonical.identity.report_id),
        format!("内容哈希          {}", canonical.identity.content_hash),
        format!("独立哈希          {}", hash),
        format!("claim 数量        {}", canonical.claims.len()),
    ])
    .with_data(Map::from_iter([
        (
            "schema_version".to_string(),
            json!(canonical.schema_version),
        ),
        ("report_id".to_string(), json!(canonical.identity.report_id)),
        (
            "content_hash".to_string(),
            json!(canonical.identity.content_hash),
        ),
        ("claims_count".to_string(), json!(canonical.claims.len())),
    ]))
}

/// Epic 4:branch_lock smoke test。
///
/// 构造三组 fixture intents(同分支同模块碰撞 / 同分支嵌套模块碰撞 / 不同分支无碰撞),
/// 调用 `detect_branch_lock_collisions`,验证碰撞检测逻辑可用。
/// 这把 branch_lock 从"仅 lib.rs 重导出 + 自身测试"激活为"doctor 可观察的健康检查项"。
///
/// 真正的生产接入点(在 MultiAgentCoordinator fork/worktree 创建子 agent 时校验)
/// 留待后续评估,因 plan.md §9.2 给定的 execute_bash / git_context 接入点
/// 经研究均不匹配(无 lane 上下文 / 纯只读模块)。
pub(crate) fn check_branch_lock_health() -> DiagnosticCheck {
    let intents = vec![
        // 碰撞 1:两个 lane 同分支同模块
        BranchLockIntent {
            lane_id: "lane-a".to_string(),
            branch: "feat/x".to_string(),
            worktree: None,
            modules: vec!["runtime/mcp".to_string()],
        },
        BranchLockIntent {
            lane_id: "lane-b".to_string(),
            branch: "feat/x".to_string(),
            worktree: None,
            modules: vec!["runtime/mcp".to_string()],
        },
        // 碰撞 2:同分支嵌套模块(runtime 与 runtime/mcp 视为重叠)
        BranchLockIntent {
            lane_id: "lane-c".to_string(),
            branch: "feat/y".to_string(),
            worktree: None,
            modules: vec!["runtime".to_string()],
        },
        BranchLockIntent {
            lane_id: "lane-d".to_string(),
            branch: "feat/y".to_string(),
            worktree: None,
            modules: vec!["runtime/mcp".to_string()],
        },
        // 无碰撞:不同分支同模块
        BranchLockIntent {
            lane_id: "lane-e".to_string(),
            branch: "feat/z".to_string(),
            worktree: None,
            modules: vec!["runtime/mcp".to_string()],
        },
    ];
    let collisions = detect_branch_lock_collisions(&intents);

    let level = if collisions.len() >= 2 {
        DiagnosticLevel::Ok
    } else {
        DiagnosticLevel::Warn
    };

    DiagnosticCheck::new(
        "BranchLock",
        level,
        "detect_branch_lock_collisions 可识别同分支与嵌套模块碰撞",
    )
    .with_details(vec![
        format!("intent 数量        {}", intents.len()),
        format!("发现碰撞数        {}", collisions.len()),
        format!("不同分支          3 (feat/x, feat/y, feat/z)"),
        format!("预期碰撞数        2 (同分支 + 嵌套模块)"),
    ])
    .with_data(Map::from_iter([
        ("intents_count".to_string(), json!(intents.len())),
        ("collisions_count".to_string(), json!(collisions.len())),
        ("distinct_branches".to_string(), json!(3)),
    ]))
}

/// Epic 5:plugin_lifecycle smoke test 用的最小 trait 实现。
///
/// 注意:这里实现的是 `runtime::PluginLifecycle`(trait),与 `plugins::PluginLifecycle`
/// (struct,有 ::default())是两个完全不同的类型 —— 命名歧义陷阱,接入时务必区分。
/// 详见 plan.md §9.2 Epic 5 命名陷阱警示。
struct DoctorSmokePluginLifecycle {
    name: String,
    shutdown_called: bool,
}

impl PluginLifecycle for DoctorSmokePluginLifecycle {
    fn validate_config(&self, _config: &RuntimePluginConfig) -> Result<(), String> {
        Ok(())
    }

    fn healthcheck(&self) -> PluginHealthcheck {
        PluginHealthcheck {
            plugin_name: self.name.clone(),
            state: if self.shutdown_called {
                PluginState::Stopped
            } else {
                PluginState::Healthy
            },
            servers: Vec::new(),
            last_check: 0,
        }
    }

    fn discover(&self) -> DiscoveryResult {
        DiscoveryResult {
            tools: Vec::new(),
            resources: Vec::new(),
            partial: false,
        }
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.shutdown_called = true;
        Ok(())
    }
}

/// Epic 5:plugin_lifecycle smoke test。
///
/// 构造一个 `DoctorSmokePluginLifecycle`(实现 `runtime::PluginLifecycle` trait),
/// 调用 validate_config / healthcheck / discover / shutdown 全部四个方法,
/// 验证 trait API 在生产 binary 中可被实现和调用。
/// 这把 plugin_lifecycle 从"零消费死链"激活为"doctor 可观察的健康检查项"。
///
/// 注意:plan.md §9.2 提到的 `PluginLifecycle::init` 在当前 trait 中不存在;
/// 实际可用 `validate_config` 充当 "init" 入口(语义:校验配置后启动)。
pub(crate) fn check_plugin_lifecycle_health() -> DiagnosticCheck {
    let mut plugin = DoctorSmokePluginLifecycle {
        name: "doctor-smoke".to_string(),
        shutdown_called: false,
    };
    let config = RuntimePluginConfig::default();
    let validate_result = plugin.validate_config(&config);
    let health_before = plugin.healthcheck();
    let discovery = plugin.discover();
    let shutdown_result = plugin.shutdown();
    let health_after = plugin.healthcheck();

    let level = if validate_result.is_ok()
        && shutdown_result.is_ok()
        && matches!(health_before.state, PluginState::Healthy)
        && matches!(health_after.state, PluginState::Stopped)
    {
        DiagnosticLevel::Ok
    } else {
        DiagnosticLevel::Warn
    };

    DiagnosticCheck::new(
        "PluginLifecycle",
        level,
        "插件生命周期 trait (validate/healthcheck/discover/shutdown) 可调用",
    )
    .with_details(vec![
        format!(
            "validate_config   {}",
            if validate_result.is_ok() {
                "正常"
            } else {
                "错误"
            }
        ),
        format!("healthcheck 前    {}", health_before.state),
        format!(
            "discover          tools={} resources={} partial={}",
            discovery.tools.len(),
            discovery.resources.len(),
            discovery.partial
        ),
        format!(
            "shutdown          {}",
            if shutdown_result.is_ok() {
                "正常"
            } else {
                "错误"
            }
        ),
        format!("healthcheck 后    {}", health_after.state),
    ])
    .with_data(Map::from_iter([
        ("validate_ok".to_string(), json!(validate_result.is_ok())),
        ("shutdown_ok".to_string(), json!(shutdown_result.is_ok())),
        (
            "state_before".to_string(),
            json!(format!("{}", health_before.state)),
        ),
        (
            "state_after".to_string(),
            json!(format!("{}", health_after.state)),
        ),
    ]))
}

/// Epic 5:mcp_tool_bridge smoke test。
///
/// 构造一个 `McpToolRegistry`,注册一个虚拟 server(含一个 tool + 一个 resource),
/// 调用 list_servers / get_server / list_tools / list_resources 验证注册表 API 可达。
/// 这把 mcp_tool_bridge 从"tools/lib.rs 全局单例空转(从不 set_manager)"激活为
/// "doctor 可观察的健康检查项"。
///
/// 生产路径接入(向 global_mcp_registry 注入 McpServerManager)需要重构
/// RuntimeMcpState(其 manager 字段非 Arc<Mutex<>>),风险较高,留待后续。
pub(crate) fn check_mcp_tool_bridge_health() -> DiagnosticCheck {
    let registry = McpToolRegistry::new();
    registry.register_server(
        "doctor-smoke-server",
        McpConnectionStatus::Connected,
        vec![McpToolInfo {
            name: "smoke_tool".to_string(),
            description: Some("doctor smoke test tool".to_string()),
            input_schema: Some(json!({"type": "object"})),
        }],
        vec![McpResourceInfo {
            uri: "smoke://resource".to_string(),
            name: "smoke_resource".to_string(),
            description: Some("doctor smoke test resource".to_string()),
            mime_type: Some("text/plain".to_string()),
        }],
        Some("doctor-smoke-server v0.1".to_string()),
    );

    let servers = registry.list_servers();
    let server_state = registry.get_server("doctor-smoke-server");
    let tools = registry.list_tools("doctor-smoke-server");
    let resources = registry.list_resources("doctor-smoke-server");

    let level =
        if servers.len() == 1 && server_state.is_some() && tools.is_ok() && resources.is_ok() {
            DiagnosticLevel::Ok
        } else {
            DiagnosticLevel::Warn
        };

    DiagnosticCheck::new(
        "McpToolBridge",
        level,
        "mcp 工具注册表 (register/list/get/tools/resources) 可调用",
    )
    .with_details(vec![
        format!("已注册服务器数      {}", servers.len()),
        format!(
            "服务器已找到        {}",
            if server_state.is_some() { "是" } else { "否" }
        ),
        format!(
            "已列出工具数        {}",
            tools.as_ref().map(|t| t.len()).unwrap_or(0)
        ),
        format!(
            "已列出资源数        {}",
            resources.as_ref().map(|r| r.len()).unwrap_or(0)
        ),
        format!("连接状态            {:?}", McpConnectionStatus::Connected),
    ])
    .with_data(Map::from_iter([
        ("servers_count".to_string(), json!(servers.len())),
        ("server_found".to_string(), json!(server_state.is_some())),
        (
            "tools_count".to_string(),
            json!(tools.as_ref().map(|t| t.len()).unwrap_or(0)),
        ),
        (
            "resources_count".to_string(),
            json!(resources.as_ref().map(|r| r.len()).unwrap_or(0)),
        ),
    ]))
}

// Epic 6:team_cron_registry smoke test — 验证 Team + Cron 两个 registry 的完整 API
// (create/get/list/delete/disable/record_run) 在生产 binary 中可被调用,激活死代码。
// 生产路径接入(Teammate 模式 cron 调度子 agent)需要 MultiAgentCoordinator 改造,风险较高,留待后续。
pub(crate) fn check_team_cron_registry_health() -> DiagnosticCheck {
    // ── TeamRegistry smoke test ──
    let team_registry = TeamRegistry::new();
    let team = team_registry.create("doctor-smoke-team", vec!["task_001".into()]);
    let team_fetched = team_registry.get(&team.team_id);
    let team_list_len = team_registry.list().len();
    let team_deleted = team_registry.delete(&team.team_id).ok();
    let team_status_after_delete = team_fetched.as_ref().map(|t| t.status);

    // ── CronRegistry smoke test ──
    let cron_registry = CronRegistry::new();
    let cron = cron_registry.create("0 * * * *", "doctor smoke", Some("hourly check"));
    let cron_fetched = cron_registry.get(&cron.cron_id);
    let cron_enabled_count = cron_registry.list(true).len();
    let cron_disable_result = cron_registry.disable(&cron.cron_id);
    let cron_disabled_count = cron_registry.list(true).len();
    let cron_record_run_result = cron_registry.record_run(&cron.cron_id);
    let cron_after_run = cron_registry.get(&cron.cron_id);
    let cron_run_count = cron_after_run.as_ref().map(|c| c.run_count).unwrap_or(0);

    let level = if team_fetched.is_some()
        && team_list_len == 1
        && team_deleted.is_some()
        && cron_fetched.is_some()
        && cron_enabled_count == 1
        && cron_disable_result.is_ok()
        && cron_disabled_count == 0
        && cron_record_run_result.is_ok()
        && cron_run_count == 1
    {
        DiagnosticLevel::Ok
    } else {
        DiagnosticLevel::Warn
    };

    DiagnosticCheck::new(
        "TeamCronRegistry",
        level,
        "team + cron 注册表 (create/get/list/delete/disable/record_run) 可调用",
    )
    .with_details(vec![
        format!("Team 已创建         {}", team.team_id),
        format!(
            "Team 已找到         {}",
            if team_fetched.is_some() { "是" } else { "否" }
        ),
        format!("Team 列表数量       {}", team_list_len),
        format!(
            "Team 已删除         {}",
            if team_deleted.is_some() { "是" } else { "否" }
        ),
        format!(
            "Team 状态(原始)    {:?}",
            team_status_after_delete.unwrap_or(TeamStatus::Created)
        ),
        format!("Cron 已创建         {}", cron.cron_id),
        format!(
            "Cron 已找到         {}",
            if cron_fetched.is_some() { "是" } else { "否" }
        ),
        format!("Cron 启用数量       {}", cron_enabled_count),
        format!(
            "Cron 禁用成功       {}",
            if cron_disable_result.is_ok() {
                "是"
            } else {
                "否"
            }
        ),
        format!("Cron 禁用后数量     {}", cron_disabled_count),
        format!(
            "Cron 运行已记录     {}",
            if cron_record_run_result.is_ok() {
                "是"
            } else {
                "否"
            }
        ),
        format!("Cron 运行次数       {}", cron_run_count),
    ])
    .with_data(Map::from_iter([
        ("team_created".to_string(), json!(team_fetched.is_some())),
        ("team_list_count".to_string(), json!(team_list_len)),
        ("team_deleted".to_string(), json!(team_deleted.is_some())),
        ("cron_created".to_string(), json!(cron_fetched.is_some())),
        ("cron_enabled_count".to_string(), json!(cron_enabled_count)),
        (
            "cron_disabled_count".to_string(),
            json!(cron_disabled_count),
        ),
        ("cron_run_count".to_string(), json!(cron_run_count)),
    ]))
}

#[derive(Debug, Clone)]
pub(crate) struct StatusContext {
    pub(crate) cwd: PathBuf,
    pub(crate) session_path: Option<PathBuf>,
    pub(crate) loaded_config_files: usize,
    pub(crate) discovered_config_files: usize,
    pub(crate) memory_file_count: usize,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) git_branch: Option<String>,
    pub(crate) git_summary: GitWorkspaceSummary,
    pub(crate) branch_freshness: BranchFreshness,
    pub(crate) stale_base_state: BaseCommitState,
    pub(crate) session_lifecycle: SessionLifecycleSummary,
    pub(crate) boot_preflight: BootPreflightSnapshot,
    pub(crate) sandbox_status: runtime::SandboxStatus,
    /// #143: when `.claw.json` (or another loaded config file) fails to parse,
    /// we capture the parse error here and still populate every field that
    /// doesn't depend on runtime config (workspace, git, sandbox defaults,
    /// discovery counts). Top-level JSON output then reports
    /// `status: "degraded"` so claws can distinguish "status ran but config
    /// is broken" from "status ran cleanly".
    pub(crate) config_load_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchFreshness {
    pub(crate) upstream: Option<String>,
    pub(crate) ahead: u32,
    pub(crate) behind: u32,
    pub(crate) fresh: Option<bool>,
}

impl BranchFreshness {
    pub(crate) fn from_git_status(status: Option<&str>) -> Self {
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

    pub(crate) fn json_value(&self) -> serde_json::Value {
        json!({
            "upstream": self.upstream,
            "ahead": self.ahead,
            "behind": self.behind,
            "fresh": self.fresh,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BinaryPreflight {
    pub(crate) name: &'static str,
    pub(crate) available: bool,
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
pub(crate) struct ControlSocketPreflight {
    pub(crate) name: &'static str,
    pub(crate) configured: bool,
    pub(crate) exists: bool,
    pub(crate) path: Option<String>,
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
pub(crate) struct BootPreflightSnapshot {
    pub(crate) repo_exists: bool,
    pub(crate) worktree_exists: bool,
    pub(crate) git_dir_exists: bool,
    pub(crate) branch_freshness: BranchFreshness,
    pub(crate) trust_gate_allowed: Option<bool>,
    pub(crate) trusted_roots_count: usize,
    pub(crate) required_binaries: Vec<BinaryPreflight>,
    pub(crate) control_sockets: Vec<ControlSocketPreflight>,
    pub(crate) mcp_startup_eligible: bool,
    pub(crate) mcp_servers_configured: usize,
    pub(crate) plugin_startup_eligible: bool,
    pub(crate) plugins_configured: usize,
    pub(crate) last_failed_boot_reason: Option<String>,
}

impl BootPreflightSnapshot {
    pub(crate) fn json_value(&self) -> serde_json::Value {
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

    pub(crate) fn summary(&self) -> String {
        let trust = self
            .trust_gate_allowed
            .map(|value| {
                if value {
                    "已加入白名单"
                } else {
                    "未加入白名单"
                }
            })
            .unwrap_or("未知");
        let freshness = self
            .branch_freshness
            .fresh
            .map(|fresh| if fresh { "最新" } else { "落后" })
            .unwrap_or("无上游");
        format!(
            "repo={} worktree={} branch={} trust={} mcp={} plugins={} last_failed={}",
            self.repo_exists,
            self.worktree_exists,
            freshness,
            trust,
            self.mcp_startup_eligible,
            self.plugin_startup_eligible,
            self.last_failed_boot_reason.as_deref().unwrap_or("无")
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StatusUsage {
    pub(crate) message_count: usize,
    pub(crate) turns: u32,
    pub(crate) latest: TokenUsage,
    pub(crate) cumulative: TokenUsage,
    pub(crate) estimated_tokens: usize,
}

#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GitWorkspaceSummary {
    pub(crate) changed_files: usize,
    pub(crate) staged_files: usize,
    pub(crate) unstaged_files: usize,
    pub(crate) untracked_files: usize,
    pub(crate) conflicted_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxPaneSnapshot {
    pub(crate) pane_id: String,
    pub(crate) current_command: String,
    pub(crate) current_path: PathBuf,
}

impl GitWorkspaceSummary {
    pub(crate) fn is_clean(self) -> bool {
        self.changed_files == 0
    }

    pub(crate) fn headline(self) -> String {
        if self.is_clean() {
            "干净".to_string()
        } else {
            let mut details = Vec::new();
            if self.staged_files > 0 {
                details.push(format!("{} 已暂存", self.staged_files));
            }
            if self.unstaged_files > 0 {
                details.push(format!("{} 未暂存", self.unstaged_files));
            }
            if self.untracked_files > 0 {
                details.push(format!("{} 未跟踪", self.untracked_files));
            }
            if self.conflicted_files > 0 {
                details.push(format!("{} 有冲突", self.conflicted_files));
            }
            format!(
                "脏 · {} 个文件 · {}",
                self.changed_files,
                details.join(", ")
            )
        }
    }
}

pub(crate) fn classify_session_lifecycle_for(workspace: &Path) -> SessionLifecycleSummary {
    classify_session_lifecycle_from_panes(workspace, discover_tmux_panes())
}

pub(crate) fn classify_session_lifecycle_from_panes(
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

pub(crate) fn discover_tmux_panes() -> Vec<TmuxPaneSnapshot> {
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

pub(crate) fn parse_tmux_pane_snapshots(output: &str) -> Vec<TmuxPaneSnapshot> {
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

pub(crate) fn pane_path_matches_workspace(pane_path: &Path, workspace: &Path) -> bool {
    if pane_path == workspace || pane_path.starts_with(workspace) {
        return true;
    }
    let pane_path = fs::canonicalize(pane_path).unwrap_or_else(|_| pane_path.to_path_buf());
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    pane_path == workspace || pane_path.starts_with(&workspace)
}

pub(crate) fn is_idle_shell_command(command: &str) -> bool {
    let command = command.rsplit('/').next().unwrap_or(command);
    matches!(
        command,
        "bash" | "zsh" | "sh" | "fish" | "nu" | "pwsh" | "powershell" | "cmd"
    )
}

pub(crate) fn git_worktree_is_dirty(workspace: &Path) -> bool {
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

pub(crate) fn parse_git_status_metadata(status: Option<&str>) -> (Option<PathBuf>, Option<String>) {
    parse_git_status_metadata_for(
        &env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        status,
    )
}

pub(crate) fn parse_git_status_branch(status: Option<&str>) -> Option<String> {
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

pub(crate) fn parse_git_workspace_summary(status: Option<&str>) -> GitWorkspaceSummary {
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

pub(crate) fn build_boot_preflight_snapshot(
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

pub(crate) fn run_git_bool(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .is_ok_and(|output| output.status.success())
}

pub(crate) fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

pub(crate) fn tmux_control_socket_preflight() -> ControlSocketPreflight {
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

pub(crate) fn last_failed_boot_reason(cwd: &Path) -> Option<String> {
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

pub(crate) fn path_matches_trusted_root_local(cwd: &Path, trusted_root: &str) -> bool {
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

pub(crate) fn resolve_git_branch_for(cwd: &Path) -> Option<String> {
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

pub(crate) fn run_git_capture_in(cwd: &Path, args: &[&str]) -> Option<String> {
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

pub(crate) fn find_git_root_in(cwd: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
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

pub(crate) fn parse_git_status_metadata_for(
    cwd: &Path,
    status: Option<&str>,
) -> (Option<PathBuf>, Option<String>) {
    let branch = resolve_git_branch_for(cwd).or_else(|| parse_git_status_branch(status));
    let project_root = find_git_root_in(cwd).ok();
    (project_root, branch)
}

#[cfg(test)]
mod cache_stats_tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde_json::Value;

    use super::{
        collect_session_cache_stats, hit_rate_pct, newest_session, render_cache_stats_json,
        render_cache_stats_text, CacheBreakStats, CacheStatsAggregate, SessionCacheStatsSnapshot,
    };

    fn temp_root(prefix: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_stats(root: &Path, session: &str, tracked: u64, read: u64, creation: u64) {
        let dir = root.join(session);
        std::fs::create_dir_all(&dir).unwrap();
        let stats = CacheBreakStats {
            tracked_requests: tracked,
            total_cache_read_input_tokens: read,
            total_cache_creation_input_tokens: creation,
            ..Default::default()
        };
        std::fs::write(dir.join("stats.json"), serde_json::to_vec(&stats).unwrap()).unwrap();
    }

    #[test]
    fn collect_partitions_main_and_subagent_sessions() {
        let root = temp_root("doctor-cache-collect");
        write_stats(&root, "session-aaa", 10, 1_000, 100);
        write_stats(&root, "subagent-session-aaa", 4, 0, 1_696);
        // 无 stats.json 的目录应被跳过
        std::fs::create_dir_all(root.join("session-empty")).unwrap();

        let sessions = collect_session_cache_stats(&root);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(sessions.len(), 2);
        let main = sessions.iter().find(|s| s.name == "session-aaa").unwrap();
        assert!(!main.is_subagent);
        assert!(main.modified_at.is_some());
        let sub = sessions
            .iter()
            .find(|s| s.name == "subagent-session-aaa")
            .unwrap();
        assert!(sub.is_subagent);
    }

    #[test]
    fn aggregate_keeps_main_and_subagent_separate() {
        let root = temp_root("doctor-cache-agg");
        write_stats(&root, "session-a", 10, 1_000, 100);
        write_stats(&root, "subagent-session-a", 4, 0, 1_696);
        let sessions = collect_session_cache_stats(&root);
        std::fs::remove_dir_all(&root).unwrap();

        let mut main_agg = CacheStatsAggregate::default();
        let mut sub_agg = CacheStatsAggregate::default();
        for s in &sessions {
            if s.is_subagent {
                sub_agg.push(&s.stats);
            } else {
                main_agg.push(&s.stats);
            }
        }

        assert_eq!(main_agg.session_count, 1);
        assert_eq!(main_agg.tracked_requests, 10);
        assert_eq!(main_agg.read_tokens, 1_000);
        assert_eq!(sub_agg.session_count, 1);
        assert_eq!(sub_agg.tracked_requests, 4);
        assert_eq!(sub_agg.read_tokens, 0);
        assert_eq!(sub_agg.creation_tokens, 1_696);
        assert_eq!(sub_agg.hit_rate_pct(), 0);
        assert_eq!(main_agg.hit_rate_pct(), 91);
    }

    #[test]
    fn newest_session_uses_modified_at() {
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        let sessions = vec![
            SessionCacheStatsSnapshot {
                name: "session-old".into(),
                is_subagent: false,
                stats: CacheBreakStats::default(),
                modified_at: Some(older),
            },
            SessionCacheStatsSnapshot {
                name: "subagent-session-new".into(),
                is_subagent: true,
                stats: CacheBreakStats::default(),
                modified_at: Some(newer),
            },
        ];
        let newest = newest_session(&sessions).expect("newest session");
        assert_eq!(newest.name, "subagent-session-new");
        assert!(newest.is_subagent);
    }

    #[test]
    fn text_report_lists_each_subagent_session_as_own_line() {
        let root = temp_root("doctor-cache-text");
        write_stats(&root, "session-a", 25, 751_872, 0);
        write_stats(&root, "subagent-session-a", 4, 0, 1_696);
        let sessions = collect_session_cache_stats(&root);
        std::fs::remove_dir_all(&root).unwrap();

        let (mut main_sessions, mut subagent_sessions) = (Vec::new(), Vec::new());
        for s in sessions {
            if s.is_subagent {
                subagent_sessions.push(s);
            } else {
                main_sessions.push(s);
            }
        }
        let mut main_agg = CacheStatsAggregate::default();
        for s in &main_sessions {
            main_agg.push(&s.stats);
        }
        let mut sub_agg = CacheStatsAggregate::default();
        for s in &subagent_sessions {
            sub_agg.push(&s.stats);
        }

        let text = render_cache_stats_text(
            &root,
            &main_sessions,
            &subagent_sessions,
            &main_agg,
            &sub_agg,
        );
        assert!(text.contains("主 session 1 个,subagent session 1 个"));
        assert!(text.contains("== Subagent 汇总(subagent-session-* 独立统计)=="));
        assert!(text.contains("[子] subagent-session-a"));
        assert!(text.contains("[主] session-a"));
    }

    #[test]
    fn json_report_splits_main_and_subagent() {
        let root = temp_root("doctor-cache-json");
        write_stats(&root, "session-a", 25, 751_872, 0);
        write_stats(&root, "subagent-session-a", 4, 0, 1_696);
        let sessions = collect_session_cache_stats(&root);
        std::fs::remove_dir_all(&root).unwrap();

        let (mut main_sessions, mut subagent_sessions) = (Vec::new(), Vec::new());
        for s in sessions {
            if s.is_subagent {
                subagent_sessions.push(s);
            } else {
                main_sessions.push(s);
            }
        }
        let mut main_agg = CacheStatsAggregate::default();
        for s in &main_sessions {
            main_agg.push(&s.stats);
        }
        let mut sub_agg = CacheStatsAggregate::default();
        for s in &subagent_sessions {
            sub_agg.push(&s.stats);
        }

        let value: Value =
            render_cache_stats_json(&main_sessions, &subagent_sessions, &main_agg, &sub_agg);
        assert_eq!(value["session_count"], 2);
        assert_eq!(value["main_session_count"], 1);
        assert_eq!(value["subagent_session_count"], 1);
        // 顶层聚合只统计主 session,不被 subagent 污染
        assert_eq!(value["tokens"]["total_cache_read_input_tokens"], 751_872);
        assert_eq!(
            value["subagent"]["tokens"]["total_cache_read_input_tokens"],
            0
        );
        let sessions_json = value["sessions"].as_array().expect("sessions array");
        assert_eq!(sessions_json.len(), 2);
        let names = sessions_json
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(names.contains(&"session-a".to_string()));
        assert!(names.contains(&"subagent-session-a".to_string()));
    }

    #[test]
    fn hit_rate_pct_rounds_and_guards_zero() {
        assert_eq!(hit_rate_pct(0, 0), 0);
        assert_eq!(hit_rate_pct(1_000, 0), 100);
        assert_eq!(hit_rate_pct(0, 1_696), 0);
        assert_eq!(hit_rate_pct(9, 1), 90);
    }
}
