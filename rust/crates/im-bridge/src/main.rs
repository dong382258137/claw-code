//! IM Bridge: `claw-im-bridge` binary.
//!
//! Bridges Feishu/WeCom IM platforms to the claw engine.

use std::collections::HashMap;
use std::sync::Arc;

use api::ProviderClient;
use im_bridge::api_adapter::BridgeApiClient;
use im_bridge::config::ImBridgeConfig;
use im_bridge::response::SessionRouter;
use im_bridge::server::run_server;
use im_bridge::session::SessionManager;
use runtime::{PermissionMode, PermissionPolicy};
use tokio_util::sync::CancellationToken;
use tools::GlobalToolRegistry;

fn main() {
    // 解析 `--setup` 参数：进入交互式配置向导，无需配置文件即可运行。
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--setup") {
        if let Err(e) = im_bridge::setup::run_setup() {
            eprintln!("Setup failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // 同时覆盖二进制 crate target(claw_im_bridge,main.rs 的日志)
                // 与库 target(im_bridge::*,server/connectors 等)。
                "claw_im_bridge=info,im_bridge=info,tower_http=info".into()
            }),
        )
        .init();

    let config = match ImBridgeConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async move {
        if let Err(e) = run_bridge(&config).await {
            eprintln!("Fatal error: {e}");
            std::process::exit(1);
        }
    });
}

