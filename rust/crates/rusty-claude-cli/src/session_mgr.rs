//! 会话核心：会话创建/恢复、工作区会话存储、热升级请求。
//!
//! Phase 3 起 CLI 展示层（会话列表渲染、导出文本、历史报告、交互选择）
//! 已随界面层删除；本模块只保留 `claw serve` 与 `LiveCli` 需要的会话生命周期。

use std::fs;
use std::path::{Path, PathBuf};

use runtime::{ContentBlock, HistoryIndex, MessageRole, Session};

pub(crate) const PRIMARY_SESSION_EXTENSION: &str = "jsonl";

pub(crate) const LEGACY_SESSION_EXTENSION: &str = "json";

/// Markdown 导出时单个工具载荷的截断上限（字符）。
pub(crate) const SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT: usize = 280;

#[derive(Debug, Clone)]
pub(crate) struct SessionHandle {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
}

pub(crate) fn session_store_for(
    workspace: &Path,
) -> Result<runtime::SessionStore, Box<dyn std::error::Error>> {
    runtime::SessionStore::from_cwd(workspace)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

/// 热升级请求(双大脑收官基石):旧进程 `/upgrade` 写入,新进程启动检测。
///
/// 放在工作区 `.claw/upgrade-request.json`(用户确认位置)。字段:
/// - `session_id`:旧进程要恢复的会话 ID(新进程 --resume 用)
/// - `plan_id`:旧进程活跃计划的文件 ID(新进程从 `.claw/plans/<plan_id>.json` 恢复)
/// - `requested_at_ms`:请求时间戳(陈旧标记清理用,>10min 视为失效)
/// - `from_version`:旧进程版本(诊断用)
/// - `old_pid`:旧进程 PID(升级助手等它退出后才 mv exe;自动退出路径写 exited 标记用)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpgradeRequest {
    pub(crate) session_id: String,
    pub(crate) plan_id: Option<String>,
    pub(crate) requested_at_ms: i64,
    pub(crate) from_version: String,
    /// 旧进程 PID。0 = 未知(旧版标记,向后兼容)。
    #[serde(default)]
    pub(crate) old_pid: u32,
    /// Web 热升级:旧 `claw serve` 实际监听端口。`None` = CLI/TUI 升级。
    #[serde(default)]
    pub(crate) web_port: Option<u16>,
    /// Web 热升级:新 serve 复用旧 token,使已打开页面无需重新拿 URL。
    #[serde(default)]
    pub(crate) web_token: Option<String>,
}

/// upgrade-request.json 相对工作区的路径。
pub(crate) const UPGRADE_REQUEST_REL: &str = ".claw/upgrade-request.json";
/// 旧进程"已退出"标记(升级助手轮询此文件确认可开始迁移 exe)。
pub(crate) const UPGRADE_EXITED_REL: &str = ".claw/upgrade-exited.json";
/// 升级标记陈旧阈值:超过此毫秒数的标记视为失效(崩溃残留),启动时忽略并清理。
pub(crate) const UPGRADE_REQUEST_STALE_MS: i64 = 10 * 60 * 1000;

/// 写入升级请求(旧进程 `/upgrade` 调用)。失败返回错误(不 panic)。
pub(crate) fn write_upgrade_request(
    workspace_root: &Path,
    req: &UpgradeRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = workspace_root.join(UPGRADE_REQUEST_REL);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(req)?;
    fs::write(&path, json)?;
    Ok(())
}

/// 读取升级请求(新进程启动检测)。不存在/解析失败返回 None(降级为普通启动)。
pub(crate) fn read_upgrade_request(workspace_root: &Path) -> Option<UpgradeRequest> {
    let path = workspace_root.join(UPGRADE_REQUEST_REL);
    let content = fs::read_to_string(path).ok()?;
    let req: UpgradeRequest = serde_json::from_str(&content).ok()?;
    Some(req)
}

/// 清理升级请求(新进程恢复完成后调用,幂等)。失败静默(非关键路径)。
pub(crate) fn clear_upgrade_request(workspace_root: &Path) {
    let path = workspace_root.join(UPGRADE_REQUEST_REL);
    let _ = fs::remove_file(path);
}

