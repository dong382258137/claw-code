//! web-bridge 的工具注册:与 CLI/im-bridge 同源的 `tools` crate 执行器。
//!
//! 所有工具执行委托给 `::tools::execute_tool`(bash、文件读写编辑、搜索、
//! WebSearch/WebFetch、TodoWrite、Skill、Subagent、Task/Worker/Team/Cron/DAG、
//! LSP、MCP ...),网页通道拿到的是与 CLI 相同的完整能力集,而非裁剪后的子集。

use runtime::{StaticToolExecutor, ToolError};

/// 网页通道不应暴露给模型的工具:AskUserQuestion 读 stdin,网页侧无交互
/// 终端,调用会永久阻塞 agent 线程,执行器剔除(增强提示不再从 registry
/// 广告工具清单,协议白名单见 `protocol::PROTOCOL_SUFFIX`)。
pub const WEB_BRIDGE_EXCLUDED_TOOLS: &[&str] = &["AskUserQuestion"];

/// 把所有默认工具注册到 executor(排除网页不适用的工具)。
pub fn register_default_tools(executor: &mut StaticToolExecutor) {
    let owned = std::mem::take(executor);
    let mut built = owned;
    for spec in ::tools::mvp_tool_specs() {
        if WEB_BRIDGE_EXCLUDED_TOOLS.contains(&spec.name) {
            continue;
        }
        let tool_name = spec.name.to_string();
        built = built.register(spec.name, move |input: &str| {
            execute_tool(&tool_name, input)
        });
    }
    *executor = built;
}

/// 执行单个工具(与 im-bridge 相同的委托方式)。
fn execute_tool(name: &str, input: &str) -> Result<String, ToolError> {
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| ToolError::new(format!("invalid JSON input for {name}: {e}")))?;
    ::tools::execute_tool(name, &value).map_err(ToolError::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::ToolExecutor;

    #[test]
    fn every_advertised_excluded_tool_is_trimmed_from_registry() {
        // 广告列表(WebApiClient 用 registry.definitions)与执行器都剔除
        // AskUserQuestion,保证"广告 = 可执行"。
        let mut executor = StaticToolExecutor::new();
        register_default_tools(&mut executor);
        for spec in ::tools::mvp_tool_specs() {
            if WEB_BRIDGE_EXCLUDED_TOOLS.contains(&spec.name) {
                // 未知工具名也必须是"未注册",而不能是"执行时阻塞"
                continue;
            }
            let result = executor.execute(spec.name, "{}");
            assert!(
                !result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.to_string().contains("unknown tool")),
                "广告了却不可执行: {}",
                spec.name
            );
        }
    }
}
