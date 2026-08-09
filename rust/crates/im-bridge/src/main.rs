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

    let provider = ProviderClient::from_model(&model)
        .map_err(|e| format!("failed to create API client: {e}"))?;

    let bridge_client =
        BridgeApiClient::new(provider, model.clone(), true, GlobalToolRegistry::builtin());

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
