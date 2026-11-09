//! 真实 LSP 端到端验证(需要本机安装 rust-analyzer / pylsp / ruff)。
//!
//! 默认忽略,手动执行:
//! `cargo test -p tools --test lsp_live -- --ignored --nocapture`

use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tools::{execute_tool_in, TurnEnv};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repo root")
}

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("claw-lsp-{tag}-{stamp}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn run_diagnostics(workspace: &Path, file: &Path) -> serde_json::Value {
    run_diagnostics_paths(workspace, &[file])
}

fn run_diagnostics_paths(workspace: &Path, files: &[&Path]) -> serde_json::Value {
    let env = TurnEnv::new(workspace.to_path_buf(), Vec::new());
    let started = Instant::now();
    let paths: Vec<String> = files
        .iter()
        .map(|file| file.display().to_string())
        .collect();
    let output = execute_tool_in(
        &env,
        "LSP",
        &json!({
            "action": "diagnostics",
            "paths": paths,
        }),
    )
    .expect("LSP tool should return JSON (degradation is reported inside JSON)");
    println!("elapsed={}ms", started.elapsed().as_millis());
    println!("{output}");
    serde_json::from_str(&output).expect("valid json")
}

#[test]
#[ignore = "live: spawns real rust-analyzer"]
fn live_rust_diagnostics_on_repo_file() {
    let workspace = repo_root();
    let file = std::env::var("CLAW_LSP_LIVE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace.join("rust/crates/runtime/src/lsp_client.rs"));
    let parsed = run_diagnostics(&workspace, &file);
    let result = &parsed["results"][0];
    assert_eq!(result["language"], "rust");
    assert!(
        result["source"] == "lsp" || result["source"] == "cargo-check",
        "unexpected source: {result}"
    );

    // 本仓库是大型 cargo workspace:首次调用 rust-analyzer 冷启动可能超过 15s 预算并
    // 降级到 cargo check,但 server 会继续在后台建索引;等待后第二次调用应命中真实 LSP。
    if result["source"] == "cargo-check" {
        std::thread::sleep(std::time::Duration::from_secs(25));
        let warm = run_diagnostics(&workspace, &file);
        let warm_result = &warm["results"][0];
        assert_eq!(
            warm_result["source"], "lsp",
            "warm call should hit the real language server: {warm_result}"
        );
    }

    // 验证"真实错误能被诊断到":配合临时改坏仓库文件使用。
    if std::env::var_os("CLAW_LSP_EXPECT_ERROR").is_some() && result["count"] == 0 {
        // 首次调用可能只拿到语法层结果,flycheck 完成后再取一次。
        std::thread::sleep(std::time::Duration::from_secs(20));
        let retried = run_diagnostics(&workspace, &file);
        let retried_result = &retried["results"][0];
        assert!(
            retried_result["count"].as_u64().unwrap_or(0) > 0,
            "expected a real diagnostic after flycheck: {retried_result}"
        );
    }
}

#[test]
#[ignore = "live: spawns real rust-analyzer"]
fn live_rust_diagnostics_reports_error_in_small_project() {
    let workspace = temp_dir("rs");
    std::fs::create_dir_all(workspace.join("src")).expect("src");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("cargo toml");
    let file = workspace.join("src/main.rs");
    std::fs::write(&file, "fn main() {\n    let x: i32 = \"not an int\";\n}\n").expect("main");

    let parsed = run_diagnostics(&workspace, &file);
    let result = &parsed["results"][0];
    println!("rust small project source={}", result["source"]);
    assert_eq!(
        result["source"], "lsp",
        "expected real rust-analyzer output"
    );
    assert!(
        result["count"].as_u64().unwrap_or(0) > 0,
        "expected at least one diagnostic: {result}"
    );
}

#[test]
#[ignore = "live: spawns real pylsp + ruff"]
fn live_python_diagnostics_marks_system_interpreter() {
    let workspace = temp_dir("py");
    let file = workspace.join("broken.py");
    std::fs::write(
        &file,
        "def broken(:\n    return 1\n\nvalue: int = undefined_name + 1\n",
    )
    .expect("python file");

    // 屏蔽外部 uv 配置,保证本临时目录只会退到系统解释器。
    let previous_uv_env = std::env::var_os("UV_PROJECT_ENVIRONMENT");
    std::env::remove_var("UV_PROJECT_ENVIRONMENT");
    let parsed = run_diagnostics(&workspace, &file);
    match previous_uv_env {
        Some(value) => std::env::set_var("UV_PROJECT_ENVIRONMENT", value),
        None => std::env::remove_var("UV_PROJECT_ENVIRONMENT"),
    }

    let result = &parsed["results"][0];
    println!(
        "python source={} interpreter_source={}",
        result["source"], result["interpreter_source"]
    );
    assert_eq!(result["language"], "python");
    assert!(
        result["source"] == "lsp" || result["source"] == "ruff" || result["source"] == "py_compile",
        "unexpected source: {result}"
    );
    assert!(
        result["count"].as_u64().unwrap_or(0) > 0,
        "expected at least one diagnostic: {result}"
    );
    assert_eq!(
        result["interpreter_source"], "system",
        "no venv present, so the interpreter must be reported as system"
    );
    assert!(
        result["interpreter_warning"]
            .as_str()
            .unwrap_or("")
            .contains("系统解释器"),
        "system interpreter must be flagged as potentially inaccurate: {result}"
    );
}

/// 一次请求同时涉及 Rust 与 Python:结果必须合并返回,并标明每个文件的来源与语言。
#[test]
#[ignore = "live: spawns rust-analyzer + pylsp in one call"]
fn live_multi_language_diagnostics_merge_in_one_call() {
    let workspace = temp_dir("multi");
    std::fs::create_dir_all(workspace.join("src")).expect("src");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"multi\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("cargo toml");
    let rust_file = workspace.join("src/main.rs");
    std::fs::write(
        &rust_file,
        "fn main() {\n    let x: i32 = \"not an int\";\n}\n",
    )
    .expect("rs");
    let py_file = workspace.join("app.py");
    std::fs::write(&py_file, "def broken(:\n    return 1\n").expect("py");

    let parsed = run_diagnostics_paths(&workspace, &[&rust_file, &py_file]);
    assert_eq!(parsed["files"], 2, "two files requested: {parsed}");
    assert_eq!(
        parsed["degraded_files"], 0,
        "both servers are available: {parsed}"
    );

    let results = parsed["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    let languages: Vec<&str> = results
        .iter()
        .filter_map(|result| result["language"].as_str())
        .collect();
    assert!(languages.contains(&"rust"), "missing rust result: {parsed}");
    assert!(
        languages.contains(&"python"),
        "missing python result: {parsed}"
    );
    assert!(
        parsed["count"].as_u64().unwrap_or(0) >= 2,
        "merged count should cover both files: {parsed}"
    );
    for result in results {
        assert_eq!(
            result["source"], "lsp",
            "expected real LSP output: {result}"
        );
        assert!(
            result["count"].as_u64().unwrap_or(0) > 0,
            "each file should report its own diagnostics: {result}"
        );
    }
}

/// 在 PATH 中查找可执行文件(测试用,避免引入 which 依赖)。
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in [name.to_owned(), format!("{name}.exe")] {
            let full = dir.join(candidate);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

/// 只有 ruff、没有 Python 语言服务器的机器:必须降级到 `ruff check` 并标明来源。
#[test]
#[ignore = "live: restricts PATH to a ruff-only bin dir"]
fn live_zz_python_falls_back_to_ruff() {
    let Some(ruff) = find_in_path("ruff") else {
        eprintln!("[lsp_live] ruff not installed; skipping");
        return;
    };
    let workspace = temp_dir("py-ruff");
    let file = workspace.join("bad.py");
    std::fs::write(&file, "value: int = undefined_name + 1\n").expect("python file");

    // 构造只含 ruff 的 PATH:语言服务器与 python 都不可见,迫使走 ruff 兜底。
    let bin = workspace.join("ruff-only-bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let target = bin.join(ruff.file_name().expect("ruff file name"));
    if std::fs::hard_link(&ruff, &target).is_err() {
        std::fs::copy(&ruff, &target).expect("copy ruff");
    }

    let previous_path = std::env::var_os("PATH");
    let previous_venv = std::env::var_os("VIRTUAL_ENV");
    let previous_uv_env = std::env::var_os("UV_PROJECT_ENVIRONMENT");
    std::env::set_var("PATH", &bin);
    std::env::remove_var("VIRTUAL_ENV");
    std::env::remove_var("UV_PROJECT_ENVIRONMENT");

    let output = run_diagnostics(&workspace, &file);

    match previous_path {
        Some(value) => std::env::set_var("PATH", value),
        None => std::env::remove_var("PATH"),
    }
    match previous_venv {
        Some(value) => std::env::set_var("VIRTUAL_ENV", value),
        None => std::env::remove_var("VIRTUAL_ENV"),
    }
    match previous_uv_env {
        Some(value) => std::env::set_var("UV_PROJECT_ENVIRONMENT", value),
        None => std::env::remove_var("UV_PROJECT_ENVIRONMENT"),
    }

    let result = &output["results"][0];
    assert_eq!(result["status"], "degraded", "{result}");
    assert_eq!(
        result["source"], "ruff",
        "should fall back to ruff: {result}"
    );
    assert!(
        result["count"].as_u64().unwrap_or(0) > 0,
        "ruff should report the undefined name: {result}"
    );
    assert!(
        result["note"]
            .as_str()
            .unwrap_or("")
            .contains("interpreter:"),
        "fallback must state which interpreter was bound: {result}"
    );
}

/// 机器上完全没有语言服务器 / 兜底工具时:必须给出明确的降级结果,而不是静默失败。
///
/// 名称以 `live_zz` 开头,保证在本进程内最后执行:它会清空 PATH 并让 python
/// 语言进入 auto-start 冷却,避免影响其他实时测试。
#[test]
#[ignore = "live: clears PATH and VIRTUAL_ENV to simulate missing servers"]
fn live_zz_python_degrades_cleanly_without_servers() {
    let workspace = temp_dir("py-degraded");
    let file = workspace.join("broken.py");
    std::fs::write(&file, "def broken(:\n    return 1\n").expect("python file");

    let empty_bin = workspace.join("empty-bin");
    std::fs::create_dir_all(&empty_bin).expect("empty bin");
    let previous_path = std::env::var_os("PATH");
    let previous_venv = std::env::var_os("VIRTUAL_ENV");
    let previous_uv_env = std::env::var_os("UV_PROJECT_ENVIRONMENT");
    std::env::set_var("PATH", &empty_bin);
    std::env::remove_var("VIRTUAL_ENV");
    std::env::remove_var("UV_PROJECT_ENVIRONMENT");

    let output = run_diagnostics(&workspace, &file);

    match previous_path {
        Some(value) => std::env::set_var("PATH", value),
        None => std::env::remove_var("PATH"),
    }
    match previous_venv {
        Some(value) => std::env::set_var("VIRTUAL_ENV", value),
        None => std::env::remove_var("VIRTUAL_ENV"),
    }
    match previous_uv_env {
        Some(value) => std::env::set_var("UV_PROJECT_ENVIRONMENT", value),
        None => std::env::remove_var("UV_PROJECT_ENVIRONMENT"),
    }

    let result = &output["results"][0];
    assert_eq!(
        result["status"], "degraded",
        "must degrade, not fail: {result}"
    );
    assert_eq!(result["degraded"], true);
    assert_eq!(result["source"], "none", "no checker available: {result}");
    assert_eq!(result["count"], 0);
    assert!(
        result["lsp_reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "must explain why the language server was unavailable: {result}"
    );
    assert!(
        result["note"]
            .as_str()
            .unwrap_or("")
            .contains("neither ruff nor Python"),
        "must explain that the fallback also failed: {result}"
    );
}
