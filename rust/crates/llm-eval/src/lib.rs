//! # llm-eval — 模型能力评测基准
//!
//! 目标:为框架的「提升模型能力」改动提供**客观度量手段**。
//! 支持两种评分模式:
//!
//! 1. **golden-keyword**(默认):每个 case 声明一组 `golden` 关键词,
//!    模型输出包含全部关键词 → pass。分数 = 命中数 / 总数。
//! 2. **rubric**(元数据):case 声明自由文本评分标准,供人工复核或未来
//!    LLM-as-judge 扩展使用(当前版本不自动评分,保留在报告中)。
//!
//! # 用法
//!
//! ```text
//! cargo run -p llm-eval -- --cases crates/llm-eval/examples/cases.jsonl --model deepseek-v4-flash
//! ```
//!
//! # 任务集格式(JSONL,每行一个 case)
//!
//! ```json
//! {"id":"code-01","name":"extract token","prompt":"...","golden":["bearer","token"],"rubric":"..."}
//! ```

use std::path::Path;

use api::{max_tokens_for_model, InputMessage, MessageRequest, ProviderClient};

/// 单个评测 case。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TaskCase {
    /// 唯一标识(如 `code-01`)。
    pub id: String,
    /// 人类可读名称。
    pub name: String,
    /// 发给模型的 prompt。
    pub prompt: String,
    /// 金标准关键词 — 大小写不敏感子串匹配。
    #[serde(default)]
    pub golden: Vec<String>,
    /// 匹配模式:false(默认)= 全部 golden 关键词必须命中;true = 命中任一即得分 1.0。
    #[serde(default)]
    pub match_any: bool,
    /// 自由文本评分标准(可选,供人工复核 / 未来 LLM-as-judge 使用)。
    #[serde(default)]
    pub rubric: Option<String>,
    /// 最少输出长度(字符),低于该值视为空响应失败。
    #[serde(default)]
    pub min_output_chars: usize,
}

/// 任务集。
#[derive(Debug, Clone, Default)]
pub struct CaseSuite {
    pub cases: Vec<TaskCase>,
}

impl CaseSuite {
    /// 从 JSONL(每行一个 case)或 JSON(数组)加载任务集。
    pub fn load(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let trimmed = content.trim();
        if trimmed.starts_with('[') {
            let cases: Vec<TaskCase> = serde_json::from_str(trimmed)
                .map_err(|e| format!("parse JSON suite {}: {e}", path.display()))?;
            return Ok(Self { cases });
        }
        let mut cases = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let case: TaskCase = serde_json::from_str(line)
                .map_err(|e| format!("parse JSONL line {} in {}: {e}", idx + 1, path.display()))?;
            cases.push(case);
        }
        Ok(Self { cases })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.cases.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cases.is_empty()
    }
}

/// 单个 case 的评测结果。
#[derive(Debug, Clone)]
pub struct CaseOutcome {
    pub case: TaskCase,
    /// 模型原始输出(截断存储避免报告过大)。
    pub output_truncated: String,
    /// 命中关键词数。
    pub hits: usize,
    /// 命中率 0.0-1.0。
    pub score: f64,
    /// 是否通过。
    pub passed: bool,
    /// 失败原因说明。
    pub detail: String,
    /// API 调用耗时(毫秒)。
    pub latency_ms: u64,
}

/// 评测 runner — 串行调用 `ProviderClient::send_message`。
pub struct EvalRunner {
    model: String,
    /// 通过所需的命中率阈值(默认 1.0 = 全部关键词命中)。
    pass_threshold: f64,
}

