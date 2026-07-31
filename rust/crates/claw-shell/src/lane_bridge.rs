//! LaneEvent → SessionNotification 桥接模块。
//!
//! 消费全局 LaneEvent sink(runtime::lane_events::drain_lane_events),
//! 转换为 ACP `SessionNotification` 推送给 IDE。
//!
//! ## 设计
//!
//! `LaneEvent` 是 runtime 内部的 lane 生命周期事件,共 23 种
//! (见 `runtime::LaneEventName`)。本模块将其映射为 ACP 0.10.4 的
//! `SessionUpdate`,通过 `AcpGatewaySender<acp::AgentSide>` 推送。
//!
//! 映射策略(参考 v0.2 设计文档 `docs/modules/ide-integration-detail.md`):
//! - 内部状态(Started/Ready)→ 不映射
//! - 文本通知(PromptMisdelivery/Green/Finished/Failed/BranchStale*)→ `AgentMessageChunk`
//! - 阻塞通知(Blocked/Red)→ `Plan`(单条目,状态 = InProgress)
//! - Git/Ship/Subagent 操作 → `ToolCall`
//!
//! ## 不依赖 1.3 API
//!
//! 本模块只使用 0.10.4 的 `agent_client_protocol`(claw-shell 直接依赖),
//! 不受 `acp-1_5` feature 影响,在默认构建下即可工作。

use agent_client_protocol as acp;
// 注意:runtime crate 的 `lane_events` 模块本身是私有的,但其内容通过
// `runtime::` 根命名空间公开 re-export(见 runtime/src/lib.rs 的 pub use)。
// 这里从根命名空间导入,而不是从 `runtime::lane_events::` 路径导入。
use runtime::{drain_lane_events, LaneEvent, LaneEventName};

use claw_acp::AcpGatewaySender;

