//! Web 热升级断点续跑(P0-4):新进程启动恢复。
//!
//! 旧进程在工具批遇到 `HotUpgrade` 时挂起,把现场写入
//! `.claw/upgrade-request.json` 后退出。本模块负责在新进程中:
//! 1. 读取待恢复的 Web 升级请求(挂起/构建中/上次恢复未完成);
//! 2. 给会话补齐 `HotUpgrade` 的工具结果与同批未执行调用的错误结果;
//! 3. 产出续跑规格,由 `serve` 入队并拉起 worker。
//!
//! 幂等:补结果走 `Session::resolve_tool_use`(已存在则跳过),重复启动安全。

use std::path::PathBuf;

use runtime::Session;

use crate::session_mgr::{
    self, QueuedPrompt, UpgradePhase, UpgradeRequest, UPGRADE_REQUEST_STALE_MS,
};

/// 续跑规格:供 `serve` 构造 `PromptJob`。
pub(crate) struct ResumePlan {
    pub(crate) session_id: String,
    pub(crate) session_path: PathBuf,
    pub(crate) workspace: PathBuf,
    pub(crate) permission: runtime::PermissionMode,
    pub(crate) model: Option<String>,
    pub(crate) plan_id: Option<String>,
    pub(crate) notice: String,
    /// 挂起时仍在排队的用户消息(续跑任务之后重新入队)。
    pub(crate) queued_prompts: Vec<QueuedPrompt>,
}

/// 读取待恢复的 Web 升级请求。CLI/TUI 标记(`phase=None`)与陈旧标记一律忽略。
///
/// 接受 `Suspended`(旧进程刚挂起)、`Building`(旧进程已启动升级助手后退出)
/// 与 `Resuming`(上次恢复中途进程崩溃) —— 三者都表示"工具结果可能尚未补齐,
/// 或续跑 turn 尚未成功收尾",补结果幂等,重复恢复安全。
pub(crate) fn load_pending(workspace: &std::path::Path) -> Option<UpgradeRequest> {
    let request = session_mgr::read_upgrade_request(workspace)?;
    if !matches!(
        request.phase,
        Some(UpgradePhase::Suspended | UpgradePhase::Building | UpgradePhase::Resuming)
    ) || request.web_port.is_none()
    {
        return None;
    }
    if request
        .suspended_tool_use_id
        .as_deref()
        .unwrap_or("")
        .is_empty()
    {
        return None;
    }
    let age_ms = current_time_millis().saturating_sub(request.requested_at_ms);
    // 挂起请求要经历"构建 + 重启",给 3 倍陈旧阈值(默认 30 分钟)覆盖慢构建。
    if age_ms > UPGRADE_REQUEST_STALE_MS.saturating_mul(3) {
        session_mgr::clear_upgrade_request(workspace);
        return None;
    }
    Some(request)
}

/// 补齐会话中的挂起工具结果并落盘,返回续跑规格。
///
/// `hot_upgrade_output` / `is_error` 区分两种结局:
/// - 升级完成:`is_error=false`,结果告知模型"升级完成,继续原任务";
/// - 升级放弃(超时/helper 失败):`is_error=true`,同批未执行调用补"未执行"。
pub(crate) fn resolve_session(
    request: &UpgradeRequest,
    hot_upgrade_output: &str,
    is_error: bool,
) -> Result<ResumePlan, String> {
    let session_path = request
        .session_path
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| "升级请求缺少 session_path".to_string())?;
    let workspace = request
        .workspace
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| "升级请求缺少 workspace".to_string())?;
    let tool_use_id = request
        .suspended_tool_use_id
        .as_deref()
        .ok_or_else(|| "升级请求缺少 suspended_tool_use_id".to_string())?;
    let tool_name = request
        .suspended_tool_name
        .as_deref()
        .unwrap_or("HotUpgrade");

    let mut session = Session::load_from_path(&session_path).map_err(|error| error.to_string())?;
    // 幂等:已有结果时 resolve 返回 false,不重复追加。
    session.resolve_tool_use(tool_use_id, tool_name, hot_upgrade_output, is_error);
    let pending_after = if request.pending_after.is_empty() {
        // 兜底:请求里没有清单时从会话最后一条 assistant 推导。
        session.unresolved_tool_uses_after(tool_use_id)
    } else {
        request.pending_after.clone()
    };
    for (id, name) in pending_after {
        session.resolve_tool_use(
            id.as_str(),
            name.as_str(),
            "未执行：热升级中断，请重新发起",
            true,
        );
    }
    session
        .save_to_path(&session_path)
        .map_err(|error| error.to_string())?;

    let permission = match request.permission.as_deref() {
        Some("read-only") => runtime::PermissionMode::ReadOnly,
        Some("danger-full-access") => runtime::PermissionMode::DangerFullAccess,
        _ => runtime::PermissionMode::WorkspaceWrite,
    };
    Ok(ResumePlan {
        session_id: session.session_id.clone(),
        session_path,
        workspace,
        permission,
        model: request.model.clone(),
        plan_id: request.plan_id.clone(),
        notice: if is_error {
            format!(
                "热升级未完成。{}\n请根据提示修复问题后，再决定是否重新发起热升级。",
                hot_upgrade_output
            )
        } else {
            format!(
                "热升级已完成。{}\n请从上次中断处继续原任务，无需重复已完成的步骤。",
                hot_upgrade_output
            )
        },
        queued_prompts: request.queued_prompts.clone(),
    })
}

