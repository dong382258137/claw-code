#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    clippy::unneeded_struct_pattern,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]
//! CLaw Web 服务 crate（Phase 9 由 `rusty-claude-cli` 更名为 `claw-server`）。
//!
//! Phase 3 起产品界面只有 Web：本 crate 只保留 `claw serve` 入口、
//! turn 执行内核、会话/检查点/热升级与会话市场能力，CLI/TUI 界面层已删除。

pub mod api_client;
pub mod app;
pub mod checkpoint;
pub mod init;
pub mod llm_clients;
pub mod plugin_state;
pub mod serve;
pub mod session_mgr;
pub mod tool_executor;
pub mod upgrade_helper;
pub mod upgrade_resume;
pub mod voice;

pub use commands::plugin_market;
/// 技能市场已下沉到 `commands`（WebUI 与内部流水线共用同一条
/// 原子安装流水线），此处重导出以保持 `crate::skill_market` 路径稳定。
pub use commands::skill_market;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use runtime::ConfigLoader;
use serde_json::{json, Value};

// 单模型架构:Pro 已下线,主会话与子任务统一使用 flash(官方 ID deepseek-flash)。
pub const DEFAULT_MODEL: &str = "deepseek-flash";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_DATE: &str = match option_env!("BUILD_DATE") {
    Some(d) => d,
    None => "unknown",
};
/// 构建指纹(build.rs 注入):工作树干净时为 git 短 sha,脏树时为内容哈希,
/// 非 git 环境为空串。用于区分"同版本号的不同构建"。
pub const GIT_FINGERPRINT: &str = match option_env!("GIT_FINGERPRINT") {
    Some(f) => f,
    None => "",
};
/// 构建时刻(build.rs 注入,`MMDD-HHMM`,分钟精度)。
pub const BUILD_TIME: &str = match option_env!("BUILD_TIME") {
    Some(t) => t,
    None => "",
};
pub const BUILD_TARGET: Option<&str> = option_env!("TARGET");
pub const GIT_SHA: Option<&str> = option_env!("GIT_SHA");

pub type AllowedToolSet = BTreeSet<String>;

/// 状态栏展示的**构建标签**:`<指纹>@<构建时刻>`。
#[must_use]
pub fn version_label() -> String {
    match (GIT_FINGERPRINT.is_empty(), BUILD_TIME.is_empty()) {
        (false, false) => format!("{GIT_FINGERPRINT}@{BUILD_TIME}"),
        (false, true) => GIT_FINGERPRINT.to_string(),
        (true, false) => format!("@{BUILD_TIME}"),
        (true, true) => format!("v{VERSION}"),
    }
}