/// 从 LaneEvent 的 `data` JSON 中提取 `subagent_id` 字段。
///
/// `SubagentHandoff` 和 `SubagentResult` 事件将 subagent_id 放在 `data` 中
/// (见 `LaneEvent::subagent_handoff` / `LaneEvent::subagent_result` 构造器)。
/// 找不到时返回占位字符串 `"unknown"`,保证 ToolCall 的 `tool_call_id` 非空。
fn extract_subagent_id(event: &LaneEvent) -> String {
    event
        .data
        .as_ref()
        .and_then(|data| data.get("subagent_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 从 LaneEvent 的 `detail` 或 `data` 中提取可读消息。
fn event_message(event: &LaneEvent) -> String {
    if let Some(detail) = &event.detail {
        return detail.clone();
    }
    if let Some(data) = &event.data {
        if let Ok(s) = serde_json::to_string(data) {
            return s;
        }
    }
    format!("{:?}", event.event)
}

/// 构造一个 `AgentMessageChunk` notification,内容为 `text`。
fn agent_message_chunk(
    session_id: &acp::SessionId,
    text: impl Into<String>,
) -> acp::SessionNotification {
    acp::SessionNotification::new(
        session_id.clone(),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    )
}

/// 构造一个 `ToolCall` notification。
///
/// `tool_call_id` 必须在 session 内唯一;调用方传入事件来源的稳定标识
/// (如 subagent_id 或事件名 + seq)。
fn tool_call_notification(
    session_id: &acp::SessionId,
    tool_call_id: impl Into<String>,
    title: impl Into<String>,
    kind: acp::ToolKind,
    status: acp::ToolCallStatus,
) -> acp::SessionNotification {
    let mut call = acp::ToolCall::new(tool_call_id.into(), title.into());
    call.kind = kind;
    call.status = status;
    acp::SessionNotification::new(session_id.clone(), acp::SessionUpdate::ToolCall(call))
}

/// 构造一个 `Plan` notification,包含单个 plan entry。
///
/// 用于 Blocked/Red/BranchStale* 等阻塞或警告事件。
fn plan_notification(
    session_id: &acp::SessionId,
    content: impl Into<String>,
    status: acp::PlanEntryStatus,
) -> acp::SessionNotification {
    let entry = acp::PlanEntry::new(content, acp::PlanEntryPriority::High, status);
    acp::SessionNotification::new(
        session_id.clone(),
        acp::SessionUpdate::Plan(acp::Plan::new(vec![entry])),
    )
}

/// 将 LaneEvent 转换为 SessionNotification(如果适用)。
///
/// 返回 `None` 的事件类型:`Started` / `Ready`(内部状态,不需要通知 IDE)。
/// 其他 21 种事件均映射为对应的 `SessionUpdate`。
///
/// # 参数
/// - `event`:来自 `drain_lane_events` 的 LaneEvent
/// - `session_id`:目标 ACP session
///
/// # 映射表
/// | LaneEventName | SessionUpdate 变体 | 说明 |
/// |---------------|-------------------|------|
/// | Started / Ready | None | 内部状态 |
/// | PromptMisdelivery | AgentMessageChunk | 警告 |
/// | Blocked / Red | Plan(InProgress) | 阻塞通知 |
/// | Green / Finished | AgentMessageChunk | 完成通知 |
/// | CommitCreated / PrOpened / MergeReady | ToolCall(Completed) | Git 操作 |
/// | Failed | AgentMessageChunk | 错误 |
/// | Reconciled / Merged / Superseded / Closed | ToolCall(Completed) | Git 状态 |
/// | BranchStaleAgainstMain / BranchWorkspaceMismatch | Plan(Pending) | 警告 |
/// | ShipPrepared / ShipCommitsSelected / ShipMerged / ShipPushedMain | ToolCall(Completed) | Ship 操作 |
/// | SubagentHandoff | ToolCall(InProgress) | 子 agent 启动 |
/// | SubagentResult | ToolCall(Completed/Failed) | 子 agent 完成 |
pub fn lane_event_to_session_update(
    event: &LaneEvent,
    session_id: &acp::SessionId,
) -> Option<acp::SessionNotification> {
    let msg = event_message(event);
    // 用事件名 + seq 作为 ToolCall 的稳定 ID,保证 session 内唯一。
    let seq = event.metadata.seq;
    let tool_call_id = format!("lane-{}-{}", event_name_str(event.event), seq);

    match event.event {
        // 内部状态事件:不映射
        LaneEventName::Started | LaneEventName::Ready => None,

        // 文本通知类:AgentMessageChunk
        LaneEventName::PromptMisdelivery => Some(agent_message_chunk(
            session_id,
            format!("[warning] prompt misdelivery: {msg}"),
        )),
        LaneEventName::Green => Some(agent_message_chunk(
            session_id,
            format!("[green] lane is healthy: {msg}"),
        )),
        LaneEventName::Finished => Some(agent_message_chunk(
            session_id,
            format!("[finished] lane completed: {msg}"),
        )),
        LaneEventName::Failed => Some(agent_message_chunk(
            session_id,
            format!("[failed] lane failed: {msg}"),
        )),
        LaneEventName::BranchStaleAgainstMain => Some(agent_message_chunk(
            session_id,
            format!("[warning] branch is stale against main: {msg}"),
        )),
        LaneEventName::BranchWorkspaceMismatch => Some(agent_message_chunk(
            session_id,
            format!("[warning] branch/workspace mismatch: {msg}"),
        )),

        // 阻塞通知:Plan(InProgress)
        LaneEventName::Blocked => Some(plan_notification(
            session_id,
            format!("[blocked] lane is blocked: {msg}"),
            acp::PlanEntryStatus::InProgress,
        )),
        LaneEventName::Red => Some(plan_notification(
            session_id,
            format!("[red] lane is in red state: {msg}"),
            acp::PlanEntryStatus::InProgress,
        )),

        // Git 操作:ToolCall(Completed)
        LaneEventName::CommitCreated => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Git commit created: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::PrOpened => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("PR opened: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::MergeReady => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Merge ready: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),

        // Git 终态:ToolCall(Completed)
        LaneEventName::Reconciled => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Reconciled: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::Merged => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Merged: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::Superseded => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Superseded: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::Closed => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Closed: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),

        // Ship 操作:ToolCall(Completed)
        LaneEventName::ShipPrepared => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship prepared: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::ShipCommitsSelected => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship commits selected: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::ShipMerged => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship merged: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),
        LaneEventName::ShipPushedMain => Some(tool_call_notification(
            session_id,
            &tool_call_id,
            format!("Ship pushed to main: {msg}"),
            acp::ToolKind::Other,
            acp::ToolCallStatus::Completed,
        )),

        // Subagent 事件:ToolCall,根据 status 区分 InProgress/Completed/Failed
        LaneEventName::SubagentHandoff => {
            let subagent_id = extract_subagent_id(event);
            Some(tool_call_notification(
                session_id,
                format!("subagent-{subagent_id}"),
                format!("Subagent handoff ({subagent_id}): {msg}"),
                acp::ToolKind::Other,
                acp::ToolCallStatus::InProgress,
            ))
        }
        LaneEventName::SubagentResult => {
            let subagent_id = extract_subagent_id(event);
            // SubagentResult 的 data 中有 status 字段(completed/failed/cancelled)
            let sub_status = event
                .data
                .as_ref()
                .and_then(|d| d.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let call_status = match sub_status {
                "failed" => acp::ToolCallStatus::Failed,
                _ => acp::ToolCallStatus::Completed,
            };
            Some(tool_call_notification(
                session_id,
                format!("subagent-{subagent_id}"),
                format!("Subagent result ({subagent_id}): {msg}"),
                acp::ToolKind::Other,
                call_status,
            ))
        }
    }
}

/// 返回 LaneEventName 的稳定字符串标识,用于构造 ToolCall 的 ID。
fn event_name_str(name: LaneEventName) -> &'static str {
    match name {
        LaneEventName::Started => "started",
        LaneEventName::Ready => "ready",
        LaneEventName::PromptMisdelivery => "prompt_misdelivery",
        LaneEventName::Blocked => "blocked",
        LaneEventName::Red => "red",
        LaneEventName::Green => "green",
        LaneEventName::CommitCreated => "commit_created",
        LaneEventName::PrOpened => "pr_opened",
        LaneEventName::MergeReady => "merge_ready",
        LaneEventName::Finished => "finished",
        LaneEventName::Failed => "failed",
        LaneEventName::Reconciled => "reconciled",
        LaneEventName::Merged => "merged",
        LaneEventName::Superseded => "superseded",
        LaneEventName::Closed => "closed",
        LaneEventName::BranchStaleAgainstMain => "branch_stale",
        LaneEventName::BranchWorkspaceMismatch => "branch_mismatch",
        LaneEventName::ShipPrepared => "ship_prepared",
        LaneEventName::ShipCommitsSelected => "ship_commits_selected",
        LaneEventName::ShipMerged => "ship_merged",
        LaneEventName::ShipPushedMain => "ship_pushed_main",
        LaneEventName::SubagentHandoff => "subagent_handoff",
        LaneEventName::SubagentResult => "subagent_result",
    }
}

/// 刷新全局 LaneEvent sink,将所有缓冲事件转换为 `SessionNotification`
/// 并通过 `AcpGatewaySender` 推送给 IDE。
///
/// 使用 `forward_fire_and_forget`:SessionNotification 是单向通知,
/// 不需要等待客户端 ACK;即使 IDE 通道关闭也不阻塞 agent。
///
/// # 参数
/// - `gateway`:agent → client 的 ACP gateway sender
/// - `session_id`:目标 ACP session
///
/// # 返回
/// 成功推送的事件数量(包括因通道关闭被丢弃的——它们仍被 drain 出来)。
pub fn flush_lane_events_to_acp(
    gateway: &AcpGatewaySender<acp::AgentSide>,
    session_id: &acp::SessionId,
) -> usize {
    let events = drain_lane_events();
    let count = events.len();
    for event in events {
        if let Some(notification) = lane_event_to_session_update(&event, session_id) {
            // fire-and-forget:与 ClawAgent::notify 的策略一致
            // (gateway.rs 中 session_notification 也用 forward_fire_and_forget)
            gateway.forward_fire_and_forget(notification);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    // 从 runtime 根命名空间导入(lane_events 模块本身是私有的)。
    use runtime::{EventProvenance, LaneEventBuilder, LaneEventStatus, LaneFailureClass};

    // `LaneEventStatus` 在非测试代码中未使用,但在测试中频繁使用。

    /// 测试序列化锁:全局 LaneEvent sink 是进程级单例,并行测试会互相干扰
    /// (一个测试 publish 的事件可能被另一个测试的 drain 消费)。
    /// 用 OnceLock + Mutex 确保使用全局 sink 的测试串行执行。
    fn sink_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 构造一个带最小 metadata 的测试 LaneEvent。
    fn make_event(name: LaneEventName, status: LaneEventStatus, seq: u64) -> LaneEvent {
        LaneEventBuilder::new(
            name,
            status,
            "2026-07-26T00:00:00Z",
            seq,
            EventProvenance::Test,
        )
        .build()
    }

    fn session_id() -> acp::SessionId {
        acp::SessionId::new("test-session")
    }

    // ---- 23 种事件映射覆盖测试 ----

    #[test]
    fn started_returns_none() {
        let event = make_event(LaneEventName::Started, LaneEventStatus::Running, 0);
        assert!(lane_event_to_session_update(&event, &session_id()).is_none());
    }

    #[test]
    fn ready_returns_none() {
        let event = make_event(LaneEventName::Ready, LaneEventStatus::Ready, 1);
        assert!(lane_event_to_session_update(&event, &session_id()).is_none());
    }

    #[test]
    fn prompt_misdelivery_maps_to_agent_message_chunk() {
        let event = make_event(
            LaneEventName::PromptMisdelivery,
            LaneEventStatus::Blocked,
            2,
        )
        .with_detail("wrong agent");
        let notif = lane_event_to_session_update(&event, &session_id())
            .expect("PromptMisdelivery should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn blocked_maps_to_plan_in_progress() {
        let event = make_event(LaneEventName::Blocked, LaneEventStatus::Blocked, 3)
            .with_detail("blocked on test");
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("Blocked should map");
        match notif.update {
            acp::SessionUpdate::Plan(plan) => {
                assert_eq!(plan.entries.len(), 1);
                assert_eq!(plan.entries[0].status, acp::PlanEntryStatus::InProgress);
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn red_maps_to_plan_in_progress() {
        let event = make_event(LaneEventName::Red, LaneEventStatus::Red, 4);
        let notif = lane_event_to_session_update(&event, &session_id()).expect("Red should map");
        assert!(matches!(notif.update, acp::SessionUpdate::Plan(_)));
    }

    #[test]
    fn green_maps_to_agent_message_chunk() {
        let event = make_event(LaneEventName::Green, LaneEventStatus::Green, 5);
        let notif = lane_event_to_session_update(&event, &session_id()).expect("Green should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn finished_maps_to_agent_message_chunk() {
        let event = make_event(LaneEventName::Finished, LaneEventStatus::Completed, 6);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("Finished should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn commit_created_maps_to_tool_call_completed() {
        let event = make_event(LaneEventName::CommitCreated, LaneEventStatus::Completed, 7);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("CommitCreated should map");
        match notif.update {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, acp::ToolCallStatus::Completed);
                assert!(call.title.contains("commit"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn pr_opened_maps_to_tool_call_completed() {
        let event = make_event(LaneEventName::PrOpened, LaneEventStatus::Ready, 8);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("PrOpened should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::ToolCall(acp::ToolCall {
                status: acp::ToolCallStatus::Completed,
                ..
            })
        ));
    }

    #[test]
    fn merge_ready_maps_to_tool_call_completed() {
        let event = make_event(LaneEventName::MergeReady, LaneEventStatus::Ready, 9);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("MergeReady should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::ToolCall(acp::ToolCall {
                status: acp::ToolCallStatus::Completed,
                ..
            })
        ));
    }

    #[test]
    fn failed_maps_to_agent_message_chunk() {
        let event = make_event(LaneEventName::Failed, LaneEventStatus::Failed, 10)
            .with_failure_class(LaneFailureClass::Test);
        let notif = lane_event_to_session_update(&event, &session_id()).expect("Failed should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn reconciled_maps_to_tool_call() {
        let event = make_event(LaneEventName::Reconciled, LaneEventStatus::Reconciled, 11);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("Reconciled should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    #[test]
    fn merged_maps_to_tool_call() {
        let event = make_event(LaneEventName::Merged, LaneEventStatus::Merged, 12);
        let notif = lane_event_to_session_update(&event, &session_id()).expect("Merged should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    #[test]
    fn superseded_maps_to_tool_call() {
        let event = make_event(LaneEventName::Superseded, LaneEventStatus::Superseded, 13);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("Superseded should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    #[test]
    fn closed_maps_to_tool_call() {
        let event = make_event(LaneEventName::Closed, LaneEventStatus::Closed, 14);
        let notif = lane_event_to_session_update(&event, &session_id()).expect("Closed should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    #[test]
    fn branch_stale_maps_to_agent_message_chunk() {
        let event = make_event(
            LaneEventName::BranchStaleAgainstMain,
            LaneEventStatus::Blocked,
            15,
        );
        let notif = lane_event_to_session_update(&event, &session_id())
            .expect("BranchStaleAgainstMain should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn branch_mismatch_maps_to_agent_message_chunk() {
        let event = make_event(
            LaneEventName::BranchWorkspaceMismatch,
            LaneEventStatus::Blocked,
            16,
        );
        let notif = lane_event_to_session_update(&event, &session_id())
            .expect("BranchWorkspaceMismatch should map");
        assert!(matches!(
            notif.update,
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn ship_prepared_maps_to_tool_call() {
        let event = make_event(LaneEventName::ShipPrepared, LaneEventStatus::Ready, 17);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("ShipPrepared should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    #[test]
    fn ship_commits_selected_maps_to_tool_call() {
        let event = make_event(
            LaneEventName::ShipCommitsSelected,
            LaneEventStatus::Ready,
            18,
        );
        let notif = lane_event_to_session_update(&event, &session_id())
            .expect("ShipCommitsSelected should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    #[test]
    fn ship_merged_maps_to_tool_call() {
        let event = make_event(LaneEventName::ShipMerged, LaneEventStatus::Completed, 19);
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("ShipMerged should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    #[test]
    fn ship_pushed_main_maps_to_tool_call() {
        let event = make_event(
            LaneEventName::ShipPushedMain,
            LaneEventStatus::Completed,
            20,
        );
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("ShipPushedMain should map");
        assert!(matches!(notif.update, acp::SessionUpdate::ToolCall(_)));
    }

    // ---- 任务要求的 5 种核心覆盖测试 ----

    #[test]
    fn subagent_handoff_maps_to_tool_call_in_progress() {
        let event = LaneEvent::subagent_handoff(
            "2026-07-26T00:00:00Z",
            "sub-123",
            "fork",
            "implement feature X",
        );
        let notif = lane_event_to_session_update(&event, &session_id())
            .expect("SubagentHandoff should map");
        match notif.update {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, acp::ToolCallStatus::InProgress);
                assert_eq!(call.tool_call_id.0.as_ref(), "subagent-sub-123");
                assert!(call.title.contains("sub-123"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn subagent_result_completed_maps_to_tool_call_completed() {
        let event =
            LaneEvent::subagent_result("2026-07-26T00:00:00Z", "sub-456", "completed", "done");
        let notif =
            lane_event_to_session_update(&event, &session_id()).expect("SubagentResult should map");
        match notif.update {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, acp::ToolCallStatus::Completed);
                assert_eq!(call.tool_call_id.0.as_ref(), "subagent-sub-456");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn subagent_result_failed_maps_to_tool_call_failed() {
        let event = LaneEvent::subagent_result("2026-07-26T00:00:00Z", "sub-789", "failed", "boom");
        let notif = lane_event_to_session_update(&event, &session_id())
            .expect("SubagentResult failed should map");
        match notif.update {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, acp::ToolCallStatus::Failed);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    /// 验证 `flush_lane_events_to_acp` 在空 sink 时不 panic 且返回 0。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 测试互斥锁:有意持有跨 await,串行化共享全局 sink
    async fn flush_empty_sink_returns_zero() {
        // 获取序列化锁:防止与其他使用全局 sink 的测试并行执行。
        let _guard = sink_lock();
        // 先 drain 清空全局 sink
        let _ = drain_lane_events();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                let gateway: AcpGatewaySender<acp::AgentSide> = AcpGatewaySender::new(tx);
                let session_id = acp::SessionId::new("test");
                let count = flush_lane_events_to_acp(&gateway, &session_id);
                assert_eq!(count, 0);
            })
            .await;
    }

    /// 验证 `flush_lane_events_to_acp` 将事件推入 gateway channel。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 测试互斥锁:有意持有跨 await,串行化共享全局 sink
    async fn flush_pushes_events_to_gateway_channel() {
        // 获取序列化锁:防止与其他使用全局 sink 的测试并行执行。
        let _guard = sink_lock();
        // 先 drain 清空全局 sink
        let _ = drain_lane_events();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                use tokio::sync::mpsc;

                // 发布 2 个事件:1 个 Started(不映射)+ 1 个 Finished(映射)
                runtime::try_publish(LaneEvent::new(
                    LaneEventName::Started,
                    LaneEventStatus::Running,
                    "2026-07-26T00:00:00Z",
                ));
                runtime::try_publish(LaneEvent::new(
                    LaneEventName::Finished,
                    LaneEventStatus::Completed,
                    "2026-07-26T00:00:01Z",
                ));

                let (tx, mut rx) = mpsc::unbounded_channel();
                let gateway: AcpGatewaySender<acp::AgentSide> = AcpGatewaySender::new(tx);
                let session_id = acp::SessionId::new("test");

                let count = flush_lane_events_to_acp(&gateway, &session_id);
                assert_eq!(count, 2, "should drain both events");

                // 应该只收到 1 个 notification(Started 不映射)
                let msg = rx.recv().await.expect("should receive notification");
                match msg {
                    claw_acp::AcpClientMessage::SessionNotification(args) => {
                        assert!(matches!(
                            args.request.update,
                            acp::SessionUpdate::AgentMessageChunk(_)
                        ));
                    }
                    other => panic!("expected SessionNotification, got {other:?}"),
                }
                // channel 应该已空(Started 被丢弃)
                assert!(rx.try_recv().is_err(), "no more messages expected");
            })
            .await;
    }
}
