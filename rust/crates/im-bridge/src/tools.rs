//! Tool handlers for the IM bridge agent.
//!
//! The IM bridge delegates **all** tool execution to the shared `tools` crate
//! (`::tools::execute_tool`), which is the exact same implementation the CLI
//! uses (bash, file read/write/edit, WebSearch/WebFetch, TodoWrite, Skill,
//! Agent subagents, Task/Worker/Team/Cron/DAG, LSP, MCP, ...). This gives the
//! Feishu/WeCom channel the full claw capability set instead of a handful of
//! duplicated, reduced handlers.
//!
//! Each advertised tool is registered as a thin wrapper: parse the JSON input
//! and forward to `::tools::execute_tool(name, &value)`. Permission gating is
//! done by the runtime `PermissionPolicy` (see `main.rs`), matching the CLI.

use runtime::{StaticToolExecutor, ToolError};

/// Tools that must NOT be exposed to the LLM via IM.
///
/// `AskUserQuestion` reads stdin for an interactive answer. Inside the IM
/// bridge there is no interactive terminal, so calling it would block the
/// agent thread forever. It is excluded both from the advertised tool list
/// (`api_adapter::BridgeApiClient::stream`) and from the executor registry
/// (here), so "advertised == executable" always holds.
pub const IM_BRIDGE_EXCLUDED_TOOLS: &[&str] = &["AskUserQuestion"];

/// Register all default tool handlers on the given executor.
///
/// Uses `std::mem::take` because `StaticToolExecutor::register` consumes
/// `self` (builder pattern), but our caller passes `&mut`. Since
/// `StaticToolExecutor: Default`, we can temporarily take ownership and
/// write the result back.
pub fn register_default_tools(executor: &mut StaticToolExecutor) {
    let owned = std::mem::take(executor);
    let mut built = owned;
    for spec in ::tools::mvp_tool_specs() {
        if IM_BRIDGE_EXCLUDED_TOOLS.contains(&spec.name) {
            continue;
        }
        let tool_name = spec.name.to_string();
        built = built.register(spec.name, move |input: &str| {
            execute_tool(&tool_name, input)
        });
    }
    *executor = built;
}

/// Execute a single tool through the shared `tools` crate.
///
/// The `tools` crate implements every tool as a self-contained synchronous
/// call (it spawns dedicated OS threads + `current_thread` runtimes where it
/// needs tokio, e.g. MCP calls, subagents), so it is safe to invoke from the
/// claw-shell agent thread — the same context the CLI uses.
fn execute_tool(name: &str, input: &str) -> Result<String, ToolError> {
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| ToolError::new(format!("invalid JSON input for {name}: {e}")))?;
    ::tools::execute_tool(name, &value).map_err(ToolError::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::ToolExecutor;

    fn fresh_executor() -> StaticToolExecutor {
        let mut executor = StaticToolExecutor::new();
        register_default_tools(&mut executor);
        executor
    }

    #[test]
    fn every_advertised_tool_is_registered() {
        let mut executor = fresh_executor();
        for spec in ::tools::mvp_tool_specs() {
            if IM_BRIDGE_EXCLUDED_TOOLS.contains(&spec.name) {
                continue;
            }
            // 空输入 `{}` 会触发参数校验错误,但绝不能是 "unknown tool"
            // (那意味着 handler 未注册,广告了却不可执行)。
            let result = executor.execute(spec.name, "{}");
            assert!(
                !result
                    .as_ref()
                    .is_err_and(|e| e.to_string().contains("unknown tool")),
                "tool {} must be registered (got: {:?})",
                spec.name,
                result.err()
            );
        }
    }

    #[test]
    fn excluded_tools_are_not_registered() {
        let mut executor = fresh_executor();
        for name in IM_BRIDGE_EXCLUDED_TOOLS {
            let result = executor.execute(name, "{}");
            assert!(
                result
                    .as_ref()
                    .is_err_and(|e| e.to_string().contains("unknown tool")),
                "excluded tool {name} must not be registered"
            );
        }
    }

    #[test]
    fn delegates_to_shared_tools_crate() {
        let mut executor = fresh_executor();
        // TodoWrite 是纯 JSON 工具,验证端到端委托到 ::tools::execute_tool。
        let input = r#"{"todos": [{"content": "x", "activeForm": "y", "status": "pending"}]}"#;
        let result = executor.execute("TodoWrite", input);
        assert!(result.is_ok(), "TodoWrite failed: {:?}", result.err());
        let json: serde_json::Value =
            serde_json::from_str(result.as_deref().unwrap_or("{}")).expect("valid JSON");
        assert_eq!(json["newTodos"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn invalid_json_returns_clean_error() {
        let mut executor = fresh_executor();
        let result = executor.execute("read_file", "not json");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid JSON input"));
    }
}
