//! SubagentDispatcher:提取自 ConversationRuntime::run_subagent_turn,
//! 为 CoordinatorExecutor 提供异步、Send + Sync 的子 agent 执行器。
//!
//! `ConversationRuntime::run_subagent_turn` 是 `&mut self` 同步方法,无法直接
//! 注入到 `SubagentRunner` 闭包(需要 `Send + Sync` + 异步)。本模块把同样的
//! LLM 调用逻辑提取为独立结构,持有 `Arc<Mutex<Box<dyn ApiClient + Send>>>`
//! 共享状态,可在多线程异步上下文中安全调用。
//!
//! # Epic 3b:多轮 tool call 循环(同步线程路径)
//!
//! `dispatch_impl` 在 `std::thread::spawn` 的**独立 OS 线程**内执行多轮循环,
//! 使用同步 `client.stream()`(内部 `block_on`,独立线程内安全,不触发嵌套
//! runtime panic)。与路径 A(`execute_subagent_llm`)共享:
//! - `process_tool_uses` 公共函数(guard + 执行 + 回填)
//! - `write_handoff` 结构化落盘
//!
//! **capability 传递**:当前默认 `Analyze`(max_iter=1,无工具),向后兼容。
//! 通过 `with_capability` builder 可设置 `ReadOnly`/`Execute` 激活多轮循环。
//! 未来 `SpawnRequest.capability` 通过 `DagNode` 传入后,每个 dispatch 可
//! 按需设置 capability。

use crate::conversation::{
    build_assistant_message, build_subagent_request, default_subagent_tool_catalog,
    process_tool_uses, ApiClient, SubagentContext, ToolExecutor,
};
use crate::multi_agent::{write_handoff, HandoffStatus, SubagentCapability, SubagentHandoff};
use crate::session::ContentBlock;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 共享状态的子 agent 调度器。
///
/// 持有 `Arc<Mutex<Box<dyn ApiClient + Send>>>` 和 `workspace_root`,
/// 可安全跨线程共享给 `SubagentRunner` 闭包。
///
/// Epic 3b:新增 `tool_executor` 和 `capability` 字段,支持多轮 tool call 循环。
#[derive(Clone)]
pub struct SubagentDispatcher {
    api_client: Arc<Mutex<Box<dyn ApiClient + Send>>>,
    workspace_root: PathBuf,
    /// 工具执行器(共享 via Arc<Mutex>)。`None` 时无法执行工具(单轮,向后兼容)。
    tool_executor: Option<Arc<Mutex<Box<dyn ToolExecutor + Send>>>>,
    /// 子智能体能力分级。默认 `Analyze`(无工具,单轮)。
    /// `ReadOnly`/`Execute` 激活多轮循环和工具白名单。
    capability: SubagentCapability,
}

impl SubagentDispatcher {
    pub fn new(api_client: Arc<Mutex<Box<dyn ApiClient + Send>>>, workspace_root: PathBuf) -> Self {
        Self {
            api_client,
            workspace_root,
            tool_executor: None,
            capability: SubagentCapability::Analyze,
        }
    }

    /// Epic 3b:注入工具执行器,启用多轮 tool call 循环。
    ///
    /// 共享 via `Arc<Mutex<...>>`,并行 dispatch 通过锁串行化工具调用。
    /// 工具执行通常远快于 LLM 调用,锁竞争可接受。
    #[must_use]
    pub fn with_tool_executor(
        mut self,
        tool_executor: Arc<Mutex<Box<dyn ToolExecutor + Send>>>,
    ) -> Self {
        self.tool_executor = Some(tool_executor);
        self
    }

    /// Epic 3b:设置子智能体能力分级。
    ///
    /// - `Analyze`(默认):max_iter=1,无工具,纯 LLM 推理
    /// - `ReadOnly`:max_iter=5,只读工具(read/grep/glob/repomap/lsp_diagnostics)
    /// - `Execute`:max_iter=10,写入工具(edit/write/bash)
    #[must_use]
    pub fn with_capability(mut self, capability: SubagentCapability) -> Self {
        self.capability = capability;
        self
    }

