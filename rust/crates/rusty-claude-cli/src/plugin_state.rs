//! Runtime plugin and MCP state construction.

use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use commands::{handle_plugins_slash_command, PluginsCommandResult};
use plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use runtime::{ConfigLoader, McpServerManager, McpTool, PermissionMode, ToolError};
use serde_json::json;
use tools::{GlobalToolRegistry, RuntimeToolDefinition};

use crate::{plugin_load_failure_json, plugin_summary_json, PluginsCommandPayload};

pub(crate) type RuntimePluginStateBuildOutput = (
    Option<Arc<Mutex<RuntimeMcpState>>>,
    Vec<RuntimeToolDefinition>,
);

pub(crate) struct RuntimePluginState {
    pub(crate) feature_config: runtime::RuntimeFeatureConfig,
    pub(crate) tool_registry: GlobalToolRegistry,
    pub(crate) plugin_registry: PluginRegistry,
    pub(crate) mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

pub(crate) struct RuntimeMcpState {
    runtime: tokio::runtime::Runtime,
    /// P2-1:用 `Arc<Mutex<McpServerManager>>` 包装,使全局 `McpToolRegistry`
    /// 单例能持有一份 Arc clone,从而 `MCPTool` 等 wrapper 工具直接调用
    /// `McpServerManager::call_tool` 等方法,无需经 `RuntimeMcpState` 中转。
    manager: Arc<Mutex<McpServerManager>>,
    pending_servers: Vec<String>,
    degraded_report: Option<runtime::McpDegradedReport>,
    /// P3-4:MCP 生命周期 FSM 校验器,记录 phase 转移历史 + 校验合法性。
    lifecycle: runtime::McpLifecycleValidator,
}

impl RuntimeMcpState {
    pub(crate) fn new(
        runtime_config: &runtime::RuntimeConfig,
    ) -> Result<Option<(Self, runtime::McpToolDiscoveryReport)>, Box<dyn std::error::Error>> {
        // P3-4:用 McpLifecycleValidator 驱动 phase 转移,记录生命周期历史。
        let mut lifecycle = runtime::McpLifecycleValidator::new();
        lifecycle.run_phase(runtime::McpLifecyclePhase::ConfigLoad);
        lifecycle.run_phase(runtime::McpLifecyclePhase::ServerRegistration);

        let mut manager = McpServerManager::from_runtime_config(runtime_config);
        if manager.server_names().is_empty() && manager.unsupported_servers().is_empty() {
            return Ok(None);
        }

        let runtime = tokio::runtime::Runtime::new()?;
        lifecycle.run_phase(runtime::McpLifecyclePhase::SpawnConnect);
        lifecycle.run_phase(runtime::McpLifecyclePhase::InitializeHandshake);

        let discovery = runtime.block_on(manager.discover_tools_best_effort());
        lifecycle.run_phase(runtime::McpLifecyclePhase::ToolDiscovery);
        let pending_servers = discovery
            .failed_servers
            .iter()
            .map(|failure| failure.server_name.clone())
            .chain(
                discovery
                    .unsupported_servers
                    .iter()
                    .map(|server| server.server_name.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let available_tools = discovery
            .tools
            .iter()
            .map(|tool| tool.qualified_name.clone())
            .collect::<Vec<_>>();
        let failed_server_names = pending_servers.iter().cloned().collect::<BTreeSet<_>>();
        let working_servers = manager
            .server_names()
            .into_iter()
            .filter(|server_name| !failed_server_names.contains(server_name))
            .collect::<Vec<_>>();
        let failed_servers =
            discovery
                .failed_servers
                .iter()
                .map(|failure| runtime::McpFailedServer {
                    server_name: failure.server_name.clone(),
                    phase: runtime::McpLifecyclePhase::ToolDiscovery,
                    error: runtime::McpErrorSurface::new(
                        runtime::McpLifecyclePhase::ToolDiscovery,
                        Some(failure.server_name.clone()),
                        failure.error.clone(),
                        std::collections::BTreeMap::from([(
                            "required".to_string(),
                            failure.required.to_string(),
                        )]),
                        true,
                    ),
                })
                .chain(discovery.unsupported_servers.iter().map(|server| {
                    runtime::McpFailedServer {
                        server_name: server.server_name.clone(),
                        phase: runtime::McpLifecyclePhase::ServerRegistration,
                        error: runtime::McpErrorSurface::new(
                            runtime::McpLifecyclePhase::ServerRegistration,
                            Some(server.server_name.clone()),
                            server.reason.clone(),
                            std::collections::BTreeMap::from([
                                (
                                    "transport".to_string(),
                                    format!("{:?}", server.transport).to_ascii_lowercase(),
                                ),
                                ("required".to_string(), server.required.to_string()),
                            ]),
                            false,
                        ),
                    }
                }))
                .collect::<Vec<_>>();
        let degraded_report = (!failed_servers.is_empty()).then(|| {
            runtime::McpDegradedReport::new(
                working_servers,
                failed_servers,
                available_tools.clone(),
                available_tools,
            )
        });

        // P3-4:discovery 有失败时,记录到 lifecycle validator;否则进入 Ready。
        if !pending_servers.is_empty() {
            for failure in &discovery.failed_servers {
                lifecycle.record_failure(runtime::McpErrorSurface::new(
                    runtime::McpLifecyclePhase::ToolDiscovery,
                    Some(failure.server_name.clone()),
                    failure.error.clone(),
                    std::collections::BTreeMap::new(),
                    true,
                ));
            }
        } else {
            lifecycle.run_phase(runtime::McpLifecyclePhase::Ready);
        }

        Ok(Some((
            Self {
                runtime,
                manager: Arc::new(Mutex::new(manager)),
                pending_servers,
                degraded_report,
                lifecycle,
            },
            discovery,
        )))
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // P3-4:记录 Shutdown → Cleanup phase 转移。
        self.lifecycle
            .run_phase(runtime::McpLifecyclePhase::Shutdown);
        self.runtime.block_on(
            self.manager
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .shutdown(),
        )?;
        self.lifecycle
            .run_phase(runtime::McpLifecyclePhase::Cleanup);
        Ok(())
    }

    pub(crate) fn pending_servers(&self) -> Option<Vec<String>> {
        (!self.pending_servers.is_empty()).then(|| self.pending_servers.clone())
    }

    pub(crate) fn degraded_report(&self) -> Option<runtime::McpDegradedReport> {
        self.degraded_report.clone()
    }

    /// P3-4:返回 MCP 生命周期校验器的状态引用(只读)。
    pub(crate) fn lifecycle(&self) -> &runtime::McpLifecycleValidator {
        &self.lifecycle
    }

    /// P2-1:将内部 McpServerManager + discovery 结果分享到全局 McpToolRegistry 单例。
    ///
    /// 分两步:
    /// 1. `set_global_mcp_manager` —— 注入 `Arc<Mutex<McpServerManager>>`,
    ///    使 `McpToolRegistry::call_tool` 在 `inner` 命中后能通过 manager
    ///    实际派发调用。
    /// 2. `populate_global_mcp_registry_from_discovery` —— 把 discovery 结果
    ///    按 server 分组注册到 `inner`,使 base 工具(`MCP`/`ListMcpResources`/
    ///    `ReadMcpResource`)能通过 `server_name` 查找到 server。
    ///
    /// **注意**:wrapper 工具(`MCPTool`/`ListMcpResourcesTool`/`ReadMcpResourceTool`)
    /// 仍通过 `RuntimeMcpState::call_tool` 中转,不走全局 registry。本方法
    /// 使能的是 base 工具路径(经 `tools::global_mcp_registry()`)。
    pub(crate) fn share_manager_to_global_registry(
        &self,
        discovery: &runtime::McpToolDiscoveryReport,
    ) {
        tools::set_global_mcp_manager(Arc::clone(&self.manager));
        tools::populate_global_mcp_registry_from_discovery(discovery);
    }

    pub(crate) fn server_names(&self) -> Vec<String> {
        self.manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .server_names()
    }

    /// Render the MCP server health overview for the system prompt.
    ///
    /// One line per configured server: `- <name>: <transport>, <status>`.
    /// Status is derived from the runtime registry (not the config):
    /// - `connected` — stdio server whose tool discovery succeeded
    /// - `error` — discovery failed (spawn / handshake / tool list error)
    /// - `unsupported (<transport>)` — transport not supported by this
    ///   runtime (sse / http / ws / sdk / managed proxy), never connected
    ///
    /// Wrapped in the canonical `## MCP Servers` section header and injected
    /// as a dynamic prompt section (after the skill catalog), so the model
    /// sees which external servers are usable without probing them blindly.
    /// Returns `None` when no servers are configured.
    pub(crate) fn mcp_catalog_string(&self) -> Option<String> {
        use runtime::McpTransport;
        let manager = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        // Non-stdio transports are never managed; surface their transport so
        // the model knows they exist but cannot be invoked through `MCPTool`.
        let unsupported: std::collections::BTreeMap<&str, McpTransport> = manager
            .unsupported_servers()
            .iter()
            .map(|server| (server.server_name.as_str(), server.transport))
            .collect();
        let pending: std::collections::BTreeSet<&str> =
            self.pending_servers.iter().map(String::as_str).collect();
        let managed_names = manager.server_names();
        let mut names: std::collections::BTreeSet<&str> =
            managed_names.iter().map(String::as_str).collect();
        names.extend(unsupported.keys().copied());
        if names.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for name in names {
            let transport = match unsupported.get(name) {
                Some(transport) => match transport {
                    McpTransport::Stdio => "stdio",
                    McpTransport::Sse => "sse",
                    McpTransport::Http => "http",
                    McpTransport::Ws => "ws",
                    McpTransport::Sdk => "sdk",
                    McpTransport::ManagedProxy => "managed_proxy",
                },
                // `McpServerManager` only manages stdio servers; everything
                // else lands in `unsupported_servers`.
                None => "stdio",
            };
            let status = if unsupported.contains_key(name) {
                // Unsupported transports are included in `pending_servers`
                // via the discovery report chain; check unsupported first.
                format!("unsupported ({transport})")
            } else if pending.contains(name) {
                "error".to_string()
            } else {
                "connected".to_string()
            };
            lines.push(format!("- {name}: {transport}, {status}"));
        }
        lines.sort();
        let catalog = runtime::render_mcp_catalog_section(&lines.join("\n"));
        (!catalog.is_empty()).then_some(catalog)
    }

    pub(crate) fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<String, ToolError> {
        let response = self
            .runtime
            .block_on(
                self.manager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .call_tool(qualified_tool_name, arguments),
            )
            .map_err(|error| ToolError::new(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned JSON-RPC error: {} ({})",
                error.message, error.code
            )));
        }

        let result = response.result.ok_or_else(|| {
            ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned no result payload"
            ))
        })?;
        serde_json::to_string_pretty(&result).map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn list_resources_for_server(
        &mut self,
        server_name: &str,
    ) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(
                self.manager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .list_resources(server_name),
            )
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "resources": result.resources,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn list_resources_for_all_servers(&mut self) -> Result<String, ToolError> {
        let mut resources = Vec::new();
        let mut failures = Vec::new();

        for server_name in self.server_names() {
            match self.runtime.block_on(
                self.manager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .list_resources(&server_name),
            ) {
                Ok(result) => resources.push(json!({
                    "server": server_name,
                    "resources": result.resources,
                })),
                Err(error) => failures.push(json!({
                    "server": server_name,
                    "error": error.to_string(),
                })),
            }
        }

        if resources.is_empty() && !failures.is_empty() {
            let message = failures
                .iter()
                .filter_map(|failure| failure.get("error").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ToolError::new(message));
        }

        serde_json::to_string_pretty(&json!({
            "resources": resources,
            "failures": failures,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn read_resource(
        &mut self,
        server_name: &str,
        uri: &str,
    ) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(
                self.manager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .read_resource(server_name, uri),
            )
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "contents": result.contents,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }
}

pub(crate) fn build_runtime_mcp_state(
    runtime_config: &runtime::RuntimeConfig,
) -> Result<RuntimePluginStateBuildOutput, Box<dyn std::error::Error>> {
    let Some((mcp_state, discovery)) = RuntimeMcpState::new(runtime_config)? else {
        return Ok((None, Vec::new()));
    };

    // P2-1:在 `Arc::new(Mutex::new(mcp_state))` 包装前 share manager +
    // discovery 结果到全局 McpToolRegistry 单例。使 base 工具
    // (`MCP`/`ListMcpResources`/`ReadMcpResource`)能通过
    // `tools::global_mcp_registry()` 查找到 server 并派发到 manager。
    // wrapper 工具(`MCPTool` 等)仍走 `RuntimeMcpState::call_tool` 中转路径。
    mcp_state.share_manager_to_global_registry(&discovery);

    let mut runtime_tools = discovery
        .tools
        .iter()
        .map(mcp_runtime_tool_definition)
        .collect::<Vec<_>>();
    if !mcp_state.server_names().is_empty() {
        runtime_tools.extend(mcp_wrapper_tool_definitions());
    }

    Ok((Some(Arc::new(Mutex::new(mcp_state))), runtime_tools))
}

pub(crate) fn mcp_runtime_tool_definition(tool: &runtime::ManagedMcpTool) -> RuntimeToolDefinition {
    RuntimeToolDefinition {
        name: tool.qualified_name.clone(),
        description: Some(
            tool.tool
                .description
                .clone()
                .unwrap_or_else(|| format!("Invoke MCP tool `{}`.", tool.qualified_name)),
        ),
        input_schema: tool
            .tool
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({ "type": "object", "additionalProperties": true })),
        required_permission: permission_mode_for_mcp_tool(&tool.tool),
        // P2-2:MCP 发现的工具统一打 "mcp" 标签。qualified_name 形如
        // `mcp__<server>__<tool>`,server name 可作为次级标签(便于按 MCP
        // server 分组召回,如 `+github` 召回所有 GitHub MCP 工具)。
        domain_tags: mcp_domain_tags_for(&tool.qualified_name),
    }
}

/// P2-2:从 `mcp__<server>__<tool>` 形式的 qualified_name 派生领域标签。
///
/// 始终包含 `"mcp"`,如果 server name 可解析则追加 `"mcp:<server>"`,
/// 使 LLM 能用 `+mcp:github` 召回特定 server 的工具。
fn mcp_domain_tags_for(qualified_name: &str) -> Vec<String> {
    let mut tags = vec!["mcp".to_string()];
    // qualified_name 形如 "mcp__<server>__<tool>":取第二段作为 server name。
    let parts: Vec<&str> = qualified_name.split("__").collect();
    if parts.len() >= 3 && parts[0] == "mcp" {
        let server = parts[1];
        if !server.is_empty() {
            tags.push(format!("mcp:{server}"));
        }
    }
    tags
}

pub(crate) fn mcp_wrapper_tool_definitions() -> Vec<RuntimeToolDefinition> {
    vec![
        RuntimeToolDefinition {
            name: "MCPTool".to_string(),
            description: Some(
                "Call a configured MCP tool by its qualified name and JSON arguments.".to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "qualifiedName": { "type": "string" },
                    "arguments": {}
                },
                "required": ["qualifiedName"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
            domain_tags: vec!["mcp".to_string(), "mcp:wrapper".to_string()],
        },
        RuntimeToolDefinition {
            name: "ListMcpResourcesTool".to_string(),
            description: Some(
                "List MCP resources from one configured server or from every connected server."
                    .to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
            domain_tags: vec![
                "mcp".to_string(),
                "mcp:wrapper".to_string(),
                "mcp:resources".to_string(),
            ],
        },
        RuntimeToolDefinition {
            name: "ReadMcpResourceTool".to_string(),
            description: Some("Read a specific MCP resource from a configured server.".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
            domain_tags: vec![
                "mcp".to_string(),
                "mcp:wrapper".to_string(),
                "mcp:resources".to_string(),
            ],
        },
    ]
}

pub(crate) fn permission_mode_for_mcp_tool(tool: &McpTool) -> PermissionMode {
    let read_only = mcp_annotation_flag(tool, "readOnlyHint");
    let destructive = mcp_annotation_flag(tool, "destructiveHint");
    let open_world = mcp_annotation_flag(tool, "openWorldHint");

    if read_only && !destructive && !open_world {
        PermissionMode::ReadOnly
    } else if destructive || open_world {
        PermissionMode::DangerFullAccess
    } else {
        PermissionMode::WorkspaceWrite
    }
}

pub(crate) fn mcp_annotation_flag(tool: &McpTool, key: &str) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn plugins_command_payload_for(
    cwd: &Path,
    action: Option<&str>,
    target: Option<&str>,
) -> Result<PluginsCommandPayload, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let (runtime_config, config_load_error) = match loader.load() {
        Ok(runtime_config) => (runtime_config, None),
        Err(error) => (runtime::RuntimeConfig::empty(), Some(error.to_string())),
    };
    let mut manager = build_plugin_manager(cwd, &loader, &runtime_config);
    let result = handle_plugins_slash_command(action, target, &mut manager)?;
    let report = manager.installed_plugin_registry_report()?;
    Ok(plugins_command_payload_from_result(
        result,
        config_load_error,
        &report,
    ))
}

pub(crate) fn plugins_command_payload_from_result(
    result: PluginsCommandResult,
    config_load_error: Option<String>,
    report: &plugins::PluginRegistryReport,
) -> PluginsCommandPayload {
    let failures = report.failures();
    let status = if config_load_error.is_some() || !failures.is_empty() {
        "degraded"
    } else {
        "ok"
    };
    let message = match config_load_error.as_deref() {
        Some(error) => format!(
            "Config load error\n  Status           fail\n  Summary          runtime config failed to load; reporting partial plugins view\n  Details          {error}\n  Hint             `claw doctor` classifies config parse errors; fix the listed field and rerun\n\n{}",
            result.message
        ),
        None => result.message,
    };
    PluginsCommandPayload {
        message,
        reload_runtime: result.reload_runtime,
        status,
        config_load_error,
        plugins: report.summaries().iter().map(plugin_summary_json).collect(),
        load_failures: failures.iter().map(plugin_load_failure_json).collect(),
    }
}

pub(crate) fn build_runtime_plugin_state() -> Result<RuntimePluginState, Box<dyn std::error::Error>>
{
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load()?;
    build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
}

pub(crate) fn build_runtime_plugin_state_with_loader(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let plugin_manager = build_plugin_manager(cwd, loader, runtime_config);
    let plugin_registry = plugin_manager.plugin_registry()?;
    let plugin_hook_config =
        runtime_hook_config_from_plugin_hooks(plugin_registry.aggregated_hooks()?);
    let feature_config = runtime_config
        .feature_config()
        .clone()
        .with_hooks(runtime_config.hooks().merged(&plugin_hook_config));
    let (mcp_state, mut runtime_tools) = build_runtime_mcp_state(runtime_config)?;
    // Register the session_search tool. Its execution is intercepted by
    // ConversationRuntime::run_turn (routed to HistoryIndex), not handled by
    // CliToolExecutor — but the spec must be in the registry so the model
    // knows the tool exists and can call it.
    runtime_tools.push(RuntimeToolDefinition {
        name: "session_search".to_string(),
        description: Some(
            "Search the conversation history using full-text search. Use this \
             to recall specific past discussions, decisions, or file references \
             that may not be in the current context window."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Full-text search query. Supports FTS5 syntax."
                },
                "top_k": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 10).",
                    "default": 10
                }
            },
            "required": ["query"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["memory".to_string(), "conversation-history".to_string()],
    });
    // P0-1:注册 notebook_update 工具 — Anthropic《Effective Context
    // Engineering for AI Agents》明确推荐的 structured note-taking 模式。
    // LLM 通过此工具维护 NOTEBOOK.md(跨压缩持久化的工作记忆),记录:
    // - 当前任务的关键决策、约束、进度(<plan>)
    // - 已 dispatch 的子智能体注册表(<subagents>)
    // - 已尝试的方案及结论(<attempted>)
    // - 用户明确表达的偏好/约束(<preferences>)
    // - 关键文件引用(<key_files>)
    //
    // 执行由 ConversationRuntime::run_turn 拦截,委托
    // runtime::notebook::execute_notebook_update 处理(原子写 .claw/NOTEBOOK.md)。
    // 直击"AI 忘记已 dispatch 过子智能体导致重复调用"的问题。
    runtime_tools.push(RuntimeToolDefinition {
        name: "notebook_update".to_string(),
        description: Some(
            "Update the persistent working memory (NOTEBOOK.md). This memory \
             survives context compaction — use it to record key decisions, \
             subagent registry, attempted approaches, user preferences, and \
             key file references. CRITICAL: always record subagent dispatches \
             here so you do not re-dispatch the same task later. Modes: 'set' \
             (overwrite section) or 'append' (add a line). Sections: plan, \
             subagents, attempted, preferences, key_files."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["set", "append"],
                    "description": "Operation mode: 'set' overwrites the entire section; 'append' adds a single line."
                },
                "section": {
                    "type": "string",
                    "enum": ["plan", "subagents", "attempted", "preferences", "key_files"],
                    "description": "Target section name."
                },
                "content": {
                    "type": "string",
                    "description": "For 'set': full section content. For 'append': a single line to add."
                }
            },
            "required": ["mode", "section", "content"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["memory".to_string(), "notebook".to_string()],
    });
    // P2:注册 memory_update 工具 — MemGPT 模型主动管理 PersistentMemory 的
    // 融合。LLM 通过此工具主动更新长期核心记忆:
    // - Persona/Human/Tasks 块(下个会话进入 system 前缀;本会话前缀冻结
    //   以维持缓存命中)
    // - 语义记忆 entries 增删改(add_entry/replace_entry/remove_entry,
    //   本会话经语义召回立即可见)
    // 与 notebook_update(工作记忆 NOTEBOOK.md)互补。
    // 执行由 ConversationRuntime::run_turn 拦截,委托
    // ConversationRuntime::execute_memory_update 处理(需经
    // with_persistent_memory 注入 memory surface)。
    runtime_tools.push(RuntimeToolDefinition {
        name: "memory_update".to_string(),
        description: Some(
            "主动管理持久记忆（MemGPT 风格）：更新 Persona/Human/Tasks 块（跨会话进入 system 前缀），或增删改语义记忆 entries（本会话经语义召回可见）。块写入下个会话生效，entries 本会话生效。"
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "block": {"type": "string", "enum": ["persona", "human", "tasks"]},
                "content": {"type": "string"},
                "op": {"type": "string", "enum": ["add_entry", "replace_entry", "remove_entry"]},
                "pattern": {"type": "string"},
                "source": {"type": "string"}
            },
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["memory".to_string(), "persistent-memory".to_string()],
    });
    // 第1项:注册 plan_update 工具 — 让 LLM 推进 PlanArtifact 顺序状态机。
    // 长程任务按 step 状态机执行(而非一次线性跑完),LLM 完成一个 step 后
    // 调用 plan_update("done: <step_id>") 标记完成,Review 阶段据此判断
    // AllPassed / Replan。step_id 见 Active Plan 中每步的 1-based 编号。
    // 执行由 ConversationRuntime::run_turn 拦截,委托
    // planner::update_plan 处理(推进 StepStatus 状态机)。
    runtime_tools.push(RuntimeToolDefinition {
        name: "plan_update".to_string(),
        description: Some(
            "Update the current execution plan's step status (Plan/Execute/Review cycle). \
             Use this to mark a step done or failed, add a step, or trigger replan. \
             Actions: 'done: <step_id>' marks a step succeeded; 'fail: <step_id>' marks it failed; \
             'replan' resets failed steps to pending; 'add: <description>' appends a new step. \
             <step_id> is the 1-based step number shown in the Active Plan (e.g. 'done: 1' or 'done: step_1'). \
             Call this after each step completes so the review gate can verify progress."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "update": {
                    "type": "string",
                    "description": "Action string, e.g. 'done: 1', 'fail: 2', 'replan', or 'add: write unit tests'."
                }
            },
            "required": ["update"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["planning".to_string(), "memory".to_string()],
    });
    // P0:注册 recall_full 工具 — 从 ToolResultArchive 检索 microcompact
    // 摘要前的原始 tool result。
    //
    // 当 LLM 在上下文中看到 `[Read output summarized: 1234 chars → ...]` 时,
    // 无法判断原始内容是否仍需要。盲目重新调用 Read 会导致:
    // - 重复读同一文件(浪费 token 和时间)
    // - LLM 忘记之前已读过的内容(典型死循环模式)
    //
    // recall_full 让 LLM 主动按 tool_use_id 检索归档的原始输出,从而:
    // - 节省重新调用工具的开销
    // - 在归档存在时直接获取完整内容,无需重新 Read
    //
    // 执行由 ConversationRuntime::run_turn 拦截,委托
    // runtime::tool_result_archive::recall_tool_result 处理。
    // 归档由 microcompact_with_archiver 在摘要替换前自动写入。
    runtime_tools.push(RuntimeToolDefinition {
        name: "recall_full".to_string(),
        description: Some(
            "Retrieve the original (pre-summary) output of a tool result that \
             was summarized by microcompact. When you see a placeholder like \
             '[Read output summarized: 1234 chars → ...]' in the context and \
             need the full content, call this tool with the tool_use_id from \
             the corresponding tool_use block instead of re-invoking the \
             original tool. Modes: (1) pass {\"tool_use_id\": \"call_xxx\"} \
             to retrieve a specific archived result; (2) pass \
             {\"list_only\": true} to list all archived ids with previews."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tool_use_id": {
                    "type": "string",
                    "description": "The tool_use_id of the summarized tool result to retrieve. Required unless list_only=true."
                },
                "list_only": {
                    "type": "boolean",
                    "description": "If true, list all archived tool result ids with previews instead of retrieving a specific one. Default: false.",
                    "default": false
                }
            },
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["memory".to_string(), "tool-result-archive".to_string()],
    });
    // create_plan:复杂任务判定交给模型自主决定(2026-08-16)。
    // 框架不再用启发式规则(字符数阈值 + 关键词 substring)在 run_turn 入口
    // 自动创建 PlanArtifact —— 启发式规则存在漏判(短输入多文件任务)与误判
    // (长输入但仅需解释/否定语境)。改为模型在执行过程中判断任务足够复杂时
    // 主动调用本工具创建计划,框架随后进入 Plan/Execute/Review 循环。
    // 执行由 ConversationRuntime::run_turn 拦截,调用
    // execute_create_plan 生成 steps(LLM-driven,失败回退启发式 decompose_task)。
    runtime_tools.push(RuntimeToolDefinition {
        name: "create_plan".to_string(),
        description: Some(
            "Create a structured execution plan for a task that is complex, \
             multi-step, or spans multiple files. Use your judgment: call this \
             when the task would benefit from an ordered, staged sequence of \
             changes with verification between phases (e.g. refactoring across \
             several modules, a multi-file feature, or a task where you'd otherwise \
             iterate many times). Do NOT call for trivial one-file edits or \
             read-only questions. Returns a plan_id; the plan is injected into \
             your context and used to guide execution with verification between \
             steps. Optional: pass a plan_description describing what to plan \
             (defaults to the latest user input)."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "plan_description": {
                    "type": "string",
                    "description": "Optional description of the task to plan. Defaults to the latest user input if omitted."
                }
            },
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["orchestration".to_string(), "planning".to_string()],
    });
    // Epic 2:注册 dispatch_subagent / check_subagent 工具 — subagent-as-tool 路由。
    // 主 agent 通过 dispatch_subagent 派发子 agent(独立 LLM 请求 + 独立 prompt cache,
    // 不污染主 agent 缓存,详见 plan.md §5.2)。check_subagent 查询状态/结果。
    // 执行由 ConversationRuntime::run_turn 拦截,路由到
    // execute_dispatch_subagent / execute_check_subagent。
    // 详见 plan.md §9.2 Epic 2。
    runtime_tools.push(RuntimeToolDefinition {
        name: "dispatch_subagent".to_string(),
        description: Some(
            "Dispatch a sub-task to a sub-agent. The sub-agent runs independently \
             with its own LLM request and prompt cache, so the main agent's cache \
             prefix is not polluted. Use this for parallelizable work, isolated \
             refactors, or verification tasks. Returns the subagent_id immediately; \
             use check_subagent to poll for completion."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Human-readable name for the sub-agent (e.g. 'refactor-auth', 'test-runner')."
                },
                "task": {
                    "type": "string",
                    "description": "The task description / prompt to send to the sub-agent."
                },
                "mode": {
                    "type": "string",
                    "enum": ["fork", "teammate", "worktree"],
                    "description": "Coordination mode: 'fork' (shared workdir, parallel), 'teammate' (shared TaskRegistry), 'worktree' (isolated git worktree).",
                    "default": "fork"
                }
            },
            "required": ["name", "task"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["orchestration".to_string(), "subagent".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "check_subagent".to_string(),
        description: Some(
            "Check the status of a previously dispatched sub-agent. Returns the \
             current status (created/running/completed/failed/cancelled) and, if \
             terminal, the result payload. Completed/failed results also emit a \
             SubagentResult lane event for observability."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "subagent_id": {
                    "type": "string",
                    "description": "The subagent_id returned by dispatch_subagent."
                }
            },
            "required": ["subagent_id"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["orchestration".to_string(), "subagent".to_string()],
    });
    // v3:注册 spawn_parallel_subagents 工具 — 批量并行派发(走 DagScheduler 真并行)。
    // 与 dispatch_subagent(单个 + retry loop)互补:适用于独立的可并行任务。
    // 执行由 ConversationRuntime::run_turn 拦截,路由到 execute_spawn_parallel_subagents,
    // 内部调用 spawn_parallel_via_dag_with_fail_fast(tasks, fail_fast)。
    runtime_tools.push(RuntimeToolDefinition {
        name: "spawn_parallel_subagents".to_string(),
        description: Some(
            "Dispatch multiple sub-agents in parallel using a DAG scheduler. \
             All tasks run concurrently in independent tokio tasks (true parallelism, \
             not sequential). Each sub-agent has its own LLM request and prompt cache. \
             Use this for independent parallelizable work (e.g. analyzing multiple \
             files, running multiple test suites). For dependent tasks, use dag_run \
             instead. Returns one result per task."
                .to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "List of sub-agent tasks to dispatch in parallel.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Human-readable name for the sub-agent (e.g. 'analyze-auth', 'test-runner')."
                            },
                            "task": {
                                "type": "string",
                                "description": "The task description / prompt to send to the sub-agent."
                            },
                            "model": {
                                "type": "string",
                                "description": "Model name for the sub-agent (e.g. 'deepseek-v4-flash', 'deepseek-v4-pro'). Required — capability check uses this to gate Budget vs Flagship tasks."
                            },
                            "mode": {
                                "type": "string",
                                "enum": ["fork", "teammate", "worktree"],
                                "description": "Coordination mode: 'fork' (shared workdir, parallel), 'teammate' (shared TaskRegistry), 'worktree' (isolated git worktree).",
                                "default": "fork"
                            },
                            "complexity": {
                                "type": "string",
                                "enum": ["simple", "diagnostic", "architectural"],
                                "description": "Task complexity. Budget-tier models (haiku/mini/nano/flash) cannot handle 'diagnostic' or 'architectural'.",
                                "default": "simple"
                            }
                        },
                        "required": ["name", "task", "model"]
                    },
                    "minItems": 1
                },
                "fail_fast": {
                    "type": "string",
                    "enum": ["on", "off"],
                    "description": "Failure propagation: 'on' (default) cancels all siblings on any failure; 'off' tolerates failures and returns partial results.",
                    "default": "on"
                }
            },
            "required": ["tasks"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["orchestration".to_string(), "subagent".to_string(), "parallel".to_string()],
    });
    // Phase 4-A:DecisionLog — register log_decision and search_past_decisions tools.
    // Logs repair decisions (problem signature, root cause hypothesis, applied solution,
    // verification result) into a SQLite + FTS5-backed decision log. The runtime intercepts
    // calls and routes them to DecisionLog::log_decision / search_decisions.
    runtime_tools.push(RuntimeToolDefinition {
        name: "log_decision".to_string(),
        description: Some(
            "Record a software repair decision into the persistent decision log (.claw/decision_log.db, survives across sessions). MUST call after applying a non-trivial fix AND verifying it works (tests pass / user confirms / command succeeds). Stores problem_signature, root_cause_hypothesis, applied_solution, affected_files, verification_result. Also call with verification_result='Refuted' for failed attempts — negative experience is equally valuable. Skip for trivial changes (typo/formatting/rename). Future sessions search this log to avoid repeating diagnosis work.".to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Current session identifier."
                },
                "problem_signature": {
                    "type": "string",
                    "description": "Concise description of the problem encountered (e.g. 'null pointer dereference in auth_handler')."
                },
                "root_cause_hypothesis": {
                    "type": "string",
                    "description": "Your hypothesis about what caused the problem."
                },
                "applied_solution": {
                    "type": "string",
                    "description": "What you did to fix it."
                },
                "affected_files": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of file paths modified by the fix."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional tags for categorization (e.g. ['null-pointer', 'auth'])."
                },
                "verification_result": {
                    "type": "string",
                    "enum": ["Confirmed", "Refuted", "Partial", "Pending"],
                    "description": "Verification status. Use 'Confirmed' only when fix is verified; 'Pending' otherwise.",
                    "default": "Pending"
                },
                "verification_evidence": {
                    "type": "string",
                    "description": "Optional evidence for the verification result."
                }
            },
            "required": ["session_id", "problem_signature", "root_cause_hypothesis", "applied_solution"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["decision-log".to_string(), "memory".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "search_past_decisions".to_string(),
        description: Some(
            "Search past software repair decisions using full-text search (BM25-ranked). SHOULD call BEFORE attempting any non-trivial fix — pass a short problem signature as query. Returns matching decisions with problem_signature, root_cause_hypothesis, applied_solution, affected_files, verification_result, and success_rate. If a match exists with high success_rate, reuse the solution instead of rediscovering it; if matches show 'Refuted' solutions, avoid those approaches. Saves time and avoids repeating past mistakes.".to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Full-text search query for finding relevant past decisions."
                },
                "top_k": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 10).",
                    "default": 10
                }
            },
            "required": ["query"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["decision-log".to_string(), "memory".to_string()],
    });
    // Phase 4-A P1-4: verify_decision — closes the success_rate learning loop.
    //
    // 之前 log_decision 只在写入时设置一次 verification_result,
    // 之后即使 LLM 在后续会话中再次遇到同问题并验证(成功/失败),
    // success_rate 也不会更新,search_past_decisions 永远显示 0% —
    // 学习环未闭合。verify_decision 让 LLM 在重新验证历史决策后调用,
    // 原子地更新 verify_count / success_rate / verified_at_ms。
    //
    // 公式(计划 §4.4):
    //   Confirmed: rate = (rate * count + 1.0) / (count + 1)
    //   Partial:   rate = (rate * count + 0.5) / (count + 1)
    //   Refuted:   rate = (rate * count + 0.0) / (count + 1)
    //   Pending:   不更新统计,只重置 verification_result(用于撤销误验证)
    runtime_tools.push(RuntimeToolDefinition {
        name: "verify_decision".to_string(),
        description: Some(
            "Verify a past decision after re-encountering the same problem in a new session, closing the success_rate learning loop. Call AFTER you have either confirmed (solution worked again), refuted (solution failed this time), or partially verified (solution worked but needed tweaks) a decision returned by search_past_decisions. Updates verify_count, success_rate, verified_at_ms, verification_result, and verification_evidence atomically (BEGIN IMMEDIATE transaction). Use 'Pending' to revoke a previously incorrect verification without altering statistics. Higher success_rate decisions will be ranked higher in future search_past_decisions results, so accurate verification improves future diagnosis quality.".to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "decision_id": {
                    "type": "integer",
                    "description": "The id of the decision to verify, as returned by search_past_decisions (the 'id=N' field)."
                },
                "verification_result": {
                    "type": "string",
                    "enum": ["Confirmed", "Refuted", "Partial", "Pending"],
                    "description": "Verification outcome. Confirmed=solution worked as-is; Refuted=solution failed; Partial=worked with modifications; Pending=revoke previous verification without changing stats."
                },
                "verification_evidence": {
                    "type": "string",
                    "description": "Optional evidence: test output, error message, user feedback, command exit code, etc. Stored verbatim and surfaced in future search results."
                }
            },
            "required": ["decision_id", "verification_result"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["decision-log".to_string(), "memory".to_string()],
    });
    // Epic 4 延续:bus_list / bus_send / bus_watch — Session Bus 统一路由层工具。
    // 让 AI 自主调用多会话总线能力(查看 peer / 发消息 / 订阅消息流)。
    // 执行由 ConversationRuntime::run_turn 拦截,路由到 execute_bus_list /
    // execute_bus_send / execute_bus_watch(conversation.rs)。
    runtime_tools.push(RuntimeToolDefinition {
        name: "bus_list".to_string(),
        description: Some(
            "List all peer sessions currently visible on the Session Bus (main session, running sub-agents, IDE panels, IM channels) with their kind, status (idle/streaming/blocked/done) and unread count. Read-only. Call this before coordinating multiple sessions (e.g. after dispatching sub-agents) to know what is running and reachable.".to_string(),
        ),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["session-bus".to_string(), "orchestration".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "bus_send".to_string(),
        description: Some(
            "Send a message to another session on the Session Bus. Target must be a peer session_id from bus_list (e.g. a subagent_id) or '*' to broadcast. If the target is a sub-agent, the message is delivered as a steering command (consumed on its next tool-call iteration). Otherwise it is delivered as a message into the target's unread queue. Permission is governed by session_bus.allow (deny by default). Returns the number of peers that received it.".to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Target peer session_id (from bus_list) or '*' for broadcast."
                },
                "text": {
                    "type": "string",
                    "description": "Message content to send."
                }
            },
            "required": ["to", "text"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["session-bus".to_string(), "orchestration".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "bus_watch".to_string(),
        description: Some(
            "Subscribe to another peer session's message stream: when that peer receives a message, a mirror is queued into this session's unread list (visible in the output view). Set unwatch=true to unsubscribe (idempotent). Useful to track a sub-agent's ongoing output. Target must be a peer session_id from bus_list and cannot be this session.".to_string(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Peer session_id to watch/unwatch (from bus_list)."
                },
                "unwatch": {
                    "type": "boolean",
                    "description": "true = unsubscribe, false/absent = subscribe.",
                    "default": false
                }
            },
            "required": ["target"]
        }),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["session-bus".to_string(), "orchestration".to_string()],
    });
    // Phase 4-B: ProjectTopology + DomainTools registration.
    runtime_tools.push(RuntimeToolDefinition {
        name: "query_project_graph".to_string(),
        description: Some("Query the cargo workspace crate dependency graph. Returns all crates, dependencies, source paths, and reverse-dependency info.".to_string()),
        input_schema: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["topology".to_string(), "analysis".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "find_boundary_crossings".to_string(),
        description: Some("Find cross-crate symbol/call-site boundaries. Optional query to filter by crate name. If ProjectTopology is building, do NOT retry - use read/grep instead.".to_string()),
        input_schema: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}, "additionalProperties": false}),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["topology".to_string(), "analysis".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "get_symbol_info".to_string(),
        description: Some("Look up a symbol in the project topology index. Returns definition location, call sites, and crate membership. Best-effort; use grep_search for exhaustive results.".to_string()),
        input_schema: serde_json::json!({"type": "object", "properties": {"symbol": {"type": "string"}}, "required": ["symbol"], "additionalProperties": false}),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["topology".to_string(), "analysis".to_string()],
    });
    // DomainTools: stateless algorithm tools.
    runtime_tools.push(RuntimeToolDefinition {
        name: "refactor_algorithm_topo".to_string(),
        description: Some("Suggestion-mode refactoring: returns a list of suggested edits (file + line + old/new signature) for renaming a symbol. Does NOT modify any files. Review suggestions then use edit_file to apply.".to_string()),
        input_schema: serde_json::json!({"type": "object", "properties": {"target_symbol": {"type": "string"}, "new_name": {"type": "string"}, "reason": {"type": "string"}}, "required": ["target_symbol"], "additionalProperties": false}),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["topology".to_string(), "refactor".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "benchmark_compare".to_string(),
        description: Some("Run a command multiple times and report timing statistics (avg, median, min, max, stddev). Supports warmup runs, configurable sample size, and per-sample exit code tracking.".to_string()),
        input_schema: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}, "timeout_seconds": {"type": "integer", "default": 60}, "sample_size": {"type": "integer", "default": 20}, "warmup_runs": {"type": "integer", "default": 2}}, "required": ["command"], "additionalProperties": false}),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["benchmark".to_string(), "analysis".to_string()],
    });

    // Phase 4-C: register rollback_transaction + transaction_status tools.
    runtime_tools.push(RuntimeToolDefinition {
        name: "rollback_transaction".to_string(),
        description: Some("Rollback all file modifications made in the current turn. Restores the working tree to its pre-turn state using git stash or file snapshots. Use when you realize the current approach is wrong and want to start fresh.".to_string()),
        input_schema: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        required_permission: PermissionMode::WorkspaceWrite,
        domain_tags: vec!["transaction".to_string(), "vcs".to_string()],
    });
    runtime_tools.push(RuntimeToolDefinition {
        name: "transaction_status".to_string(),
        description: Some("Check the current transaction status: whether a snapshot is active, which files have been modified this turn, and whether the environment supports rollback (git repo / detached HEAD / non-git).".to_string()),
        input_schema: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        required_permission: PermissionMode::ReadOnly,
        domain_tags: vec!["transaction".to_string(), "vcs".to_string()],
    });

    let tool_registry = GlobalToolRegistry::with_plugin_tools(plugin_registry.aggregated_tools()?)?
        .with_runtime_tools(runtime_tools)?;
    Ok(RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        mcp_state,
    })
}

pub(crate) fn build_plugin_manager(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> PluginManager {
    let plugin_settings = runtime_config.plugins();
    let mut plugin_config = PluginManagerConfig::new(loader.config_home().to_path_buf());
    plugin_config.enabled_plugins = plugin_settings.enabled_plugins().clone();
    plugin_config.external_dirs = plugin_settings
        .external_directories()
        .iter()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path))
        .collect();
    plugin_config.install_root = plugin_settings
        .install_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.registry_path = plugin_settings
        .registry_path()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.bundled_root = plugin_settings
        .bundled_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    PluginManager::new(plugin_config)
}

pub(crate) fn resolve_plugin_path(cwd: &Path, config_home: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if value.starts_with('.') {
        cwd.join(path)
    } else {
        config_home.join(path)
    }
}

pub(crate) fn runtime_hook_config_from_plugin_hooks(
    hooks: PluginHooks,
) -> runtime::RuntimeHookConfig {
    runtime::RuntimeHookConfig::new(
        hooks.pre_tool_use,
        hooks.post_tool_use,
        hooks.post_tool_use_failure,
    )
}