async fn run_bridge(config: &ImBridgeConfig) -> Result<(), String> {
    let model = std::env::var("DEEPSEEK_MODEL")
        .or_else(|_| std::env::var("CLAW_MODEL"))
        .unwrap_or_else(|_| "deepseek-v4-pro".to_string());

    tracing::info!("starting IM bridge with model: {model}");

    // 初始化 MCP server(与 CLI 同源):从 runtime 配置加载 server,
    // best-effort 发现工具,并把 manager + discovery 注入全局 McpToolRegistry,
    // 使 `MCP`/`ListMcpResources`/`ReadMcpResource` 等工具真正可用。
    init_mcp_tools();

    // Agent 工作区根配置：切换到配置的主工作区根，使 bash 与文件工具的
    // 沙盒边界（默认 = 进程 cwd）跟随配置，实现跨目录/跨盘访问。
    if let Some(root) = &config.agent.workspace_root {
        match std::env::set_current_dir(root) {
            Ok(()) => tracing::info!(
                "agent workspace_root set to {} (bash cwd + file-tool boundary)",
                root.display()
            ),
            Err(e) => {
                tracing::warn!(
                    "failed to set workspace_root to {}: {e}; falling back to startup directory",
                    root.display()
                );
            }
        }
    }

    let provider = ProviderClient::from_model(&model)
        .map_err(|e| format!("failed to create API client: {e}"))?;

    let mut registry = GlobalToolRegistry::builtin();
    // 未显式配置 workspace_roots 时,自动探测本机所有盘符根作为白名单,
    // 实现"最大权限"(跨盘任意文件访问),编译后零配置即生效。
    // 显式配置时以显式值为准(可借此收窄权限)。
    let extra_roots = if config.agent.workspace_roots.is_empty() {
        detect_drive_roots()
    } else {
        config.agent.workspace_roots.clone()
    };
    if !extra_roots.is_empty() {
        registry = registry.with_workspace_roots(extra_roots.clone());
        tracing::info!(
            "agent workspace_roots: {}",
            extra_roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let bridge_client = BridgeApiClient::new(provider, model.clone(), true, registry);

    let system_prompt = vec![
        format!(
            "You are claw-code, an AI coding assistant. Today's date is {}.",
            chrono::Local::now().format("%Y-%m-%d")
        ),
        "You are responding via IM. Keep responses concise.\n\
         You have the full claw toolset available (bash, file read/write/edit, \
         web search/fetch, todo, skills, sub-agents, MCP, ...). Use tools to \
         complete the user's request, then reply with the result."
            .to_string(),
    ];

    // IM 通道没有交互式审批界面(无 prompter),`WorkspaceWrite` 会把所有
    // 需要更高权限的工具(DangerFullAccess,如 bash)无条件 Deny 掉。
    // 这里与 CLI `--permission-mode allow` 语义一致:飞书/企微消息即视为
    // 信任用户,全部放行;tools crate 内部的命令/路径动态分级仍然生效。
    let policy = PermissionPolicy::new(PermissionMode::Allow);
    let cancel = CancellationToken::new();
    let spawn_result = SessionManager::spawn(
        bridge_client,
        system_prompt,
        policy,
        cancel,
        config.session_timeout_secs,
    );

    let session_router: SessionRouter = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let server = run_server(config, spawn_result, session_router).await?;

    tracing::info!("IM bridge started. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutting down...");

    // Abort background tasks
    server.server_task.abort();
    server.collector_task.abort();
    server.persist_task.abort();
    if let Some(ws_task) = server.feishu_ws_task {
        ws_task.abort();
    }

    // Wait for tasks to finish
    let _ = tokio::join!(
        server.server_task,
        server.collector_task,
        server.persist_task,
    );

    tracing::info!("IM bridge stopped");
    Ok(())
}

/// Load MCP servers from the runtime config and share them with the global
/// `McpToolRegistry`, exactly like the CLI's `RuntimePluginState` does.
///
/// Best-effort: a missing/broken config only disables MCP tools — the
/// built-in toolset keeps working.
fn init_mcp_tools() {
    use runtime::{ConfigLoader, McpServerManager};

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = match loader.load() {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!("MCP disabled: failed to load runtime config: {e}");
            return;
        }
    };

    let mut manager = McpServerManager::from_runtime_config(&runtime_config);
    if manager.server_names().is_empty() && manager.unsupported_servers().is_empty() {
        tracing::info!("no MCP servers configured — skipping MCP init");
        return;
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!("MCP disabled: failed to build tokio runtime: {e}");
            return;
        }
    };
    let discovery = rt.block_on(manager.discover_tools_best_effort());
    drop(rt);

    tools::set_global_mcp_manager(Arc::new(std::sync::Mutex::new(manager)));
    tools::populate_global_mcp_registry_from_discovery(&discovery);

    let server_count = discovery
        .tools
        .iter()
        .map(|t| t.server_name.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    tracing::info!(
        "MCP ready: {} servers / {} tools discovered ({} failed, {} unsupported)",
        server_count,
        discovery.tools.len(),
        discovery.failed_servers.len(),
        discovery.unsupported_servers.len()
    );
}

/// 枚举本机所有存在的盘符根（Windows，如 `C:\`、`D:\`）作为默认工作区
/// 白名单，实现"最大权限"：文件工具可跨盘访问任意路径。盘符机器相关，
/// 运行时探测比硬编码通用；显式配置 `workspace_roots` 时以其为准。
#[must_use]
fn detect_drive_roots() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|letter| std::path::PathBuf::from(format!("{}:\\", letter as char)))
            .filter(|root| std::fs::metadata(root).is_ok_and(|m| m.is_dir()))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![std::path::PathBuf::from("/")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_drive_roots_includes_current_dir_drive() {
        let roots = detect_drive_roots();
        assert!(!roots.is_empty(), "至少探测到一个盘符根: {roots:?}");
        // 当前工作目录所在盘必须存在
        if let Ok(cwd) = std::env::current_dir() {
            let cwd = cwd.to_string_lossy().to_uppercase();
            let drive = cwd
                .chars()
                .next()
                .map(|c| format!("{}:\\", c))
                .unwrap_or_default();
            assert!(
                roots
                    .iter()
                    .any(|r| r.to_string_lossy().eq_ignore_ascii_case(&drive)),
                "应包含当前盘 {drive}: {roots:?}"
            );
        }
    }
}
