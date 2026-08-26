//! harness_evolution — LLM 驱动的自进化 Harness(design-gaps #2)。
//!
//! 遵循 `docs/2026-07-24-p3-self-evolving-harness-design.md`:
//! - **Stage 1 Weakness Mining**(确定性代码):复用 `TraceAnalyzer::cluster_failures`
//!   聚类失败 → 过滤 `occurrence_count < min_occurrences` → 提取 pathology。
//! - **Stage 2 Mixed Proposer**(规则优先,MVP 零 LLM 调用):7+ 种预定义错误模式
//!   直接映射为 HarnessEdit;未命中模式留待 Phase 3 LLM Proposer。
//! - **两重门控验证**:Validity(基础设施噪声过滤 + pathology 出现确认) +
//!   Significance(z-test,alpha=0.05),防止 misevolution。
//! - **全量注入**:Active edits(≤10 条)注入 `SystemPromptSplit::dynamic_sections`。
//!
//! 防 misevolution 硬约束:LLM 只提议、确定性代码归因;无 TaskSuccessRate 的
//! edit 永远停留 Candidate;禁止无 pathology 提议。

pub mod archive;
pub mod types;

pub use archive::{ArchiveError, HarnessArchive};
pub use types::*;

use crate::decision_log::compute_simhash;
use crate::failure_trace::{FailureTrace, TraceToolStep};
use crate::harness_evolution::archive::{current_timestamp_ms, generate_edit_id};
use crate::tool_call_stats::ToolCallStat;
use crate::trace_analyzer::TraceAnalyzer;

use std::sync::{Arc, OnceLock};

/// 样本错误消息上限(每 cluster)。
const MAX_SAMPLE_ERRORS: usize = 5;

/// LLM 驱动的 harness 规则提议接口(依赖倒置,Phase 3)。
///
/// runtime 不依赖 api,生产实现在 rusty-claude-cli 用 `LlmBridge` 注入。
/// 规则式 Proposer([`RULE_PATTERNS`])未命中时调用本接口,让 LLM 从失败模式
/// 提出新的 harness 指导规则,突破硬编码规则的覆盖上限。
///
/// # 防 misevolution 约束(与规则式路径一致)
/// - LLM 只提议,确定性代码负责归因(validity / significance 两重门控);
/// - 无 pathology 提议禁止(由 `propose_edits` 的 weakness 入参保证);
/// - 提议内容截断到 [`archive::MAX_EDIT_CONTENT_CHARS`]。
pub trait HarnessProposer: Send + Sync {
    /// 为一个 weakness 信号提议 harness edit。
    ///
    /// 返回 `(content, reasoning)`;无法提议(LLM 失败 / 响应无效)时返回 `None`。
    fn propose(&self, weakness: &WeaknessSignal) -> Option<(String, String)>;
}

/// 进程级 LLM Proposer 单例(OnceLock)。
static GLOBAL_HARNESS_PROPOSER: OnceLock<Arc<dyn HarnessProposer>> = OnceLock::new();

/// 注入全局 LLM Proposer(幂等,重复调用静默忽略)。
pub fn set_global_harness_proposer(proposer: Arc<dyn HarnessProposer>) {
    let _ = GLOBAL_HARNESS_PROPOSER.set(proposer);
}

/// 读取全局 LLM Proposer(未注入返回 None)。
fn global_harness_proposer() -> Option<&'static Arc<dyn HarnessProposer>> {
    GLOBAL_HARNESS_PROPOSER.get()
}

/// 预定义错误模式 → HarnessEdit 映射。
/// `(pathology_keyword, edit_content, reasoning)`。
const RULE_PATTERNS: &[(&str, &str, &str)] = &[
    (
        "old_string not found",
        "When Edit tool fails with 'old_string not found', first run Grep to locate the exact current text before retrying. Common causes: whitespace differences, partial matches, stale memory.",
        "Rule: edit_old_string_not_found — force Grep before Edit retry",
    ),
    (
        "cannot find value",
        "When Rust compile fails with 'cannot find value', check: (1) variable scope, (2) import statements, (3) typo in identifier. Use Grep to find the declaration.",
        "Rule: rust_cannot_find_value — systematic scope/import/typo check",
    ),
    (
        "unresolved import",
        "When Rust reports 'unresolved import', verify: (1) module path exists, (2) crate is in Cargo.toml, (3) use crate:: vs use :: for external crates.",
        "Rule: rust_unresolved_import — verify module path and Cargo.toml",
    ),
    (
        "connection refused",
        "When encountering 'connection refused' or 'ECONNREFUSED', before retrying: (1) check if service is running, (2) verify port number, (3) check firewall rules. Do not blindly retry.",
        "Rule: network_connection_refused — diagnose before retry",
    ),
    (
        "permission denied",
        "When 'permission denied' occurs, check: (1) file permissions (ls -la), (2) process user, (3) parent directory write access. Use chmod only if appropriate.",
        "Rule: fs_permission_denied — check permissions before write",
    ),
    (
        "no such file or directory",
        "When 'no such file or directory' occurs, verify path with LS or Glob before assuming the file exists. Common cause: relative vs absolute path confusion.",
        "Rule: fs_not_found — verify path with LS/Glob",
    ),
    (
        "test result: FAILED",
        "When tests fail, read the full failure output before modifying code. Identify: (1) which test failed, (2) assertion vs panic, (3) expected vs actual. Do not guess the fix.",
        "Rule: test_failure — analyze before fixing",
    ),
];

// ---------------------------------------------------------------------------
// Stage 1: Weakness Mining
// ---------------------------------------------------------------------------

