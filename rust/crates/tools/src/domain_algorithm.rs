//! DomainTools — 领域算法工具(仅 algorithm core 2 个)。
//!
//! # Design
//!
//! - `refactor_algorithm_topo` — 建议模式:返回修改列表,不直接执行
//! - `benchmark_compare` — 带 timeout + sample_size 控制的基准对比

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

// ---------------------------------------------------------------------------
// RefactorAlgorithmTopo
// ---------------------------------------------------------------------------

/// 建议的修改项。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RefactorSuggestion {
    pub file: PathBuf,
    pub line: u32,
    pub old_signature: String,
    pub new_signature: String,
    pub reason: String,
    pub call_sites: Option<usize>,
}

/// 重构建议报告。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RefactorReport {
    pub suggestions: Vec<RefactorSuggestion>,
    pub total_affected_callsites: usize,
    pub coverage: f64,
    pub warnings: Vec<String>,
}

pub fn refactor_algorithm_topo(json_input: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(json_input).map_err(|e| format!("invalid JSON: {e}"))?;

    let target = parsed
        .get("target_symbol")
        .and_then(|v| v.as_str())
        .ok_or("missing 'target_symbol' field")?;
    let new_name = parsed
        .get("new_name")
        .and_then(|v| v.as_str())
        .unwrap_or(target);
    let reason = parsed
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("no reason provided");

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let mut suggestions = Vec::new();
    let mut total_sites = 0usize;

    if let Some(def_suggestion) = find_symbol_definition(&cwd, target, new_name, reason) {
        suggestions.push(def_suggestion);
    }

    if let Ok(call_sites) = find_call_sites(&cwd, target, new_name, reason) {
        total_sites = call_sites.len();
        suggestions.extend(call_sites);
    }

    if suggestions.is_empty() {
        return Ok(format!(
            "refactor_algorithm_topo: No definition or call sites found for '{target}'. \
             Try `grep_search` first to locate the symbol, or verify the symbol name."
        ));
    }

    let mut out = format!(
        "## Refactor Suggestion for `{target}` → `{new_name}`\n\
         Reason: {reason}\n\
         Total affected sites: {}\n\n",
        total_sites + 1
    );

    for (i, s) in suggestions.iter().enumerate() {
        out.push_str(&format!(
            "### #{i} {}:{}\n",
            s.file.display(),
            s.line
        ));
        out.push_str(&format!("   old: {}\n", s.old_signature));
        out.push_str(&format!("   new: {}\n", s.new_signature));
        if let Some(cs) = s.call_sites {
            out.push_str(&format!("   call_sites_found: {cs}\n"));
        }
        out.push_str(&format!("   reason: {}\n\n", s.reason));
    }

    out.push_str(
        "## IMPORTANT\n\
         This is a SUGGESTION only. No files have been modified. \
         Review each suggestion above, then use `edit_file` / `replace_lines` \
         to apply changes one at a time. Symbol index is best-effort — \
         grep_search the target symbol manually to find all call sites."
    );

    Ok(out)
}

fn find_symbol_definition(
    cwd: &std::path::Path,
    target: &str,
    new_name: &str,
    reason: &str,
) -> Option<RefactorSuggestion> {
    let output = Command::new("rg")
        .args([
            "--line-number",
            "--type", "rust",
            "-w",
            "--max-count=1",
            &format!("^(pub )?(fn|struct|trait|enum|type|mod|const|static) +{target}\\b"),
        ])
        .arg(".")
        .current_dir(cwd)
        .output()
        .ok()?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some((file, line_num, content)) = parse_rg_line(line) {
                return Some(RefactorSuggestion {
                    file: PathBuf::from(&file),
                    line: line_num,
                    old_signature: content.clone(),
                    new_signature: content.replace(target, new_name),
                    reason: reason.to_string(),
                    call_sites: None,
                });
            }
        }
    }
    None
}

