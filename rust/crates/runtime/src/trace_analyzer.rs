//! Trace Analyzer — Step 3.3 Telemetry 导出与 Trace Analyzer 基础。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 3.3
//!
//! 架构:
//! - [`TraceRecord`]:简化的本地 trace 记录类型(一行 CSV 对应一条记录)。
//! - [`TraceAnalyzer`]:加载/导出 CSV,计算基础统计,简单失败聚类。
//! - [`TraceStats`]:turn latency / tool call count / compact 触发率等指标直方图。
//! - [`FailureCluster`]:按 `failure_kind` 简单分桶(阶段 4 替换为 K-means on embeddings)。
//!
//! **不在本步骤实现**(留到阶段 4 Self-Improving Harness):
//! - OTLP exporter(默认 CSV exporter 已足够支撑阶段 4 入口)
//! - 真正的 K-means 聚类(需要 `error_message` embedding)
//! - 闭环反馈到 `RecoveryOrchestrator`
//!
//! **缓存影响**:无 — 纯观测层,不进入 prompt 稳定区/变动区。

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// CSV 表头,固定顺序与 [`TraceRecord`] 字段一一对应。
pub const CSV_HEADER: &str = "turn_id,latency_ms,tool_calls,compact_triggered,failure_kind,error_message";

/// 每个聚类最多保留的样本错误消息条数,避免聚类膨胀。
pub const MAX_SAMPLE_ERRORS_PER_CLUSTER: usize = 5;

/// 简化的本地 trace 记录类型 — 一行 CSV 对应一条记录。
///
/// 字段对应 CSV 列(见 [`CSV_HEADER`]):
/// `turn_id,latency_ms,tool_calls,compact_triggered,failure_kind,error_message`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRecord {
    /// Turn 唯一标识。
    pub turn_id: String,
    /// 该 turn 端到端延迟(毫秒)。
    pub latency_ms: u64,
    /// 该 turn 内工具调用次数。
    pub tool_calls: u32,
    /// 该 turn 是否触发了 compact(上下文压缩)。
    pub compact_triggered: bool,
    /// 失败类别;`None` 表示该 turn 未失败。
    pub failure_kind: Option<String>,
    /// 错误消息;`None` 表示该 turn 未失败或无错误消息。
    pub error_message: Option<String>,
}

impl TraceRecord {
    /// 构造一条未失败、未触发 compact 的基础记录。
    #[must_use]
    pub fn new(turn_id: impl Into<String>, latency_ms: u64, tool_calls: u32) -> Self {
        Self {
            turn_id: turn_id.into(),
            latency_ms,
            tool_calls,
            compact_triggered: false,
            failure_kind: None,
            error_message: None,
        }
    }

    /// 链式设置 compact 触发标志。
    #[must_use]
    pub fn with_compact_triggered(mut self, triggered: bool) -> Self {
        self.compact_triggered = triggered;
        self
    }

    /// 链式设置失败类别与错误消息。
    #[must_use]
    pub fn with_failure(mut self, kind: impl Into<String>, message: impl Into<String>) -> Self {
        self.failure_kind = Some(kind.into());
        self.error_message = Some(message.into());
        self
    }
}

/// 失败聚类 — 按 `failure_kind` 简单分桶。
///
/// 阶段 4 将替换为 K-means on `(failure_kind, error_message_embedding)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureCluster {
    /// 聚类标签(等于 `failure_kind`)。
    pub label: String,
    /// 该聚类下的样本数。
    pub count: u32,
    /// 该聚类下的样本错误消息(最多 [`MAX_SAMPLE_ERRORS_PER_CLUSTER`] 条)。
    pub sample_errors: Vec<String>,
}

/// 基础统计 — turn latency / tool call count / compact 触发率等指标。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceStats {
    pub total_turns: u64,
    pub total_tool_calls: u64,
    pub avg_turn_latency_ms: f64,
    pub p50_turn_latency_ms: u64,
    pub p99_turn_latency_ms: u64,
    pub compact_trigger_count: u64,
    /// `compact_trigger_count / total_turns`;`total_turns == 0` 时为 0.0。
    pub compact_trigger_rate: f64,
    /// `failure_kind` → count。
    pub failure_counts: HashMap<String, u32>,
    pub failure_clusters: Vec<FailureCluster>,
}

impl Default for TraceStats {
    fn default() -> Self {
        Self {
            total_turns: 0,
            total_tool_calls: 0,
            avg_turn_latency_ms: 0.0,
            p50_turn_latency_ms: 0,
            p99_turn_latency_ms: 0,
            compact_trigger_count: 0,
            compact_trigger_rate: 0.0,
            failure_counts: HashMap::new(),
            failure_clusters: Vec::new(),
        }
    }
}

