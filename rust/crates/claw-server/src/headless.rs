//! 无头单轮执行入口(内部,非产品面)。
//!
//! **为什么需要**:Phase 3(`f69b5316`)删除 CLI 参数解析后,`-p` 一次性提示与
//! `evolve-source` 子命令一并消失,但三处调用方仍在按旧语法拉起子进程:
//!
//! - [`runtime::evolution_auto::spawn_pipeline_detached`] → `claw evolve-source auto`
//! - [`runtime::evolution_auto::run_pipeline`] → `claw --dangerously-skip-permissions -p <prompt>`
//! - `llm-eval/benchmarks/run_pass1.py` → `claw <flags> -p <prompt>`
//!
//! 子进程在**参数解析阶段**就 `exit(1)`,而 spawn 侧 `stderr` 已被置 null ⇒
//! 失败完全静默。后果:P0 自动进化闭环与 P1 pass^1 度量同时失效,而
//! `.last_spawn` 仍在写、inbox 仍在积压,从外部看"一切正常"。
//!
//! 本模块恢复这两条通道,但**不恢复公开子命令** —— 保持 Phase 3
//! "只保留 Web 入口"的产品面,无头能力仅作为内部契约供管道与评测器使用。

use std::path::PathBuf;
use std::sync::Arc;

use runtime::{PermissionMode, PermissionPromptDecision, PermissionPrompter, PermissionRequest};

use crate::app::LiveCli;

/// 无头模式权限询问器:一律放行。
///
/// 与 `--dangerously-skip-permissions` 语义一致。无头进程没有终端,任何
/// 交互式询问都只会挂死或拿到 EOF 空答案,故不提供交互分支。
struct AllowAllPrompter;

impl PermissionPrompter for AllowAllPrompter {
    fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
        PermissionPromptDecision::Allow
    }
}

/// 跑一次无头 turn,返回进程退出码(0 成功 / 1 失败)。
///
/// 工作区取进程当前目录 —— 调用方(`run_pass1.py` 与进化管道)都以
/// `current_dir` 指定目标树,与 `cargo test` 的判分目录保持一致。
pub fn run_prompt_once(prompt: &str) -> i32 {
    let workspace: PathBuf = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("[headless] 无法确定当前目录: {error}");
            return 1;
        }
    };
    let model =
        crate::config_model_for(&workspace).unwrap_or_else(|| crate::DEFAULT_MODEL.to_string());
    let mut cli = match LiveCli::new(
        model,
        true,
        None,
        PermissionMode::DangerFullAccess,
        Vec::new(),
        &workspace,
    ) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("[headless] 初始化失败: {error}");
            return 1;
        }
    };
    // 流式文本直出 stdout:评测器与管道都把子进程输出重定向到文件,
    // 因此这里必须**及时 flush**,否则进程退出前的尾部内容可能丢失。
    let emitter: runtime::StatusEmitter = Arc::new(|event: runtime::StatusEvent| {
        if let runtime::StatusEvent::TextDelta(text) = event {
            use std::io::Write;
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    });
    cli.set_status_emitter(emitter);

    let mut prompter = AllowAllPrompter;
    match cli.run_turn_silent_with(prompt, &mut prompter) {
        Ok(()) => {
            println!();
            0
        }
        Err(error) => {
            eprintln!("[headless] turn 失败: {error}");
            1
        }
    }
}

/// 跑一次进化管道,返回进程退出码。
///
/// 源码根走既有注册表(`~/.claw/source-root` / `CLAW_SOURCE_ROOT`),
/// 与热升级助手同一套解析逻辑,避免"部署能找到源码、进化找不到"的漂移。
pub fn run_pipeline_cli() -> i32 {
    let Some(source_root) = runtime::source_root::resolve_source_root() else {
        eprintln!("[evolve-pipeline] 未登记源码根(见 ~/.claw/source-root)");
        return 1;
    };
    match runtime::evolution_auto::run_pipeline(&source_root) {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("[evolve-pipeline] {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 权限询问器必须无条件放行:无头进程没有终端,任何询问都会挂死。
    #[test]
    fn allow_all_prompter_always_allows() {
        let mut prompter = AllowAllPrompter;
        let request = PermissionRequest {
            tool_name: "write_file".to_string(),
            input: "{}".to_string(),
            current_mode: PermissionMode::ReadOnly,
            required_mode: PermissionMode::DangerFullAccess,
            reason: None,
        };
        assert_eq!(
            prompter.decide(&request),
            PermissionPromptDecision::Allow,
            "无头模式不得因权限询问而阻塞"
        );
    }
}
