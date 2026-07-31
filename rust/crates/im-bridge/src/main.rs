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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "im_bridge=info,tower_http=info".into()),
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
         Use tools to read, write, edit files, and run commands."
            .to_string(),
    ];

    let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite);
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

    // Wait for tasks to finish
    let _ = tokio::join!(
        server.server_task,
        server.collector_task,
        server.persist_task,
    );

    tracing::info!("IM bridge stopped");
    Ok(())
}
