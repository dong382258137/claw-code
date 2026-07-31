//! llm-eval CLI — 运行模型能力评测基准。
//!
//! ```text
//! cargo run -p llm-eval -- --cases <suite.jsonl> --model deepseek-v4-flash [--threshold 0.8]
//! ```
//!
//! 退出码:0 = 通过率 >= 100%(全通过)或 `--threshold` 指定值;1 = 未达阈值或运行错误。

use std::path::PathBuf;

use llm_eval::{render_report, CaseSuite, EvalRunner};

struct Args {
    cases: PathBuf,
    model: String,
    threshold: f64,
}

fn parse_args() -> Result<Args, String> {
    let mut cases: Option<PathBuf> = None;
    let mut model: Option<String> = None;
    let mut threshold = 1.0;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cases" | "-c" => {
                cases = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--cases requires a path".to_string())?,
                ));
            }
            "--model" | "-m" => {
                model = Some(
                    args.next()
                        .ok_or_else(|| "--model requires a name".to_string())?,
                );
            }
            "--threshold" => {
                let raw = args
                    .next()
                    .ok_or_else(|| "--threshold requires a value".to_string())?;
                threshold = raw
                    .parse::<f64>()
                    .map_err(|e| format!("invalid threshold {raw}: {e}"))?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: llm-eval --cases <suite.jsonl|suite.json> --model <model> [--threshold <0-1>]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        cases: cases.ok_or_else(|| "missing --cases".to_string())?,
        model: model.ok_or_else(|| "missing --model".to_string())?,
        threshold,
    })
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let suite = match CaseSuite::load(&args.cases) {
        Ok(suite) => suite,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    if suite.is_empty() {
        eprintln!("error: suite {} contains no cases", args.cases.display());
        std::process::exit(2);
    }

    let runner = EvalRunner::new(&args.model).with_pass_threshold(args.threshold);

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: create tokio runtime: {e}");
            std::process::exit(2);
        }
    };
    let outcomes = runtime.block_on(runner.run(&suite));
    let report = render_report(&args.model, &outcomes);
    println!("{report}");

    let passed = outcomes.iter().filter(|o| o.passed).count();
    let pass_rate = passed as f64 / outcomes.len() as f64;
    std::process::exit(if pass_rate >= args.threshold { 0 } else { 1 });
}