/// 旧进程退出时写"已退出"标记(任务 2 一键化:旧进程自动退出前调用)。
///
/// 内容含 old_pid 与退出时间,升级助手轮询此文件出现即确认旧进程已退出、
/// exe 文件锁已释放,可安全执行 `mv claw.exe claw.exe.old`。
/// 幂等:文件已存在则覆写(无副作用)。
pub(crate) fn write_upgrade_exited(
    workspace_root: &Path,
    old_pid: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = workspace_root.join(UPGRADE_EXITED_REL);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let exited_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let json = serde_json::json!({
        "old_pid": old_pid,
        "exited_at_ms": exited_ms,
    });
    fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

/// 读取"已退出"标记(升级助手轮询)。不存在/解析失败返回 None。
pub(crate) fn read_upgrade_exited(workspace_root: &Path) -> Option<serde_json::Value> {
    let path = workspace_root.join(UPGRADE_EXITED_REL);
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// 清理"已退出"标记(升级助手完成迁移后调用,幂等)。
pub(crate) fn clear_upgrade_exited(workspace_root: &Path) {
    let path = workspace_root.join(UPGRADE_EXITED_REL);
    let _ = fs::remove_file(path);
}

/// 与 [`new_cli_session_in`] 类似，但额外附加 `--add-dir` 提供的工作区根。
/// 这些根与主工作区根一起构成多根校验集合（见 `WorkspacePathScope`）。
pub(crate) fn new_cli_session_with_roots_in(
    workspace: &Path,
    additional_roots: Vec<PathBuf>,
) -> Result<Session, Box<dyn std::error::Error>> {
    let cwd = workspace.to_path_buf();
    let session = Session::new()
        .with_workspace_root(cwd.clone())
        .with_additional_workspace_roots(additional_roots);
    Ok(attach_session_indexes(&cwd, session))
}

/// 给会话挂上旁路索引：FTS5 历史检索 + Phase 5 会话读取索引。
///
/// 两类索引都是 best-effort：打不开（权限/磁盘）时只禁用对应能力，
/// 会话本身仍可正常读写。
pub(crate) fn attach_session_indexes(workspace: &Path, mut session: Session) -> Session {
    let history_db = workspace.join(".claw").join("history.db");
    if let Ok(mut index) = HistoryIndex::open(&history_db) {
        // v4:注入进程级共享 embedding provider(BGE-small 单例)启用稠密检索;
        // 未编译 embedding feature 时返回 None,索引保持纯 FTS5 行为。
        if let Some(provider) = runtime::build_embedding_provider() {
            index = index.with_embedder(provider);
        }
        session = session.with_history_index(std::sync::Arc::new(index));
    }
    let session_db = workspace.join(".claw").join("sessions.db");
    if let Ok(index) = runtime::SessionIndex::open(&session_db) {
        session = session.with_session_index(std::sync::Arc::new(index));
    }
    session
}

pub(crate) fn create_managed_session_handle_in(
    workspace: &Path,
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let handle = session_store_for(workspace)?.create_handle(session_id);
    Ok(SessionHandle {
        id: handle.id,
        path: handle.path,
    })
}

/// 按显式 workspace 恢复会话引用。`claw serve` 的进程 cwd 是中性临时目录，
/// 必须走这个入口，否则会话会被拿去和临时目录做 workspace 校验。
pub(crate) fn load_session_reference_in(
    workspace: &Path,
    reference: &str,
) -> Result<(SessionHandle, Session), Box<dyn std::error::Error>> {
    let loaded = session_store_for(workspace)?
        .load_session(reference)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    // Phase 5：Web 恢复会话与新建会话一致，挂上 history.db 与 sessions.db，
    // 否则恢复后的 `session_search` 与列表索引都不会更新。
    let session = attach_session_indexes(workspace, loaded.session);
    Ok((
        SessionHandle {
            id: loaded.handle.id,
            path: loaded.handle.path,
        },
        session,
    ))
}

// ===== 会话导出（Phase 4 Web 导出接口的复用入口）=====

pub(crate) fn default_export_filename(session: &Session) -> String {
    let stem = session
        .messages
        .iter()
        .find_map(|message| match message.role {
            MessageRole::User => message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .map_or("conversation", |text| {
            text.lines().next().unwrap_or("conversation")
        })
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let fallback = if stem.is_empty() {
        "conversation"
    } else {
        &stem
    };
    format!("{fallback}.txt")
}

pub(crate) fn resolve_export_path(
    workspace: &Path,
    requested_path: Option<&str>,
    session: &Session,
) -> PathBuf {
    let file_name =
        requested_path.map_or_else(|| default_export_filename(session), ToOwned::to_owned);
    let final_name = if Path::new(&file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
    {
        file_name
    } else {
        format!("{file_name}.txt")
    };
    workspace.join(final_name)
}

pub(crate) fn summarize_tool_payload_for_markdown(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.split_whitespace().collect::<Vec<_>>().join(" "),
    };
    if compact.is_empty() {
        return String::new();
    }
    crate::truncate_for_summary(&compact, SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT)
}

/// 导出会话为 Markdown 文件，返回（写入路径，消息数）。
pub(crate) fn run_export_in(
    workspace: &Path,
    session_reference: &str,
    output_path: Option<&Path>,
) -> Result<(PathBuf, usize), Box<dyn std::error::Error>> {
    let (handle, session) = load_session_reference_in(workspace, session_reference)?;
    let markdown = render_session_markdown(&session, &handle.id, &handle.path);
    let path = output_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| resolve_export_path(workspace, None, &session));
    fs::write(&path, markdown)?;
    Ok((path, session.messages.len()))
}

pub(crate) fn render_session_markdown(
    session: &Session,
    session_id: &str,
    session_path: &Path,
) -> String {
    let mut lines = vec![
        "# Conversation Export".to_string(),
        String::new(),
        format!("- **Session**: `{session_id}`"),
        format!("- **File**: `{}`", session_path.display()),
        format!("- **Messages**: {}", session.messages.len()),
    ];
    if let Some(workspace_root) = session.workspace_root() {
        lines.push(format!("- **Workspace**: `{}`", workspace_root.display()));
    }
    if let Some(fork) = &session.fork {
        let branch = fork.branch_name.as_deref().unwrap_or("(unnamed)");
        lines.push(format!(
            "- **Forked from**: `{}` (branch `{branch}`)",
            fork.parent_session_id
        ));
    }
    if let Some(compaction) = &session.compaction {
        lines.push(format!(
            "- **Compactions**: {} (last removed {} messages)",
            compaction.count, compaction.removed_message_count
        ));
    }
    lines.push(String::new());
    lines.push("---".to_string());
    lines.push(String::new());

    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "System",
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Tool => "Tool",
        };
        lines.push(format!("## {}. {role}", index + 1));
        lines.push(String::new());
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => {
                    let trimmed = text.trim_end();
                    if !trimmed.is_empty() {
                        lines.push(trimmed.to_string());
                        lines.push(String::new());
                    }
                }
                ContentBlock::Thinking { .. } => {}
                ContentBlock::ToolUse { id, name, input } => {
                    lines.push(format!(
                        "**Tool call** `{name}` _(id `{}`)_",
                        crate::short_tool_id(id)
                    ));
                    let summary = summarize_tool_payload_for_markdown(input);
                    if !summary.is_empty() {
                        lines.push(format!("> {summary}"));
                    }
                    lines.push(String::new());
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => {
                    let status = if *is_error { "error" } else { "ok" };
                    lines.push(format!(
                        "**Tool result** `{tool_name}` _(id `{}`, {status})_",
                        crate::short_tool_id(tool_use_id)
                    ));
                    let summary = summarize_tool_payload_for_markdown(output);
                    if !summary.is_empty() {
                        lines.push(format!("> {summary}"));
                    }
                    lines.push(String::new());
                }
            }
        }
        if let Some(usage) = message.usage {
            lines.push(format!(
                "_tokens: in={} out={} cache_create={} cache_read={}_",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
            ));
            lines.push(String::new());
        }
    }
    lines.join("\n")
}
