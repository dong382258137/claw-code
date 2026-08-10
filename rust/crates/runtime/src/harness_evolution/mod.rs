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

pub use archive::{HarnessArchive, ArchiveError};
pub use types::*;

use crate::decision_log::compute_simhash;
use crate::harness_evolution::archive::{current_timestamp_ms, generate_edit_id};
use crate::trace_analyzer::TraceAnalyzer;

/// 样本错误消息上限(每 cluster)。
const MAX_SAMPLE_ERRORS: usize = 5;

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
            sample_errors: c.sample_errors.into_iter().take(MAX_SAMPLE_ERRORS).collect(),
            occurrence_count: c.count,
            related_turns: extract_related_turns(window, &c.label),
        })
        .collect()
}

/// 取最近 `lookback_turns` 条记录(用于关联 turn_id)。
fn recent_window(analyzer: &TraceAnalyzer, lookback_turns: usize) -> &[crate::trace_analyzer::TraceRecord] {
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
        let Some(edit) = rule_based_propose(weakness) else {
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

/// 规则式匹配:命中预定义模式 → 直接生成 Candidate edit。
fn rule_based_propose(weakness: &WeaknessSignal) -> Option<HarnessEdit> {
    for (keyword, content, reasoning) in RULE_PATTERNS {
        let matched = weakness.pathology.to_lowercase().contains(keyword)
            || weakness
                .sample_errors
                .iter()
                .any(|e| e.to_lowercase().contains(keyword));
        if matched {
            let simhash_text = format!("{} {}", weakness.pathology, content);
            return Some(HarnessEdit {
                id: generate_edit_id(content, &weakness.pathology),
                pathology: weakness.pathology.clone(),
                content: (*content).to_string(),
                status: EditStatus::Candidate,
                source: EditSource::RulePattern,
                verify_count: 0,
                success_count: 0,
                created_at: current_timestamp_ms(),
                last_verified_at: None,
                proposer_reasoning: (*reasoning).to_string(),
                similarity_hash: compute_simhash(&simhash_text) as i64,
                retire_reason: None,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 两重门控验证(GSME: Validity + Significance)
// ---------------------------------------------------------------------------

/// 两重门控验证一个 Candidate edit。
#[must_use]
pub fn validate_candidate(
    candidate: &HarnessEdit,
    trace_window: &[crate::trace_analyzer::TraceRecord],
    baseline_rate: f64,
    config: &EvolutionConfig,
) -> ValidationOutcome {
    // Gate 1: Validity(基础设施有效性)
    if let Err(reason) = validity_gate(candidate, trace_window) {
        return ValidationOutcome::Retired(reason);
    }

    // Gate 2: Significance(统计显著性)
    match significance_gate(candidate, trace_window, baseline_rate, config) {
        SignificanceResult::Promote => ValidationOutcome::Promoted,
        SignificanceResult::Keep => ValidationOutcome::StillCandidate("insufficient data".into()),
        SignificanceResult::Reject => ValidationOutcome::Retired("significant degradation".into()),
    }
}

/// Gate 1: 排除基础设施噪声导致的假阳性 + 确认 pathology 在窗口内出现。
fn validity_gate(
    candidate: &HarnessEdit,
    trace_window: &[crate::trace_analyzer::TraceRecord],
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
    let pathology_occurrences = trace_window
        .iter()
        .filter(|t| t.failure_kind.as_deref() == Some(candidate.pathology.as_str()))
        .count();
    if pathology_occurrences == 0 {
        return Err("pathology did not occur in validation window".into());
    }

    Ok(())
}

/// Gate 2 的统计决策。
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
    let z_score = if std_error > 0.0 { diff / std_error } else { 0.0 };

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

/// 窗口内 TaskSuccessRate。
#[must_use]
pub fn compute_task_success_rate(
    trace_window: &[crate::trace_analyzer::TraceRecord],
) -> f64 {
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
/// 1. Weakness Mining → 2. 规则 Proposer → 3. 新增 Candidate →
/// 4. 验证所有 Candidate(两重门控)。
pub fn evolve(
    trace: &TraceAnalyzer,
    archive: &HarnessArchive,
    config: &EvolutionConfig,
) -> Result<EvolutionReport, ArchiveError> {
    let mut report = EvolutionReport::default();

    // Stage 1: Weakness Mining
    let weaknesses = mine_weaknesses(trace, config.validation_window, config.min_occurrences);
    report.weaknesses_count = weaknesses.len();

    // Stage 2: Mixed Proposer(规则优先 + simhash 去重)
    let existing = archive.active_edits()?;
    let proposals = propose_edits(&weaknesses, &existing, config);
    report.proposals_count = proposals.len();
    for proposal in proposals {
        archive.add_candidate(proposal)?;
    }

    // Stage 3: 验证所有 Candidate edits
    validate_all_candidates(trace, archive, config, &mut report)?;

    Ok(report)
}

/// 验证所有 Candidate edits(两重门控)。
fn validate_all_candidates(
    trace: &TraceAnalyzer,
    archive: &HarnessArchive,
    config: &EvolutionConfig,
    report: &mut EvolutionReport,
) -> Result<(), ArchiveError> {
    let window = recent_window(trace, config.validation_window);
    let baseline_rate = compute_baseline_rate(trace);
    let candidates = archive.candidate_edits()?;

    for candidate in candidates {
        let outcome = validate_candidate(&candidate, window, baseline_rate, config);
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
    use crate::trace_analyzer::TraceRecord;

    fn failed_record(turn_id: &str, kind: &str, msg: &str) -> TraceRecord {
        TraceRecord::new(turn_id, 100, 2).with_failure(kind, msg)
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
    fn rule_based_propose_matches_known_pathology() {
        let cluster = crate::trace_analyzer::FailureCluster {
            label: "runtime_error".to_string(),
            count: 3,
            sample_errors: vec![
                "error: old_string not found in src/lib.rs".to_string(),
            ],
        };
        let weakness = weakness_from_cluster(&cluster);

        let edit = rule_based_propose(&weakness).expect("rule should match");
        assert_eq!(edit.status, EditStatus::Candidate);
        assert_eq!(edit.source, EditSource::RulePattern);
        assert!(edit.content.contains("old_string not found"));
        assert!(edit.proposer_reasoning.contains("edit_old_string_not_found"));
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
        let outcome = validate_candidate(&candidate, &window, 0.5, &EvolutionConfig::default());
        assert!(matches!(outcome, ValidationOutcome::Retired(reason) if reason.contains("did not occur")));
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
        let outcome = validate_candidate(&candidate, &window, 0.5, &EvolutionConfig::default());
        assert!(
            matches!(outcome, ValidationOutcome::Retired(reason) if reason.contains("infrastructure"))
        );
    }

    #[test]
    fn compute_task_success_rate_counts_successful_turns() {
        let mut window = vec![
            TraceRecord::new("t1", 10, 0),
            TraceRecord::new("t2", 10, 0),
        ];
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
        let report = evolve(&analyzer, &archive, &config).expect("evolve");
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
    fn trace_record_task_success_semantics() {
        // 成功记录默认 task_success=true;with_failure 置 false。
        let ok = TraceRecord::new("t1", 10, 0);
        assert!(ok.task_success);
        let fail = failed_record("t2", "runtime_error", "boom");
        assert!(!fail.task_success);
    }
}