    /// 执行子 agent turn(异步包装同步 stream 调用)。
    ///
    /// 逻辑与 ConversationRuntime::run_subagent_turn 一致:
    /// 1. 构造 system_prompt + user message
    /// 2. 调用 api_client.stream(在独立 OS 线程中执行)
    /// 3. 解析 assistant response,提取 text
    /// 4. 写到 .claw/subagents/{id}.md
    /// 5. 返回 result_ref 路径
    pub async fn dispatch(&self, subagent_id: String, task: String) -> Result<String, String> {
        // SubagentRunner 签名只有 (id, task),用 subagent_id 作为 name。
        let name = subagent_id.clone();
        Self::dispatch_impl(
            &self.api_client,
            &self.workspace_root,
            self.tool_executor.clone(),
            self.capability,
            &subagent_id,
            &name,
            &task,
        )
        .await
    }

    /// Epic 3b:多轮 tool call 循环 — 同步线程路径(§3.3.2)。
    ///
    /// 在 `std::thread::spawn` 的独立 OS 线程内执行多轮循环,使用同步
    /// `client.stream()`(内部 `block_on`,独立线程内安全)。
    ///
    /// **与路径 A(`execute_subagent_llm`)的差异**:
    /// - 路径 A 用 `stream_async().await`(tokio runtime 内)
    /// - 路径 B 用同步 `stream()`(独立 OS 线程内,不能 `.await`)
    ///
    /// **共享逻辑**:
    /// - `process_tool_uses`:guard(递归/白名单)+ 执行 + 回填
    /// - `write_handoff`:结构化 handoff 落盘
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_impl(
        api_client: &Arc<Mutex<Box<dyn ApiClient + Send>>>,
        workspace_root: &std::path::Path,
        tool_executor: Option<Arc<Mutex<Box<dyn ToolExecutor + Send>>>>,
        capability: SubagentCapability,
        subagent_id: &str,
        name: &str,
        task: &str,
    ) -> Result<String, String> {
        // 知识新鲜度门控(Phase 1):Novel 任务注入调研摘要到 task 文本。
        // 缓存命中(execute 已调过)零成本;未命中(client 未注入)降级为原 task。
        let gated = crate::knowledge_freshness::gate_task(task, 0).await;
        let enhanced_task = gated.enhance_task(task);

        // 3a/3b DRY:共享同一公共构造。
        // DAG 路径无 complexity 概念,与现状一致走 Simple(无 SOP)。
        let complexity = crate::multi_agent::TaskComplexity::Simple;
        // design-gaps #5:注入工具签名目录(按 capability 白名单过滤,与路径 A 一致)。
        // 短名与 process_tool_uses 白名单 guard 一致;Analyze 过滤后为空(无工具层)。
        let ctx = SubagentContext {
            tool_summaries: default_subagent_tool_catalog()
                .into_iter()
                .filter(|ts| capability.allowed_tools().contains(&ts.name.as_str()))
                .collect(),
            ..SubagentContext::default()
        };
        let initial_request = build_subagent_request(
            subagent_id,
            name,
            &enhanced_task,
            complexity,
            capability,
            &ctx,
        );

        // system_prompt 构建一次,多轮循环中不变(保 prefix cache 命中)。
        // build_subagent_request 内部已构造 system_prompt,这里提取用于后续轮次。
        let system_prompt = initial_request.system_prompt.clone();

        // 初始 user message
        let mut messages = initial_request.messages.clone();

        let max_iter = capability.max_iterations();
        let api_client = api_client.clone();
        let workspace_root = workspace_root.to_path_buf();
        // &str → owned String,移动进 'static 线程闭包
        let subagent_id = subagent_id.to_string();
        let name = name.to_string();
        let task = task.to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();

        // move 进独立 OS 线程(不能 .await,用同步 client.stream())
        std::thread::spawn(move || {
            let result = (|| {
                let mut client = api_client
                    .lock()
                    .map_err(|e| format!("api_client lock poisoned: {e}"))?;

                let mut iterations = 0;
                let mut tools_used: Vec<String> = Vec::new();
                let mut changed_files: Vec<String> = Vec::new();
                let mut final_text = String::new();

                loop {
                    iterations += 1;
                    if iterations > max_iter {
                        // §8.1:截断 → 落盘 Truncated handoff + Err
                        let handoff = SubagentHandoff::new(
                            subagent_id.clone(),
                            name.clone(),
                            capability,
                            complexity,
                            iterations,
                            tools_used.clone(),
                            changed_files.clone(),
                            &final_text,
                            &final_text,
                        )
                        .with_status(HandoffStatus::Truncated)
                        .with_task(&task);
                        let _ = write_handoff(&workspace_root, &handoff);
                        return Err(format!(
                            "subagent exceeded max_iterations ({max_iter}); partial result at .claw/subagents/{subagent_id}.md"
                        ));
                    }

                    let request = crate::conversation::ApiRequest {
                        system_prompt: system_prompt.clone(),
                        messages: messages.clone(),
                        request_kind: crate::conversation::RequestKind::Subagent,
                    };

                    // ⚠ 同步 stream()(streaming.rs:365),不是 stream_async().await
                    // 独立 OS 线程内 block_on 安全,不触发嵌套 runtime panic
                    let events = client
                        .stream(request)
                        .map_err(|e| format!("subagent LLM request failed: {e}"))?;

                    let (assistant_message, _usage, _cache_events) =
                        build_assistant_message(events)
                            .map_err(|e| format!("subagent response parsing failed: {e}"))?;

                    // 提取 ToolUse blocks(cloned — assistant_message 随后 move 进 messages)
                    let tool_uses: Vec<ContentBlock> = assistant_message
                        .blocks
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                        .cloned()
                        .collect();

                    // 累积 text 内容(最终 summary/details 来源)
                    for block in &assistant_message.blocks {
                        if let ContentBlock::Text { text } = block {
                            final_text.push_str(text);
                            final_text.push('\n');
                        }
                    }

                    messages.push(assistant_message);

                    if tool_uses.is_empty() {
                        break; // 正常终止:无工具调用
                    }

                    // 工具调用处理(guard + 执行 + 回填)
                    // 需要 &mut dyn ToolExecutor — 从 Arc<Mutex<...>> 获取
                    let tool_exec = match &tool_executor {
                        Some(te) => te,
                        None => {
                            // 无 tool_executor 但 LLM 请求了工具 → 落盘 Failed handoff
                            let err_msg = format!(
                                "subagent requested tools but no tool_executor configured (capability={capability:?})"
                            );
                            let handoff = SubagentHandoff::new(
                                subagent_id.clone(),
                                name.clone(),
                                capability,
                                complexity,
                                iterations,
                                tools_used.clone(),
                                changed_files.clone(),
                                &err_msg,
                                &err_msg,
                            )
                            .with_status(HandoffStatus::Failed)
                            .with_task(&task);
                            let _ = write_handoff(&workspace_root, &handoff);
                            return Err(err_msg);
                        }
                    };

                    let mut te_guard = match tool_exec.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            let err_msg = format!("tool_executor lock poisoned: {e}");
                            let handoff = SubagentHandoff::new(
                                subagent_id.clone(),
                                name.clone(),
                                capability,
                                complexity,
                                iterations,
                                tools_used.clone(),
                                changed_files.clone(),
                                &err_msg,
                                &err_msg,
                            )
                            .with_status(HandoffStatus::Failed)
                            .with_task(&task);
                            let _ = write_handoff(&workspace_root, &handoff);
                            return Err(err_msg);
                        }
                    };

                    if let Err(e) = process_tool_uses(
                        capability,
                        &tool_uses,
                        &mut **te_guard,
                        &workspace_root,
                        &mut messages,
                        &mut tools_used,
                        &mut changed_files,
                    ) {
                        // guard 违规(递归/白名单)→ 落盘 Failed handoff + Err
                        let handoff = SubagentHandoff::new(
                            subagent_id.clone(),
                            name.clone(),
                            capability,
                            complexity,
                            iterations,
                            tools_used.clone(),
                            changed_files.clone(),
                            e.to_string(),
                            e.to_string(),
                        )
                        .with_status(HandoffStatus::Failed)
                        .with_task(&task);
                        let _ = write_handoff(&workspace_root, &handoff);
                        return Err(format!("subagent guard violation: {e}"));
                    }
                    // te_guard 在此 drop,释放锁
                }

                if final_text.trim().is_empty() {
                    return Err("subagent produced no text content".to_string());
                }

                // 正常完成 → 落盘 Completed handoff(Epic 5 结构化协议)
                let handoff = SubagentHandoff::new(
                    subagent_id,
                    name,
                    capability,
                    complexity,
                    iterations,
                    tools_used,
                    changed_files,
                    &final_text,
                    &final_text,
                )
                .with_task(&task);
                write_handoff(&workspace_root, &handoff)
                    .map_err(|e| format!("failed to write subagent handoff: {e}"))
            })();
            // tx 收到 Err 仅表示调用方提前 drop(如取消),无需处理
            let _ = tx.send(result);
        });

        rx.await
            .map_err(|e| format!("subagent dispatch channel closed: {e}"))?
    }
}