/// 提取 weakness signals(无状态函数)。
///
/// 复用 `TraceAnalyzer::cluster_failures`,过滤低频噪声。
#[must_use]
pub fn mine_weaknesses(
    analyzer: &TraceAnalyzer,
    lookback_turns: usize,
    min_occurrences: usize,
) -> Vec<WeaknessSignal> {
    let window = recent_window(analyzer, lookback_turns);
    let clusters = analyzer.cluster_failures();

    clusters
        .into_iter()
        .filter(|c| c.count as usize >= min_occurrences)
        .map(|c| WeaknessSignal {
            pathology: c.label.clone(),
            sample_errors: c
                .sample_errors
                .into_iter()
                .take(MAX_SAMPLE_ERRORS)
                .collect(),
            occurrence_count: c.count,
            related_turns: extract_related_turns(window, &c.label),
        })
        .collect()
}

/// 从失败轨迹切片提取 tool 调用级 weakness signals(无状态函数)。
///
/// 与 [`mine_weaknesses`] 的区别：后者复用 turn 级 `TraceAnalyzer` 的
/// `failure_kind` 粗分类；本函数把 pathology 降粒度到工具级签名
/// (`"{tool_name}"` 或 `"{tool_name}:{rule_keyword}"`)，使 Stage 2 Proposer
/// 的规则匹配从"整个 turn 的错误消息"精准到"具体失败的工具"。
///
/// 每个 [`FailureTrace`] 取**第一个** `is_error=true` 的步骤作为失败点
/// (后续失败通常是首个失败的连锁反应)。pathology 签名规则：
/// - 失败步骤 output 命中 [`RULE_PATTERNS`] 关键词 → `"{tool_name}:{keyword}"`
/// - 否则 → `"{tool_name}"`
///
/// 输出按 occurrence_count 降序、pathology 升序(与 [`mine_weaknesses`]
/// 经由 `cluster_failures` 的顺序一致，保证稳定)。
#[must_use]
pub fn mine_weaknesses_from_traces(
    traces: &[FailureTrace],
    min_occurrences: usize,
) -> Vec<WeaknessSignal> {
    // pathology → (count, sample_errors, related_turns)。
    let mut buckets: std::collections::HashMap<String, (u32, Vec<String>, Vec<String>)> =
        std::collections::HashMap::new();

    for trace in traces {
        let Some(failed_step) = trace.steps.iter().find(|s| s.is_error) else {
            continue; // 全成功轨迹(理论上已由 extract 过滤，防御性跳过)
        };
        let pathology = tool_signature(failed_step);
        let entry = buckets.entry(pathology).or_default();
        entry.0 += 1;
        if entry.1.len() < MAX_SAMPLE_ERRORS {
            entry.1.push(failed_step.output.clone());
        }
        entry.2.push(trace.turn_id.clone());
    }

    let mut signals: Vec<WeaknessSignal> = buckets
        .into_iter()
        .filter(|(_, (count, _, _))| *count as usize >= min_occurrences)
        .map(
            |(pathology, (count, sample_errors, related_turns))| WeaknessSignal {
                pathology,
                sample_errors,
                occurrence_count: count,
                related_turns,
            },
        )
        .collect();
    signals.sort_by(|a, b| {
        b.occurrence_count
            .cmp(&a.occurrence_count)
            .then_with(|| a.pathology.cmp(&b.pathology))
    });
    signals
}

/// 从失败步骤推导工具级 pathology 签名。
///
/// 匹配 [`RULE_PATTERNS`] 关键词时返回 `"{tool_name}:{keyword}"`，否则返回
/// `"{tool_name}"`。大小写处理与 [`rule_based_propose`] 保持一致：
/// 输出转小写后 `contains` 关键词(关键词保持原样)。
fn tool_signature(step: &TraceToolStep) -> String {
    let lower_output = step.output.to_lowercase();
    let keyword = RULE_PATTERNS
        .iter()
        .find(|(kw, _, _)| lower_output.contains(kw))
        .map(|(kw, _, _)| *kw);
    match keyword {
        Some(kw) => format!("{}:{kw}", step.tool_name),
        None => step.tool_name.clone(),
    }
}

/// 取最近 `lookback_turns` 条记录(用于关联 turn_id)。
fn recent_window(
    analyzer: &TraceAnalyzer,
    lookback_turns: usize,
) -> &[crate::trace_analyzer::TraceRecord] {
    let len = analyzer.records.len();
    let start = len.saturating_sub(lookback_turns);
    &analyzer.records[start..]
}