/// Trace Analyzer — 加载/导出 CSV,计算基础统计,简单失败聚类。
///
/// # Example
/// ```
/// use runtime::trace_analyzer::{TraceAnalyzer, TraceRecord};
///
/// let mut analyzer = TraceAnalyzer::new();
/// analyzer.add_record(TraceRecord::new("t1", 100, 3));
/// let stats = analyzer.stats();
/// assert_eq!(stats.total_turns, 1);
/// assert_eq!(stats.total_tool_calls, 3);
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceAnalyzer {
    /// 已加载的 trace 记录。
    pub records: Vec<TraceRecord>,
}

impl TraceAnalyzer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条 trace 记录。
    pub fn add_record(&mut self, record: TraceRecord) {
        self.records.push(record);
    }

    /// 从 CSV 文件加载 trace 记录。
    ///
    /// 期望 CSV 第一行为表头(见 [`CSV_HEADER`]),后续每行对应一条 [`TraceRecord`]。
    /// 若第一行无法匹配 6 列结构,会被当作数据行处理。
    pub fn load_csv(path: &Path) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let rows = parse_csv_content(&content);
        let mut analyzer = Self::new();
        let mut iter = rows.into_iter();
        if let Some(first) = iter.next() {
            if is_header_row(&first) {
                // skip header
            } else {
                analyzer.add_record(parse_record_fields(&first)?);
            }
            for row in iter {
                // 跳过空行(无法构成完整 6 列结构时,且所有字段为空)。
                if row.iter().all(String::is_empty) {
                    continue;
                }
                analyzer.add_record(parse_record_fields(&row)?);
            }
        }
        Ok(analyzer)
    }

    /// 导出当前 records 到 CSV 文件(覆盖写)。
    ///
    /// 自动创建父目录。第一行为表头,后续每行对应一条 [`TraceRecord`]。
    pub fn export_csv(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "{CSV_HEADER}")?;
        for record in &self.records {
            writeln!(writer, "{}", format_csv_row(record))?;
        }
        writer.flush()?;
        Ok(())
    }

    /// 计算基础统计(直方图、分位数、compact 触发率、失败聚类)。
    ///
    /// `records` 为空时返回 [`TraceStats::default()`](零值)。
    #[must_use]
    pub fn stats(&self) -> TraceStats {
        let total_turns = self.records.len() as u64;
        if total_turns == 0 {
            return TraceStats::default();
        }
        let total_tool_calls: u64 = self.records.iter().map(|r| u64::from(r.tool_calls)).sum();
        let compact_trigger_count: u64 = self
            .records
            .iter()
            .filter(|r| r.compact_triggered)
            .count() as u64;
        let total_latency: u64 = self.records.iter().map(|r| r.latency_ms).sum();
        let avg_turn_latency_ms = total_latency as f64 / total_turns as f64;
        let mut latencies: Vec<u64> = self.records.iter().map(|r| r.latency_ms).collect();
        latencies.sort_unstable();
        let p50_turn_latency_ms = percentile(&latencies, 50);
        let p99_turn_latency_ms = percentile(&latencies, 99);
        let compact_trigger_rate = compact_trigger_count as f64 / total_turns as f64;
        let mut failure_counts: HashMap<String, u32> = HashMap::new();
        for record in &self.records {
            if let Some(kind) = &record.failure_kind {
                *failure_counts.entry(kind.clone()).or_insert(0) += 1;
            }
        }
        let failure_clusters = self.cluster_failures();
        TraceStats {
            total_turns,
            total_tool_calls,
            avg_turn_latency_ms,
            p50_turn_latency_ms,
            p99_turn_latency_ms,
            compact_trigger_count,
            compact_trigger_rate,
            failure_counts,
            failure_clusters,
        }
    }

    /// 简单失败聚类 — 按 `failure_kind` 分桶。
    ///
    /// 阶段 4 将替换为 K-means on `(failure_kind, error_message_embedding)`。
    /// 返回顺序:count 降序,label 升序(确保稳定输出便于断言)。
    #[must_use]
    pub fn cluster_failures(&self) -> Vec<FailureCluster> {
        let mut buckets: HashMap<String, (u32, Vec<String>)> = HashMap::new();
        for record in &self.records {
            let Some(kind) = &record.failure_kind else { continue };
            let (count, errors) = buckets.entry(kind.clone()).or_default();
            *count += 1;
            if let Some(msg) = &record.error_message {
                if errors.len() < MAX_SAMPLE_ERRORS_PER_CLUSTER {
                    errors.push(msg.clone());
                }
            }
        }
        let mut clusters: Vec<FailureCluster> = buckets
            .into_iter()
            .map(|(label, (count, mut errors))| {
                errors.sort();
                FailureCluster {
                    label,
                    count,
                    sample_errors: errors,
                }
            })
            .collect();
        clusters.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
        clusters
    }
}