// 静态断言:确保 SubagentDispatcher 是 Send + Sync(被 SubagentRunner 闭包捕获需要)。
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SubagentDispatcher>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{ApiRequest, AssistantEvent, RuntimeError, StaticToolExecutor};

    /// 测试用 ApiClient:返回预设的 assistant events。
    struct MockApiClient {
        responses: Vec<Vec<AssistantEvent>>,
        call_count: usize,
    }

    impl MockApiClient {
        fn new(responses: Vec<Vec<AssistantEvent>>) -> Self {
            Self {
                responses,
                call_count: 0,
            }
        }
    }

    impl ApiClient for MockApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            let idx = self.call_count.min(self.responses.len().saturating_sub(1));
            self.call_count += 1;
            Ok(self.responses[idx].clone())
        }
    }

    fn make_text_event(text: &str) -> Vec<AssistantEvent> {
        vec![
            AssistantEvent::TextDelta(text.to_string()),
            AssistantEvent::MessageStop,
        ]
    }

    fn make_tool_use_event(id: &str, name: &str, input: &str) -> Vec<AssistantEvent> {
        vec![
            AssistantEvent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: input.to_string(),
            },
            AssistantEvent::MessageStop,
        ]
    }

    #[tokio::test]
    async fn dispatch_analyze_single_round_writes_handoff() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();

        let client = MockApiClient::new(vec![make_text_event("Analysis complete.")]);
        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));
        let dispatcher = SubagentDispatcher::new(api_client, workspace)
            .with_capability(SubagentCapability::Analyze);

        let result = dispatcher
            .dispatch("subagent-test-1".into(), "analyze task".into())
            .await
            .expect("dispatch should succeed");

        assert!(result.contains("subagent-test-1"));

        // 验证 handoff 文件已写入
        let handoff_path = tmp.path().join(".claw/subagents/subagent-test-1.md");
        assert!(handoff_path.exists(), "handoff file should exist");

        let content = std::fs::read_to_string(&handoff_path).expect("read handoff");
        assert!(content.starts_with("---"), "should have frontmatter");
        assert!(content.contains("status: completed"));
        assert!(content.contains("Analysis complete."));
    }

    #[tokio::test]
    async fn dispatch_readonly_multi_round_with_tools() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();

        // 创建一个测试文件供 read 工具读取
        let test_file = workspace.join("test.txt");
        std::fs::write(&test_file, "hello world").expect("write test file");

        // 第 1 轮:LLM 请求 read 工具
        // 第 2 轮:LLM 返回纯文本(无工具调用,终止循环)
        let client = MockApiClient::new(vec![
            make_tool_use_event("tu-1", "read", r#"{"file_path":"test.txt"}"#),
            make_text_event("Read result: hello world"),
        ]);

        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));

        // 创建一个能执行 read 的 tool_executor
        let tool_exec = StaticToolExecutor::new().register("read", move |input| {
            // 简单 mock:解析 file_path 并返回内容
            let parsed: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
            let path = parsed
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Ok(format!("content of {path}: hello world"))
        });

        let tool_executor: Arc<Mutex<Box<dyn ToolExecutor + Send>>> =
            Arc::new(Mutex::new(Box::new(tool_exec)));

        let dispatcher = SubagentDispatcher::new(api_client, workspace)
            .with_capability(SubagentCapability::ReadOnly)
            .with_tool_executor(tool_executor);

        let result = dispatcher
            .dispatch("subagent-ro-1".into(), "read test.txt".into())
            .await
            .expect("dispatch should succeed");

        assert!(result.contains("subagent-ro-1"));

        // 验证 handoff
        let handoff_path = tmp.path().join(".claw/subagents/subagent-ro-1.md");
        let content = std::fs::read_to_string(&handoff_path).expect("read handoff");
        assert!(content.contains("status: completed"));
        assert!(content.contains("read"), "tools_used should contain read");
        assert!(
            content.contains("iterations: 2"),
            "should have 2 iterations"
        );
    }

    #[tokio::test]
    async fn dispatch_guard_violation_writes_failed_handoff() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();

        // LLM 尝试调用 dispatch_subagent(递归派发 guard)
        let client = MockApiClient::new(vec![make_tool_use_event(
            "tu-1",
            "dispatch_subagent",
            r#"{"task":"recursive"}"#,
        )]);

        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));

        let tool_executor: Arc<Mutex<Box<dyn ToolExecutor + Send>>> =
            Arc::new(Mutex::new(Box::new(StaticToolExecutor::new())));

        let dispatcher = SubagentDispatcher::new(api_client, workspace)
            .with_capability(SubagentCapability::Execute)
            .with_tool_executor(tool_executor);

        let err = dispatcher
            .dispatch("subagent-guard-1".into(), "recursive task".into())
            .await
            .expect_err("should fail with guard violation");

        assert!(err.contains("guard violation"));
        assert!(err.contains("recursion"));

        // 验证 Failed handoff
        let handoff_path = tmp.path().join(".claw/subagents/subagent-guard-1.md");
        let content = std::fs::read_to_string(&handoff_path).expect("read handoff");
        assert!(content.contains("status: failed"));
    }

    #[tokio::test]
    async fn dispatch_no_tool_executor_with_tool_request_fails() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();

        // LLM 请求工具,但无 tool_executor
        let client = MockApiClient::new(vec![make_tool_use_event(
            "tu-1",
            "read",
            r#"{"file_path":"test.txt"}"#,
        )]);

        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));

        // ReadOnly capability 但无 tool_executor
        let dispatcher = SubagentDispatcher::new(api_client, workspace)
            .with_capability(SubagentCapability::ReadOnly);

        let err = dispatcher
            .dispatch("subagent-noteexec-1".into(), "read task".into())
            .await
            .expect_err("should fail without tool_executor");

        assert!(err.contains("no tool_executor configured"));

        // 验证 Failed handoff
        let handoff_path = tmp.path().join(".claw/subagents/subagent-noteexec-1.md");
        let content = std::fs::read_to_string(&handoff_path).expect("read handoff");
        assert!(content.contains("status: failed"));
    }

    #[tokio::test]
    async fn dispatch_backward_compat_no_capability_no_tools() {
        // 向后兼容:不设置 capability(默认 Analyze)和 tool_executor
        // 行为应与改造前一致:单轮 LLM 调用 + handoff 落盘
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();

        let client = MockApiClient::new(vec![make_text_event("Simple analysis result.")]);
        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));

        // 不调用 with_capability / with_tool_executor — 完全向后兼容
        let dispatcher = SubagentDispatcher::new(api_client, workspace);

        let result = dispatcher
            .dispatch("subagent-compat-1".into(), "simple task".into())
            .await
            .expect("dispatch should succeed");

        assert!(result.starts_with(".claw/subagents/"));

        let handoff_path = tmp.path().join(".claw/subagents/subagent-compat-1.md");
        let content = std::fs::read_to_string(&handoff_path).expect("read handoff");
        assert!(content.contains("status: completed"));
        assert!(content.contains("capability: analyze"));
        assert!(content.contains("Simple analysis result."));
    }
}