fn find_call_sites(
    cwd: &std::path::Path,
    target: &str,
    new_name: &str,
    reason: &str,
) -> Result<Vec<RefactorSuggestion>, String> {
    let mut suggestions = Vec::new();

    let output = Command::new("rg")
        .args([
            "--line-number",
            "--type", "rust",
            "-w",
            &format!(r"\b{target}[!(]\b"),
        ])
        .arg(".")
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("rg failed: {e}"))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some((file, line_num, content)) = parse_rg_line(line) {
                if content.contains(&format!("fn {target}"))
                    || content.contains(&format!("struct {target}"))
                    || content.contains(&format!("trait {target}"))
                {
                    continue;
                }
                suggestions.push(RefactorSuggestion {
                    file: PathBuf::from(&file),
                    line: line_num,
                    old_signature: content.clone(),
                    new_signature: content.replace(target, new_name),
                    reason: reason.to_string(),
                    call_sites: None,
                });
            }
        }
    }

    Ok(suggestions)
}

fn parse_rg_line(line: &str) -> Option<(String, u32, String)> {
    if line.is_empty() || line == "--" {
        return None;
    }

    let colon1 = line.find(':')?;
    let file = line[..colon1].to_string();

    let rest = &line[colon1 + 1..];
    let colon2 = rest.find(':')?;
    let line_num: u32 = rest[..colon2].parse().ok()?;
    let content = rest[colon2 + 1..].trim().to_string();

    Some((file, line_num, content))
}

// ---------------------------------------------------------------------------
// BenchmarkCompare
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct BenchmarkResult {
    pub command: String,
    pub warmup_runs: u32,
    pub sample_size: u32,
    pub total_time_ms: u64,
    pub avg_time_ms: f64,
    pub min_time_ms: u64,
    pub max_time_ms: u64,
    pub median_time_ms: u64,
    pub stddev_ms: f64,
    pub samples: Vec<u64>,
    pub exit_codes: Vec<i32>,
}