/// Nearest-rank percentile: `rank = ceil(p/100 * n)`, `index = rank - 1`(clamp 到 `[0, n-1]`)。
///
/// 输入要求:`sorted` 已升序排序。
fn percentile(sorted: &[u64], p: u32) -> u64 {
    let n = sorted.len();
    if n == 0 {
        return 0;
    }
    // ceil(p/100 * n) = (p * n).div_ceil(100)
    let rank = (u64::from(p) * n as u64).div_ceil(100);
    let idx = rank.saturating_sub(1).min((n - 1) as u64) as usize;
    sorted[idx]
}

/// CSV 字段转义:含逗号/引号/换行时,用双引号包裹并把内部双引号加倍。
/// 空字符串输出为 `""`(与 CSV 标准一致,便于 round-trip)。
fn escape_csv_field(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 将一条记录格式化为 CSV 行(不含换行符)。
fn format_csv_row(record: &TraceRecord) -> String {
    [
        escape_csv_field(&record.turn_id),
        record.latency_ms.to_string(),
        record.tool_calls.to_string(),
        record.compact_triggered.to_string(),
        escape_csv_field(record.failure_kind.as_deref().unwrap_or("")),
        escape_csv_field(record.error_message.as_deref().unwrap_or("")),
    ]
    .join(",")
}

/// 判断一行是否为 CSV 表头。
fn is_header_row(fields: &[String]) -> bool {
    let expected = [
        "turn_id",
        "latency_ms",
        "tool_calls",
        "compact_triggered",
        "failure_kind",
        "error_message",
    ];
    fields.len() == expected.len()
        && fields
            .iter()
            .zip(expected.iter())
            .all(|(f, e)| f == e)
}

/// 将一行 CSV 字段解析为 [`TraceRecord`]。
fn parse_record_fields(fields: &[String]) -> Result<TraceRecord, std::io::Error> {
    if fields.len() != 6 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected 6 CSV fields, got {}", fields.len()),
        ));
    }
    let turn_id = fields[0].clone();
    let latency_ms: u64 = fields[1].parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid latency_ms `{}`: {e}", fields[1]),
        )
    })?;
    let tool_calls: u32 = fields[2].parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid tool_calls `{}`: {e}", fields[2]),
        )
    })?;
    let compact_triggered = match fields[3].as_str() {
        "true" => true,
        "false" => false,
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid compact_triggered `{other}`"),
            ));
        }
    };
    let failure_kind = if fields[4].is_empty() {
        None
    } else {
        Some(fields[4].clone())
    };
    let error_message = if fields[5].is_empty() {
        None
    } else {
        Some(fields[5].clone())
    };
    Ok(TraceRecord {
        turn_id,
        latency_ms,
        tool_calls,
        compact_triggered,
        failure_kind,
        error_message,
    })
}

