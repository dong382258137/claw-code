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
    default_subagent_tool_catalog, execute_subagent_llm, ApiClient, SubagentContext, ToolError,
    ToolExecutor,
};
use crate::multi_agent::SubagentCapability;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 共享状态的子 agent 调度器。
///
/// 持有 `Arc<Mutex<Box<dyn ApiClient + Send>>>` 和 `workspace_root`,
/// 可安全跨线程共享给 `SubagentRunner` 闭包。
///
/// Epic 3b:新增 `tool_executor` 和 `capability` 字段,支持多轮 tool call 循环。
/// Epic 1 T6:新增 `workspace_override` 字段,支持绑定子目录 workspace —
/// 工具作用域收窄(Guard 2.5 禁全仓扫描/bash + Guard 3 越界拒绝),
/// handoff 落盘到子目录,与路径 A(dispatch_subagent 的 workspace 字段)治理对齐。
#[derive(Clone)]
pub struct SubagentDispatcher {
    api_client: Arc<Mutex<Box<dyn ApiClient + Send>>>,
    workspace_root: PathBuf,
    /// 工具执行器(共享 via Arc<Mutex>)。`None` 时无法执行工具(单轮,向后兼容)。
    tool_executor: Option<Arc<Mutex<Box<dyn ToolExecutor + Send>>>>,
    /// 子智能体能力分级。默认 `Analyze`(无工具,单轮)。
    /// `ReadOnly`/`Execute` 激活多轮循环和工具白名单。
    capability: SubagentCapability,
    /// 绑定的子目录 workspace(路径 B 目录隔离)。`None` = 主 root(向后兼容)。
    workspace_override: Option<PathBuf>,
}

