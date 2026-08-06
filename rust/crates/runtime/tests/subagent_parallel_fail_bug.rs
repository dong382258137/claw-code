//! 回归测试:FailFast::Off + 全部节点失败 → 调用方必须拿到真实失败原因。
//! 修复前:`spawn_parallel_via_dag_with_fail_fast` 返回误导性的
//! "result missing after DAG run"(因 `run()` 用 `into_successes()` 丢弃了失败信息)。
//! 修复后:改用 `run_with_details()`,失败节点携带真实错误
//! (如 "subagent execution failed: subagent response parsing failed: ...")。

use std::sync::Arc;

use runtime::multi_agent::dag::FailFast;
use runtime::multi_agent::{
    CoordinationMode, MultiAgentCoordinator, SpawnRequest, TaskComplexity,
};
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, ConversationRuntime, PermissionMode, PermissionPolicy,
    RuntimeError, Session, StaticToolExecutor,
};

/// 模拟 LLM 完全失败:返回空事件流(缺 MessageStop),build_assistant_message 将报错。
struct EmptyApi;

impl ApiClient for EmptyApi {
    fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        Ok(vec![]) // 空事件 → 解析失败
    }
}

#[test]
fn spawn_parallel_all_nodes_fail_reports_real_reason() {
    let coordinator = Arc::new(MultiAgentCoordinator::new());
    let tempdir = tempfile::tempdir().expect("temp workspace");

    let runtime = ConversationRuntime::new(
        Session::new(),
        EmptyApi,
        StaticToolExecutor::new(),
        PermissionPolicy::new(PermissionMode::DangerFullAccess),
        vec!["system".to_string()],
    )
    .with_dag_coordinator(coordinator.clone(), EmptyApi, tempdir.path().to_path_buf(), None);

    // 3 个并行子任务,全部会失败(LLM 返回空流)
    let tasks = vec![
        SpawnRequest::new(
            "agent-a",
            "task A",
            CoordinationMode::Fork,
            "deepseek-v4-flash",
            TaskComplexity::Simple,
        ),
        SpawnRequest::new(
            "agent-b",
            "task B",
            CoordinationMode::Fork,
            "deepseek-v4-flash",
            TaskComplexity::Simple,
        ),
        SpawnRequest::new(
            "agent-c",
            "task C",
            CoordinationMode::Fork,
            "deepseek-v4-flash",
            TaskComplexity::Simple,
        ),
    ];

    let results = runtime.spawn_parallel_via_dag_with_fail_fast(tasks, FailFast::Off);

    // 1) 3 个结果应全部为 Err
    assert_eq!(results.len(), 3, "应返回 3 个结果");
    let mut with_real_reason = 0;
    for (i, r) in results.iter().enumerate() {
        match r {
            Err(msg) => {
                eprintln!("task {i} 实际错误: {msg}");
                // 2) 不得出现误导性的 "result missing"
                assert!(
                    !msg.contains("result missing"),
                    "task {i} 的错误被替换为误导性的 'result missing': {msg}"
                );
                // 3) 必须携带真实失败原因
                assert!(
                    msg.contains("subagent execution failed")
                        && msg.contains("response parsing failed"),
                    "task {i} 的错误应包含真实失败原因(解析失败),实际: {msg}"
                );
                with_real_reason += 1;
            }
            Ok(_) => panic!("task {i} 不应成功(LLM 返回空流)"),
        }
    }
    assert_eq!(with_real_reason, 3, "3 个任务都应携带真实失败原因");

    // 4) coordinator 状态机应同样记录了真实失败原因(与返回结果一致)
    assert_eq!(coordinator.list().len(), 3, "coordinator 应有 3 条 subagent 记录");
    for a in coordinator.list() {
        eprintln!(
            ">>> coordinator 中 subagent {}: status={:?}, result={:?}",
            a.id, a.status, a.result
        );
        assert_eq!(a.status, runtime::multi_agent::SubagentStatus::Failed);
        let result = a
            .result
            .as_ref()
            .unwrap_or_else(|| panic!("subagent {} 应有失败结果", a.id));
        assert!(
            result.contains("response parsing failed"),
            "coordinator 中 subagent {} 的 result 应含真实原因,实际: {result}",
            a.id
        );
    }
}
