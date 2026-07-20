//! Trace Analyzer — Step 3.3 Telemetry 导出与 Trace Analyzer 基础。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 3.3
//!
//! 架构:
//! - [`TraceRecord`]:简化的本地 trace 记录类型(一行 CSV 对应一条记录)。
//! - [`TraceAnalyzer`]:加载/导出 CSV,计算基础统计,失败聚类。
//! - [`TraceStats`]:turn latency / tool call count / compact 触发率等指标直方图。
//! - [`FailureCluster`]:失败聚类结果。
//! - 双模式聚类:
//!   - [`TraceAnalyzer::cluster_failures`]:按 `failure_kind` 简单分桶(向后兼容)。
//!   - [`TraceAnalyzer::cluster_failures_kmeans`]:K-means on
//!     `(failure_kind, error_message_embedding)` — Step 3.3 真正实现。
//!     需注入 [`EmbeddingProvider`](crate::memory_semantic::EmbeddingProvider);
//!     `None` 时退化为按 `failure_kind` 分桶。
//!
//! **不在本步骤实现**(留到阶段 4 Self-Improving Harness):
//! - OTLP exporter(默认 CSV exporter 已足够支撑阶段 4 入口)
//! - 闭环反馈到 `RecoveryOrchestrator`
//!
//! **缓存影响**:无 — 纯观测层,不进入 prompt 稳定区/变动区。

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::memory_semantic::{cosine_similarity, EmbeddingProvider};

/// CSV 表头,固定顺序与 [`TraceRecord`] 字段一一对应。
pub const CSV_HEADER: &str = "turn_id,latency_ms,tool_calls,compact_triggered,failure_kind,error_message";

/// 每个聚类最多保留的样本错误消息条数,避免聚类膨胀。
pub const MAX_SAMPLE_ERRORS_PER_CLUSTER: usize = 5;

/// K-means 聚类时,每个 `failure_kind` 分组内的最大 cluster 数。
///
/// Step 3.3:同一 `failure_kind` 下用 K-means 二次切分,避免一个 kind 内
/// 几十条样本全堆在一个 cluster。3 是经验值(网络/权限/超时等典型 kind
/// 内部通常有 2-3 个语义子簇)。
pub const MAX_KMEANS_CLUSTERS_PER_KIND: usize = 3;

/// K-means 最大迭代次数。10 轮对 384 维 BGE-small 足够收敛。
pub const KMEANS_MAX_ITERATIONS: usize = 10;

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