impl SubagentDispatcher {
    pub fn new(api_client: Arc<Mutex<Box<dyn ApiClient + Send>>>, workspace_root: PathBuf) -> Self {
        Self {
            api_client,
            workspace_root,
            tool_executor: None,
            capability: SubagentCapability::Analyze,
            workspace_override: None,
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

    /// Epic 1 T6:绑定子目录 workspace,收窄工具作用域并迁移 handoff 落盘基准。
    ///
    /// 与路径 A(dispatch_subagent 的 `workspace` 字段)对齐:存在时 Guard 2.5
    /// (禁 repomap/lsp_diagnostics/bash)+ Guard 3(越界拒绝)生效,handoff
    /// 写入 `{workspace}/.claw/subagents/`。`None` 保持主 root 行为(向后兼容)。
    #[must_use]
    pub fn with_workspace_override(mut self, workspace_override: Option<PathBuf>) -> Self {
        self.workspace_override = workspace_override;
        self
    }

    /// 执行子 agent turn(异步包装同步 stream 调用)。
    ///
    /// 逻辑与 ConversationRuntime::run_subagent_turn 一致:
    /// 1. 构造 system_prompt + user message
    /// 2. 调用 api_client.stream(在独立 OS 线程中执行)
    /// 3. 解析 assistant response,提取 text
    /// 4. 写到 .claw/subagents/{id}.md(绑定 workspace 时写到 `{workspace}/.claw/subagents/`)
    /// 5. 返回 result_ref 路径
    pub async fn dispatch(&self, subagent_id: String, task: String) -> Result<String, String> {
        // SubagentRunner 签名只有 (id, task),用 subagent_id 作为 name。
        let name = subagent_id.clone();
        Self::dispatch_impl(
            &self.api_client,
            &self.workspace_root,
            self.tool_executor.clone(),
            self.capability,
            self.workspace_override.as_deref(),
            &subagent_id,
            &name,
            &task,
        )
        .await
    }

    /// Epic 1 T7:统一执行链 — 委托 [`execute_subagent_llm`](crate::conversation::execute_subagent_llm)。
    ///
    /// 在 `std::thread::spawn` 的独立 OS 线程内自建 current_thread runtime,
    /// `block_on` 执行统一的 async 执行链(`stream_async`),`oneshot` 桥接保留。
    /// 与路径 A 共享一处 guard / 多轮循环 / prompt 构造,消除双执行循环漂移。
    ///
    /// 路径 B 额外语义:
    /// - 绑定 workspace 时先 `revalidate_subworkspace`(T5 TOCTOU 一致性:失效即拒)
    /// - 无 `tool_executor` 时注入拒绝型 stub(`NoToolExecutor`),保持统一签名完整
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_impl(
        api_client: &Arc<Mutex<Box<dyn ApiClient + Send>>>,
        workspace_root: &std::path::Path,
        tool_executor: Option<Arc<Mutex<Box<dyn ToolExecutor + Send>>>>,
        capability: SubagentCapability,
        workspace_override: Option<&std::path::Path>,
        subagent_id: &str,
        name: &str,
        task: &str,
    ) -> Result<String, String> {
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

        let api_client = api_client.clone();
        let workspace_root = workspace_root.to_path_buf();
        // Epic 1 T5(TOCTOU 一致性):绑定 workspace 时,执行前重新校验(失效即拒),
        // 与路径 A(`run_subagent_turn_with_model`)行为对齐。
        let workspace_override = match workspace_override {
            Some(ws) => Some(crate::subworkspace::revalidate_subworkspace(
                &workspace_root,
                ws,
            )?),
            None => None,
        };
        // &str → owned String,移动进 'static 线程闭包
        let subagent_id = subagent_id.to_string();
        let name = name.to_string();
        let task = task.to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();

        // move 进独立 OS 线程:自建 current_thread runtime 后 block_on 统一 async 执行链
        // (execute_subagent_llm 内部用 stream_async,不能在无 runtime 的线程直接 .await)
        std::thread::spawn(move || {
            let result = (|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("build subagent runtime failed: {e}"))?;
                let mut client = api_client
                    .lock()
                    .map_err(|e| format!("api_client lock poisoned: {e}"))?;
                // 工具执行器:共享锁借用;None 时注入拒绝型 stub(保持统一签名完整 —
                // 白名单外工具先被 Guard 2 拦,白名单内工具经 stub 报"no tool_executor configured")。
                let mut te_guard = match &tool_executor {
                    Some(te) => Some(
                        te.lock()
                            .map_err(|e| format!("tool_executor lock poisoned: {e}"))?,
                    ),
                    None => None,
                };
                let mut noop_exec = NoToolExecutor;
                let tool_exec: &mut dyn ToolExecutor = match te_guard.as_mut() {
                    Some(g) => &mut ***g,
                    None => &mut noop_exec,
                };
                rt.block_on(execute_subagent_llm(
                    &workspace_root,
                    workspace_override.as_deref(),
                    &mut **client,
                    tool_exec,
                    &subagent_id,
                    &name,
                    &task,
                    complexity,
                    capability,
                    &ctx,
                ))
            })();
            // tx 收到 Err 仅表示调用方提前 drop(如取消),无需处理
            let _ = tx.send(result);
        });

        rx.await
            .map_err(|e| format!("subagent dispatch channel closed: {e}"))?
    }
}

/// Epic 1 T7:无 `tool_executor` 时的拒绝型 stub — 所有工具执行失败,保持
/// [`execute_subagent_llm`] 的 `&mut dyn ToolExecutor` 签名完整(路径 B 兼容
/// 不注入 executor 的旧调用)。白名单外的工具先被 Guard 2 拒绝;白名单内工具
/// 经此 stub 报 `no tool_executor configured`。
struct NoToolExecutor;

impl ToolExecutor for NoToolExecutor {
    fn execute(&mut self, name: &str, _input: &str) -> Result<String, ToolError> {
        Err(ToolError::new(format!(
            "tool {name} unavailable: no tool_executor configured"
        )))
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

        // 第 1 轮:LLM 请求 read_file 工具
        // 第 2 轮:LLM 返回纯文本(无工具调用,终止循环)
        let client = MockApiClient::new(vec![
            make_tool_use_event("tu-1", "read_file", r#"{"file_path":"test.txt"}"#),
            make_text_event("Read result: hello world"),
        ]);

        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));

