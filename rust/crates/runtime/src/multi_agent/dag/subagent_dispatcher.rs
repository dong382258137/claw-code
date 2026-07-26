//! SubagentDispatcher:提取自 ConversationRuntime::run_subagent_turn,
//! 为 CoordinatorExecutor 提供异步、Send + Sync 的子 agent 执行器。
//!
//! `ConversationRuntime::run_subagent_turn` 是 `&mut self` 同步方法,无法直接
//! 注入到 `SubagentRunner` 闭包(需要 `Send + Sync` + 异步)。本模块把同样的
//! LLM 调用逻辑提取为独立结构,持有 `Arc<Mutex<Box<dyn ApiClient + Send>>>`
//! 共享状态,可在多线程异步上下文中安全调用。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::conversation::{ApiClient, ApiRequest};
use crate::prompt::SystemPromptSplit;
use crate::session::{ContentBlock, ConversationMessage, MessageRole};

/// 共享状态的子 agent 调度器。
///
/// 持有 `Arc<Mutex<Box<dyn ApiClient + Send>>>` 和 `workspace_root`,
/// 可安全跨线程共享给 `SubagentRunner` 闭包。
#[derive(Clone)]
pub struct SubagentDispatcher {
    api_client: Arc<Mutex<Box<dyn ApiClient + Send>>>,
    workspace_root: PathBuf,
}

impl SubagentDispatcher {
    pub fn new(
        api_client: Arc<Mutex<Box<dyn ApiClient + Send>>>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            api_client,
            workspace_root,
        }
    }

    /// 执行子 agent turn(异步包装同步 stream 调用)。
    ///
    /// 逻辑与 ConversationRuntime::run_subagent_turn 一致:
    /// 1. 构造 system_prompt + user message
    /// 2. 调用 api_client.stream(在 spawn_blocking 中执行)
    /// 3. 解析 assistant response,提取 text
    /// 4. 写到 .claw/subagents/{id}.md
    /// 5. 返回 result_ref 路径
    pub async fn dispatch(&self, subagent_id: String, task: String) -> Result<String, String> {
        // SubagentRunner 签名只有 (id, task),用 subagent_id 作为 name。
        let name = subagent_id.clone();
        Self::dispatch_impl(
            &self.api_client,
            &self.workspace_root,
            &subagent_id,
            &name,
            &task,
        )
        .await
    }

    async fn dispatch_impl(
        api_client: &Arc<Mutex<Box<dyn ApiClient + Send>>>,
        workspace_root: &PathBuf,
        subagent_id: &str,
        name: &str,
        task: &str,
    ) -> Result<String, String> {
        // 构造请求(与 run_subagent_turn 完全一致)
        let subagent_system_prompt = SystemPromptSplit::from_sections(vec![format!(
            "# Subagent: {name} ({subagent_id})\n\
             \n\
             你是一个子智能体,由主智能体派发执行独立任务。\n\
             \n\
             ## 任务\n\
             {task}\n\
             \n\
             ## 约束\n\
             - 你拥有独立的工作上下文,不共享主智能体的对话历史\n\
             - 你的响应将被写入文件,主智能体会后续读取\n\
             - 请提供完整、自包含的分析结果\n\
             - 不需要调用工具,直接给出你的分析和结论\n\
             \n\
             ## 输出格式\n\
             请直接输出你的分析结果,使用 Markdown 格式。包含:\n\
             1. 任务理解(简要复述)\n\
             2. 分析过程\n\
             3. 关键发现\n\
             4. 结论和建议"
        )]);

        let user_message = ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text {
                text: format!("请执行以下任务:\n\n{task}"),
            }],
            usage: None,
        };

        let request = ApiRequest {
            system_prompt: subagent_system_prompt,
            messages: vec![user_message],
        };

        // 同步 stream 调用放在 spawn_blocking 中,避免阻塞 async runtime。
        // ApiClient::stream 是 &mut self 同步方法,需要先 lock 再调。
        let api_client = api_client.clone();
        let events = tokio::task::spawn_blocking(move || {
            let mut client = api_client
                .lock()
                .map_err(|e| format!("api_client lock poisoned: {e}"))?;
            client
                .stream(request)
                .map_err(|e| format!("subagent LLM request failed: {e}"))
        })
        .await
        .map_err(|e| format!("spawn_blocking join failed: {e}"))??;

        // 解析 assistant response(与 run_subagent_turn 一致)
        let (assistant_message, _usage, _cache_events) =
            crate::conversation::build_assistant_message(events)
                .map_err(|e| format!("subagent response parsing failed: {e}"))?;

        let mut text_content = String::new();
        for block in &assistant_message.blocks {
            if let ContentBlock::Text { text } = block {
                text_content.push_str(text);
                text_content.push('\n');
            }
        }
        if text_content.trim().is_empty() {
            return Err("subagent produced no text content".to_string());
        }

        // 写文件(与 run_subagent_turn 一致)
        let subagents_dir = workspace_root.join(".claw").join("subagents");
        std::fs::create_dir_all(&subagents_dir)
            .map_err(|e| format!("failed to create subagents dir: {e}"))?;
        let result_path = subagents_dir.join(format!("{subagent_id}.md"));
        let tmp_path = subagents_dir.join(format!("{subagent_id}.md.tmp"));

        let file_content = format!(
            "# Subagent Result: {name} ({subagent_id})\n\
             \n\
             **Task:** {task}\n\
             **Timestamp:** {}\n\
             \n\
             ---\n\
             \n\
             {text_content}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| format!("{} (unix epoch)", d.as_secs()))
                .unwrap_or_else(|_| "unknown".to_string())
        );

        std::fs::write(&tmp_path, &file_content)
            .map_err(|e| format!("failed to write subagent result tmp file: {e}"))?;
        std::fs::rename(&tmp_path, &result_path)
            .map_err(|e| format!("failed to rename subagent result file: {e}"))?;

        Ok(format!(".claw/subagents/{subagent_id}.md"))
    }
}

// 静态断言:确保 SubagentDispatcher 是 Send + Sync(被 SubagentRunner 闭包捕获需要)。
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SubagentDispatcher>();
};
