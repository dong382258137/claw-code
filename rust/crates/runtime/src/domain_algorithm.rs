//! DomainTools — 算法级重构建议与基准对比工具(无状态)。
//!
//! # Design (v4.2)
//!
//! - [`refactor_algorithm_topo`] — **建议模式**,不直接修改文件。基于
//!   [`ProjectTopology`](crate::project_topology::ProjectTopology) 的 SymbolIndex
//!   (definitions + callers) 生成"建议修改列表"(file + line + old/new text),
//!   并附调用点覆盖率报告。LLM 拿到建议后用 `edit_file` 逐个应用。
//!
//!   原因:SymbolIndex 是 best-effort,LSP 可能遗漏宏展开/条件编译调用点,
//!   直接批量修改存在"部分成功、部分遗漏"风险。
//!
//! - [`benchmark_compare`] — 运行命令多次,报告计时统计
//!   (avg/median/min/max/stddev),支持 warmup runs、可配置 sample_size、
//!   per-sample exit code 跟踪,`timeout_seconds` 控制单次运行上限。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::project_topology::{ProjectTopology, TopologyState};

// ---------------------------------------------------------------------------
// refactor_algorithm_topo
// ---------------------------------------------------------------------------

/// 单条建议修改(建议模式:仅建议,不执行)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedEdit {
    pub file: PathBuf,
    pub line: u32,
    /// 修改类型:`definition` 或 `call_site`。
    pub edit_kind: String,
    /// 原始文本行(来自符号定义上下文或调用点上下文)。
    pub old_text: String,
    /// 建议替换后的文本行(`target_symbol` → `new_name`)。
    pub new_text: String,
}

/// 重构建议结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefactorSuggestion {
    pub target_symbol: String,
    pub new_name: Option<String>,
    pub reason: Option<String>,
    pub suggested_edits: Vec<SuggestedEdit>,
    /// 检索到的调用点总数。
    pub call_site_count: usize,
    /// 生成建议覆盖的调用点数。
    pub covered_call_sites: usize,
    /// best-effort 警告:符号索引可能遗漏宏展开/条件编译调用点。
    pub coverage_note: String,
    /// 拓扑状态(uninitialized/building/ready/failed)。
    pub topology_state: String,
}

/// 生成符号重命名建议(建议模式,不修改任何文件)。
///
/// - 若 `new_name` 为 `None`:退化为"查找所有引用"报告(列出定义 + 调用点)。
/// - 若 ProjectTopology 未 Ready 或无 SymbolIndex,返回携带状态的提示。
pub fn refactor_algorithm_topo(
    topology: &ProjectTopology,
    target_symbol: &str,
    new_name: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    let state = topology.ensure_built();
    let topo_state_str = state.to_string();

    let data = match state {
        TopologyState::Ready(data) => data,
        TopologyState::Failed(e) => {
            return Ok(format!(
                "refactor_algorithm_topo: topology build failed ({e}). \
                 Use grep_search/read_file to find references manually."
            ));
        }
        TopologyState::Building { .. } => {
            return Ok("refactor_algorithm_topo: topology is still building. \
                 Do NOT retry immediately — use grep_search/read_file instead."
                .to_string());
        }
        TopologyState::Uninitialized => {
            return Ok("refactor_algorithm_topo: topology not initialized. \
                 Use grep_search/read_file to find references."
                .to_string());
        }
    };

    let Some(si) = &data.symbol_index else {
        return Ok(format!(
            "refactor_algorithm_topo: symbol index unavailable (LSP not attached). \
             Topology state: {topo_state_str}. \
             Use grep_search to find references to `{target_symbol}` manually."
        ));
    };

    let defs = si
        .definitions
        .get(target_symbol)
        .cloned()
        .unwrap_or_default();
    let callers = si.callers.get(target_symbol).cloned().unwrap_or_default();
    let call_site_count = callers.len();

    let mut suggested_edits: Vec<SuggestedEdit> = Vec::new();

    // 定义点建议
    for def in &defs {
        let old_text = format!(
            "{} {} ({}:{})",
            def.kind,
            def.name,
            def.file.display(),
            def.line
        );
        let new_text = match new_name {
            Some(nn) => format!("{} {} ({}:{})", def.kind, nn, def.file.display(), def.line),
            None => old_text.clone(),
        };
        suggested_edits.push(SuggestedEdit {
            file: def.file.clone(),
            line: def.line,
            edit_kind: "definition".to_string(),
            old_text,
            new_text,
        });
    }

    // 调用点建议:对上下文行做 target_symbol → new_name 替换
    let mut covered = 0usize;
    for cs in &callers {
        let old_text = cs
            .context
            .clone()
            .unwrap_or_else(|| format!("{}:{}", cs.file.display(), cs.line));
        let new_text = match new_name {
            Some(nn) => replace_symbol_occurrences(&old_text, target_symbol, nn),
            None => old_text.clone(),
        };
        if new_text != old_text || new_name.is_none() {
            covered += 1;
        }
        suggested_edits.push(SuggestedEdit {
            file: cs.file.clone(),
            line: cs.line,
            edit_kind: "call_site".to_string(),
            old_text,
            new_text,
        });
    }

    let covered_call_sites = covered;
    let coverage_note = if new_name.is_some() && covered_call_sites < call_site_count {
        format!(
            "WARNING: only {covered_call_sites}/{call_site_count} call sites could be \
             auto-rewritten (context unavailable). SymbolIndex is best-effort and may miss \
             macro-expanded / cfg-gated call sites — verify with grep_search before applying."
        )
    } else {
        "SymbolIndex is best-effort; macro-expanded / cfg-gated call sites may be missing. \
         Verify with grep_search before applying edits."
            .to_string()
    };

    let suggestion = RefactorSuggestion {
        target_symbol: target_symbol.to_string(),
        new_name: new_name.map(str::to_string),
        reason: reason.map(str::to_string),
        suggested_edits,
        call_site_count,
        covered_call_sites,
        coverage_note,
        topology_state: topo_state_str,
    };

    serde_json::to_string_pretty(&suggestion).map_err(|e| format!("serialization error: {e}"))
}