/// 解析 CSV 文本为多行字段(支持引号包裹的多行字段)。
///
/// 返回 `Vec<Vec<String>>`:外层每项一行,内层每项一个字段。
/// 末尾换行符后的空行不会产生额外空行。
fn parse_csv_content(content: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_field = String::new();
    let mut in_quotes = false;
    let mut row_started = false;

    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                if i + 1 < chars.len() && chars[i + 1] == '"' {
                    current_field.push('"');
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            current_field.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                row_started = true;
            }
            ',' => {
                current_row.push(std::mem::take(&mut current_field));
                row_started = true;
            }
            '\n' => {
                if row_started {
                    current_row.push(std::mem::take(&mut current_field));
                    rows.push(std::mem::take(&mut current_row));
                }
                row_started = false;
            }
            '\r' => {
                // 略 — \n 会处理行结束。
            }
            _ => {
                current_field.push(c);
                row_started = true;
            }
        }
        i += 1;
    }
    // 文件末尾无换行符的最后一行。
    if row_started || !current_field.is_empty() {
        current_row.push(current_field);
        rows.push(current_row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!("trace-analyzer-{name}-{ts}.csv"));
        path
    }

    #[test]
    fn add_record_increments_count() {
        let mut analyzer = TraceAnalyzer::new();
        assert_eq!(analyzer.records.len(), 0);
        analyzer.add_record(TraceRecord::new("t1", 100, 2));
        analyzer.add_record(TraceRecord::new("t2", 200, 4));
        assert_eq!(analyzer.records.len(), 2);
        assert_eq!(analyzer.records[0].turn_id, "t1");
        assert_eq!(analyzer.records[1].turn_id, "t2");
    }

    #[test]
    fn stats_computes_avg_latency_correctly() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 1));
        analyzer.add_record(TraceRecord::new("t2", 200, 2));
        analyzer.add_record(TraceRecord::new("t3", 300, 3));
        let stats = analyzer.stats();
        assert_eq!(stats.total_turns, 3);
        // (100+200+300)/3 = 200
        assert!((stats.avg_turn_latency_ms - 200.0).abs() < f64::EPSILON);
        assert_eq!(stats.total_tool_calls, 6);
    }

    #[test]
    fn stats_computes_p50_and_p99_latency() {
        let mut analyzer = TraceAnalyzer::new();
        // 100 turns, latency 1..=100
        for i in 1..=100u64 {
            analyzer.add_record(TraceRecord::new(format!("t{i}"), i, 0));
        }
        let stats = analyzer.stats();
        assert_eq!(stats.total_turns, 100);
        // sorted = [1, 2, ..., 100]; rank(50) = ceil(50) = 50, idx = 49 → 50
        assert_eq!(stats.p50_turn_latency_ms, 50);
        // rank(99) = ceil(99) = 99, idx = 98 → 99
        assert_eq!(stats.p99_turn_latency_ms, 99);
    }

    #[test]
    fn stats_p50_p99_single_record() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("only", 42, 0));
        let stats = analyzer.stats();
        assert_eq!(stats.p50_turn_latency_ms, 42);
        assert_eq!(stats.p99_turn_latency_ms, 42);
    }

    #[test]
    fn stats_computes_compact_trigger_rate() {
        let mut analyzer = TraceAnalyzer::new();
        for i in 1..=10 {
            let triggered = i % 3 == 0; // i = 3, 6, 9 → 3 次
            analyzer.add_record(TraceRecord::new(format!("t{i}"), i * 10, 0).with_compact_triggered(triggered));
        }
        let stats = analyzer.stats();
        assert_eq!(stats.compact_trigger_count, 3);
        // 3 / 10 = 0.3
        assert!((stats.compact_trigger_rate - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn cluster_failures_groups_by_failure_kind() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 0).with_failure("timeout", "req timed out"));
        analyzer.add_record(TraceRecord::new("t2", 200, 0).with_failure("timeout", "another timeout"));
        analyzer.add_record(TraceRecord::new("t3", 300, 0).with_failure("auth", "401 unauthorized"));
        analyzer.add_record(TraceRecord::new("t4", 400, 0)); // no failure
        analyzer.add_record(TraceRecord::new("t5", 500, 0).with_failure("timeout", "third timeout"));

        let clusters = analyzer.cluster_failures();
        assert_eq!(clusters.len(), 2, "expected 2 failure kinds, got {clusters:?}");
        // timeout (count=3) should be first due to count desc.
        assert_eq!(clusters[0].label, "timeout");
        assert_eq!(clusters[0].count, 3);
        assert_eq!(clusters[0].sample_errors.len(), 3);
        assert_eq!(clusters[1].label, "auth");
        assert_eq!(clusters[1].count, 1);
    }

    #[test]
    fn cluster_failures_caps_sample_errors() {
        let mut analyzer = TraceAnalyzer::new();
        for i in 0..(MAX_SAMPLE_ERRORS_PER_CLUSTER + 3) {
            analyzer.add_record(
                TraceRecord::new(format!("t{i}"), 100, 0)
                    .with_failure("timeout", format!("error #{i}")),
            );
        }
        let clusters = analyzer.cluster_failures();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, (MAX_SAMPLE_ERRORS_PER_CLUSTER + 3) as u32);
        assert_eq!(
            clusters[0].sample_errors.len(),
            MAX_SAMPLE_ERRORS_PER_CLUSTER,
            "sample errors should be capped"
        );
    }

    #[test]
    fn export_csv_writes_header_and_rows() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 3));
        analyzer.add_record(
            TraceRecord::new("t2", 250, 5)
                .with_compact_triggered(true)
                .with_failure("timeout", "req timed out, retry exhausted"),
        );

        let path = fixture_path("export-header");
        analyzer.export_csv(&path).expect("export should succeed");

        let content = std::fs::read_to_string(&path).expect("file should be readable");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "expected header + 2 rows, got: {content}");
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(lines[1], "t1,100,3,false,\"\",\"\"");
        // error_message contains comma → quoted
        assert_eq!(lines[2], "t2,250,5,true,timeout,\"req timed out, retry exhausted\"");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_csv_round_trips_with_export_csv() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("turn-a", 100, 1));
        analyzer.add_record(
            TraceRecord::new("turn-b", 250, 5)
                .with_compact_triggered(true)
                .with_failure("auth", "missing token"),
        );
        analyzer.add_record(
            TraceRecord::new("turn-c", 500, 0)
                .with_failure("timeout", "req timed out, \"connection reset\""),
        );

        let path = fixture_path("roundtrip");
        analyzer.export_csv(&path).expect("export should succeed");
        let loaded = TraceAnalyzer::load_csv(&path).expect("load should succeed");

        assert_eq!(loaded.records.len(), analyzer.records.len());
        for (original, loaded) in analyzer.records.iter().zip(loaded.records.iter()) {
            assert_eq!(original, loaded, "round-trip mismatch");
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_csv_handles_quoted_newlines_in_error_message() {
        let path = fixture_path("quoted-newlines");
        // Build CSV with an actual newline inside a quoted error_message field.
        let mut csv_content = String::from(CSV_HEADER);
        csv_content.push('\n');
        csv_content.push_str("t1,100,3,false,\"\",\"line1");
        csv_content.push('\n'); // newline inside quoted field
        csv_content.push_str("line2\"\n");
        std::fs::write(&path, csv_content).expect("write should succeed");

        let loaded = TraceAnalyzer::load_csv(&path).expect("load should succeed");
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].error_message.as_deref(), Some("line1\nline2"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_analyzer_returns_zero_stats() {
        let analyzer = TraceAnalyzer::new();
        let stats = analyzer.stats();
        assert_eq!(stats.total_turns, 0);
        assert_eq!(stats.total_tool_calls, 0);
        assert!(stats.avg_turn_latency_ms.is_finite() && stats.avg_turn_latency_ms == 0.0);
        assert_eq!(stats.p50_turn_latency_ms, 0);
        assert_eq!(stats.p99_turn_latency_ms, 0);
        assert_eq!(stats.compact_trigger_count, 0);
        assert!(stats.compact_trigger_rate.is_finite() && stats.compact_trigger_rate == 0.0);
        assert!(stats.failure_counts.is_empty());
        assert!(stats.failure_clusters.is_empty());
    }

    #[test]
    fn empty_analyzer_export_csv_writes_only_header() {
        let analyzer = TraceAnalyzer::new();
        let path = fixture_path("empty-export");
        analyzer.export_csv(&path).expect("export should succeed");

        let content = std::fs::read_to_string(&path).expect("file should be readable");
        assert_eq!(content.trim_end(), CSV_HEADER);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_csv_skips_trailing_empty_lines() {
        let path = fixture_path("trailing-empty");
        let csv_content = format!("{CSV_HEADER}\nt1,100,3,false,\"\",\"\"\n\n\n");
        std::fs::write(&path, csv_content).expect("write should succeed");

        let loaded = TraceAnalyzer::load_csv(&path).expect("load should succeed");
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].turn_id, "t1");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_csv_without_header_treats_first_row_as_data() {
        let path = fixture_path("no-header");
        // No header line — first row is data.
        std::fs::write(&path, "t1,100,3,false,\"\",\"\"\n").expect("write should succeed");

        let loaded = TraceAnalyzer::load_csv(&path).expect("load should succeed");
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].turn_id, "t1");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn percentile_function_handles_edge_cases() {
        // Empty → 0
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[], 99), 0);
        // Single element
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 99), 42);
        // p=0 (rank = 0, idx saturates to 0)
        assert_eq!(percentile(&[10, 20, 30], 0), 10);
    }

    #[test]
    fn export_csv_creates_parent_directories() {
        let mut path = std::env::temp_dir();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!("trace-analyzer-nested-{ts}"));
        path.push("subdir");
        path.push("traces.csv");

        let analyzer = TraceAnalyzer::new();
        analyzer
            .export_csv(&path)
            .expect("export should create parent dirs");
        assert!(path.exists(), "file should exist after export");

        let _ = std::fs::remove_file(&path);
        // Best-effort cleanup of created parent directories.
        if let Some(grandparent) = path.parent().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir_all(grandparent);
        }
    }
}