fn current_time_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ContentBlock, ConversationMessage, MessageRole};

    fn write_suspended_session(dir: &std::path::Path) -> (std::path::PathBuf, UpgradeRequest) {
        let path = dir.join("session.jsonl");
        let mut session = Session::new();
        session.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "edit_file".to_string(),
                    input: "{}".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_2".to_string(),
                    name: "HotUpgrade".to_string(),
                    input: "{}".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_3".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                },
            ],
            usage: None,
            created_at_ms: 1,
        });
        // 旧进程已执行 call_1。
        session.messages.push(ConversationMessage::tool_result(
            "call_1",
            "edit_file",
            "ok",
            false,
        ));
        session.save_to_path(&path).expect("save session");

        let request = UpgradeRequest {
            session_id: session.session_id.clone(),
            plan_id: Some("plan-1".to_string()),
            requested_at_ms: current_time_millis(),
            from_version: "test".to_string(),
            old_pid: 1,
            web_port: Some(4399),
            web_token: Some("t".to_string()),
            session_path: Some(path.to_string_lossy().to_string()),
            workspace: Some(dir.to_string_lossy().to_string()),
            permission: Some("workspace-write".to_string()),
            model: Some("deepseek-flash".to_string()),
            suspended_tool_use_id: Some("call_2".to_string()),
            suspended_tool_name: Some("HotUpgrade".to_string()),
            pending_after: vec![("call_3".to_string(), "bash".to_string())],
            phase: Some(UpgradePhase::Suspended),
            queued_prompts: Vec::new(),
        };
        (path, request)
    }

    #[test]
    fn resolve_session_backfills_upgrade_and_pending_results_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let (path, request) = write_suspended_session(dir.path());

        let plan = resolve_session(&request, "热升级完成：版本 test", false).expect("resolve");
        assert_eq!(plan.session_id, request.session_id);
        assert_eq!(plan.plan_id.as_deref(), Some("plan-1"));

        let session = Session::load_from_path(&path).unwrap();
        assert_eq!(session.messages.len(), 4, "assistant + 3 条工具结果");
        match &session.messages[2].blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                output,
                ..
            } => {
                assert_eq!(tool_use_id, "call_2");
                assert!(!is_error);
                assert!(output.contains("热升级完成"));
            }
            other => panic!("unexpected block: {other:?}"),
        }
        match &session.messages[3].blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "call_3");
                assert!(is_error);
            }
            other => panic!("unexpected block: {other:?}"),
        }

        // 幂等:重复恢复不追加结果。
        resolve_session(&request, "热升级完成：版本 test", false).expect("second resolve");
        let mut session = Session::load_from_path(&path).unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.repair_dangling_tool_uses(), 0);
    }

    #[test]
    fn load_pending_accepts_suspended_and_building() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mut request) = write_suspended_session(dir.path());
        session_mgr::write_upgrade_request(dir.path(), &request).unwrap();
        assert!(load_pending(dir.path()).is_some());

        // 旧进程启动升级助手后退出,新进程看到的阶段是 Building。
        request.phase = Some(UpgradePhase::Building);
        session_mgr::write_upgrade_request(dir.path(), &request).unwrap();
        assert!(load_pending(dir.path()).is_some());

        // 已被本进程接手(Resuming,上次恢复中途崩溃)仍可重试;CLI/TUI 标记忽略。
        request.phase = Some(UpgradePhase::Resuming);
        session_mgr::write_upgrade_request(dir.path(), &request).unwrap();
        assert!(load_pending(dir.path()).is_some());
        request.phase = None;
        session_mgr::write_upgrade_request(dir.path(), &request).unwrap();
        assert!(load_pending(dir.path()).is_none());
    }

    /// P1:构建结果文件的读写与清理。
    #[test]
    fn upgrade_result_roundtrip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        assert!(session_mgr::read_upgrade_result(dir.path()).is_none());

        session_mgr::write_upgrade_result(
            dir.path(),
            &crate::session_mgr::UpgradeResult {
                ok: false,
                elapsed_ms: Some(1234),
                stderr_tail: Some("error[E0308]: mismatched types".to_string()),
                message: None,
            },
        )
        .unwrap();
        let result = session_mgr::read_upgrade_result(dir.path()).expect("read result");
        assert!(!result.ok);
        assert_eq!(result.elapsed_ms, Some(1234));
        assert!(result
            .stderr_tail
            .as_deref()
            .unwrap_or("")
            .contains("E0308"));

        session_mgr::clear_upgrade_result(dir.path());
        assert!(session_mgr::read_upgrade_result(dir.path()).is_none());
    }

    /// P1:挂起时排队的用户消息随续跑规格带出，供恢复后重新入队。
    #[test]
    fn resume_plan_carries_queued_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mut request) = write_suspended_session(dir.path());
        request.queued_prompts = vec![QueuedPrompt {
            text: "挂起后排队的消息".to_string(),
            permission: Some("workspace-write".to_string()),
            model: Some("deepseek-flash".to_string()),
        }];

        let plan = resolve_session(&request, "热升级完成", false).expect("resolve");
        assert_eq!(plan.queued_prompts.len(), 1);
        assert_eq!(plan.queued_prompts[0].text, "挂起后排队的消息");
        assert_eq!(
            plan.queued_prompts[0].model.as_deref(),
            Some("deepseek-flash")
        );
    }

    /// 构建失败时续跑通知应明确"未完成"，而不是"已完成"。
    #[test]
    fn failure_notice_is_not_reported_as_completed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, request) = write_suspended_session(dir.path());
        let plan = resolve_session(&request, "热升级失败：编译未通过", true).expect("resolve");
        assert!(plan.notice.contains("热升级未完成"), "got: {}", plan.notice);
        assert!(!plan.notice.contains("热升级已完成"));
    }
}