        // 创建一个能执行 read_file 的 tool_executor
        let tool_exec = StaticToolExecutor::new().register("read_file", move |input| {
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
        assert!(
            content.contains("read_file"),
            "tools_used should contain read_file"
        );
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
            "read_file",
            r#"{"file_path":"test.txt"}"#,
        )]);

        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));

        // ReadOnly capability 但无 tool_executor(T7 统一后注入拒绝型 stub:
        // 工具执行失败是 tool-level 回填,不中止循环 → mock 持续请求工具 →
        // 耗尽 max_iterations → Truncated 失败,语义仍为"无 executor 时工具请求无法完成")。
        let dispatcher = SubagentDispatcher::new(api_client, workspace)
            .with_capability(SubagentCapability::ReadOnly);

        let err = dispatcher
            .dispatch("subagent-noteexec-1".into(), "read task".into())
            .await
            .expect_err("should fail without tool_executor");

        assert!(err.contains("exceeded max_iterations"), "got: {err}");

        // 验证 Truncated handoff
        let handoff_path = tmp.path().join(".claw/subagents/subagent-noteexec-1.md");
        let content = std::fs::read_to_string(&handoff_path).expect("read handoff");
        assert!(content.contains("status: truncated"));
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

    // Epic 1 T6:绑定子目录 workspace 后,handoff 落盘到 `{workspace}/.claw/subagents/`,
    // 主 root 下不产生 handoff(路径 B 落盘基准与路径 A 对齐)。
    #[tokio::test]
    async fn dispatch_with_workspace_override_writes_handoff_to_scope() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();
        let sub = workspace.join("crates/api");
        std::fs::create_dir_all(&sub).expect("create subdir");
        std::fs::write(sub.join("Cargo.toml"), "[package]").expect("write Cargo.toml");

        let client = MockApiClient::new(vec![make_text_event("Scoped analysis.")]);
        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));
        let dispatcher = SubagentDispatcher::new(api_client, workspace.clone())
            .with_capability(SubagentCapability::Analyze)
            .with_workspace_override(Some(sub.clone()));

        let result = dispatcher
            .dispatch("subagent-scope-1".into(), "analyze scoped task".into())
            .await
            .expect("dispatch should succeed");

        assert!(result.contains("subagent-scope-1"));

        // handoff 落盘到子目录,主 root 下无 handoff
        let scoped_path = sub.join(".claw/subagents/subagent-scope-1.md");
        assert!(
            scoped_path.exists(),
            "handoff should be written under the scoped workspace: {}",
            scoped_path.display()
        );
        let root_path = workspace.join(".claw/subagents/subagent-scope-1.md");
        assert!(
            !root_path.exists(),
            "handoff must NOT be under workspace root"
        );

        let content = std::fs::read_to_string(&scoped_path).expect("read handoff");
        assert!(content.contains("status: completed"));
    }

    // Epic 1 T6:绑定子目录后,LLM 越界读(主 root 内但不在子目录)→ Guard 3 拒绝,
    // 工具不执行(回填 is_error),handoff 仍落盘到子目录。
    #[tokio::test]
    async fn dispatch_with_workspace_override_rejects_out_of_scope_tool() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();
        let sub = workspace.join("crates/api");
        std::fs::create_dir_all(&sub).expect("create subdir");
        std::fs::write(sub.join("Cargo.toml"), "[package]").expect("write Cargo.toml");
        // 兄弟目录文件:canonicalize 通过,但仍越出 scope
        std::fs::create_dir_all(workspace.join("crates/core")).expect("create sibling dir");
        let sibling = workspace.join("crates/core/other.txt");
        std::fs::write(&sibling, "sibling secret").expect("write sibling file");

        // 第 1 轮:LLM 请求读兄弟目录(越界)→ Guard 3 拒绝
        // 第 2 轮:纯文本,终止循环
        let client = MockApiClient::new(vec![
            make_tool_use_event(
                "tu-1",
                "read_file",
                r#"{"file_path":"crates/core/other.txt"}"#,
            ),
            make_text_event("Scoped result."),
        ]);

        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));
        let tool_exec = StaticToolExecutor::new().register("read_file", |input| {
            let parsed: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
            let path = parsed
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Ok(format!("content of {path}: UNEXPECTED-READ"))
        });
        let tool_executor: Arc<Mutex<Box<dyn ToolExecutor + Send>>> =
            Arc::new(Mutex::new(Box::new(tool_exec)));

        let dispatcher = SubagentDispatcher::new(api_client, workspace.clone())
            .with_capability(SubagentCapability::ReadOnly)
            .with_tool_executor(tool_executor)
            .with_workspace_override(Some(sub.clone()));

        dispatcher
            .dispatch("subagent-scope-2".into(), "read sibling".into())
            .await
            .expect("dispatch should succeed (scope violation is tool-level rejection)");

        // handoff 落盘到子目录,且工具被拒(无 UNEXPECTED-READ 标记:越界 handler 未执行)
        let scoped_path = sub.join(".claw/subagents/subagent-scope-2.md");
        let content = std::fs::read_to_string(&scoped_path).expect("read handoff");
        assert!(content.contains("status: completed"));
        assert!(
            content.contains("Scoped result."),
            "final text should be captured, got: {content}"
        );
        assert!(
            !content.contains("UNEXPECTED-READ"),
            "sibling read handler must not run, got: {content}"
        );
        assert!(
            !content.contains("read_file"),
            "out-of-scope read_file must not be recorded as used, got: {content}"
        );
    }

    // Epic 1 T6:绑定子目录后,LLM 请求 bash(Execute 白名单允许但 scope 下禁用)
    // → Guard 2.5 拒绝,dispatch 返回 Err,handoff(Failed)落盘到子目录。
    #[tokio::test]
    async fn dispatch_with_workspace_override_rejects_whole_repo_scan_tool() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let workspace = tmp.path().to_path_buf();
        let sub = workspace.join("crates/api");
        std::fs::create_dir_all(&sub).expect("create subdir");
        std::fs::write(sub.join("Cargo.toml"), "[package]").expect("write Cargo.toml");

        // 首轮请求 bash → Guard 2.5 拒绝(scope 子代理禁 bash)
        let client = MockApiClient::new(vec![make_tool_use_event(
            "tu-1",
            "bash",
            r#"{"command":"echo hi"}"#,
        )]);
        let api_client: Arc<Mutex<Box<dyn ApiClient + Send>>> =
            Arc::new(Mutex::new(Box::new(client)));
        let tool_executor: Arc<Mutex<Box<dyn ToolExecutor + Send>>> =
            Arc::new(Mutex::new(Box::new(StaticToolExecutor::new())));

        let dispatcher = SubagentDispatcher::new(api_client, workspace.clone())
            .with_capability(SubagentCapability::Execute)
            .with_tool_executor(tool_executor)
            .with_workspace_override(Some(sub.clone()));

        let err = dispatcher
            .dispatch("subagent-scope-3".into(), "run bash".into())
            .await
            .expect_err("bash must be rejected for workspace-scoped subagent");

        assert!(err.contains("guard violation"), "got: {err}");
        assert!(err.contains("not allowed"), "got: {err}");

        // Failed handoff 落盘到子目录
        let scoped_path = sub.join(".claw/subagents/subagent-scope-3.md");
        assert!(
            scoped_path.exists(),
            "failed handoff should be under scoped workspace"
        );
        let content = std::fs::read_to_string(&scoped_path).expect("read handoff");
        assert!(content.contains("status: failed"));
    }
}