/// 在文本行中将 `target` 作为整词替换为 `new_name`。
///
/// 使用简单的字符边界判断(非字母数字/下划线视为边界),避免子串误匹配。
fn replace_symbol_occurrences(text: &str, target: &str, new_name: &str) -> String {
    if target.is_empty() {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let target_bytes = target.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if i + target_bytes.len() <= bytes.len()
            && &bytes[i..i + target_bytes.len()] == target_bytes
        {
            let before = if i > 0 { bytes[i - 1] } else { b' ' };
            let after = if i + target_bytes.len() < bytes.len() {
                bytes[i + target_bytes.len()]
            } else {
                b' '
            };
            if !is_ident_byte(before) && !is_ident_byte(after) {
                out.push_str(new_name);
                i += target_bytes.len();
                continue;
            }
        }
        // 安全追加一个 UTF-8 字符(避免从字节中间截断)
        let ch_len = utf8_char_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
            out.push_str(s);
        }
        i = end;
    }
    out
}

#[inline]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 返回 UTF-8 首字节对应的字符长度。
#[inline]
fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1 // 无效首字节,退化为 1
    }
}

// ---------------------------------------------------------------------------
// benchmark_compare
// ---------------------------------------------------------------------------

/// 单次运行采样。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub index: usize,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// 基准对比统计结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub command: String,
    pub sample_size: usize,
    pub warmup_runs: usize,
    pub timeout_seconds: u64,
    pub samples: Vec<Sample>,
    /// 仅统计未超时样本的计时统计。
    pub stats: Stats,
    pub failure_count: usize,
    pub timeout_count: usize,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub avg_ms: f64,
    pub median_ms: f64,
    pub min_ms: u64,
    pub max_ms: u64,
    pub stddev_ms: f64,
    pub valid_count: usize,
}