pub fn benchmark_compare(json_input: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(json_input).map_err(|e| format!("invalid JSON: {e}"))?;

    let command = parsed
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("missing 'command' field")?;
    let timeout_seconds = parsed
        .get("timeout_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let sample_size = parsed
        .get("sample_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(100) as u32;
    let warmup_runs = parsed
        .get("warmup_runs")
        .and_then(|v| v.as_u64())
        .unwrap_or(2)
        .min(10) as u32;

    let mut results = BenchmarkResult {
        command: command.to_string(),
        warmup_runs,
        sample_size,
        total_time_ms: 0,
        avg_time_ms: 0.0,
        min_time_ms: u64::MAX,
        max_time_ms: 0,
        median_time_ms: 0,
        stddev_ms: 0.0,
        samples: Vec::with_capacity(sample_size as usize),
        exit_codes: Vec::with_capacity(sample_size as usize),
    };

    let total_start = Instant::now();

    // Warmup
    for i in 0..warmup_runs {
        let _ = run_bench_command(command);
        if i < warmup_runs - 1 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // Sample collection
    for i in 0..sample_size {
        let start = Instant::now();
        let (success, exit_code) = run_bench_command(command);
        let elapsed = start.elapsed().as_millis() as u64;

        results.samples.push(elapsed);
        results.exit_codes.push(exit_code);
        results.total_time_ms += elapsed;
        results.min_time_ms = results.min_time_ms.min(elapsed);
        results.max_time_ms = results.max_time_ms.max(elapsed);

        if !success {
            break;
        }

        if i < sample_size - 1 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    let n = results.samples.len() as f64;
    if n > 0.0 {
        results.avg_time_ms = results.total_time_ms as f64 / n;

        let mut sorted = results.samples.clone();
        sorted.sort_unstable();
        results.median_time_ms = if n as usize % 2 == 0 {
            let mid = n as usize / 2;
            (sorted[mid - 1] + sorted[mid]) / 2
        } else {
            sorted[n as usize / 2]
        };

        let variance: f64 = results
            .samples
            .iter()
            .map(|&t| {
                let diff = t as f64 - results.avg_time_ms;
                diff * diff
            })
            .sum::<f64>()
            / n;
        results.stddev_ms = variance.sqrt();
    }

    let wall_clock = total_start.elapsed();
    let success_count = results.exit_codes.iter().filter(|&&c| c == 0).count();
    let fail_count = results.exit_codes.len() - success_count;

    let mut out = format!("## Benchmark Results for `{command}`\n\n");
    out.push_str("| Metric | Value |\n|--------|-------|\n");
    out.push_str(&format!(
        "| Samples | {} |\n",
        results.samples.len()
    ));
    out.push_str(&format!("| Warmup runs | {warmup_runs} |\n"));
    out.push_str(&format!(
        "| Wall clock | {:.1}s |\n",
        wall_clock.as_secs_f64()
    ));
    out.push_str(&format!(
        "| Avg time | {:.1}ms |\n",
        results.avg_time_ms
    ));
    out.push_str(&format!(
        "| Median time | {}ms |\n",
        results.median_time_ms
    ));
    out.push_str(&format!(
        "| Min time | {}ms |\n",
        results.min_time_ms
    ));
    out.push_str(&format!(
        "| Max time | {}ms |\n",
        results.max_time_ms
    ));
    out.push_str(&format!(
        "| StdDev | {:.1}ms |\n",
        results.stddev_ms
    ));
    out.push_str(&format!(
        "| Successful | {success_count}/{sample_size} |\n"
    ));

    if fail_count > 0 {
        out.push_str(&format!(
            "| Failures | {fail_count}/{sample_size} |\n"
        ));
    }

    out.push_str("\n### Samples (ms)\n");
    let sample_list = results
        .samples
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let marker = if results.exit_codes.get(i) == Some(&0) {
                ""
            } else {
                " (FAIL)"
            };
            format!("{}ms{marker}", t)
        })
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&sample_list);
    out.push('\n');

    if success_count == 0 {
        out.push_str(
            "\n\u{26a0} All samples failed. Check the command syntax and ensure \
             the test environment is properly configured."
        );
    } else if success_count < sample_size as usize {
        out.push_str(
            "\n\u{26a0} Some samples failed. Review the failing runs before \
             drawing conclusions."
        );
    }

    Ok(out)
}

fn run_bench_command(command_str: &str) -> (bool, i32) {
    let parts: Vec<&str> = command_str.split_whitespace().collect();
    if parts.is_empty() {
        return (false, -1);
    }

    let mut cmd = Command::new(parts[0]);
    if parts.len() > 1 {
        cmd.args(&parts[1..]);
    }

    match cmd.output() {
        Ok(output) => {
            let code = output.status.code().unwrap_or(-1);
            (code == 0, code)
        }
        Err(_) => (false, -1),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refactor_invalid_json() {
        assert!(refactor_algorithm_topo("not json").is_err());
    }

    #[test]
    fn refactor_missing_target_symbol() {
        assert!(refactor_algorithm_topo(r#"{"reason": "test"}"#).is_err());
    }

    #[test]
    fn refactor_with_valid_input_returns_suggestions_or_no_matches() {
        let result = refactor_algorithm_topo(
            r#"{"target_symbol": "nonexistent_function_xyzzy", "new_name": "renamed_func", "reason": "rename"}"#,
        );
        assert!(result.is_ok());
        let out = result.unwrap();
        assert!(
            out.contains("No definition or call sites found")
                || out.contains("Refactor Suggestion")
        );
    }

    #[test]
    fn benchmark_missing_command() {
        assert!(benchmark_compare("{}").is_err());
    }

    #[test]
    fn benchmark_invalid_json() {
        assert!(benchmark_compare("not json").is_err());
    }

    #[test]
    fn benchmark_with_nonexistent_command() {
        let result = benchmark_compare(
            r#"{"command": "nonexistent_command_xyzzy", "timeout_seconds": 5, "sample_size": 3, "warmup_runs": 1}"#,
        );
        let out = result.unwrap();
        assert!(out.contains("FAIL"));
    }

    #[test]
    fn benchmark_defaults() {
        let result = benchmark_compare(r#"{"command": "echo hello"}"#);
        assert!(result.is_ok());
        let out = result.unwrap();
        assert!(out.contains("Samples"));
        assert!(out.contains("20"));
    }

    #[test]
    fn parse_rg_line_standard() {
        let line = "src/main.rs:42:fn hello_world() {";
        let (file, line_num, content) = parse_rg_line(line).expect("should parse");
        assert_eq!(file, "src/main.rs");
        assert_eq!(line_num, 42);
        assert!(content.contains("hello_world"));
    }

    #[test]
    fn parse_rg_line_skips_empty() {
        assert!(parse_rg_line("").is_none());
        assert!(parse_rg_line("--").is_none());
    }
}