impl EvalRunner {
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            pass_threshold: 1.0,
        }
    }

    #[must_use]
    pub fn with_pass_threshold(mut self, threshold: f64) -> Self {
        self.pass_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// 对单个 case 执行一次调用并评分。
    pub async fn evaluate(&self, case: &TaskCase) -> Result<CaseOutcome, String> {
        let client = ProviderClient::from_model(&self.model)
            .map_err(|e| format!("create provider client: {e}"))?;
        let request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model),
            messages: vec![InputMessage::user_text(&case.prompt)],
            stream: false,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let response = client
            .send_message(&request)
            .await
            .map_err(|e| format!("case {} API error: {e}", case.id))?;
        let latency_ms = started.elapsed().as_millis() as u64;

        let text: String = response
            .content
            .iter()
            .filter_map(|block| {
                if let api::OutputContentBlock::Text { text } = block {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");

        let (hits, score, passed, detail) = score_case(case, &text, self.pass_threshold);
        let truncated: String = text.chars().take(600).collect();
        Ok(CaseOutcome {
            case: case.clone(),
            output_truncated: truncated,
            hits,
            score,
            passed,
            detail,
            latency_ms,
        })
    }

    /// 批量评测全部 case。
    pub async fn run(&self, suite: &CaseSuite) -> Vec<CaseOutcome> {
        let mut outcomes = Vec::with_capacity(suite.cases.len());
        for case in &suite.cases {
            match self.evaluate(case).await {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => {
                    // 单 case 失败不中断整体评测 — 记录为失败结果。
                    outcomes.push(CaseOutcome {
                        case: case.clone(),
                        output_truncated: String::new(),
                        hits: 0,
                        score: 0.0,
                        passed: false,
                        detail: e,
                        latency_ms: 0,
                    });
                }
            }
        }
        outcomes
    }
}

/// 评分:关键词子串匹配(大小写不敏感),rubric 模式仅计入 detail。
fn score_case(case: &TaskCase, output: &str, threshold: f64) -> (usize, f64, bool, String) {
    let trimmed = output.trim();
    if case.min_output_chars > 0 && trimmed.chars().count() < case.min_output_chars {
        let detail = format!(
            "output too short ({} < {} chars)",
            trimmed.chars().count(),
            case.min_output_chars
        );
        return (0, 0.0, false, detail);
    }
    if case.golden.is_empty() {
        // 无关键词:非空输出即通过(仅统计性检查)。
        let passed = !trimmed.is_empty();
        let detail = if passed {
            "no golden keywords; non-empty output".to_string()
        } else {
            "empty output".to_string()
        };
        return (0, if passed { 1.0 } else { 0.0 }, passed, detail);
    }
    let output_lower = trimmed.to_lowercase();
    let matched: Vec<bool> = case
        .golden
        .iter()
        .map(|kw| output_lower.contains(&kw.to_lowercase()))
        .collect();
    let hits = matched.iter().filter(|m| **m).count();
    let missing: Vec<&str> = case
        .golden
        .iter()
        .zip(&matched)
        .filter(|(_, m)| !**m)
        .map(|(kw, _)| kw.as_str())
        .collect();
    // any 模式:命中任一关键词即满分;all 模式:按命中比例计分。
    let score = if case.match_any {
        if hits > 0 {
            1.0
        } else {
            0.0
        }
    } else {
        hits as f64 / case.golden.len() as f64
    };
    let passed = score >= threshold;
    let detail = if passed {
        format!("all {hits} golden keyword(s) matched")
    } else {
        format!(
            "missing {} keyword(s): {}",
            missing.len(),
            missing.join(", ")
        )
    };
    (hits, score, passed, detail)
}

/// 汇总报告(文本)。
#[must_use]
pub fn render_report(model: &str, outcomes: &[CaseOutcome]) -> String {
    let passed = outcomes.iter().filter(|o| o.passed).count();
    let total = outcomes.len();
    let avg_score: f64 = if total == 0 {
        0.0
    } else {
        outcomes.iter().map(|o| o.score).sum::<f64>() / total as f64
    };
    let mut lines = vec![
        format!("Eval report — model {model}"),
        format!("  Passed           {passed}/{total}"),
        format!("  Avg score        {avg_score:.3}"),
        format!("  Avg latency      {:.0} ms", {
            let sum: u64 = outcomes.iter().map(|o| o.latency_ms).sum();
            if total == 0 {
                0
            } else {
                sum / total as u64
            }
        }),
        String::new(),
    ];
    for outcome in outcomes {
        lines.push(format!(
            "[{}] {} — {} (score {:.2}, hits {}/{})",
            if outcome.passed { "PASS" } else { "FAIL" },
            outcome.case.id,
            outcome.case.name,
            outcome.score,
            outcome.hits,
            outcome.case.golden.len()
        ));
        if !outcome.passed {
            lines.push(format!("      detail: {}", outcome.detail));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "Pass rate {:.1}% (threshold {:.0}%)",
        if total == 0 {
            0.0
        } else {
            passed as f64 / total as f64 * 100.0
        },
        // 汇总通过率阈值展示:与 runner 内部一致(1.0)
        100.0
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(golden: &[&str]) -> TaskCase {
        TaskCase {
            id: "t1".into(),
            name: "test".into(),
            prompt: "do it".into(),
            golden: golden.iter().map(|s| s.to_string()).collect(),
            match_any: false,
            rubric: None,
            min_output_chars: 0,
        }
    }

    fn case_any(golden: &[&str]) -> TaskCase {
        TaskCase {
            match_any: true,
            ..case(golden)
        }
    }

    #[test]
    fn scoring_passes_when_all_keywords_match() {
        let c = case(&["bearer", "token"]);
        let (hits, score, passed, _) = score_case(&c, "use Bearer Token here", 1.0);
        assert_eq!(hits, 2);
        assert_eq!(score, 1.0);
        assert!(passed);
    }

    #[test]
    fn scoring_fails_on_missing_keyword() {
        let c = case(&["bearer", "token"]);
        let (hits, score, passed, detail) = score_case(&c, "use a plain token", 1.0);
        assert_eq!(hits, 1);
        assert_eq!(score, 0.5);
        assert!(!passed);
        assert!(detail.contains("bearer"));
    }

    #[test]
    fn scoring_case_insensitive() {
        let c = case(&["BEARER"]);
        let (hits, _, passed, _) = score_case(&c, "bearer", 1.0);
        assert_eq!(hits, 1);
        assert!(passed);
    }

    #[test]
    fn scoring_respects_threshold() {
        let c = case(&["a", "b", "c"]);
        let (_, score, passed, _) = score_case(&c, "a b", 0.5);
        assert!((score - 2.0 / 3.0).abs() < 1e-9);
        assert!(passed);
    }

    #[test]
    fn scoring_empty_output_fails() {
        let c = case(&["a"]);
        let (_, _, passed, detail) = score_case(&c, "   ", 1.0);
        assert!(!passed);
        assert!(detail.contains("missing") || detail.contains("empty"));
    }

    #[test]
    fn scoring_any_mode_passes_on_single_hit() {
        let c = case_any(&["yes", "no"]);
        let (hits, score, passed, _) = score_case(&c, "yes", 1.0);
        assert_eq!(hits, 1);
        assert_eq!(score, 1.0);
        assert!(passed);
    }

    #[test]
    fn scoring_any_mode_fails_on_zero_hits() {
        let c = case_any(&["yes", "no"]);
        let (hits, score, passed, detail) = score_case(&c, "maybe", 1.0);
        assert_eq!(hits, 0);
        assert_eq!(score, 0.0);
        assert!(!passed);
        assert!(detail.contains("yes, no"));
    }

    #[test]
    fn suite_loads_jsonl_ignoring_comments() {
        let dir = std::env::temp_dir().join("llm-eval-test-suite.jsonl");
        std::fs::write(
            &dir,
            "# comment line\n{\"id\":\"a\",\"name\":\"A\",\"prompt\":\"p\",\"golden\":[\"x\"]}\n{\"id\":\"b\",\"name\":\"B\",\"prompt\":\"p\"}\n",
        )
        .unwrap();
        let suite = CaseSuite::load(&dir).expect("suite should load");
        assert_eq!(suite.len(), 2);
        assert_eq!(suite.cases[1].golden.len(), 0);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn report_counts_passes() {
        let c = case(&["ok"]);
        let outcomes = vec![
            CaseOutcome {
                case: c.clone(),
                output_truncated: "ok".into(),
                hits: 1,
                score: 1.0,
                passed: true,
                detail: "all matched".into(),
                latency_ms: 10,
            },
            CaseOutcome {
                case: c,
                output_truncated: "no".into(),
                hits: 0,
                score: 0.0,
                passed: false,
                detail: "missing ok".into(),
                latency_ms: 5,
            },
        ];
        let report = render_report("test-model", &outcomes);
        assert!(report.contains("Passed           1/2"));
        assert!(report.contains("[PASS] t1"));
        assert!(report.contains("[FAIL] t1"));
    }
}