pub fn filter_tool_specs(
    tool_registry: &tools::GlobalToolRegistry,
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<api::ToolDefinition> {
    tool_registry.definitions(allowed_tools)
}

pub fn max_tokens_for_model(model: &str) -> u32 {
    api::max_tokens_for_model(model)
}

pub fn plugin_command_json(
    action: &str,
    target: Option<&str>,
    result: &commands::PluginsCommandResult,
    report: &plugins::PluginRegistryReport,
) -> Value {
    let failures = report.failures();
    json!({
        "kind": "plugin",
        "action": action,
        "target": target,
        "status": if failures.is_empty() { "ok" } else { "degraded" },
        "message": result.message,
        "reload_runtime": result.reload_runtime,
        "plugins": report.summaries().iter().map(plugin_summary_json).collect::<Vec<_>>(),
        "load_failures": failures.iter().map(plugin_load_failure_json).collect::<Vec<_>>(),
    })
}

pub fn plugin_summary_json(plugin: &plugins::PluginSummary) -> Value {
    json!({
        "id": &plugin.metadata.id,
        "name": &plugin.metadata.name,
        "version": &plugin.metadata.version,
        "description": &plugin.metadata.description,
        "kind": plugin.metadata.kind.to_string(),
        "source": &plugin.metadata.source,
        "enabled": plugin.enabled,
        "lifecycle_state": plugin.lifecycle_state(),
        "lifecycle": {
            "configured": !plugin.lifecycle.is_empty(),
            "init": {
                "configured": !plugin.lifecycle.init.is_empty(),
                "command_count": plugin.lifecycle.init.len(),
            },
            "shutdown": {
                "configured": !plugin.lifecycle.shutdown.is_empty(),
                "command_count": plugin.lifecycle.shutdown.len(),
            },
        },
    })
}

pub fn plugin_load_failure_json(failure: &plugins::PluginLoadFailure) -> Value {
    json!({
        "plugin_root": failure.plugin_root.display().to_string(),
        "kind": failure.kind.to_string(),
        "source": &failure.source,
        "lifecycle_state": "load_failed",
        "error": failure.error().to_string(),
    })
}

pub struct PluginsCommandPayload {
    pub message: String,
    pub reload_runtime: bool,
    pub status: &'static str,
    pub config_load_error: Option<String>,
    pub plugins: Vec<Value>,
    pub load_failures: Vec<Value>,
}

/// 细粒度诊断日志（`~/.claw/claw-diag.log`）。
pub(crate) fn diag_log(msg: &str) {
    use std::io::Write;
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let claw_dir = home.join(".claw");
    let _ = std::fs::create_dir_all(&claw_dir);
    let path = claw_dir.join("claw-diag.log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "[{ts}] {msg}");
        let _ = f.flush();
    }
}

pub fn command_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn permission_mode_from_label(mode: &str) -> runtime::PermissionMode {
    match mode {
        "read-only" => runtime::PermissionMode::ReadOnly,
        "workspace-write" => runtime::PermissionMode::WorkspaceWrite,
        "danger-full-access" => runtime::PermissionMode::DangerFullAccess,
        other => panic!("unsupported permission mode label: {other}"),
    }
}

pub fn config_model_for(workspace: &Path) -> Option<String> {
    let loader = ConfigLoader::default_for(workspace);
    loader.load().ok()?.model().map(ToOwned::to_owned)
}

/// 摘要截断：按字符数截断并在溢出时追加省略号。
pub(crate) fn truncate_for_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// 工具 ID 摘要：超过 12 字符时截断并追加省略号。
pub(crate) fn short_tool_id(id: &str) -> String {
    let char_count = id.chars().count();
    if char_count <= 12 {
        return id.to_string();
    }
    let prefix: String = id.chars().take(12).collect();
    format!("{prefix}…")
}

fn help_text() -> String {
    format!(
        "claw {version}

用法:
  claw serve [选项]      启动 Web UI 服务（唯一产品入口）
  claw --version         打印版本
  claw --help            显示本帮助

说明:
  Phase 3 起 CLI/TUI 界面已移除。所有交互请通过 `claw serve` 打开 Web UI。
  以 `claw-web` 文件名启动时等价于 `claw serve`。
",
        version = version_label()
    )
}

/// `claw` binary 的唯一入口。
///
/// 只处理三件事：`--upgrade-helper`、`serve`、`--version/--help`。
pub fn main_entry() {
    // 诊断：注册 panic hook,落盘到 ~/.claw/claw-crash.log
    // 双击运行时 stderr 不可见,panic hook 是唯一能确认"是否 panic"的可靠信号。
    runtime::diag::install_panic_hook();

    // 热升级助手分支(任务 2 一键化):检测 --upgrade-helper <user_cwd>。
    // 由旧进程 /upgrade spawn 的独立进程(新终端),自动化
    // "等旧进程退出 → mv exe.old → cargo build → 启动新进程"。
    let mut argv: Vec<String> = std::env::args().collect();
    if argv.iter().any(|a| a == "--upgrade-helper") {
        let user_cwd = argv
            .iter()
            .position(|a| a == "--upgrade-helper")
            .and_then(|i| argv.get(i + 1))
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if let Err(e) = upgrade_helper::run_helper(&user_cwd) {
            let msg = format!("[upgrade-helper] 失败: {e}");
            eprintln!("{msg}");
            // 错误落盘:helper 独立新终端关闭后输出即丢失,日志供事后诊断。
            let log_path = upgrade_helper::upgrade_log_path(&user_cwd);
            if let Some(parent) = log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                use std::io::Write;
                let _ = writeln!(file, "{msg}");
            }
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    // 快速启动器：同一个二进制以 `claw-web` 命名（副本或硬链接）时等价于
    // `claw serve`，便于建桌面快捷方式后双击启动 Web UI。
    let invoked_as_web = std::path::Path::new(&argv[0])
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("claw-web"));
    // --version/--help 原样透传，方便确认启动器版本。
    let passthrough = argv
        .get(1)
        .map(String::as_str)
        .is_some_and(|arg| matches!(arg, "--version" | "-V" | "--help" | "-h"));
    if invoked_as_web && !passthrough && argv.get(1).map(String::as_str) != Some("serve") {
        argv.insert(1, "serve".to_string());
    }

    match argv.get(1).map(String::as_str) {
        Some("serve") => {
            // 只绑定 127.0.0.1，启动时打印带一次性 token 的 URL。
            if let Err(error) = serve::run_web(&argv[2..]) {
                eprintln!("serve failed: {error}");
                std::process::exit(1);
            }
        }
        Some("--version" | "-V") => println!("claw {}", version_label()),
        Some("--help" | "-h") | None => println!("{}", help_text()),
        Some(other) => {
            eprintln!(
                "error: 不支持的参数 `{other}` —— Phase 3 起 CLI 子命令已移除。\n\n{}",
                help_text()
            );
            std::process::exit(1);
        }
    }
}