/// 失败聚类 — 双模式聚类的统一输出类型。
///
/// - **简单模式** [`TraceAnalyzer::cluster_failures`]:
///   按 `failure_kind` 简单分桶,`label = failure_kind`。
/// - **K-means 模式** [`TraceAnalyzer::cluster_failures_kmeans`] (Step 3.3):
///   按 `(failure_kind, error_message_embedding)` 二次切分,
///   `label = "{failure_kind}-{cluster_idx}"`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureCluster {
    /// 聚类标签。
    /// - 简单模式:等于 `failure_kind`。
    /// - K-means 模式:`"{failure_kind}-{cluster_idx}"`。
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

    /// 简单失败聚类 — 按 `failure_kind` 分桶(向后兼容)。
    ///
    /// 返回顺序:count 降序,label 升序(确保稳定输出便于断言)。
    /// K-means 语义聚类见 [`cluster_failures_kmeans`](Self::cluster_failures_kmeans)。
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

    /// K-means 失败聚类 — Step 3.3 真正实现。
    ///
    /// 算法:
    /// 1. `provider = None` → 退化为 [`cluster_failures`](Self::cluster_failures)
    ///    (按 `failure_kind` 简单分桶),保证无 embedding 环境下向后兼容。
    /// 2. `provider = Some` → 按 `failure_kind` 分组,组内对 `error_message` 的
    ///    embedding 跑 K-means,`K = min(MAX_KMEANS_CLUSTERS_PER_KIND, 组内样本数)`。
    ///    - 单样本组直接成 1 个 cluster(`label = "{kind}-0"`)。
    ///    - K-means 收敛条件:assignment 不再变化 或 `KMEANS_MAX_ITERATIONS` 轮。
    ///    - label 格式:`"{failure_kind}-{cluster_idx}"`。
    ///
    /// **确定性**:初始 centroid 选组内前 K 个点(按 turn_id 排序),保证可测试。
    ///
    /// **降级**:若 `embed_batch` 失败(如 provider 故障),该 kind 退化为单 cluster。
    ///
    /// 返回顺序:count 降序,label 升序(与 [`cluster_failures`](Self::cluster_failures) 一致)。
    #[must_use]
    pub fn cluster_failures_kmeans(
        &self,
        provider: Option<&dyn EmbeddingProvider>,
    ) -> Vec<FailureCluster> {
        // 1. 无 provider → 退化为简单分桶。
        let Some(provider) = provider else {
            return self.cluster_failures();
        };

        // 2. 按 failure_kind 分组,组内收集 (turn_id, error_message) 用于排序与展示。
        let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for record in &self.records {
            let Some(kind) = &record.failure_kind else { continue };
            let msg = record.error_message.clone().unwrap_or_default();
            groups
                .entry(kind.clone())
                .or_default()
                .push((record.turn_id.clone(), msg));
        }

        let mut clusters: Vec<FailureCluster> = Vec::new();
        for (kind, mut samples) in groups {
            // 组内按 turn_id 排序,保证初始 centroid 选择稳定。
            samples.sort_by(|a, b| a.0.cmp(&b.0));

            let n = samples.len();
            let k = MAX_KMEANS_CLUSTERS_PER_KIND.min(n);

            if k <= 1 {
                // 单样本或 K=1:直接成单 cluster。
                clusters.push(FailureCluster {
                    label: format!("{kind}-0"),
                    count: n as u32,
                    sample_errors: take_sample_errors(&samples),
                });
                continue;
            }

            // 3. 计算 embeddings。失败时退化为单 cluster。
            let texts: Vec<&str> = samples.iter().map(|(_, m)| m.as_str()).collect();
            let embeddings = match provider.embed_batch(&texts) {
                Ok(emb) => emb,
                Err(_) => {
                    clusters.push(FailureCluster {
                        label: format!("{kind}-0"),
                        count: n as u32,
                        sample_errors: take_sample_errors(&samples),
                    });
                    continue;
                }
            };

            // 4. K-means 聚类。
            let assignments = kmeans_cluster(&embeddings, k, KMEANS_MAX_ITERATIONS);

            // 5. 按 cluster_idx 分桶,生成 FailureCluster。
            let mut bucket: HashMap<usize, (u32, Vec<String>)> = HashMap::new();
            for (idx, assignment) in assignments.iter().enumerate() {
                let entry = bucket.entry(*assignment).or_default();
                entry.0 += 1;
                let msg = &samples[idx].1;
                if !msg.is_empty() && entry.1.len() < MAX_SAMPLE_ERRORS_PER_CLUSTER {
                    entry.1.push(msg.clone());
                }
            }

            let mut cluster_ids: Vec<usize> = bucket.keys().copied().collect();
            cluster_ids.sort_unstable();
            for cluster_idx in cluster_ids {
                let (count, mut errors) = bucket.remove(&cluster_idx).unwrap_or_default();
                errors.sort();
                clusters.push(FailureCluster {
                    label: format!("{kind}-{cluster_idx}"),
                    count,
                    sample_errors: errors,
                });
            }
        }

        // 6. 排序:count 降序,label 升序。
        clusters.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
        clusters
    }
}

/// 从组内样本中提取最多 [`MAX_SAMPLE_ERRORS_PER_CLUSTER`] 条非空错误消息(已排序)。
fn take_sample_errors(samples: &[(String, String)]) -> Vec<String> {
    let mut errors: Vec<String> = samples
        .iter()
        .map(|(_, m)| m.clone())
        .filter(|m| !m.is_empty())
        .take(MAX_SAMPLE_ERRORS_PER_CLUSTER)
        .collect();
    errors.sort();
    errors
}