/// 提取与该 cluster label 相关的 turn_id(窗口内 failure_kind 匹配的记录)。
fn extract_related_turns(
    window: &[crate::trace_analyzer::TraceRecord],
    label: &str,
) -> Vec<String> {
    window
        .iter()
        .filter(|r| r.failure_kind.as_deref() == Some(label))
        .map(|r| r.turn_id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Stage 2: Mixed Proposer(规则优先 + simhash 去重)
// ---------------------------------------------------------------------------

/// 混合 Proposer(MVP 规则式路径,零 LLM 调用)。返回新增提案数。
///
/// - 规则匹配 [`RULE_PATTERNS`] → 生成 Candidate edit;
/// - simhash 去重:与现有 edits(Active + Retired)汉明距离 ≤ 3 视为重复跳过;
/// - `max_proposals` 限制单轮新增数。
pub fn propose_edits(
    weaknesses: &[WeaknessSignal],
    existing_edits: &[HarnessEdit],
    config: &EvolutionConfig,
) -> Vec<HarnessEdit> {
    let mut existing_hashes: std::collections::HashSet<i64> =
        existing_edits.iter().map(|e| e.similarity_hash).collect();

    let mut proposals: Vec<HarnessEdit> = Vec::new();
    for weakness in weaknesses {
        if proposals.len() >= config.max_proposals {
            break;
        }
        // 规则式优先,未命中时回退到 LLM Proposer(Phase 3)。
        let edit = rule_based_propose(weakness).or_else(|| llm_based_propose(weakness));
        let Some(edit) = edit else {
            continue;
        };
        // simhash 去重(与已有 edits + 本轮已提案)。
        if existing_hashes.contains(&edit.similarity_hash) {
            continue;
        }
        if proposals
            .iter()
            .any(|p| p.similarity_hash == edit.similarity_hash)
        {
            continue;
        }
        existing_hashes.insert(edit.similarity_hash);
        proposals.push(edit);
    }
    proposals
}

/// 构造一个 Candidate edit(规则式与 LLM 路径共用)。
fn build_edit(
    weakness: &WeaknessSignal,
    content: &str,
    reasoning: String,
    source: EditSource,
) -> HarnessEdit {
    let simhash_text = format!("{} {}", weakness.pathology, content);
    HarnessEdit {
        id: generate_edit_id(content, &weakness.pathology),
        pathology: weakness.pathology.clone(),
        content: content.to_string(),
        status: EditStatus::Candidate,
        source,
        verify_count: 0,
        success_count: 0,
        created_at: current_timestamp_ms(),
        last_verified_at: None,
        proposer_reasoning: reasoning,
        similarity_hash: compute_simhash(&simhash_text) as i64,
        retire_reason: None,
    }
}

/// 规则式匹配:命中预定义模式 → 直接生成 Candidate edit。
fn rule_based_propose(weakness: &WeaknessSignal) -> Option<HarnessEdit> {
    for (keyword, content, reasoning) in RULE_PATTERNS {
        let matched = weakness.pathology.to_lowercase().contains(keyword)
            || weakness
                .sample_errors
                .iter()
                .any(|e| e.to_lowercase().contains(keyword));
        if matched {
            return Some(build_edit(
                weakness,
                content,
                (*reasoning).to_string(),
                EditSource::RulePattern,
            ));
        }
    }
    None
}

/// LLM 式提议(Phase 3):规则未命中时,用注入的 [`HarnessProposer`] 从失败
/// 模式提出新规则。未注入 / 提议失败时返回 None(保持规则式路径零 LLM 回退)。
fn llm_based_propose(weakness: &WeaknessSignal) -> Option<HarnessEdit> {
    // Tier S #3 穷鬼模式:激活时跳过 LLM 提议(省 token),保持纯规则路径。
    if crate::poor_mode::is_active() {
        return None;
    }
    let proposer = global_harness_proposer()?;
    let (content, reasoning) = proposer.propose(weakness)?;
    // 防 misevolution:截断到单条 content 上限,避免 LLM 输出过长注入。
    let content: String = content
        .chars()
        .take(crate::harness_evolution::archive::MAX_EDIT_CONTENT_CHARS)
        .collect();
    if content.trim().is_empty() {
        return None;
    }
    Some(build_edit(
        weakness,
        &content,
        reasoning,
        EditSource::LlmProposer,
    ))
}

// ---------------------------------------------------------------------------
// 两重门控验证(GSME: Validity + Significance)
// ---------------------------------------------------------------------------

/// 两重门控验证一个 Candidate edit。
///
/// 工具级 candidate（pathology 只出现在 `failure_traces`，不出现在 turn 级
/// trace）跳过 Gate 2 的 task_success z-test（该统计只适用于 turn 级信号），
/// 保持 Candidate 等待阶段 3 的工具级验证逻辑。
#[must_use]
pub fn validate_candidate(
    candidate: &HarnessEdit,
    trace_window: &[crate::trace_analyzer::TraceRecord],
    failure_traces: &[FailureTrace],
    tool_stats: &[ToolCallStat],
    baseline_rate: f64,
    config: &EvolutionConfig,
) -> ValidationOutcome {
    // Gate 1: Validity(基础设施有效性 + pathology 出现确认)
    if let Err(reason) = validity_gate(candidate, trace_window, failure_traces) {
        return ValidationOutcome::Retired(reason);
    }

    // 工具级 candidate：pathology 不在 turn 级 trace 的 failure_kind 里，
    // 说明其失败样本来自 failure_traces（工具调用级），走工具级失败率 z-test。
    let is_tool_level = !trace_window
        .iter()
        .any(|t| t.failure_kind.as_deref() == Some(candidate.pathology.as_str()));
    if is_tool_level {
        return match tool_level_significance_gate(candidate, failure_traces, tool_stats, config) {
            SignificanceResult::Promote => ValidationOutcome::Promoted,
            SignificanceResult::Keep => {
                ValidationOutcome::StillCandidate("insufficient data".into())
            }
            SignificanceResult::Reject => {
                ValidationOutcome::Retired("significant failure-rate increase".into())
            }
        };
    }

    // Gate 2: Significance(统计显著性，仅 turn 级)
    match significance_gate(candidate, trace_window, baseline_rate, config) {
        SignificanceResult::Promote => ValidationOutcome::Promoted,
        SignificanceResult::Keep => ValidationOutcome::StillCandidate("insufficient data".into()),
        SignificanceResult::Reject => ValidationOutcome::Retired("significant degradation".into()),
    }
}

/// Gate 1: 排除基础设施噪声导致的假阳性 + 确认 pathology 在窗口内出现。
///
/// pathology 出现检查同时覆盖两个来源：
/// - turn 级 trace 的 `failure_kind`（turn 级 weakness）
/// - failure_traces 的工具级签名（tool 级 weakness）
/// 任一来源出现即视为可归因。
fn validity_gate(
    candidate: &HarnessEdit,
    trace_window: &[crate::trace_analyzer::TraceRecord],
    failure_traces: &[FailureTrace],
) -> Result<(), String> {
    // 1. 基础设施失败占比(网络超时/沙箱崩溃等):超过 1/3 时结果不可信。
    let infra_failures = trace_window
        .iter()
        .filter(|t| {
            matches!(
                t.failure_kind.as_deref(),
                Some("network_timeout")
                    | Some("sandbox_crash")
                    | Some("verifier_timeout")
                    | Some("api_rate_limit")
            )
        })
        .count();
    if !trace_window.is_empty() && infra_failures * 3 > trace_window.len() {
        return Err("infrastructure failures dominate window, results unreliable".into());
    }

    // 2. pathology 必须在窗口内出现(无失败样本 → 无法归因)。
    // 工具级 candidate 的 pathology（如 "edit_file:old_string not found"）不在
    // turn 级 failure_kind 里，需在 failure_traces 的工具签名里确认出现。
    let turn_occurrences = trace_window
        .iter()
        .filter(|t| t.failure_kind.as_deref() == Some(candidate.pathology.as_str()))
        .count();
    let tool_occurrences = failure_traces
        .iter()
        .filter(|ft| {
            ft.steps
                .iter()
                .any(|s| s.is_error && tool_signature(s) == candidate.pathology)
        })
        .count();
    if turn_occurrences == 0 && tool_occurrences == 0 {
        return Err("pathology did not occur in validation window".into());
    }

    Ok(())
}

/// Gate 2 的统计决策。
#[derive(Debug)]
enum SignificanceResult {
    Promote,
    Keep,
    Reject,
}

/// Gate 2: 窗口内 TaskSuccessRate 与 baseline 的 z-test。
fn significance_gate(
    _candidate: &HarnessEdit,
    trace_window: &[crate::trace_analyzer::TraceRecord],
    baseline_rate: f64,
    config: &EvolutionConfig,
) -> SignificanceResult {
    let window_rate = compute_task_success_rate(trace_window);
    let n = trace_window.len() as f64;
    if n < 3.0 {
        return SignificanceResult::Keep; // 样本不足
    }

    let diff = window_rate - baseline_rate;
    let std_error = (baseline_rate * (1.0 - baseline_rate) / n).sqrt();
    let z_score = if std_error > 0.0 {
        diff / std_error
    } else {
        0.0
    };

    // alpha = 0.05 → |z| > 1.96
    let threshold = 1.96;
    if z_score > threshold && window_rate > config.promote_threshold {
        return SignificanceResult::Promote;
    }
    if z_score < -threshold {
        return SignificanceResult::Reject;
    }
    SignificanceResult::Keep
}

/// 工具级失败率 z-test 的最小样本阈值（该工具总调用次数低于此则样本不足）。
const MIN_TOOL_CALLS: usize = 5;

/// Gate 2（工具级）：工具级 candidate 的失败率 z-test。
///
/// pathology = `"{tool_name}:{keyword}"`，失败率 = 该 pathology 失败次数 /
/// 该 tool 总调用次数。观察窗口（最近 `validation_window` 条工具调用）的失败率
/// 与基线失败率做 z-test，窗口失败率**显著更低**（下降）→ Promote。
///
/// 与 turn 级 [`significance_gate`] 的方向相反：turn 级是 task_success 上升
/// 为有效，工具级是失败率下降为有效。
fn tool_level_significance_gate(
    candidate: &HarnessEdit,
    failure_traces: &[FailureTrace],
    tool_stats: &[ToolCallStat],
    config: &EvolutionConfig,
) -> SignificanceResult {
    // 从 pathology 提取 tool_name（"edit_file:old_string not found" → "edit_file"）。
    let tool_name = candidate
        .pathology
        .split(':')
        .next()
        .unwrap_or(&candidate.pathology)
        .to_string();

    // 基线：全部数据的失败率。
    let baseline_failures = count_pathology_failures(failure_traces, &candidate.pathology);
    let baseline_calls = tool_stats
        .iter()
        .filter(|ts| ts.tool_name == tool_name)
        .count();
    if baseline_calls < MIN_TOOL_CALLS {
        return SignificanceResult::Keep; // 样本不足
    }
    let baseline_rate = baseline_failures as f64 / baseline_calls as f64;

    // 观察窗口：最近 validation_window 条工具调用的时间起点。
    let window_start = tool_stats
        .iter()
        .rev()
        .take(config.validation_window)
        .map(|ts| ts.recorded_at_ms)
        .min()
        .unwrap_or(0);
    let window_failures = failure_traces
        .iter()
        .filter(|ft| {
            ft.recorded_at_ms >= window_start
                && ft
                    .steps
                    .iter()
                    .any(|s| s.is_error && tool_signature(s) == candidate.pathology)
        })
        .count();
    let window_calls = tool_stats
        .iter()
        .filter(|ts| ts.recorded_at_ms >= window_start && ts.tool_name == tool_name)
        .count();
    let n = window_calls as f64;
    if n < 3.0 {
        return SignificanceResult::Keep; // 窗口样本不足
    }
    let window_rate = window_failures as f64 / window_calls as f64;

    // z-test：窗口失败率显著低于基线（diff > 0）→ 规则有效。
    let diff = baseline_rate - window_rate;
    let std_error = (baseline_rate * (1.0 - baseline_rate) / n).sqrt();
    let z_score = if std_error > 0.0 {
        diff / std_error
    } else {
        0.0
    };

    let threshold = 1.96;
    if z_score > threshold {
        return SignificanceResult::Promote;
    }
    if z_score < -threshold {
        return SignificanceResult::Reject; // 失败率显著上升 → 规则无效
    }
    SignificanceResult::Keep
}

/// 统计某个 pathology 在 failure_traces 里的失败记录数。
fn count_pathology_failures(failure_traces: &[FailureTrace], pathology: &str) -> usize {
    failure_traces
        .iter()
        .filter(|ft| {
            ft.steps
                .iter()
                .any(|s| s.is_error && tool_signature(s) == pathology)
        })
        .count()
}

/// 窗口内 TaskSuccessRate。
#[must_use]
pub fn compute_task_success_rate(trace_window: &[crate::trace_analyzer::TraceRecord]) -> f64 {
    if trace_window.is_empty() {
        return 0.0;
    }
    let successes = trace_window.iter().filter(|t| t.task_success).count();
    successes as f64 / trace_window.len() as f64
}

/// 全部记录上的 baseline TaskSuccessRate。
fn compute_baseline_rate(trace: &TraceAnalyzer) -> f64 {
    compute_task_success_rate(&trace.records)
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// Evolution 主入口(无状态函数,同步执行)。
///
/// 在 conversation.rs 每 `evolution_interval` turn 调用一次:
/// 1. Weakness Mining(工具级 `failure_traces` + turn 级 `trace` 合并)
/// → 2. 规则 Proposer → 3. 新增 Candidate → 4. 验证所有 Candidate(两重门控)。
pub fn evolve(
    trace: &TraceAnalyzer,
    failure_traces: &[FailureTrace],
    tool_stats: &[ToolCallStat],
    archive: &HarnessArchive,
    config: &EvolutionConfig,
) -> Result<EvolutionReport, ArchiveError> {
    let mut report = EvolutionReport::default();

    // Stage 1: Weakness Mining
    // 工具级信号（failure_traces）优先且更精确，turn 级信号（trace）补充。
    // 两者 pathology 签名不同（"{tool_name}:{keyword}" vs failure_kind），
    // 且捕获的失败类型互补（工具调用失败 vs turn 级 LLM 失败），合并不重复。
    // 工具级在前，propose_edits 的 max_proposals 会优先处理工具级信号。
    let mut weaknesses = mine_weaknesses_from_traces(failure_traces, config.min_occurrences);
    weaknesses.extend(mine_weaknesses(
        trace,
        config.validation_window,
        config.min_occurrences,
    ));
    report.weaknesses_count = weaknesses.len();

    // Stage 2: Mixed Proposer(规则优先 + simhash 去重)
    let existing = archive.active_edits()?;
    let proposals = propose_edits(&weaknesses, &existing, config);
    report.proposals_count = proposals.len();
    for proposal in proposals {
        archive.add_candidate(proposal)?;
    }

    // Stage 3: 验证所有 Candidate edits（工具级走失败率 z-test）
    validate_all_candidates(
        trace,
        failure_traces,
        tool_stats,
        archive,
        config,
        &mut report,
    )?;

    Ok(report)
}

/// 验证所有 Candidate edits(两重门控)。
fn validate_all_candidates(
    trace: &TraceAnalyzer,
    failure_traces: &[FailureTrace],
    tool_stats: &[ToolCallStat],
    archive: &HarnessArchive,
    config: &EvolutionConfig,
    report: &mut EvolutionReport,
) -> Result<(), ArchiveError> {
    let window = recent_window(trace, config.validation_window);
    let baseline_rate = compute_baseline_rate(trace);
    let candidates = archive.candidate_edits()?;

    for candidate in candidates {
        let outcome = validate_candidate(
            &candidate,
            window,
            failure_traces,
            tool_stats,
            baseline_rate,
            config,
        );
        match outcome {
            ValidationOutcome::Promoted => {
                archive.update_status(&candidate.id, EditStatus::Active)?;
                report.promoted_count += 1;
            }
            ValidationOutcome::StillCandidate(_) => {
                // 不改状态,继续观察。
            }
            ValidationOutcome::Retired(reason) => {
                archive.update_status(&candidate.id, EditStatus::Retired)?;
                archive.set_retire_reason(&candidate.id, &reason)?;
                report.retired_count += 1;
            }
        }
    }
    Ok(())
}

/// 生成注入到 dynamic_sections 的文本(全量注入,≤ MAX_ACTIVE_EDITS 条)。
///
/// 每条为一段独立指令。总 token 受 Active 上限(10 条 × 500 chars)约束。
pub fn render_for_injection(archive: &HarnessArchive) -> Result<Vec<String>, ArchiveError> {
    let active = archive.active_edits()?;
    Ok(active.into_iter().map(|e| e.content).collect())
}

/// 从 FailureCluster 构造 WeaknessSignal(供单元测试)。
#[cfg(test)]
pub(crate) fn weakness_from_cluster(
    cluster: &crate::trace_analyzer::FailureCluster,
) -> WeaknessSignal {
    WeaknessSignal {
        pathology: cluster.label.clone(),
        sample_errors: cluster
            .sample_errors
            .iter()
            .take(MAX_SAMPLE_ERRORS)
            .cloned()
            .collect(),
        occurrence_count: cluster.count,
        related_turns: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure_trace::{FailureTrace, TraceToolStep};
    use crate::tool_call_stats::ToolCallStat;
    use crate::trace_analyzer::TraceRecord;

    fn failed_record(turn_id: &str, kind: &str, msg: &str) -> TraceRecord {
        TraceRecord::new(turn_id, 100, 2).with_failure(kind, msg)
    }

    /// 构造一条含一个失败步骤的 FailureTrace(前面一个成功步骤 + 失败步骤)。
    fn failed_trace(turn_id: &str, failed_tool: &str, output: &str) -> FailureTrace {
        FailureTrace::new(
            turn_id,
            "sess",
            "tool_error",
            vec![
                TraceToolStep {
                    tool_name: "Read".to_string(),
                    input: "{}".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                },
                TraceToolStep {
                    tool_name: failed_tool.to_string(),
                    input: "{}".to_string(),
                    output: output.to_string(),
                    is_error: true,
                },
            ],
        )
    }

    /// 构造一条带指定时间戳的失败轨迹（用于时间窗口测试）。
    fn trace_at(turn_id: &str, failed_tool: &str, output: &str, ts: u64) -> FailureTrace {
        FailureTrace {
            turn_id: turn_id.to_string(),
            session_id: "sess".to_string(),
            failure_kind: "tool_error".to_string(),
            steps: vec![
                TraceToolStep {
                    tool_name: "Read".to_string(),
                    input: "{}".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                },
                TraceToolStep {
                    tool_name: failed_tool.to_string(),
                    input: "{}".to_string(),
                    output: output.to_string(),
                    is_error: true,
                },
            ],
            recorded_at_ms: ts,
        }
    }

    /// 构造一条带指定时间戳的工具调用统计。
    fn stat_at(tool_name: &str, ts: u64) -> ToolCallStat {
        ToolCallStat {
            tool_name: tool_name.to_string(),
            is_error: false,
            recorded_at_ms: ts,
        }
    }

    /// 构造一个工具级 candidate（pathology = "edit_file:old_string not found"）。
    fn tool_candidate() -> HarnessEdit {
        HarnessEdit {
            id: "edit-tool".to_string(),
            pathology: "edit_file:old_string not found".to_string(),
            content: "grep first".to_string(),
            status: EditStatus::Candidate,
            source: EditSource::RulePattern,
            verify_count: 0,
            success_count: 0,
            created_at: 0,
            last_verified_at: None,
            proposer_reasoning: "r".to_string(),
            similarity_hash: 0,
            retire_reason: None,
        }
    }

    #[test]
    fn tool_level_significance_gate_promotes_when_failure_rate_drops() {
        // 10 条 edit_file 调用（t=1..10），早期 5 条失败，近期 5 条成功 → 失败率下降。
        let tool_stats: Vec<ToolCallStat> = (1..=10).map(|t| stat_at("edit_file", t)).collect();
        let failure_traces: Vec<FailureTrace> = (1..=5)
            .map(|t| trace_at(&format!("t{t}"), "edit_file", "old_string not found", t))
            .collect();
        let candidate = tool_candidate();
        let config = EvolutionConfig {
            validation_window: 5,
            ..EvolutionConfig::default()
        };
        let result =
            tool_level_significance_gate(&candidate, &failure_traces, &tool_stats, &config);
        assert!(
            matches!(result, SignificanceResult::Promote),
            "失败率下降应晋升，got {result:?}"
        );
    }

    #[test]
    fn tool_level_significance_gate_rejects_when_failure_rate_rises() {
        // 早期 5 条成功，近期 5 条失败 → 失败率上升。
        let tool_stats: Vec<ToolCallStat> = (1..=10).map(|t| stat_at("edit_file", t)).collect();
        let failure_traces: Vec<FailureTrace> = (6..=10)
            .map(|t| trace_at(&format!("t{t}"), "edit_file", "old_string not found", t))
            .collect();
        let candidate = tool_candidate();
        let config = EvolutionConfig {
            validation_window: 5,
            ..EvolutionConfig::default()
        };
        let result =
            tool_level_significance_gate(&candidate, &failure_traces, &tool_stats, &config);
        assert!(
            matches!(result, SignificanceResult::Reject),
            "失败率上升应退役，got {result:?}"
        );
    }

    #[test]
    fn tool_level_significance_gate_keeps_when_insufficient_samples() {
        // 只有 2 条调用（< MIN_TOOL_CALLS=5）→ 样本不足。
        let tool_stats: Vec<ToolCallStat> = (1..=2).map(|t| stat_at("edit_file", t)).collect();
        let failure_traces: Vec<FailureTrace> =
            vec![trace_at("t1", "edit_file", "old_string not found", 1)];
        let candidate = tool_candidate();
        let result = tool_level_significance_gate(
            &candidate,
            &failure_traces,
            &tool_stats,
            &EvolutionConfig::default(),
        );
        assert!(matches!(result, SignificanceResult::Keep));
    }

    #[test]
    fn mine_weaknesses_filters_low_frequency_clusters() {
        let mut analyzer = TraceAnalyzer::new();
        // 两条相同 failure_kind(≥ min_occurrences=2)。
        analyzer.add_record(failed_record("t1", "runtime_error", "old_string not found"));
        analyzer.add_record(failed_record("t2", "runtime_error", "old_string not found"));
        // 一条低频(应被过滤)。
        analyzer.add_record(failed_record("t3", "sandbox_crash", "boom"));

        let signals = mine_weaknesses(&analyzer, 10, 2);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].pathology, "runtime_error");
        assert_eq!(signals[0].occurrence_count, 2);
        assert_eq!(signals[0].related_turns.len(), 2);
    }

    #[test]
    fn mine_weaknesses_from_traces_groups_by_tool_signature() {
        let traces = vec![
            failed_trace(
                "t1",
                "edit_file",
                "error: old_string not found in src/lib.rs",
            ),
            failed_trace("t2", "edit_file", "old_string not found"),
        ];
        let signals = mine_weaknesses_from_traces(&traces, 2);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].pathology, "edit_file:old_string not found");
        assert_eq!(signals[0].occurrence_count, 2);
        assert_eq!(signals[0].related_turns, vec!["t1", "t2"]);
        // sample_errors 保留原始失败输出（用于后续 Proposer 展示）。
        assert_eq!(signals[0].sample_errors.len(), 2);
    }

    #[test]
    fn mine_weaknesses_from_traces_filters_low_frequency() {
        let traces = vec![
            failed_trace("t1", "edit_file", "old_string not found"),
            failed_trace("t2", "edit_file", "old_string not found"),
            failed_trace("t3", "grep_search", "connection refused"),
        ];
        let signals = mine_weaknesses_from_traces(&traces, 2);
        assert_eq!(signals.len(), 1, "低频的 connection refused 应被过滤");
        assert_eq!(signals[0].pathology, "edit_file:old_string not found");
    }

    #[test]
    fn mine_weaknesses_from_traces_uses_tool_name_when_no_keyword() {
        let traces = vec![
            failed_trace("t1", "custom_tool", "some novel failure"),
            failed_trace("t2", "custom_tool", "another novel failure"),
        ];
        let signals = mine_weaknesses_from_traces(&traces, 2);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].pathology, "custom_tool");
    }

    #[test]
    fn mine_weaknesses_from_traces_takes_first_failed_step() {
        // 两个失败步骤：第一个是根本原因，第二个是连锁失败。
        let trace = FailureTrace::new(
            "t1",
            "sess",
            "tool_error",
            vec![
                TraceToolStep {
                    tool_name: "edit_file".to_string(),
                    input: "{}".to_string(),
                    output: "old_string not found".to_string(),
                    is_error: true,
                },
                TraceToolStep {
                    tool_name: "bash".to_string(),
                    input: "{}".to_string(),
                    output: "connection refused".to_string(),
                    is_error: true,
                },
            ],
        );
        let signals = mine_weaknesses_from_traces(&[trace], 1);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].pathology, "edit_file:old_string not found");
    }

    #[test]
    fn mine_weaknesses_from_traces_skips_all_success() {
        // 全成功轨迹(防御性：理论上 extract 已过滤，但入口仍应安全跳过)。
        let trace = FailureTrace::new(
            "t1",
            "sess",
            "tool_error",
            vec![TraceToolStep {
                tool_name: "Read".to_string(),
                input: "{}".to_string(),
                output: "ok".to_string(),
                is_error: false,
            }],
        );
        let signals = mine_weaknesses_from_traces(&[trace], 1);
        assert!(signals.is_empty());
    }

    #[test]
    fn rule_based_propose_matches_known_pathology() {
        let cluster = crate::trace_analyzer::FailureCluster {
            label: "runtime_error".to_string(),
            count: 3,
            sample_errors: vec!["error: old_string not found in src/lib.rs".to_string()],
        };
        let weakness = weakness_from_cluster(&cluster);

        let edit = rule_based_propose(&weakness).expect("rule should match");
        assert_eq!(edit.status, EditStatus::Candidate);
        assert_eq!(edit.source, EditSource::RulePattern);
        assert!(edit.content.contains("old_string not found"));
        assert!(edit
            .proposer_reasoning
            .contains("edit_old_string_not_found"));
    }

    #[test]
    fn rule_based_propose_ignores_unknown_pathology() {
        let cluster = crate::trace_analyzer::FailureCluster {
            label: "custom_api_error".to_string(),
            count: 5,
            sample_errors: vec!["totally novel failure".to_string()],
        };
        let weakness = weakness_from_cluster(&cluster);
        assert!(rule_based_propose(&weakness).is_none());
    }

    #[test]
    fn propose_edits_deduplicates_by_simhash() {
        let cluster = crate::trace_analyzer::FailureCluster {
            label: "runtime_error".to_string(),
            count: 3,
            sample_errors: vec!["old_string not found".to_string()],
        };
        let weakness = weakness_from_cluster(&cluster);
        let existing = propose_edits(
            std::slice::from_ref(&weakness),
            &[],
            &EvolutionConfig::default(),
        );
        assert_eq!(existing.len(), 1);

        // 同一 weakness 再次提议:与已有 edit simhash 重复 → 跳过。
        let second = propose_edits(&[weakness], &existing, &EvolutionConfig::default());
        assert_eq!(second.len(), 0, "simhash 重复的 edit 应被去重");
    }

    #[test]
    fn validate_candidate_retires_when_pathology_absent() {
        // Validity Gate:pathology 未在窗口内出现 → Retired。
        let candidate = HarnessEdit {
            id: "edit-1".to_string(),
            pathology: "edit_old_string_not_found".to_string(),
            content: "use grep first".to_string(),
            status: EditStatus::Candidate,
            source: EditSource::RulePattern,
            verify_count: 0,
            success_count: 0,
            created_at: 0,
            last_verified_at: None,
            proposer_reasoning: "r".to_string(),
            similarity_hash: 0,
            retire_reason: None,
        };
        let window = vec![
            failed_record("t1", "other_error", "different failure"),
            failed_record("t2", "other_error", "different failure"),
        ];
        let outcome = validate_candidate(
            &candidate,
            &window,
            &[],
            &[],
            0.5,
            &EvolutionConfig::default(),
        );
        assert!(
            matches!(outcome, ValidationOutcome::Retired(reason) if reason.contains("did not occur"))
        );
    }

    #[test]
    fn validate_candidate_retires_when_infra_failures_dominate() {
        let candidate = HarnessEdit {
            id: "edit-2".to_string(),
            pathology: "network_timeout".to_string(),
            content: "check service".to_string(),
            status: EditStatus::Candidate,
            source: EditSource::RulePattern,
            verify_count: 0,
            success_count: 0,
            created_at: 0,
            last_verified_at: None,
            proposer_reasoning: "r".to_string(),
            similarity_hash: 0,
            retire_reason: None,
        };
        let window = vec![
            failed_record("t1", "network_timeout", "timeout"),
            failed_record("t2", "network_timeout", "timeout"),
            failed_record("t3", "network_timeout", "timeout"),
        ];
        let outcome = validate_candidate(
            &candidate,
            &window,
            &[],
            &[],
            0.5,
            &EvolutionConfig::default(),
        );
        assert!(
            matches!(outcome, ValidationOutcome::Retired(reason) if reason.contains("infrastructure"))
        );
    }

    #[test]
    fn compute_task_success_rate_counts_successful_turns() {
        let mut window = vec![TraceRecord::new("t1", 10, 0), TraceRecord::new("t2", 10, 0)];
        window.push(failed_record("t3", "runtime_error", "boom"));
        let rate = compute_task_success_rate(&window);
        assert!((rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn evolve_produces_candidate_from_rule_pattern() {
        // 端到端:两条同类失败 → evolve 产生一条 Candidate 规则 edit。
        let tmp = std::env::temp_dir().join(format!(
            "claw-harness-evolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let archive = HarnessArchive::open(&tmp).expect("open archive");
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(failed_record("t1", "runtime_error", "old_string not found"));
        analyzer.add_record(failed_record("t2", "runtime_error", "old_string not found"));

        let config = EvolutionConfig {
            validation_window: 10,
            min_occurrences: 2,
            ..EvolutionConfig::default()
        };
        let report = evolve(&analyzer, &[], &[], &archive, &config).expect("evolve");
        assert_eq!(report.weaknesses_count, 1);
        assert_eq!(report.proposals_count, 1);

        let candidates = archive.candidate_edits().expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].status, EditStatus::Candidate);
        assert!(candidates[0].content.contains("old_string not found"));

        // Active edits 暂为空(门控需窗口数据 + baseline),注入渲染为空。
        let sections = render_for_injection(&archive).expect("render");
        assert!(sections.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn evolve_produces_candidate_from_failure_traces() {
        // 端到端：工具级失败轨迹 → evolve 产生工具级 Candidate edit。
        // 用空 TraceAnalyzer 隔离 turn 级信号，验证工具级 weakness 真正进入闭环。
        let tmp = std::env::temp_dir().join(format!(
            "claw-harness-evolve-traces-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let archive = HarnessArchive::open(&tmp).expect("open archive");
        let analyzer = TraceAnalyzer::new(); // 空：无 turn 级信号

        // 两条工具级失败轨迹（edit_file: old_string not found）。
        let traces = vec![
            failed_trace("t1", "edit_file", "old_string not found in src/lib.rs"),
            failed_trace("t2", "edit_file", "old_string not found"),
        ];

        let config = EvolutionConfig {
            validation_window: 10,
            min_occurrences: 2,
            ..EvolutionConfig::default()
        };
        let report = evolve(&analyzer, &traces, &[], &archive, &config).expect("evolve");
        assert_eq!(report.weaknesses_count, 1);
        assert_eq!(report.proposals_count, 1);

        let candidates = archive.candidate_edits().expect("candidates");
        assert_eq!(candidates.len(), 1);
        // pathology 应为工具级签名（而非 turn 级 "runtime_error"）。
        assert_eq!(candidates[0].pathology, "edit_file:old_string not found");
        assert!(candidates[0].content.contains("old_string not found"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn trace_record_task_success_semantics() {
        // 成功记录默认 task_success=true;with_failure 置 false。
        let ok = TraceRecord::new("t1", 10, 0);
        assert!(ok.task_success);
        let fail = failed_record("t2", "runtime_error", "boom");
        assert!(!fail.task_success);
    }

    /// 注入 stub proposer,验证 llm_based_propose 走 LLM 路径、生成 LlmProposer
    /// edit 并对超长 content 截断到 MAX_EDIT_CONTENT_CHARS。
    #[test]
    fn llm_based_propose_uses_injected_proposer_and_truncates() {
        struct StubProposer;
        impl HarnessProposer for StubProposer {
            fn propose(&self, weakness: &WeaknessSignal) -> Option<(String, String)> {
                // 返回超长 content,验证 llm_based_propose 的截断逻辑。
                let content = format!("Always check {}{}", weakness.pathology, "x".repeat(2000));
                Some((content, "stub reasoning".to_string()))
            }
        }
        // OnceLock 全局单例:测试进程内只 set 一次;无其他测试依赖"未注入"状态。
        set_global_harness_proposer(std::sync::Arc::new(StubProposer));

        let cluster = crate::trace_analyzer::FailureCluster {
            label: "novel_api_error".to_string(),
            count: 3,
            sample_errors: vec!["brand new failure mode".to_string()],
        };
        let weakness = weakness_from_cluster(&cluster);

        let edit = llm_based_propose(&weakness).expect("llm proposer should produce edit");
        assert_eq!(edit.source, EditSource::LlmProposer);
        assert!(edit.content.contains("novel_api_error"));
        assert_eq!(edit.proposer_reasoning, "stub reasoning");
        assert_eq!(edit.status, EditStatus::Candidate);
        // 超长 content 被截断到上限。
        assert_eq!(
            edit.content.chars().count(),
            crate::harness_evolution::archive::MAX_EDIT_CONTENT_CHARS
        );
    }
}
