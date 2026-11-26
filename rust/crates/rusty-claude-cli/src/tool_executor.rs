//! 工具执行器：从原 `tool_display.rs` 抽出的执行逻辑。
//!
//! 终端卡片渲染、输出截断与 ANSI 格式化已随 CLI 界面层删除；
//! 本模块只保留 `runtime::ToolExecutor` 实现与工具调度本身，
//! 状态事件经 `runtime::StatusEmitter` 输出给 Web SSE。

use std::sync::{Arc, Mutex};

use runtime::{ToolError, ToolExecutor};
use tools::GlobalToolRegistry;

use crate::plugin_state::RuntimeMcpState;
use crate::AllowedToolSet;

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct ToolSearchRequest {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct McpToolRequest {
    #[serde(rename = "qualifiedName")]
    qualified_name: Option<String>,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct ListMcpResourcesRequest {
    server: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub(crate) struct ReadMcpResourceRequest {
    server: String,
    uri: String,
}

pub(crate) struct CliToolExecutor {
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    /// 状态事件出口（Web SSE）：工具执行完成时据此补发 ToolResult 事件，
    /// 界面才能闭合工具卡片。
    status_emitter: Option<runtime::StatusEmitter>,
    /// Phase 4 认知外骨骼：共享 ProjectTopology 实例，用于 refactor_algorithm_topo。
    /// 与 ConversationRuntime 共享同一个 Arc，避免重复构建。
    topology: Option<Arc<runtime::project_topology::ProjectTopology>>,
}

impl CliToolExecutor {
    pub(crate) fn new(
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            allowed_tools,
            tool_registry,
            mcp_state,
            status_emitter: None,
            topology: None,
        }
    }

    /// Phase 4：注入 ProjectTopology 实例，启用 refactor_algorithm_topo 工具。
    pub(crate) fn with_project_topology(
        mut self,
        topology: Option<Arc<runtime::project_topology::ProjectTopology>>,
    ) -> Self {
        self.topology = topology;
        self
    }

    pub(crate) fn set_status_emitter(&mut self, emitter: runtime::StatusEmitter) {
        self.status_emitter = Some(emitter);
    }

    /// 注入本轮取消令牌：bash 轮询循环据此在 Web「停止」后
    /// 立即终止子进程树（不再是进程级全局标志）。
    pub(crate) fn set_turn_cancel(
        &mut self,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.tool_registry.set_turn_cancel(cancel);
    }

    /// S3-M2:注入会话级进程登记表，bash/后台命令 spawn 后据此登记。
    pub(crate) fn set_session_processes(
        &mut self,
        processes: std::sync::Arc<runtime::process_registry::SessionProcesses>,
    ) {
        self.tool_registry.set_session_processes(processes);
    }

    pub(crate) fn clear_status_emitter(&mut self) {
        self.status_emitter = None;
    }

    fn execute_search_tool(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        serde_json::to_string_pretty(&self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        ))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    /// Phase 4：refactor_algorithm_topo 工具执行入口。
    ///
    /// 解析 JSON 输入后调用 runtime::domain_algorithm 版本，
    /// 消除双实现分歧。如果没有 topology 实例，从 cwd 临时构造一个。
    fn execute_refactor_algorithm_topo(&self, input: &str) -> Result<String, ToolError> {
        let parsed: serde_json::Value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let target_symbol = parsed
            .get("target_symbol")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("missing 'target_symbol' field".to_string()))?;
        let new_name = parsed.get("new_name").and_then(|v| v.as_str());
        let reason = parsed.get("reason").and_then(|v| v.as_str());

        // 如果有共享的 topology 实例，直接使用；否则从 cwd 临时构造。
        if let Some(topo) = &self.topology {
            runtime::domain_algorithm::refactor_algorithm_topo(
                topo,
                target_symbol,
                new_name,
                reason,
            )
            .map_err(ToolError::new)
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let topo = runtime::project_topology::ProjectTopology::new(cwd);
            runtime::domain_algorithm::refactor_algorithm_topo(
                &topo,
                target_symbol,
                new_name,
                reason,
            )
            .map_err(ToolError::new)
        }
    }

    /// Phase 4：benchmark_compare 工具执行入口。
    ///
    /// 解析 JSON 输入后调用 runtime::domain_algorithm 版本（带真正的 timeout + kill）。
    fn execute_benchmark_compare(&self, input: &str) -> Result<String, ToolError> {
        let parsed: serde_json::Value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let command = parsed
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("missing 'command' field".to_string()))?;
        let timeout_seconds = parsed
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(60);
        let sample_size = parsed
            .get("sample_size")
            .and_then(|v| v.as_u64())
            .unwrap_or(20) as usize;
        let warmup_runs = parsed
            .get("warmup_runs")
            .and_then(|v| v.as_u64())
            .unwrap_or(3) as usize;
        let cwd = std::env::current_dir().ok();
        runtime::domain_algorithm::benchmark_compare(
            command,
            cwd.as_deref(),
            timeout_seconds,
            sample_size,
            warmup_runs,
        )
        .map_err(ToolError::new)
    }

    fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = tools::parse_tool_call_input(tool_name, input).map_err(ToolError::new)?;
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if tool_name == "refactor_algorithm_topo" {
            self.execute_refactor_algorithm_topo(input)
        } else if tool_name == "benchmark_compare" {
            self.execute_benchmark_compare(input)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute(tool_name, &value)
                .map_err(ToolError::new)
        };
        match result {
            Ok(output) => {
                // Emit ToolResult event for Web SSE consumption.
                if let Some(emitter) = &self.status_emitter {
                    emitter(runtime::StatusEvent::ToolResult {
                        id: String::new(),
                        name: tool_name.to_string(),
                        output: output.clone(),
                        is_error: false,
                    });
                }
                Ok(output)
            }
            Err(error) => {
                // Emit ToolResult error event for Web SSE consumption.
                if let Some(emitter) = &self.status_emitter {
                    emitter(runtime::StatusEvent::ToolResult {
                        id: String::new(),
                        name: tool_name.to_string(),
                        output: error.to_string(),
                        is_error: true,
                    });
                }
                Err(error)
            }
        }
    }
}