/// 运行命令多次并报告计时统计。
///
/// - `cwd`:工作目录(`workspace_root`),`None` 表示继承当前目录。
/// - `timeout_seconds`:单次运行超时上限(超时则 kill 并记为 timed_out)。
/// - `sample_size`:正式采样次数。
/// - `warmup_runs`:预热运行次数(结果丢弃,用于填充分支预测/文件系统缓存)。
pub fn benchmark_compare(
    command: &str,
    cwd: Option<&Path>,
    timeout_seconds: u64,
    sample_size: usize,
    warmup_runs: usize,
) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    if sample_size == 0 {
        return Err("sample_size must be >= 1".to_string());
    }
    let timeout = Duration::from_secs(timeout_seconds.max(1));

    // 预热运行(丢弃结果)
    for i in 0..warmup_runs {
        let _ = run_one(command, cwd, timeout, i);
    }

    let mut samples: Vec<Sample> = Vec::with_capacity(sample_size);
    let mut failure_count = 0usize;
    let mut timeout_count = 0usize;
    for i in 0..sample_size {
        let s = run_one(command, cwd, timeout, i)?;
        if s.timed_out {
            timeout_count += 1;
        } else if s.exit_code != Some(0) {
            failure_count += 1;
        }
        samples.push(s);
    }

    let valid_times: Vec<u64> = samples
        .iter()
        .filter(|s| !s.timed_out)
        .map(|s| s.elapsed_ms)
        .collect();

    let stats = compute_stats(&valid_times);
    let note = if timeout_count == sample_size {
        "All samples timed out — command may be too slow or hanging. \
         Increase timeout_seconds or reduce workload."
            .to_string()
    } else if failure_count > 0 {
        format!(
            "{failure_count} sample(s) exited non-zero; stats computed over \
             non-timed-out samples only."
        )
    } else {
        "All samples completed successfully.".to_string()
    };

    let result = BenchmarkResult {
        command: command.to_string(),
        sample_size,
        warmup_runs,
        timeout_seconds,
        samples,
        stats,
        failure_count,
        timeout_count,
        note,
    };

    serde_json::to_string_pretty(&result).map_err(|e| format!("serialization error: {e}"))
}