/// K-means 聚类 — 基于 cosine similarity 的简化实现。
///
/// - **初始化**:前 K 个点作为初始 centroid(确定性,便于测试)。
/// - **Assign**:每个点分配到 cosine similarity 最大的 centroid。
/// - **Update**:centroid = cluster 内点的均值(对 cosine similarity 排序保持等价,
///   因为 cosine similarity 对正缩放不变)。
/// - **收敛**:assignment 不变 或 达到 `max_iterations`。
/// - **空 cluster**:保留旧 centroid(不重新初始化,保证确定性)。
///
/// 返回每个点的 cluster assignment index(0..k)。
fn kmeans_cluster(points: &[Vec<f32>], k: usize, max_iterations: usize) -> Vec<usize> {
    let n = points.len();
    if n == 0 {
        return Vec::new();
    }
    if k == 0 {
        return vec![0; n];
    }
    // k >= n:每个点自成一类。
    if k >= n {
        return (0..n).collect();
    }

    let dim = points[0].len();
    // 初始 centroid:前 K 个点。
    let mut centroids: Vec<Vec<f32>> = points[..k].to_vec();
    let mut assignments = vec![0usize; n];

    for _ in 0..max_iterations {
        let mut changed = false;

        // Assign step:每个点找 cosine similarity 最大的 centroid。
        for (i, point) in points.iter().enumerate() {
            let mut best = 0usize;
            let mut best_sim = f32::NEG_INFINITY;
            for (j, centroid) in centroids.iter().enumerate() {
                let sim = cosine_similarity(point, centroid);
                if sim > best_sim {
                    best_sim = sim;
                    best = j;
                }
            }
            if assignments[i] != best {
                assignments[i] = best;
                changed = true;
            }
        }

        if !changed {
            break;
        }

        // Update step:centroid = cluster 内点的均值。
        let mut new_centroids: Vec<Vec<f32>> = vec![vec![0.0f32; dim]; k];
        let mut counts: Vec<usize> = vec![0; k];
        for (i, point) in points.iter().enumerate() {
            let c = assignments[i];
            counts[c] += 1;
            for (j, v) in point.iter().enumerate() {
                new_centroids[c][j] += v;
            }
        }
        for (i, c) in new_centroids.iter_mut().enumerate() {
            if counts[i] > 0 {
                let cnt = counts[i] as f32;
                for v in c.iter_mut() {
                    *v /= cnt;
                }
                centroids[i] = std::mem::take(c);
            }
            // counts[i] == 0:保留旧 centroid(centroids[i] 不变)。
        }
    }

    assignments
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
    use crate::memory_semantic::{EmbeddingError, HashEmbeddingProvider};
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

    // ========================================================================
    // Step 3.3 K-means 失败聚类测试
    // ========================================================================

    /// Provider=None 时,cluster_failures_kmeans 退化为 cluster_failures。
    #[test]
    fn kmeans_falls_back_to_simple_bucketing_when_no_provider() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 0).with_failure("timeout", "req timed out"));
        analyzer.add_record(TraceRecord::new("t2", 200, 0).with_failure("timeout", "another timeout"));
        analyzer.add_record(TraceRecord::new("t3", 300, 0).with_failure("auth", "401 unauthorized"));

        let simple = analyzer.cluster_failures();
        let kmeans_none = analyzer.cluster_failures_kmeans(None);

        assert_eq!(
            simple, kmeans_none,
            "provider=None should produce identical output to cluster_failures()"
        );
    }

    /// 单样本 kind 直接成单 cluster,label = "{kind}-0"。
    #[test]
    fn kmeans_single_sample_kind_produces_single_cluster() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 0).with_failure("timeout", "req timed out"));

        let provider = HashEmbeddingProvider::new(32);
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].label, "timeout-0");
        assert_eq!(clusters[0].count, 1);
        assert_eq!(clusters[0].sample_errors, vec!["req timed out".to_string()]);
    }

    /// 多 kind 场景:每个 kind 至少产生 1 个 cluster,label 格式正确。
    #[test]
    fn kmeans_multiple_kinds_produce_labelled_clusters() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 0).with_failure("timeout", "connection reset"));
        analyzer.add_record(TraceRecord::new("t2", 200, 0).with_failure("timeout", "read timeout"));
        analyzer.add_record(TraceRecord::new("t3", 300, 0).with_failure("auth", "401 unauthorized"));
        analyzer.add_record(TraceRecord::new("t4", 400, 0).with_failure("auth", "token expired"));

        let provider = HashEmbeddingProvider::new(32);
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));

        // 至少 2 个 cluster(每个 kind 至少 1 个),且所有 label 都以 kind 开头。
        assert!(clusters.len() >= 2, "expected >=2 clusters, got {clusters:?}");
        for c in &clusters {
            assert!(
                c.label.starts_with("timeout-") || c.label.starts_with("auth-"),
                "unexpected label: {}",
                c.label
            );
        }
        // 总样本数守恒:2 timeout + 2 auth = 4。
        let total: u32 = clusters.iter().map(|c| c.count).sum();
        assert_eq!(total, 4, "sample count should be conserved");
    }

    /// K 被 `MAX_KMEANS_CLUSTERS_PER_KIND` 上限封顶。
    /// 当组内样本数远超 K_max 时,cluster 数应 <= K_max。
    #[test]
    fn kmeans_caps_k_to_max_clusters_per_kind() {
        let mut analyzer = TraceAnalyzer::new();
        // 同一 kind 下放 20 条不同错误消息,远超 MAX_KMEANS_CLUSTERS_PER_KIND。
        for i in 0..20 {
            analyzer.add_record(
                TraceRecord::new(format!("t{i:02}"), 100, 0)
                    .with_failure("network", format!("error variant #{i}")),
            );
        }

        let provider = HashEmbeddingProvider::new(64);
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));

        // 该 kind 下 cluster 数应 <= MAX_KMEANS_CLUSTERS_PER_KIND。
        let network_clusters: Vec<&FailureCluster> =
            clusters.iter().filter(|c| c.label.starts_with("network-")).collect();
        assert!(
            !network_clusters.is_empty(),
            "expected at least 1 network-* cluster"
        );
        assert!(
            network_clusters.len() <= MAX_KMEANS_CLUSTERS_PER_KIND,
            "expected <= {} clusters, got {}",
            MAX_KMEANS_CLUSTERS_PER_KIND,
            network_clusters.len()
        );
        // 样本守恒:20 条全部归入 network-* clusters。
        let total: u32 = network_clusters.iter().map(|c| c.count).sum();
        assert_eq!(total, 20);
    }

    /// K-means 函数:空输入 → 空输出。
    #[test]
    fn kmeans_cluster_handles_empty_input() {
        let assignments = kmeans_cluster(&[], 3, KMEANS_MAX_ITERATIONS);
        assert!(assignments.is_empty());
    }

    /// K-means 函数:K=0 → 所有点归 cluster 0(避免除零)。
    #[test]
    fn kmeans_cluster_k_zero_returns_all_zero() {
        let points = vec![vec![1.0f32, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]];
        let assignments = kmeans_cluster(&points, 0, KMEANS_MAX_ITERATIONS);
        assert_eq!(assignments, vec![0, 0, 0]);
    }

    /// K-means 函数:K >= n → 每个点自成一类。
    #[test]
    fn kmeans_cluster_k_ge_n_returns_identity() {
        let points = vec![vec![1.0f32, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]];
        // K == n
        let assignments = kmeans_cluster(&points, 3, KMEANS_MAX_ITERATIONS);
        assert_eq!(assignments, vec![0, 1, 2]);
        // K > n
        let assignments = kmeans_cluster(&points, 5, KMEANS_MAX_ITERATIONS);
        assert_eq!(assignments, vec![0, 1, 2]);
    }

    /// K-means 函数:确定性 — 同样的输入两次调用得到同样的 assignment。
    #[test]
    fn kmeans_cluster_is_deterministic() {
        let points: Vec<Vec<f32>> = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
        ];
        let a1 = kmeans_cluster(&points, 3, KMEANS_MAX_ITERATIONS);
        let a2 = kmeans_cluster(&points, 3, KMEANS_MAX_ITERATIONS);
        assert_eq!(a1, a2, "K-means should be deterministic given fixed init");
        // 3 个清晰分离的 cluster:前 2 / 中 2 / 后 2 应分别归入同一 cluster。
        assert_eq!(a1[0], a1[1], "points 0,1 should be in same cluster");
        assert_eq!(a1[2], a1[3], "points 2,3 should be in same cluster");
        assert_eq!(a1[4], a1[5], "points 4,5 should be in same cluster");
        assert_ne!(a1[0], a1[2], "cluster 0,2 should differ");
        assert_ne!(a1[0], a1[4], "cluster 0,4 should differ");
        assert_ne!(a1[2], a1[4], "cluster 2,4 should differ");
    }

    /// K-means 函数:相同点应归入同一 cluster(余弦相似度 = 1)。
    ///
    /// 注:K 必须 < n,否则会触发 `k >= n` 早退路径(每个点自成一类)。
    #[test]
    fn kmeans_cluster_identical_points_collapse_to_one_cluster() {
        // 4 个相同点,K=3 → K < n,K-means 实际运行。
        let points: Vec<Vec<f32>> = vec![
            vec![1.0, 0.0, 0.0],
            vec![1.0, 0.0, 0.0],
            vec![1.0, 0.0, 0.0],
            vec![1.0, 0.0, 0.0],
        ];
        let assignments = kmeans_cluster(&points, 3, KMEANS_MAX_ITERATIONS);
        // 初始 centroid = 前 3 个点(都相同)。第一轮所有点 cos sim to all centroids = 1,
        // tie-break 选 cluster 0。Update 后 centroid 0 = 点本身,1/2 保持不变。
        // 第二轮 assignment 不变 → 收敛。所有点归 cluster 0。
        assert_eq!(assignments, vec![0, 0, 0, 0]);
    }

    /// K-means 函数:clearly separated 3-cluster 输入应得到 3 个 cluster,
    /// 且每个 cluster 内点数正确。
    ///
    /// 注:初始 centroid = 前 K 个点,因此输入顺序必须让前 K 个点跨越所有 K 个组,
    /// 否则 K-means 会因 init 不多样而陷入次优解。
    #[test]
    fn kmeans_cluster_separates_three_distinct_groups() {
        // 3 组,每组 4 个相同点,组间正交。**交错排列**让前 3 个点分属 3 个组。
        let mut points: Vec<Vec<f32>> = Vec::new();
        for _ in 0..4 {
            points.push(vec![1.0, 0.0, 0.0]);
            points.push(vec![0.0, 1.0, 0.0]);
            points.push(vec![0.0, 0.0, 1.0]);
        }

        let assignments = kmeans_cluster(&points, 3, KMEANS_MAX_ITERATIONS);

        // 统计每个 cluster 的点数。
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for a in &assignments {
            *counts.entry(*a).or_default() += 1;
        }
        let mut sorted_counts: Vec<usize> = counts.values().copied().collect();
        sorted_counts.sort_unstable();
        // 3 个 cluster,每个 4 个点。
        assert_eq!(sorted_counts, vec![4, 4, 4]);
    }

    /// cluster_failures_kmeans:provider.embed_batch 失败时,该 kind 退化为单 cluster。
    #[test]
    fn kmeans_degrades_to_single_cluster_when_embedding_fails() {
        /// 故意失败的 provider,用于测试降级路径。
        struct FailingProvider;
        impl EmbeddingProvider for FailingProvider {
            fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
                Err(EmbeddingError::Inference("simulated failure".to_string()))
            }
            fn dim(&self) -> usize {
                8
            }
            fn name(&self) -> &str {
                "failing"
            }
        }

        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 0).with_failure("timeout", "error a"));
        analyzer.add_record(TraceRecord::new("t2", 200, 0).with_failure("timeout", "error b"));
        analyzer.add_record(TraceRecord::new("t3", 300, 0).with_failure("timeout", "error c"));

        let provider = FailingProvider;
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));

        // embedding 失败 → 该 kind 退化为单 cluster。
        assert_eq!(clusters.len(), 1, "expected 1 cluster on embed failure");
        assert_eq!(clusters[0].label, "timeout-0");
        assert_eq!(clusters[0].count, 3);
    }

    /// cluster_failures_kmeans:无 failure_kind 的记录被忽略。
    #[test]
    fn kmeans_ignores_records_without_failure_kind() {
        let mut analyzer = TraceAnalyzer::new();
        analyzer.add_record(TraceRecord::new("t1", 100, 0).with_failure("timeout", "err"));
        analyzer.add_record(TraceRecord::new("t2", 200, 0)); // 无 failure
        analyzer.add_record(TraceRecord::new("t3", 300, 0)); // 无 failure

        let provider = HashEmbeddingProvider::new(16);
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].label, "timeout-0");
        assert_eq!(clusters[0].count, 1);
    }

    /// cluster_failures_kmeans:空 analyzer 返回空 vec。
    #[test]
    fn kmeans_empty_analyzer_returns_empty() {
        let analyzer = TraceAnalyzer::new();
        let provider = HashEmbeddingProvider::new(16);
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));
        assert!(clusters.is_empty());
    }

    /// cluster_failures_kmeans:sample_errors 被封顶到 MAX_SAMPLE_ERRORS_PER_CLUSTER。
    #[test]
    fn kmeans_caps_sample_errors_per_cluster() {
        let mut analyzer = TraceAnalyzer::new();
        // 同 kind、同 embedding → K-means 应归入 1 个 cluster(MAX_KMEANS=3,但
        // 所有点相同,实际收敛到 1 个 cluster)。
        for i in 0..(MAX_SAMPLE_ERRORS_PER_CLUSTER + 5) {
            analyzer.add_record(
                TraceRecord::new(format!("t{i:02}"), 100, 0)
                    .with_failure("timeout", "identical error"),
            );
        }

        let provider = HashEmbeddingProvider::new(32);
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));

        // 总样本数守恒。
        let total: u32 = clusters.iter().map(|c| c.count).sum();
        assert_eq!(total, (MAX_SAMPLE_ERRORS_PER_CLUSTER + 5) as u32);
        // 至少一个 cluster 的 sample_errors 被封顶(不一定所有 cluster 都触及上限)。
        let max_sample_len = clusters.iter().map(|c| c.sample_errors.len()).max().unwrap_or(0);
        assert!(
            max_sample_len <= MAX_SAMPLE_ERRORS_PER_CLUSTER,
            "sample_errors should be capped, got {max_sample_len}"
        );
    }

    /// cluster_failures_kmeans:K-means 输出顺序遵循 count 降序、label 升序。
    #[test]
    fn kmeans_output_sorted_by_count_desc_then_label_asc() {
        let mut analyzer = TraceAnalyzer::new();
        // auth kind:5 条
        for i in 0..5 {
            analyzer.add_record(
                TraceRecord::new(format!("a{i:02}"), 100, 0)
                    .with_failure("auth", format!("auth error {i}")),
            );
        }
        // timeout kind:2 条
        for i in 0..2 {
            analyzer.add_record(
                TraceRecord::new(format!("t{i:02}"), 100, 0)
                    .with_failure("timeout", format!("timeout error {i}")),
            );
        }

        let provider = HashEmbeddingProvider::new(32);
        let clusters = analyzer.cluster_failures_kmeans(Some(&provider));

        // 验证排序:count 降序,同 count 时 label 升序。
        for w in clusters.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            assert!(
                a.count > b.count || (a.count == b.count && a.label <= b.label),
                "sort violation: {:?} before {:?}",
                a,
                b
            );
        }
    }
}
