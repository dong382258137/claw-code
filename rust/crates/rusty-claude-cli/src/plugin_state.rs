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
    manager: McpServerManager,
    pending_servers: Vec<String>,
    degraded_report: Option<runtime::McpDegradedReport>,
}

impl RuntimeMcpState {
    pub(crate) fn new(
        runtime_config: &runtime::RuntimeConfig,
    ) -> Result<Option<(Self, runtime::McpToolDiscoveryReport)>, Box<dyn std::error::Error>> {
        let mut manager = McpServerManager::from_runtime_config(runtime_config);
        if manager.server_names().is_empty() && manager.unsupported_servers().is_empty() {
            return Ok(None);
        }

        let runtime = tokio::runtime::Runtime::new()?;
        let discovery = runtime.block_on(manager.discover_tools_best_effort());
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

        Ok(Some((
            Self {
                runtime,
                manager,
                pending_servers,
                degraded_report,
            },
            discovery,
        )))
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.block_on(self.manager.shutdown())?;
        Ok(())
    }

    pub(crate) fn pending_servers(&self) -> Option<Vec<String>> {
        (!self.pending_servers.is_empty()).then(|| self.pending_servers.clone())
    }

    pub(crate) fn degraded_report(&self) -> Option<runtime::McpDegradedReport> {
        self.degraded_report.clone()
    }

    pub(crate) fn server_names(&self) -> Vec<String> {
        self.manager.server_names()
    }

    pub(crate) fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<String, ToolError> {
        let response = self
            .runtime
            .block_on(self.manager.call_tool(qualified_tool_name, arguments))
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
            .block_on(self.manager.list_resources(server_name))
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
            match self
                .runtime
                .block_on(self.manager.list_resources(&server_name))
            {
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
            .block_on(self.manager.read_resource(server_name, uri))
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
    }
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