/// 运行单次命令(经 shell 包装以支持管道/参数),带超时 kill。
fn run_one(
    command: &str,
    cwd: Option<&Path>,
    timeout: Duration,
    index: usize,
) -> Result<Sample, String> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // 丢弃输出,避免缓冲区阻塞;只关心计时与退出码。
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn command `{command}`: {e}"))?;

    let start = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("wait failed for command `{command}`: {e}"))?
        {
            Some(status) => {
                return Ok(Sample {
                    index,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    exit_code: status.code(),
                    timed_out: false,
                });
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait(); // 回收僵尸进程
                    return Ok(Sample {
                        index,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        exit_code: None,
                        timed_out: true,
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn compute_stats(times: &[u64]) -> Stats {
    if times.is_empty() {
        return Stats {
            avg_ms: 0.0,
            median_ms: 0.0,
            min_ms: 0,
            max_ms: 0,
            stddev_ms: 0.0,
            valid_count: 0,
        };
    }
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    let sum: u64 = sorted.iter().sum();
    let count = sorted.len();
    let avg = sum as f64 / count as f64;
    let median = if count % 2 == 1 {
        sorted[count / 2] as f64
    } else {
        (sorted[count / 2 - 1] as f64 + sorted[count / 2] as f64) / 2.0
    };
    let variance: f64 = sorted
        .iter()
        .map(|t| (*t as f64 - avg).powi(2))
        .sum::<f64>()
        / count as f64;
    Stats {
        avg_ms: avg,
        median_ms: median,
        min_ms: *sorted.first().unwrap(),
        max_ms: *sorted.last().unwrap(),
        stddev_ms: variance.sqrt(),
        valid_count: count,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_symbol_occurrences_replaces_whole_word_only() {
        assert_eq!(
            replace_symbol_occurrences("foo(foo_bar, foobar, foo)", "foo", "renamed"),
            "renamed(foo_bar, foobar, renamed)"
        );
    }

    #[test]
    fn replace_symbol_occurrences_handles_unicode() {
        // CJK 上下文:符号前后非 ASCII 标识符字节,应替换
        assert_eq!(
            replace_symbol_occurrences("调用 foo 结束", "foo", "bar"),
            "调用 bar 结束"
        );
    }

    #[test]
    fn replace_symbol_occurrences_empty_target_noop() {
        assert_eq!(replace_symbol_occurrences("abc", "", "x"), "abc");
    }

    #[test]
    fn replace_symbol_occurrences_multiple_occurrences() {
        assert_eq!(
            replace_symbol_occurrences("a + a + baz", "a", "alpha"),
            "alpha + alpha + baz"
        );
    }

    #[test]
    fn refactor_topo_handles_uninitialized_topology() {
        let topo = ProjectTopology::new(PathBuf::from("/nonexistent/project/xyz"));
        // P2-1: ensure_built() 现在是异步的,会立即返回 Building 并派发后台线程。
        // 在非 cargo 目录下,后台线程会很快失败并转为 Failed。
        // 我们先调用 ensure_built_blocking() 确保到达终态(Failed),再测试 refactor_algorithm_topo。
        let _ = topo.ensure_built_blocking();
        let out = refactor_algorithm_topo(&topo, "some_symbol", Some("new_name"), None)
            .expect("should not error");
        assert!(
            out.contains("failed") || out.contains("unavailable") || out.contains("manually"),
            "expected graceful failure message, got: {out}"
        );
    }

    #[test]
    fn refactor_topo_handles_building_state() {
        // P2-1:测试异步化后的 Building 状态——refactor_algorithm_topo 应返回
        // "do NOT retry" 提示而非阻塞或报错。
        let topo = ProjectTopology::new(PathBuf::from("/nonexistent/project/xyz"));
        let _ = topo.ensure_built(); // 派发后台线程,立即返回 Building
                                     // 此时状态可能为 Building(后台线程尚未完成)
        let state = topo.state();
        if matches!(
            state,
            crate::project_topology::TopologyState::Building { .. }
        ) {
            let out = refactor_algorithm_topo(&topo, "some_symbol", Some("new_name"), None)
                .expect("should not error");
            assert!(
                out.contains("building") || out.contains("Do NOT retry"),
                "expected building/Do NOT retry message, got: {out}"
            );
        }
        // 如果后台线程已完成(转为 Failed),则由上一个测试覆盖
    }

    #[test]
    fn compute_stats_empty_returns_zeros() {
        let s = compute_stats(&[]);
        assert_eq!(s.valid_count, 0);
        assert_eq!(s.avg_ms, 0.0);
    }

    #[test]
    fn compute_stats_basic() {
        let s = compute_stats(&[10, 20, 30]);
        assert_eq!(s.min_ms, 10);
        assert_eq!(s.max_ms, 30);
        assert_eq!(s.avg_ms, 20.0);
        assert_eq!(s.median_ms, 20.0);
        assert_eq!(s.valid_count, 3);
        assert!(s.stddev_ms > 0.0);
    }

    #[test]
    fn compute_stats_even_count_median() {
        let s = compute_stats(&[10, 20, 30, 40]);
        assert_eq!(s.median_ms, 25.0);
    }

    #[test]
    fn benchmark_compare_rejects_empty_command() {
        let err = benchmark_compare("", None, 5, 1, 0).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn benchmark_compare_rejects_zero_sample_size() {
        let err = benchmark_compare("echo hi", None, 5, 0, 0).unwrap_err();
        assert!(err.contains("sample_size"));
    }

    #[test]
    fn benchmark_compare_runs_fast_command() {
        // 快速命令,1 次采样,无预热
        // echo 在 Windows(cmd) 和 Unix(shell)下都可用,无需平台分支
        let out = benchmark_compare("echo hello", None, 10, 2, 0).expect("should succeed");
        assert!(out.contains("\"sample_size\": 2"));
        assert!(out.contains("\"valid_count\": 2"));
        assert!(out.contains("\"timeout_count\": 0"));
    }

    #[test]
    fn benchmark_compare_detects_timeout() {
        // 跨平台 sleep:Windows 用 ping 超时,Unix 用 sleep
        let cmd = if cfg!(windows) {
            "ping -n 10 127.0.0.1 >NUL"
        } else {
            "sleep 5"
        };
        let out = benchmark_compare(cmd, None, 1, 1, 0).expect("should succeed");
        assert!(
            out.contains("\"timeout_count\": 1"),
            "expected timeout, got: {out}"
        );
    }

    #[test]
    fn suggested_edit_serialization_roundtrip() {
        let edit = SuggestedEdit {
            file: PathBuf::from("src/lib.rs"),
            line: 42,
            edit_kind: "definition".to_string(),
            old_text: "fn old_name()".to_string(),
            new_text: "fn new_name()".to_string(),
        };
        let json = serde_json::to_string(&edit).expect("serialize");
        let back: SuggestedEdit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.line, 42);
        assert_eq!(back.edit_kind, "definition");
    }

    #[test]
    fn refactor_suggestion_serialization_roundtrip() {
        let s = RefactorSuggestion {
            target_symbol: "foo".to_string(),
            new_name: Some("bar".to_string()),
            reason: None,
            suggested_edits: vec![],
            call_site_count: 0,
            covered_call_sites: 0,
            coverage_note: "test".to_string(),
            topology_state: "ready".to_string(),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(json.contains("\"target_symbol\":\"foo\""));
    }
}
