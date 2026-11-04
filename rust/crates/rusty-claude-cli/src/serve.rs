//! B1：本地只读 Web 视图（单人自用）。
//!
//! 只绑定 `127.0.0.1`，所有 API 需要启动时打印的一次性 token。
//! 端点：
//!   GET /                          → 内嵌单页
//!   GET /api/sessions?t=           → 会话列表（按修改时间倒序，最多 50 条）
//!   GET /api/session/{id}?t=       → 单个会话的消息快照
//!   GET /api/session/{id}/stream?t=→ SSE 快照流（内容变化才推送，约 0.9s 一轮）
//!
//! 数据直接来自磁盘上的会话 JSONL；此阶段不写入、不驱动 turn。

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use api::{
    InputContentBlock, InputMessage, MessageRequest, OutputContentBlock, ProviderClient,
    SystemContent,
};

use crate::session_mgr::PRIMARY_SESSION_EXTENSION;

mod scheduler;

use scheduler::{ConcurrencyConfig, EnqueueError, EventHub, PromptJob, Scheduler, SessionState};

// React 前端的单文件构建产物（web/dist/index.html，由 web/ 下的 Vite 工程生成并提交）。
// 构建期不需要 Node：dist 已内联 JS/CSS。
const INDEX_HTML: &str = include_str!("../web/dist/index.html");
const MAX_SESSIONS: usize = 50;
const STREAM_INTERVAL: Duration = Duration::from_millis(900);

#[derive(Clone)]
struct AppState {
    token: String,
    /// 当前工作区的会话目录：`<workspace>/.claw/sessions/<fingerprint>/`。
    sessions: PathBuf,
    /// 工作区根，作为 turn 的 cwd。
    workspace: PathBuf,
    /// 实际监听端口（热升级时传给 helper，重启后沿用同一端口）。
    port: u16,
    /// 调度器：并发门闸 + 每会话排队 + 会话状态。
    scheduler: Arc<Scheduler>,
    /// 每会话独立事件流（不再是一条全局 broadcast 再按 session 过滤）。
    events: Arc<EventHub>,
    /// 等待用户在 Web 端批准的越权工具调用（id → 会话 + 应答通道）。
    approvals: Arc<Mutex<HashMap<String, PendingApproval>>>,
    /// 多工作区下串行切换进程 cwd（LiveCli 以 cwd 作为工作区根）。
    /// Phase 2 删除；Phase 1 并发度仍为 1，作为最后一道闸保留。
    turn_lock: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct PendingApproval {
    session: String,
    sender: std::sync::mpsc::Sender<bool>,
}

/// 提取 panic payload 的可读信息（`&str` / `String` 两种常见形态）。
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "未知 panic 载荷".to_string()
    }
}

#[derive(Deserialize)]
struct AuthQuery {
    t: Option<String>,
}

#[derive(Deserialize)]
struct PromptBody {
    text: String,
    /// 权限模式：read-only / workspace-write / danger-full-access（默认 workspace-write）。
    permission: Option<String>,
    /// 本轮使用的模型；缺省时用工作区配置的默认模型。
    model: Option<String>,
}

/// 语音讨论的单轮对话记录（前端持有，后端不落盘）。
#[derive(Deserialize)]
struct VoiceTurn {
    /// "user" 或 "assistant"。
    role: String,
    text: String,
}

/// 语音讨论请求体。
///
/// 与 [`PromptBody`] 的关键差别：这里**没有** permission / session 概念。
/// 语音讨论是纯对话，不执行任何工具、不写任何会话文件，因此不需要权限模式。
#[derive(Deserialize)]
struct VoiceChatBody {
    /// 讨论历史，按时间顺序；最后一条通常是用户刚说完的话。
    messages: Vec<VoiceTurn>,
    /// 本轮模型；缺省用工作区配置的默认模型。
    model: Option<String>,
    /// `true` 表示本次请求是「结束讨论，生成总结」。
    #[serde(default)]
    summarize: bool,
}

#[derive(Deserialize)]
struct TodoItemBody {
    text: String,
    #[serde(default)]
    done: bool,
}

#[derive(Deserialize)]
struct TodosBody {
    items: Vec<TodoItemBody>,
}

fn web_todos_path(workspace: &Path, id: &str) -> PathBuf {
    workspace
        .join(".claw")
        .join("web-todos")
        .join(format!("{id}.json"))
}

fn load_web_todos(workspace: &Path, id: &str) -> Vec<Value> {
    std::fs::read_to_string(web_todos_path(workspace, id))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value["items"].as_array().cloned())
        .unwrap_or_default()
}

/// 把 Web 端维护的待办清单作为上下文前缀注入 prompt，让 agent 看得到当前进度。
fn prompt_with_web_todos(workspace: &Path, id: &str, text: &str) -> String {
    let mut lines = Vec::new();
    for item in load_web_todos(workspace, id).iter().take(100) {
        let label = item["text"].as_str().unwrap_or("").trim();
        if label.is_empty() {
            continue;
        }
        let done = item["done"].as_bool().unwrap_or(false);
        lines.push(format!("- [{}] {}", if done { 'x' } else { ' ' }, label));
    }
    if lines.is_empty() {
        return text.to_string();
    }
    format!(
        "[Web 待办状态（用户在 Web 端维护，可作为当前任务清单）]\n{}\n\n{}",
        lines.join("\n"),
        text
    )
}

/// 上下文引用：`@path` / `@path:12` / `@path:12-40`（行号从 1 起）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mention {
    path: String,
    start: usize,
    end: usize,
}

/// 单次展开的引用数上限、单个引用的行数上限、整条消息的展开字符预算。
const MAX_MENTIONS: usize = 20;
const MAX_MENTION_LINES: usize = 200;
const MAX_MENTION_CHARS: usize = 40_000;

/// 解析 `12` / `12-40`。非法或倒序返回 None。
fn parse_line_range(text: &str) -> Option<(usize, usize)> {
    let (start, end) = match text.split_once('-') {
        Some((left, right)) => (left.parse().ok()?, right.parse().ok()?),
        None => {
            let line = text.parse().ok()?;
            (line, line)
        }
    };
    if start == 0 || end < start {
        return None;
    }
    Some((start, end))
}

/// 解析单个 `@...` token。不是合法引用返回 None，调用方据此保留原文。
fn parse_mention(token: &str) -> Option<Mention> {
    let body = token.strip_prefix('@')?;
    if body.is_empty() {
        return None;
    }
    // 从最后一个冒号试切行号：`src/a.rs:12-40`。Windows 盘符不会出现（路径恒为工作区相对路径），
    // 且行号段必须是纯数字，所以 `@a:b.rs` 这类仍会整体当作路径。
    if let Some((path, range)) = body.rsplit_once(':') {
        if !path.is_empty() {
            if let Some((start, end)) = parse_line_range(range) {
                return Some(Mention {
                    path: path.to_string(),
                    start,
                    end,
                });
            }
        }
    }
    Some(Mention {
        path: body.to_string(),
        start: 1,
        end: usize::MAX,
    })
}

/// 把引用渲染成「带行号的代码块」注入 prompt，省掉 agent 一次 read_file 往返。
fn render_mention(root: &Path, mention: &Mention, budget: usize) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let path = resolve_in_workspace(root, &mention.path).ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    // lines() 顺带吃掉 CRLF 的 \r，行号与编辑器一致。
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 || mention.start > total {
        return None;
    }
    let end = mention
        .end
        .min(total)
        .min(mention.start + MAX_MENTION_LINES - 1);
    let width = end.to_string().len();
    let mut block = String::new();
    block.push_str(&format!(
        "[引用 {} 第 {}-{} 行（文件共 {} 行）]\n",
        mention.path, mention.start, end, total
    ));
    for (offset, line) in lines[mention.start - 1..end].iter().enumerate() {
        let number = mention.start + offset;
        block.push_str(&format!("{number:>width$}| {line}\n"));
    }
    // 行数按 EOF 收敛后再算剩余量：请求 2-999 而文件只有 2 行时不该出现「还有 0 行」。
    let remaining = mention.end.min(total) - end;
    if remaining > 0 {
        block.push_str(&format!("…（该引用还有 {remaining} 行未展开）\n"));
    }
    // 预算不足时整块丢弃，避免把半截代码塞进上下文。
    if block.len() > budget {
        return None;
    }
    Some(block)
}

/// 展开消息里所有 `@path[:range]` 引用，其余字节原样保留。
fn expand_mentions(root: &Path, text: &str) -> String {
    const BOUNDARY_BEFORE: &str = "([{\"'";
    // 注意：`:` 不能作为 token 终止符，否则 `@a.rs:12-40` 会在冒号处被截断、行号丢失。
    // 行号段的合法性交给 parse_mention 判断（非数字即整体回退为路径）。
    const BOUNDARY_AFTER: &str = ")]}\"'`,;。，、";
    let mut out = String::with_capacity(text.len());
    let mut expanded = 0usize;
    let mut used = 0usize;
    let mut index = 0usize;
    while index < text.len() {
        let ch = text[index..].chars().next().unwrap_or(' ');
        if ch == '@' {
            let left_ok = index == 0
                || text[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|prev| prev.is_whitespace() || BOUNDARY_BEFORE.contains(prev));
            if left_ok {
                let start = index + 1;
                let mut end = start;
                for (offset, current) in text[start..].char_indices() {
                    if current.is_whitespace() || BOUNDARY_AFTER.contains(current) {
                        break;
                    }
                    end = start + offset + current.len_utf8();
                }
                if end > start {
                    let token = &text[index..end];
                    if let Some(mention) = parse_mention(token) {
                        if expanded < MAX_MENTIONS {
                            let budget = MAX_MENTION_CHARS.saturating_sub(used);
                            if let Some(block) = render_mention(root, &mention, budget) {
                                used += block.len();
                                expanded += 1;
                                out.push_str(&block);
                                index = end;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// 已登记工作区注册表：`~/.claw/web-workspaces.json`（数组 of path）。
fn workspaces_registry_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".claw").join("web-workspaces.json"))
}

fn load_workspaces() -> Vec<PathBuf> {
    let Some(path) = workspaces_registry_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(raw) = item.as_str() else {
            continue;
        };
        let path = PathBuf::from(raw);
        if path.is_dir() && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn save_workspaces(list: &[PathBuf]) {
    let Some(path) = workspaces_registry_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let value = Value::Array(
        list.iter()
            .map(|item| Value::String(item.display().to_string()))
            .collect(),
    );
    let _ = atomic_write(&path, &value.to_string());
}

fn register_workspace(path: &Path) {
    let mut list = load_workspaces();
    if list.iter().any(|item| item == path) {
        return;
    }
    list.push(path.to_path_buf());
    save_workspaces(&list);
}

/// 解析请求里的 workspace 参数：缺省=当前默认工作区；显式路径必须存在，并自动登记。
fn resolve_workspace(
    state: &AppState,
    requested: Option<&str>,
) -> Result<PathBuf, (StatusCode, String)> {
    let Some(raw) = requested.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(state.workspace.clone());
    };
    let canonical = std::fs::canonicalize(raw)
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("工作区不存在：{raw}")))?;
    let path = strip_verbatim(canonical);
    if !path.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "工作区不是目录".to_string()));
    }
    register_workspace(&path);
    Ok(path)
}

fn session_roots(state: &AppState) -> Vec<PathBuf> {
    let mut roots = vec![state.workspace.clone()];
    for root in load_workspaces() {
        if root != state.workspace && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

fn validate_session_id(id: &str) -> Result<(), (StatusCode, String)> {
    let valid = !id.is_empty()
        && !id.contains("..")
        && !id.contains('/')
        && !id.contains('\\')
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err((StatusCode::BAD_REQUEST, "无效的会话 ID".to_string()))
    }
}

fn find_session_file(dir: &Path, id: &str) -> Option<PathBuf> {
    session_files(dir)
        .into_iter()
        .find(|path| path.file_stem().and_then(|stem| stem.to_str()) == Some(id))
}

/// 跨工作区定位会话：先当前工作区，再已登记工作区。返回 (会话文件, 工作区根)。
fn resolve_session(state: &AppState, id: &str) -> Result<(PathBuf, PathBuf), (StatusCode, String)> {
    validate_session_id(id)?;
    for root in session_roots(state) {
        let Ok(store) = runtime::SessionStore::from_cwd(&root) else {
            continue;
        };
        if let Some(path) = find_session_file(store.sessions_dir(), id) {
            return Ok((path, root));
        }
    }
    Err((StatusCode::NOT_FOUND, format!("会话 {id} 不存在")))
}

/// `.claw/sessions/<fp>/file.jsonl` → `<workspace>`。
fn workspace_of_session_path(path: &Path) -> Option<PathBuf> {
    path.ancestors().nth(3).map(Path::to_path_buf)
}

#[derive(Deserialize)]
struct PathQuery {
    t: Option<String>,
    path: Option<String>,
    /// 非默认工作区时指定（多工作区文件/预览/diff）。
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct WebSettingsBody {
    theme: Option<String>,
    permission: Option<String>,
    panel_width: Option<u32>,
    panel_open: Option<bool>,
    panel_tab: Option<String>,
    panel_side: Option<String>,
    sidebar_collapsed: Option<bool>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct WriteBody {
    path: String,
    content: String,
    /// 读取时的 mtime（毫秒）；不匹配说明文件在编辑期间被改动。
    expected_mtime_ms: Option<u64>,
    /// 非默认工作区时指定。
    workspace: Option<String>,
}

const IGNORED_DIRS: [&str; 7] = [
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".claw",
    ".venv",
];
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 512 * 1024;
const MAX_FILES: usize = 2000;

/// 解析参数并启动服务；阻塞到进程退出。
///
/// `claw-web.exe`（同一二进制的副本/硬链接）双击启动时等价于 `claw serve`：
/// 未显式给 `--cwd` 时按"当前目录 → 上次工作区 → 用户主目录"推断，
/// 端口被占用时自动改用系统分配端口。
pub fn run_web(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut port: u16 = 4317;
    let mut open = true;
    let mut cwd: Option<PathBuf> = None;
    let mut arg_index = 0;
    while arg_index < args.len() {
        match args[arg_index].as_str() {
            "--port" | "-p" => {
                arg_index += 1;
                port = args
                    .get(arg_index)
                    .and_then(|value| value.parse().ok())
                    .ok_or("--port 需要一个有效端口号")?;
            }
            "--cwd" | "-C" => {
                arg_index += 1;
                let raw = args.get(arg_index).ok_or("--cwd 需要一个目录")?;
                cwd = Some(
                    std::fs::canonicalize(raw)
                        .map_err(|error| format!("--cwd {raw} 无效: {error}"))?,
                );
            }
            "--no-open" => open = false,
            "--open" => open = true,
            other if cwd.is_none() && Path::new(other).is_dir() => {
                // 支持把项目文件夹拖到 claw-web.exe 上：第一个存在的目录即工作区。
                cwd = Some(std::fs::canonicalize(other)?);
            }
            other => {
                return Err(format!(
                    "未知参数 {other}；用法: claw serve [--port 4317] [--cwd DIR] [--no-open]"
                )
                .into())
            }
        }
        arg_index += 1;
    }

    let cwd = match cwd {
        Some(path) => strip_verbatim(path),
        None => strip_verbatim(default_workspace()),
    };
    // 进程 cwd 固定到中性临时目录：工具层/hooks/LSP 的任何“漏改”隐式 cwd
    // 都会落到这里而不是某个用户工作区，便于测试暴露跨工作区串味。
    // 真正的执行目录由 `TurnEnv` 逐会话显式传入。
    let neutral_dir = std::env::temp_dir().join("claw-serve");
    std::fs::create_dir_all(&neutral_dir)?;
    std::env::set_current_dir(&neutral_dir)?;
    // 默认工作区自动登记，侧栏才能把它的会话与其它工作区一起展示。
    register_workspace(&cwd);
    let store = runtime::SessionStore::from_cwd(&cwd)?;
    let sessions = store.sessions_dir().to_path_buf();
    // 热升级交接：helper 通过 CLAW_WEB_TOKEN 传入旧 token，新进程复用它，
    // 已打开的页面才能在服务重启后继续重连。手动启动则始终生成新 token。
    let token = web_token_from_env().unwrap_or_else(generate_token);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(error) => {
                // 热升级交接：旧进程刚 exit，Windows 上端口可能还被残留的监听
                // 条目占着（owner 已是死 PID）。此时必须等它释放并复用同一端口，
                // 否则会退回随机端口，已打开的页面就失联了。
                if std::env::var("CLAW_WEB_TOKEN").is_ok() && port != 0 {
                    eprintln!("端口 {port} 暂未释放（{error}），等待旧进程退出后接管…");
                    let mut bound = None;
                    for _ in 0..40 {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
                            bound = Some(listener);
                            break;
                        }
                    }
                    match bound {
                        Some(listener) => listener,
                        None => {
                            eprintln!("端口 {port} 10 秒内未释放，退回随机端口（页面需手动用新 URL 打开）。");
                            tokio::net::TcpListener::bind(SocketAddr::from((
                                Ipv4Addr::LOCALHOST,
                                0,
                            )))
                            .await?
                        }
                    }
                } else {
                    eprintln!("端口 {port} 不可用（{error}），改用系统分配端口。");
                    tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?
                }
            }
        };
        let actual_port = listener.local_addr()?.port();
        // Windows 句柄继承陷阱：serve 会 spawn helper / PTY / git 等子进程，
        // 默认它们会继承监听 socket。旧进程退出后，子进程仍持有 4399 的
        // LISTEN 句柄，新进程就永远 bind 不上。这里显式清掉继承标志。
        make_listener_non_inheritable(&listener);
        let scheduler = Arc::new(Scheduler::new(ConcurrencyConfig::from_env()));
        let state = AppState {
            token: token.clone(),
            sessions,
            workspace: cwd.clone(),
            port: actual_port,
            scheduler,
            events: Arc::new(EventHub::default()),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            turn_lock: Arc::new(Mutex::new(())),
        };
        spawn_session_watchdog(state.clone());
        let app = Router::new()
            .route("/", get(index))
            .route("/api/sessions", get(api_sessions).post(api_create_session))
            .route(
                "/api/session/{id}",
                get(api_session)
                    .patch(api_session_rename)
                    .delete(api_session_delete),
            )
            .route("/api/session/{id}/stream", get(api_stream))
            .route("/api/session/{id}/prompt", post(api_prompt))
            .route("/api/session/{id}/interrupt", post(api_interrupt))
            .route("/api/session/{id}/status", get(api_status))
            .route("/api/workspace/files", get(api_files))
            .route("/api/workspace/file", get(api_file).post(api_file_write))
            .route("/api/workspace/raw", get(api_raw))
            .route("/api/workspace/diff", get(api_diff))
            .route("/api/workspace/diff/discard", post(api_diff_discard))
            .route("/api/workspace/goal", get(api_goal))
            .route("/api/workspace/plans", get(api_plans))
            .route("/api/attachments", get(api_attachments))
            .route(
                "/api/workspaces",
                get(api_workspaces).post(api_workspace_add),
            )
            .route("/api/workspaces/pick", post(api_workspace_pick))
            .route("/api/client-error", post(api_client_error))
            .route("/api/upload", post(api_upload))
            .route("/api/meta", get(api_meta))
            .route("/ws/terminal", get(ws_terminal))
            .route("/api/session/{id}/tasks", get(api_tasks))
            .route(
                "/api/session/{id}/todos",
                get(api_todos_get).post(api_todos_set),
            )
            .route("/api/session/{id}/events", get(api_events))
            .route("/api/session/{id}/approval", post(api_approval))
            // 语音讨论：无工具纯对话，与 agent 会话完全隔离（不落盘、不碰工作区）。
            .route("/api/voice/chat", post(api_voice_chat))
            .route(
                "/api/web-settings",
                get(api_web_settings_get).post(api_web_settings_set),
            )
            .with_state(state);

        let url = format!("http://127.0.0.1:{actual_port}/?t={token}");
        println!("claw web: {url}");
        println!("工作区: {}", cwd.display());
        println!("按 Ctrl+C 停止。");
        save_last_workspace(&cwd);
        if open {
            open_browser(&url);
        }

        axum::serve(listener, app).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}

/// 双击启动时的默认工作区：当前目录若像项目就用它，否则用上次的项目，最后退回主目录。
fn default_workspace() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if looks_like_workspace(&cwd) {
        return cwd;
    }
    if let Some(saved) = load_last_workspace() {
        if saved.is_dir() {
            return saved;
        }
    }
    home_dir().unwrap_or(cwd)
}

fn looks_like_workspace(path: &Path) -> bool {
    // 用户主目录里的 `.claw` 是配置目录，不能作为“这是项目”的判据。
    if home_dir().as_deref() == Some(path) {
        return false;
    }
    [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        ".git",
    ]
    .iter()
    .any(|marker| path.join(marker).exists())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Windows 的 canonicalize 会带上 `\\?\` 前缀；显示与持久化时去掉，避免用户困惑。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

fn last_workspace_file() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".claw").join("web-last-workspace.txt"))
}

fn load_last_workspace() -> Option<PathBuf> {
    let path = last_workspace_file()?;
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn save_last_workspace(workspace: &Path) {
    let Some(path) = last_workspace_file() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, workspace.display().to_string());
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[derive(Deserialize)]
struct SessionsQuery {
    t: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    /// 聚合所有已登记工作区的会话（侧栏多工作区视图）。接受 `1`/`true`。
    all: Option<String>,
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct CreateSessionBody {
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceQuery {
    t: Option<String>,
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceAddBody {
    path: String,
}

async fn api_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionsQuery>,
) -> Response {
    if query.t.as_deref() != Some(state.token.as_str()) {
        return unauthorized();
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0);

    if matches!(query.all.as_deref(), Some("1" | "true")) {
        let mut sessions: Vec<Value> = Vec::new();
        for root in session_roots(&state) {
            let Ok(store) = runtime::SessionStore::from_cwd(&root) else {
                continue;
            };
            if let Ok(Value::Object(map)) = list_sessions(store.sessions_dir(), &root, 0, 50) {
                if let Some(items) = map.get("sessions").and_then(Value::as_array) {
                    sessions.extend(items.iter().cloned());
                }
            }
        }
        sessions.sort_by_key(|item| std::cmp::Reverse(item["modified"].as_u64().unwrap_or(0)));
        let total = sessions.len();
        let page: Vec<Value> = sessions.into_iter().skip(offset).take(limit).collect();
        return Json(json!({
            "sessions": page,
            "total": total,
            "offset": offset,
            "limit": limit,
        }))
        .into_response();
    }

    let workspace = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    let Ok(store) = runtime::SessionStore::from_cwd(&workspace) else {
        return (StatusCode::BAD_REQUEST, "无法读取该工作区的会话目录").into_response();
    };
    match list_sessions(store.sessions_dir(), &workspace, offset, limit) {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

#[derive(Deserialize)]
struct RenameBody {
    title: String,
}

async fn api_session_rename(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<RenameBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let (_path, workspace) = match resolve_session(&state, &id) {
        Ok(found) => found,
        Err((code, message)) => return (code, message).into_response(),
    };
    let title = body.title.trim();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "标题不能为空").into_response();
    }
    let title = truncate_chars(title, 120);
    match save_session_title(&workspace, &id, &title) {
        Ok(()) => Json(json!({ "id": id, "title": title })).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn api_session_delete(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let (path, workspace) = match resolve_session(&state, &id) {
        Ok(found) => found,
        Err((code, message)) => return (code, message).into_response(),
    };
    if state.scheduler.is_active(&id) {
        return (StatusCode::CONFLICT, "会话正在执行或仍在排队，不能删除").into_response();
    }
    if let Err(error) = std::fs::remove_file(&path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    state.scheduler.forget(&id);
    state.events.forget(&id);
    remove_session_title(&workspace, &id);
    let _ = std::fs::remove_file(web_todos_path(&workspace, &id));
    Json(json!({ "deleted": id })).into_response()
}

async fn api_session(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let path = match resolve_session(&state, &id) {
        Ok((path, _workspace)) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    match session_view(&path) {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("会话不可读（可能只有 header 或已损坏）: {error}"),
        )
            .into_response(),
    }
}

async fn api_stream(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let path = match resolve_session(&state, &id) {
        Ok((path, _workspace)) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "会话不存在").into_response();
    }

    let stream = futures_util::stream::unfold((path, String::new()), |(path, last)| async move {
        tokio::time::sleep(STREAM_INTERVAL).await;
        let payload = session_view(&path)
            .map(|value| serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|error| json!({ "error": error }).to_string());
        if payload == last {
            return Some((
                Ok::<Event, Infallible>(Event::default().event("keepalive").data("")),
                (path, last),
            ));
        }
        Some((
            Ok::<Event, Infallible>(Event::default().event("snapshot").data(payload.clone())),
            (path, payload),
        ))
    });

    Sse::new(stream).into_response()
}

const PERMISSION_MODES: [&str; 3] = ["read-only", "workspace-write", "danger-full-access"];

/// Web 端审批：danger-full-access 直接放行；其余模式把越权工具发给前端，
/// 阻塞等待用户点击“允许/拒绝”（120 秒超时按拒绝处理）。
struct WebPermissionPrompter {
    mode: runtime::PermissionMode,
    session_id: String,
    events: Arc<EventHub>,
    approvals: Arc<Mutex<HashMap<String, PendingApproval>>>,
}

/// 审批 id 的会话内序号：并发下毫秒时间戳会撞车，必须由单调计数器消歧。
static APPROVAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn permission_label(mode: runtime::PermissionMode) -> &'static str {
    match mode {
        runtime::PermissionMode::ReadOnly => "只读",
        runtime::PermissionMode::WorkspaceWrite => "工作区可写",
        runtime::PermissionMode::DangerFullAccess => "完全访问",
        _ => "更高",
    }
}

impl runtime::PermissionPrompter for WebPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        if self.mode == runtime::PermissionMode::DangerFullAccess {
            return runtime::PermissionPromptDecision::Allow;
        }

        let id = format!(
            "approval-{}-{}",
            self.session_id,
            APPROVAL_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1
        );
        let (sender, receiver) = std::sync::mpsc::channel::<bool>();
        {
            let mut map = self
                .approvals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(
                id.clone(),
                PendingApproval {
                    session: self.session_id.clone(),
                    sender,
                },
            );
        }
        self.events.emit(
            &self.session_id,
            json!({
                "kind": "approval_request",
                "id": id,
                "tool": request.tool_name,
                "input": truncate_chars(&request.input, 2000),
                "required_mode": permission_label(request.required_mode),
                "reason": request.reason,
            }),
        );

        let allowed = receiver
            .recv_timeout(Duration::from_secs(120))
            .unwrap_or(false);
        self.approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);

        if allowed {
            runtime::PermissionPromptDecision::Allow
        } else {
            runtime::PermissionPromptDecision::Deny {
                reason: "用户拒绝或审批超时（120 秒）".to_string(),
            }
        }
    }
}

/// 一次 Web turn 的输入。
struct WebTurn<'a> {
    session_path: &'a Path,
    workspace: PathBuf,
    lock: Arc<Mutex<()>>,
    text: &'a str,
    mode: runtime::PermissionMode,
    model_override: Option<String>,
    signal: runtime::HookAbortSignal,
    events: Arc<EventHub>,
    approvals: Arc<Mutex<HashMap<String, PendingApproval>>>,
    session_id: &'a str,
    /// 流式事件心跳，看门狗据此判断本轮是否还在推进。
    activity: ActivityClock,
}

/// 活动心跳：每个流式事件都会刷新，供看门狗判断“是否还有进展”。
type ActivityClock = Arc<AtomicU64>;

/// 会话级看门狗：一个后台线程扫描所有在跑的会话，替代「每 turn 一个
/// sleep 线程」的旧实现。语义保持不变：超过阈值没有任何流式事件就发
/// error + done 并释放会话占用。阈值由 `CLAW_WEB_TURN_WATCHDOG_SECS`
/// 覆盖，默认 900 秒，`0` 表示禁用。
fn spawn_session_watchdog(state: AppState) {
    let threshold_secs = std::env::var("CLAW_WEB_TURN_WATCHDOG_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(900);
    if threshold_secs == 0 {
        return;
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        let threshold_ms = threshold_secs.saturating_mul(1000);
        for turn in state.scheduler.expired(threshold_ms) {
            // 抢占收尾权：失败说明 turn 刚好自己结束了，交给它正常收尾。
            if turn.finished.swap(true, Ordering::SeqCst) {
                continue;
            }
            // 尝试协作式中止（LLM/工具循环会检查该信号）。
            turn.signal.abort();
            state.events.emit(
                &turn.session,
                json!({
                    "kind": "error",
                    "message": format!(
                        "[看门狗] 已 {} 秒没有任何输出，本轮已中止；后台线程可能仍在收尾。",
                        turn.silent_ms / 1000
                    ),
                }),
            );
            state.scheduler.force_finish(&turn.session);
            state.events.emit(&turn.session, json!({ "kind": "done" }));
        }
    });
}

/// 在 serve 进程内跑一轮（不再拉子进程），从而拿到 token 级流式事件。
fn run_web_turn(turn: WebTurn<'_>) -> Result<(), String> {
    let WebTurn {
        session_path,
        workspace,
        lock,
        text,
        mode,
        model_override,
        signal,
        events,
        approvals,
        session_id,
        activity,
    } = turn;
    // 进程 cwd 固定在启动时的中性临时目录；这里不再切换，所有路径都由
    // `workspace` 显式传入 LiveCli / 工具层。
    let _turn_guard = acquire_turn_lock(&lock, Duration::from_secs(10))?;
    let model = model_override
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| {
            crate::config_model_for(&workspace).unwrap_or_else(|| crate::DEFAULT_MODEL.to_string())
        });
    let mut cli = crate::app::LiveCli::new(
        model,
        true,
        None,
        mode,
        Vec::new(),
        crate::render::OutputVerbosity::default(),
        &workspace,
    )
    .map_err(|error| error.to_string())?;
    cli.resume_from_path(session_path)
        .map_err(|error| error.to_string())?;
    cli.set_external_abort_signal(signal);

    let sender = Arc::clone(&events);
    let sid = session_id.to_string();
    let activity_clock = Arc::clone(&activity);
    cli.set_status_emitter(Arc::new(move |event: runtime::StatusEvent| {
        // 任何流式事件都算“有进展”，刷新看门狗的心跳。
        activity_clock.store(now_ms(), Ordering::SeqCst);
        let payload = match event {
            runtime::StatusEvent::TextDelta(text) => {
                json!({ "session": sid, "kind": "delta", "text": text })
            }
            runtime::StatusEvent::ToolUse { id, name, input } => {
                json!({ "session": sid, "kind": "tool_use", "id": id, "name": name, "input": input })
            }
            runtime::StatusEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => json!({ "session": sid, "kind": "tool_result", "id": id, "name": name, "output": output, "is_error": is_error }),
            runtime::StatusEvent::Thinking { text, done, .. } => {
                json!({ "session": sid, "kind": "thinking", "text": text, "done": done })
            }
            runtime::StatusEvent::StreamError { message, recoverable } => {
                json!({ "session": sid, "kind": "error", "message": message, "recoverable": recoverable })
            }
            runtime::StatusEvent::StreamStart => json!({ "session": sid, "kind": "start" }),
            runtime::StatusEvent::MessageStop => json!({ "session": sid, "kind": "message_stop" }),
            runtime::StatusEvent::Usage(_) => return,
        };
        sender.emit(&sid, payload);
    }));

    let mut prompter = WebPermissionPrompter {
        mode,
        session_id: session_id.to_string(),
        events: events.clone(),
        approvals,
    };
    cli.run_turn_silent_with(text, &mut prompter)
        .map_err(|error| error.to_string())
}

/// 每会话 dispatcher：串行处理自己的队列，只有拿到全局 + 工作区许可后才真正开跑。
///
/// 排队本身不再是一次请求的生命周期：`POST /prompt` 入队后立即 202 返回，
/// 真正等待许可发生在该线程里。
fn spawn_session_dispatcher(state: AppState, session_id: String) {
    let task_state = state.clone();
    let worker_session = session_id.clone();
    let thread_name = format!(
        "claw-web-{}",
        &worker_session[..worker_session.len().min(12)]
    );
    let spawned = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || loop {
            let Some(turn) = task_state.scheduler.take_next(&worker_session) else {
                return;
            };
            // 已被取消的排队项无需再等许可。
            if turn.signal.is_aborted() {
                if !turn.finished.swap(true, Ordering::SeqCst) {
                    task_state
                        .events
                        .emit(&worker_session, json!({ "kind": "done" }));
                }
                task_state.scheduler.finish(&worker_session);
                continue;
            }
            let permits = task_state.scheduler.acquire_permits(&turn.job.workspace);
            // 等许可期间被 interrupt：直接放弃这一轮。
            if turn.signal.is_aborted() {
                if !turn.finished.swap(true, Ordering::SeqCst) {
                    task_state
                        .events
                        .emit(&worker_session, json!({ "kind": "done" }));
                }
                task_state.scheduler.finish(&worker_session);
                drop(permits);
                continue;
            }
            task_state.scheduler.mark_running(&worker_session);

            let workspace = turn.job.workspace.clone();
            let session_path = turn.job.session_path;
            let text = turn.job.text;
            let mode = turn.job.permission;
            let model = turn.job.model;
            let activity = Arc::clone(&turn.activity);
            let signal = turn.signal.clone();
            // turn 线程一旦 panic，必须转成普通错误，否则前端永久转圈。
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_web_turn(WebTurn {
                    session_path: &session_path,
                    workspace: workspace.clone(),
                    lock: Arc::clone(&task_state.turn_lock),
                    text: &text,
                    mode,
                    model_override: model,
                    signal,
                    events: Arc::clone(&task_state.events),
                    approvals: Arc::clone(&task_state.approvals),
                    session_id: &worker_session,
                    activity,
                })
            }))
            .unwrap_or_else(|payload| {
                Err(format!(
                    "本轮执行线程异常终止（panic）：{}",
                    panic_payload_message(&*payload)
                ))
            });
            let failed = outcome.is_err();
            if let Err(message) = &outcome {
                task_state.events.emit(
                    &worker_session,
                    json!({ "kind": "error", "message": message }),
                );
            }
            // 收尾权由 dispatcher 与看门狗共享：谁先 swap 成功谁发 done。
            if !turn.finished.swap(true, Ordering::SeqCst) {
                task_state
                    .events
                    .emit(&worker_session, json!({ "kind": "done" }));
            }
            task_state.scheduler.finish(&worker_session);
            drop(permits);

            // Agent 在 turn 里调用了 `HotUpgrade`：只有全服务已安静
            // （无执行中、无排队）时才允许重启，否则把请求留在原地。
            if !failed && task_state.scheduler.is_quiet() && tools::take_hot_upgrade_request() {
                begin_web_upgrade(
                    &workspace,
                    task_state.port,
                    &task_state.token,
                    &worker_session,
                    &task_state.events,
                );
            }
        });
    if let Err(error) = spawned {
        // 线程起不来：清掉队列并明确告知，避免该会话永久停在 queued。
        state.scheduler.interrupt(&session_id);
        state.scheduler.force_finish(&session_id);
        state.events.emit(
            &session_id,
            json!({ "kind": "error", "message": format!("调度线程启动失败：{error}") }),
        );
        state.events.emit(&session_id, json!({ "kind": "done" }));
    }
}

/// 写入通道：入队后立即 202 返回状态与排队位置；流式事件走
/// `/api/session/{id}/events`。不再返回「等待工作区锁」这类错误。
async fn api_prompt(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<PromptBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    if body.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "消息内容为空").into_response();
    }
    let (path, session_workspace) = match resolve_session(&state, &id) {
        Ok(found) => found,
        Err((code, message)) => return (code, message).into_response(),
    };
    let permission = body.permission.as_deref().unwrap_or("workspace-write");
    if !PERMISSION_MODES.contains(&permission) {
        return (
            StatusCode::BAD_REQUEST,
            format!("不支持的权限模式：{permission}"),
        )
            .into_response();
    }

    // `@path:12-40` 这类引用直接展开成带行号的代码块，省掉 agent 首轮 read_file 往返。
    let expanded_text = expand_mentions(&session_workspace, &body.text);
    let text = prompt_with_web_todos(&session_workspace, &id, &expanded_text);
    let job = PromptJob {
        session_id: id.clone(),
        session_path: path,
        workspace: session_workspace,
        text,
        permission: crate::permission_mode_from_label(permission),
        model: body.model.clone(),
    };

    match state.scheduler.enqueue(job) {
        Err(EnqueueError::QueueFull) => (
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "该会话排队已满（最多 {} 条），请等待前面的消息处理完成后再发送",
                scheduler::MAX_QUEUE_PER_SESSION
            ),
        )
            .into_response(),
        Ok(enqueued) => {
            if enqueued.start_worker {
                spawn_session_dispatcher(state.clone(), id.clone());
            }
            (
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "queued",
                    "state": SessionState::Queued.label(),
                    "position": enqueued.position,
                    "session": id,
                })),
            )
                .into_response()
        }
    }
}

/// 流式事件 SSE：每个会话一条独立广播，只转发本会话事件。
///
/// 缓冲不足时不再静默 `continue`，而是补一条 `kind=reset` 事件（带当前
/// 单调序号与丢弃条数），前端可据此重拉快照。
async fn api_events(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let receiver = state.events.subscribe(&id);
    let hub = Arc::clone(&state.events);
    let stream =
        futures_util::stream::unfold((receiver, id, hub), |(mut receiver, id, hub)| async move {
            match receiver.recv().await {
                Ok(value) => Some((
                    Ok::<Event, Infallible>(
                        Event::default().event("stream").data(value.to_string()),
                    ),
                    (receiver, id, hub),
                )),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    let reset = hub.next_reset(&id, dropped as usize);
                    Some((
                        Ok::<Event, Infallible>(
                            Event::default().event("stream").data(reset.to_string()),
                        ),
                        (receiver, id, hub),
                    ))
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            }
        });
    Sse::new(stream).into_response()
}

/// 语音讨论的引导式系统提示词。
///
/// 关键约束「绝不调用工具」不是客套：语音讨论若挂上工具与工作区上下文，
/// 模型会开始查证代码、把对话变成任务执行，用户就失去了「聊天理思路」
/// 这个产品意图。因此该端点刻意不经过 `LiveCli`。
const VOICE_CHAT_SYSTEM: &str = "\
你是用户的思路整理伙伴，正在通过语音与他对话。

规则：
1. 绝不调用任何工具，不读文件、不查代码、不执行命令。你只负责聊天。
2. 用户的话来自语音识别，可能有错字、断句错误和口语重复。按最合理的理解往下接，不要纠错、不要打断。
3. 你的任务不是替他回答，而是帮他把想法理清楚：追问含糊的地方、指出自相矛盾之处、补全他没想到的角度、在岔路口给出两三个方向让他挑。
4. 回复必须短，一到三句，像真人在说话，不要分点罗列长文——这段文字会被朗读出来。一次只问一个问题。
5. 不要复述他说过的话，直接推进。";

/// 结束讨论时的总结提示词。
const VOICE_SUMMARY_SYSTEM: &str = "\
把用户这轮语音讨论整理成一份可以直接拿去用的需求描述。

要求：
- 用中文，markdown，严格按下面的结构，不要增删小节；
- 直接给结论，不要寒暄，不要复述讨论过程；
- 只写讨论中真实出现过的内容，没聊到的写「未定」，不要替他编造；
- 全文控制在 400 字以内。

## 目标
## 关键决策
## 待确认
## 下一步";

/// 总结轮追加的 user 消息（触发模型收尾）。
const VOICE_SUMMARY_INSTRUCTION: &str = "讨论到这里，请按约定结构输出总结。";

/// 语音讨论：纯对话端点（无工具、无会话、不落盘）。
///
/// 与 `/api/session/{id}/prompt` 的本质区别：
/// - 不走 `LiveCli`，因此没有工具循环、没有 repomap 系统提示词、不写会话 JSONL；
/// - 讨论历史由前端持有，后端每次收全量再转发给模型（无状态）。
async fn api_voice_chat(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<VoiceChatBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    if body.messages.is_empty() {
        return (StatusCode::BAD_REQUEST, "讨论内容为空").into_response();
    }
    let model = body
        .model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| {
            crate::config_model_for(&state.workspace)
                .unwrap_or_else(|| crate::DEFAULT_MODEL.to_string())
        });

    let mut messages: Vec<InputMessage> = body
        .messages
        .iter()
        .map(|turn| InputMessage {
            role: if turn.role == "assistant" {
                "assistant".to_string()
            } else {
                "user".to_string()
            },
            content: vec![InputContentBlock::Text {
                text: turn.text.clone(),
            }],
        })
        .collect();
    if body.summarize {
        messages.push(InputMessage::user_text(VOICE_SUMMARY_INSTRUCTION));
    }

    let request = MessageRequest {
        model: model.clone(),
        // 讨论轮短（要朗读），总结轮才需要更长篇幅。
        max_tokens: if body.summarize { 2000 } else { 800 },
        messages,
        system: Some(SystemContent::from_text(if body.summarize {
            VOICE_SUMMARY_SYSTEM
        } else {
            VOICE_CHAT_SYSTEM
        })),
        stream: false,
        // 语音对话要求低延迟，关掉思维链。
        thinking_mode: Some(false),
        ..Default::default()
    };

    let client = match ProviderClient::from_model(&model) {
        Ok(client) => client,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("创建模型客户端失败：{error}"),
            )
                .into_response()
        }
    };
    let response = match client.send_message(&request).await {
        Ok(response) => response,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, format!("模型调用失败：{error}")).into_response()
        }
    };
    let reply = response
        .content
        .iter()
        .filter_map(|block| match block {
            OutputContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string();
    if reply.is_empty() {
        return (StatusCode::BAD_GATEWAY, "模型返回了空回复").into_response();
    }
    Json(json!({ "reply": reply })).into_response()
}

async fn api_interrupt(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let interrupted = state.scheduler.interrupt(&id);
    if interrupted.aborted || interrupted.dropped > 0 {
        (
            StatusCode::OK,
            Json(json!({
                "status": "interrupted",
                "session": id,
                "dropped": interrupted.dropped,
            })),
        )
            .into_response()
    } else {
        (StatusCode::CONFLICT, "该会话没有正在执行的任务").into_response()
    }
}

/// 会话是否正在跑 turn（前端据此禁用在跑期间的发送按钮并显示状态）。
async fn api_status(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let status = state.scheduler.status(&id);
    Json(json!({
        "session": id,
        "state": status.state.label(),
        "position": status.position,
        "queued": status.queued,
        // 向后兼容：前端只用 running 判断是否禁用发送。
        "running": status.state != SessionState::Idle,
    }))
    .into_response()
}

// ---------------- P3：终端面板（PTY + WebSocket） ----------------

/// WebSocket 终端：升级后在工作区里开一个真实 PTY（Windows 为 ConPTY）。
async fn ws_terminal(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let workspace = state.workspace.clone();
    ws.on_upgrade(move |socket| terminal_session(socket, workspace))
}

struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
    /// 必须持有 master，否则 drop 会关闭 ConPTY 并让子进程立刻退出。
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

fn spawn_pty(
    workspace: &Path,
    output: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<PtySession, String> {
    use std::io::Read;

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| error.to_string())?;

    // Windows 上用 COMSPEC（cmd.exe）最稳；PowerShell 路径因机器而异。
    let shell = if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string())
    } else {
        "/bin/bash".to_string()
    };
    let mut command = portable_pty::CommandBuilder::new(shell);
    command.cwd(workspace);
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| error.to_string())?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| error.to_string())?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| error.to_string())?;

    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if output.blocking_send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Write as _;
        while let Ok(data) = input_rx.recv() {
            if writer.write_all(&data).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    Ok(PtySession {
        child,
        input: input_tx,
        _master: pair.master,
    })
}

async fn terminal_session(mut socket: WebSocket, workspace: PathBuf) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let mut session = match spawn_pty(&workspace, tx) {
        Ok(session) => session,
        Err(error) => {
            let _ = socket
                .send(Message::Text(format!("无法启动终端: {error}\n").into()))
                .await;
            return;
        }
    };

    loop {
        tokio::select! {
            chunk = rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        if socket.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Binary(data))) => {
                        let _ = session.input.send(data.to_vec());
                    }
                    Some(Ok(Message::Text(text))) => {
                        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        match value["type"].as_str() {
                            Some("input") => {
                                if let Some(data) = value["data"].as_str() {
                                    let _ = session.input.send(data.as_bytes().to_vec());
                                }
                            }
                            Some("interrupt") => {
                                let _ = session.child.kill();
                                break;
                            }
                            Some("resize") => {
                                let cols = value["cols"].as_u64().unwrap_or(120).clamp(20, 400) as u16;
                                let rows = value["rows"].as_u64().unwrap_or(30).clamp(5, 200) as u16;
                                let _ = session._master.resize(portable_pty::PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                            _ => {
                                let _ = session.input.send(text.as_bytes().to_vec());
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    let _ = session.child.kill();
}

// ---------------- 工作区：文件 / 预览 / diff / 任务 / goal / Web 设置 ----------------

fn authorize_path(state: &AppState, query: &PathQuery) -> bool {
    query.t.as_deref() == Some(state.token.as_str())
}

fn resolve_in_workspace(root: &Path, rel: &str) -> Result<PathBuf, (StatusCode, String)> {
    if rel.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "path 不能为空".to_string()));
    }
    let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let candidate = root_canon.join(rel);
    let canonical = std::fs::canonicalize(&candidate)
        .map_err(|_| (StatusCode::NOT_FOUND, format!("文件不存在: {rel}")))?;
    if !canonical.starts_with(&root_canon) {
        return Err((StatusCode::FORBIDDEN, "path 越出工作区".to_string()));
    }
    Ok(canonical)
}

fn file_kind(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => "image",
        "pdf" => "pdf",
        "md" | "markdown" => "markdown",
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "c" | "h" | "cpp" | "hpp"
        | "cs" | "rb" | "php" | "sh" | "ps1" | "bat" | "toml" | "yaml" | "yml" | "json"
        | "jsonl" | "workflow" | "lock" | "txt" | "csv" | "log" | "env" | "ini" | "cfg"
        | "html" | "css" | "scss" | "sql" | "proto" => "text",
        _ => "binary",
    }
}

fn mime_for(path: &Path) -> &'static str {
    match file_kind(path) {
        "image" => match path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "svg" => "image/svg+xml",
            _ => "image/jpeg",
        },
        "pdf" => "application/pdf",
        "markdown" => "text/markdown; charset=utf-8",
        "text" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn api_files(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let root = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let mut files = Vec::new();
    collect_workspace_entries(&root, &root, 0, &mut files);
    Json(json!({
        "root": root.display().to_string(),
        "files": files,
    }))
    .into_response()
}

fn collect_workspace_entries(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Value>) {
    if depth > 6 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if out.len() >= MAX_FILES {
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_workspace_entries(root, &path, depth + 1, out);
        } else {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            out.push(json!({
                "path": relative.display().to_string().replace('\\', "/"),
                "size": size,
            }));
        }
    }
}

async fn api_file(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let Some(rel) = query.path.as_deref() else {
        return (StatusCode::BAD_REQUEST, "缺少 path").into_response();
    };
    let root = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let path = match resolve_in_workspace(&root, rel) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return (StatusCode::NOT_FOUND, "文件不存在").into_response();
    };
    if !metadata.is_file() {
        return (StatusCode::BAD_REQUEST, "不是文件").into_response();
    }
    if metadata.len() > MAX_FILE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "文件过大（{} MB），只做下载预览",
                metadata.len() / 1024 / 1024
            ),
        )
            .into_response();
    }

    let kind = file_kind(&path);
    let mtime_ms = modified_ms(&path);
    if kind == "image" || kind == "pdf" {
        return Json(json!({
            "kind": kind,
            "path": rel,
            "size": metadata.len(),
            "mtime_ms": mtime_ms,
        }))
        .into_response();
    }

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    let truncated = bytes.len() > MAX_TEXT_BYTES;
    let slice = if truncated {
        &bytes[..MAX_TEXT_BYTES]
    } else {
        &bytes[..]
    };
    match String::from_utf8(slice.to_vec()) {
        Ok(text) => Json(json!({
            "kind": if kind == "markdown" { "markdown" } else { "text" },
            "path": rel,
            "size": metadata.len(),
            "mtime_ms": mtime_ms,
            "truncated": truncated,
            "content": text,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "kind": "binary",
            "path": rel,
            "size": metadata.len(),
            "mtime_ms": mtime_ms,
        }))
        .into_response(),
    }
}

async fn api_create_session(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
    body: Option<Json<CreateSessionBody>>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let requested = body
        .as_ref()
        .and_then(|Json(body)| body.workspace.as_deref());
    let workspace = match resolve_workspace(&state, requested) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    let Ok(store) = runtime::SessionStore::from_cwd(&workspace) else {
        return (StatusCode::BAD_REQUEST, "无法读取该工作区的会话目录").into_response();
    };
    let sessions_dir = store.sessions_dir().to_path_buf();
    let now = now_ms();
    let model =
        crate::config_model_for(&workspace).unwrap_or_else(|| crate::DEFAULT_MODEL.to_string());
    let mut attempt = 0u32;
    let (id, path) = loop {
        let id = if attempt == 0 {
            format!("session-{now}-0")
        } else {
            format!("session-{now}-{attempt}")
        };
        let path = sessions_dir.join(format!("{id}.jsonl"));
        if !path.exists() || attempt >= 100 {
            break (id, path);
        }
        attempt += 1;
    };
    if let Err(error) = std::fs::create_dir_all(&sessions_dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    let meta = json!({
        "type": "session_meta",
        "version": 1,
        "session_id": id,
        "model": model,
        "created_at_ms": now,
        "updated_at_ms": now,
        "workspace_root": workspace.display().to_string(),
    });
    match std::fs::write(&path, format!("{meta}\n")) {
        Ok(()) => Json(json!({
            "id": id,
            "title": "新会话",
            "created_at_ms": now,
            "workspace": workspace.display().to_string(),
        }))
        .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// 带超时地获取 turn 全局锁。
///
/// 该锁串行化“切换进程 cwd + 跑一轮”，一旦某个 turn 卡死，无限 `lock()`
/// 会让后续所有会话静默挂起。超时后直接报错，把问题暴露给用户。
fn acquire_turn_lock<'a>(
    lock: &'a Mutex<()>,
    timeout: Duration,
) -> Result<std::sync::MutexGuard<'a, ()>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "上一个 turn 仍在执行（等待工作区锁 {} 秒未释放），请稍后重试",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn modified_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis() as u64)
}

fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// 写文件：只允许工作区内文本文件；带 mtime 乐观锁，避免覆盖并发修改。
async fn api_file_write(
    State(state): State<AppState>,
    Query(query): Query<PathQuery>,
    Json(body): Json<WriteBody>,
) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let root = match resolve_workspace(
        &state,
        body.workspace.as_deref().or(query.workspace.as_deref()),
    ) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let path = match resolve_in_workspace(&root, &body.path) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return (StatusCode::NOT_FOUND, "文件不存在").into_response();
    };
    if !metadata.is_file() {
        return (StatusCode::BAD_REQUEST, "不是文件").into_response();
    }
    if metadata.len() > MAX_FILE_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "文件过大，不支持在线编辑").into_response();
    }
    if body.content.len() > MAX_TEXT_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "内容超过 512KB，拒绝写入").into_response();
    }
    if let Some(expected) = body.expected_mtime_ms {
        if let Some(actual) = modified_ms(&path) {
            if actual != expected {
                return (
                    StatusCode::CONFLICT,
                    format!("文件已被修改（{actual} != {expected}），请重新加载后再编辑"),
                )
                    .into_response();
            }
        }
    }
    match atomic_write(&path, &body.content) {
        Ok(()) => Json(json!({
            "ok": true,
            "path": body.path,
            "mtime_ms": modified_ms(&path),
            "size": body.content.len(),
        }))
        .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

/// `.claw/plans/*.json`：计划步骤与状态（由 CLI 的计划执行器写入）。
async fn api_plans(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let root = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let dir = root.join(".claw").join("plans");
    let mut plans = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or(Value::Null);
            plans.push(json!({
                "id": id,
                "updated_ms": modified_ms(&path),
                "plan": value,
            }));
        }
    }
    plans.sort_by_key(|plan| std::cmp::Reverse(plan["updated_ms"].as_u64().unwrap_or(0)));
    Json(json!({ "plans": plans })).into_response()
}

/// 附件：工作区图片 + `.claw/paste-cache` 里的粘贴文本，供预览面板列取。
fn workspace_item(root: &Path) -> Value {
    let (count, modified) = runtime::SessionStore::from_cwd(root)
        .ok()
        .map(|store| {
            let files = session_files(store.sessions_dir());
            let modified = files
                .iter()
                .filter_map(|path| modified_ms(path))
                .max()
                .unwrap_or(0);
            (files.len(), modified)
        })
        .unwrap_or((0, 0));
    json!({
        "path": root.display().to_string(),
        "name": root
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| root.display().to_string()),
        "session_count": count,
        "modified_ms": modified,
        "exists": root.is_dir(),
    })
}

/// 已登记工作区列表（侧栏“项目”分组）。
async fn api_workspaces(State(state): State<AppState>, Query(auth): Query<AuthQuery>) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let items: Vec<Value> = session_roots(&state)
        .iter()
        .map(|root| workspace_item(root))
        .collect();
    Json(json!({ "workspaces": items })).into_response()
}

/// 手动添加工作区（目录路径）。
async fn api_workspace_add(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<WorkspaceAddBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    match resolve_workspace(&state, Some(&body.path)) {
        Ok(root) => Json(json!({ "workspace": workspace_item(&root) })).into_response(),
        Err((code, message)) => (code, message).into_response(),
    }
}

/// 弹系统文件夹选择框（Windows）。返回 None 表示用户取消。
///
/// Windows 上有两个必须在这里处理的坑，否则 Web UI 里点“+”会出现
/// “选择框弹在其它窗口后面”和“工作区不存在：D:\+乱码”（GBK 被当成 UTF-8）。
///
/// 1. 没有所有者窗口的 `FolderBrowserDialog` 从后台进程（`claw serve`）里弹出时
///    不算前台窗口，会被系统压到所有窗口后面；这里先用一个 1x1 全透明的
///    TopMost 窗体当 owner，对话框作为它的 owned window 同样保持置顶。
/// 2. 中文 Windows 上 `[Console]::Out` 默认按 GBK(CP936) 输出，Rust 端按 UTF-8
///    解析会把 `D:\测试项目` 读成一串替换字符（`�`），`canonicalize` 必然失败。
///    脚本因此绕开 `Console`，直接用 UTF-8（无 BOM）把路径写进标准输出。
#[cfg(windows)]
fn pick_folder_dialog() -> Result<Option<PathBuf>, String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = r#"Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$owner = New-Object System.Windows.Forms.Form
$owner.StartPosition = 'CenterScreen'
$owner.FormBorderStyle = 'None'
$owner.ShowInTaskbar = $false
$owner.TopMost = $true
$owner.Opacity = 0
$owner.Size = New-Object System.Drawing.Size(1, 1)
$owner.Show()
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog
$dialog.Description = '选择 CLAW 工作区目录'
$dialog.ShowNewFolderButton = $true
$result = [System.Windows.Forms.DialogResult]::Cancel
try {
  $result = $dialog.ShowDialog($owner)
} finally {
  $owner.Close()
  $owner.Dispose()
}
if ($result -eq [System.Windows.Forms.DialogResult]::OK) {
  $writer = New-Object System.IO.StreamWriter([Console]::OpenStandardOutput(), (New-Object System.Text.UTF8Encoding($false)))
  $writer.Write($dialog.SelectedPath)
  $writer.Flush()
  $writer.Dispose()
}"#;
    let mut last_error = String::new();
    for exe in ["powershell.exe", "pwsh.exe"] {
        match std::process::Command::new(exe)
            .args(["-NoProfile", "-STA", "-Command", script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) => {
                if let Some(path) = parse_picker_output(&output.stdout) {
                    return Ok(Some(path));
                }
                // 正常退出但没有输出 = 用户点了取消。
                if output.status.success() {
                    return Ok(None);
                }
                last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(format!("无法打开文件夹选择框：{last_error}"))
}

/// 解析选择框的标准输出：脚本固定写 UTF-8，空输出表示用户取消。
#[cfg(windows)]
fn parse_picker_output(stdout: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    let text = text.trim();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

#[cfg(not(windows))]
fn pick_folder_dialog() -> Result<Option<PathBuf>, String> {
    Err("当前平台不支持系统文件夹选择框".to_string())
}

/// 弹出系统文件夹选择框并登记选中的工作区。
async fn api_workspace_pick(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    match tokio::task::spawn_blocking(pick_folder_dialog).await {
        Ok(Ok(Some(path))) => match resolve_workspace(&state, Some(&path.display().to_string())) {
            Ok(root) => Json(json!({ "workspace": workspace_item(&root) })).into_response(),
            Err((code, message)) => (code, message).into_response(),
        },
        Ok(Ok(None)) => Json(json!({ "workspace": null })).into_response(),
        Ok(Err(message)) => (StatusCode::NOT_IMPLEMENTED, message).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ClientErrorBody {
    message: String,
    stack: Option<String>,
    url: Option<String>,
}

/// 前端渲染错误上报：写入工作区 `.claw/web-client-errors.log`，便于定位白屏。
async fn api_client_error(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<ClientErrorBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let path = state.workspace.join(".claw").join("web-client-errors.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!(
        "[{}] {}\nurl: {}\nstack:\n{}\n\n",
        now_ms(),
        body.message.chars().take(500).collect::<String>(),
        body.url.as_deref().unwrap_or(""),
        body.stack
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(8000)
            .collect::<String>()
    );
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn api_attachments(
    State(state): State<AppState>,
    Query(query): Query<PathQuery>,
) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let root = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let mut items = Vec::new();
    let mut files = Vec::new();
    collect_workspace_entries(&root, &root, 0, &mut files);
    for file in files {
        let Some(path) = file["path"].as_str() else {
            continue;
        };
        if file_kind(Path::new(path)) == "image" {
            items.push(json!({
                "kind": "image",
                "path": path,
                "size": file["size"],
            }));
        }
        if items.len() >= 100 {
            break;
        }
    }

    let paste_dir = root.join(".claw").join("paste-cache");
    let mut pastes = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&paste_dir) {
        for entry in entries.flatten().take(100) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string());
            pastes.push(json!({
                "kind": "paste",
                "path": format!(".claw/paste-cache/{}", name.unwrap_or_default()),
                "size": entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                "updated_ms": modified_ms(&path),
            }));
        }
    }
    pastes.sort_by_key(|item| std::cmp::Reverse(item["updated_ms"].as_u64().unwrap_or(0)));

    let upload_dir = root.join(".claw").join("uploads");
    let mut uploads = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&upload_dir) {
        for entry in entries.flatten().take(100) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            uploads.push(json!({
                "kind": "upload",
                "name": name,
                "path": format!(".claw/uploads/{name}"),
                "size": entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                "image": file_kind(&path) == "image",
                "updated_ms": modified_ms(&path),
            }));
        }
    }
    uploads.sort_by_key(|item| std::cmp::Reverse(item["updated_ms"].as_u64().unwrap_or(0)));

    Json(json!({ "items": items, "pastes": pastes, "uploads": uploads })).into_response()
}

#[derive(Deserialize)]
struct UploadQuery {
    t: Option<String>,
    name: Option<String>,
    workspace: Option<String>,
}

/// 浏览器上传：原始字节写入 `.claw/uploads/`（10MB 上限）。
async fn api_upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    const MAX_UPLOAD: usize = 10 * 1024 * 1024;
    if query.t.as_deref() != Some(state.token.as_str()) {
        return unauthorized();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "空文件").into_response();
    }
    if body.len() > MAX_UPLOAD {
        return (StatusCode::PAYLOAD_TOO_LARGE, "文件超过 10MB").into_response();
    }
    let raw_name = query.name.as_deref().unwrap_or("upload.bin");
    let base = raw_name.rsplit(['/', '\\']).next().unwrap_or("upload.bin");
    let sanitized: String = base
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = if sanitized.trim_matches('_').is_empty() {
        "upload.bin".to_string()
    } else {
        sanitized
    };
    let root = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let dir = root.join(".claw").join("uploads");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    let name = format!("{}-{}", now_ms(), sanitized);
    let path = dir.join(&name);
    match std::fs::write(&path, &body) {
        Ok(()) => Json(json!({
            "name": name,
            "path": format!(".claw/uploads/{name}"),
            "size": body.len(),
        }))
        .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn api_raw(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let Some(rel) = query.path.as_deref() else {
        return (StatusCode::BAD_REQUEST, "缺少 path").into_response();
    };
    let root = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let path = match resolve_in_workspace(&root, rel) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return (StatusCode::NOT_FOUND, "文件不存在").into_response();
    };
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "文件不可直接预览").into_response();
    }
    match std::fs::read(&path) {
        Ok(bytes) => ([(header::CONTENT_TYPE, mime_for(&path))], bytes).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

/// 一个 unified diff hunk 的结构化视图（供 Web 端逐块审查）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffHunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    /// `@@ ... @@` 之后的段标题（无则为空）。
    section: String,
    /// 正文行，保留 ` `/`-`/`+` 前缀。
    lines: Vec<String>,
}

/// hunk 正文行的一侧归属：`Some(' ')` 上下文、`Some('+')` 新增、`Some('-')` 删除；
/// `None` 表示非正文行（`\ No newline at end of file` 之类）。
fn hunk_tag(line: &str) -> Option<char> {
    if line.starts_with('\\') {
        return None;
    }
    match line.chars().next() {
        Some(ch @ (' ' | '+' | '-')) => Some(ch),
        // 容错：某些工具会裁掉空上下文行的尾随空格。
        _ => Some(' '),
    }
}

fn hunk_body(line: &str) -> &str {
    if line.is_empty() {
        ""
    } else {
        &line[1..]
    }
}

impl DiffHunk {
    /// 内容指纹：校验「前端展示的 hunk」与「服务端当前 hunk」是否同一个。
    fn fingerprint(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        self.old_start.hash(&mut hasher);
        self.new_start.hash(&mut hasher);
        for line in &self.lines {
            line.hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
    }

    /// 新文件侧内容（上下文 + 新增行），即当前工作区里应有的样子。
    fn new_side(&self) -> Vec<&str> {
        self.side(true)
    }

    /// 旧文件侧内容（上下文 + 删除行），即撤销后应恢复的样子。
    fn old_side(&self) -> Vec<&str> {
        self.side(false)
    }

    fn side(&self, keep_added: bool) -> Vec<&str> {
        let mut out = Vec::new();
        for line in &self.lines {
            match hunk_tag(line) {
                Some(' ') => out.push(hunk_body(line)),
                Some('+') if keep_added => out.push(hunk_body(line)),
                Some('-') if !keep_added => out.push(hunk_body(line)),
                _ => {}
            }
        }
        out
    }
}

/// 单个文件的 diff（hunk 顺序与 `git diff` 输出一致，index 即数组下标）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FileDiff {
    path: String,
    hunks: Vec<DiffHunk>,
}

fn parse_hunk_range(text: &str) -> Option<(usize, usize)> {
    let mut parts = text.split(',');
    let start = parts.next()?.parse().ok()?;
    // `-1,+1` 省略计数时默认 1。
    let count = match parts.next() {
        Some(value) => value.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// 解析 `@@ -1,2 +1,3 @@ section` 中 `@@ ` 之后的部分。
fn parse_hunk_header(rest: &str) -> Option<DiffHunk> {
    let (ranges, section) = match rest.find("@@") {
        Some(pos) => (&rest[..pos], rest[pos + 2..].trim().to_string()),
        None => (rest, String::new()),
    };
    let mut parts = ranges.split_whitespace();
    let (old_start, old_count) = parse_hunk_range(parts.next()?.trim_start_matches('-'))?;
    let (new_start, new_count) = parse_hunk_range(parts.next()?.trim_start_matches('+'))?;
    Some(DiffHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        section,
        lines: Vec::new(),
    })
}

/// 把 `git diff --no-color` 文本解析成按文件分组的 hunk 列表。
fn parse_unified_diff(text: &str) -> Vec<FileDiff> {
    fn push_hunk(files: &mut [FileDiff], file_index: Option<usize>, hunk: Option<DiffHunk>) {
        if let (Some(index), Some(hunk)) = (file_index, hunk) {
            files[index].hunks.push(hunk);
        }
    }

    let mut files: Vec<FileDiff> = Vec::new();
    let mut file_index: Option<usize> = None;
    let mut hunk: Option<DiffHunk> = None;

    // patch 以换行结尾时 split 会产生末尾空串，它不是 hunk 正文（否则会给最后一块
    // 凭空加一条上下文行，导致 new_side 与非 revert 校验错位）。
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let is_trailing_artifact = line.is_empty() && lines.peek().is_none();
        if is_trailing_artifact {
            break;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            push_hunk(&mut files, file_index, hunk.take());
            let path = rest.split(" b/").nth(1).unwrap_or("").to_string();
            files.push(FileDiff {
                path,
                hunks: Vec::new(),
            });
            file_index = Some(files.len() - 1);
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            // 删除文件时右侧是 /dev/null，保留 diff --git 推出来的路径。
            if !rest.starts_with("/dev/null") {
                if let Some(index) = file_index {
                    if files[index].path.is_empty() {
                        files[index].path = rest.trim_start_matches("b/").to_string();
                    }
                }
            }
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            push_hunk(&mut files, file_index, hunk.take());
            hunk = parse_hunk_header(rest);
            continue;
        }
        if let Some(current) = hunk.as_mut() {
            current.lines.push(line.to_string());
        }
    }
    push_hunk(&mut files, file_index, hunk);

    // 丢掉没有 hunk 的文件（纯 mode 变更等）。
    files.retain(|file| !file.hunks.is_empty());
    files
}

/// 从工作区文件内容中撤销单个 hunk（等价 `git checkout -p` 里对该块答 no）。
///
/// 失败即拒绝：只有当前文件在 `new_start` 处的内容与 hunk 的新侧完全一致才动手，
/// 否则说明文件在展示后被改过，按索引硬改会错位。
fn revert_hunk(content: &str, hunk: &DiffHunk) -> Result<String, String> {
    let expected = hunk.new_side();
    let replacement = hunk.old_side();
    // git diff 输出恒为 LF，而 Windows 工作树里读到的文件是 CRLF：
    // 比较前统一剥掉行尾 \r，写回时再按原文件风格还原，否则每行都会误判"已变化"。
    let crlf = content.contains("\r\n");
    let mut lines: Vec<String> = content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    let start = hunk.new_start.saturating_sub(1);
    let end = start + expected.len();
    if end > lines.len() {
        return Err(format!(
            "hunk 越界：文件 {len} 行，该块需要第 {start}–{end} 行",
            len = lines.len(),
            start = start + 1,
            end = end
        ));
    }
    let actual: Vec<&str> = lines[start..end].iter().map(|line| line.as_str()).collect();
    if actual != expected {
        return Err("文件内容在展示后已变化，请刷新后重试".to_string());
    }
    // 前缀保留 → 中间换成旧侧 → 跳过错位图里的新侧区域后接回尾部。
    // （必须 drain 掉 `expected.len()` 行，否则新增行会被原样留在文件里。）
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 8);
    out.extend(lines.drain(..start));
    out.extend(replacement.into_iter().map(|line| line.to_string()));
    out.extend(lines.drain(expected.len()..));
    Ok(out.join(if crlf { "\r\n" } else { "\n" }))
}

fn git_diff_text(root: &Path, path: Option<&str>) -> String {
    let mut command = runtime::sandbox::quiet_command("git");
    command.arg("-C").arg(root).args(["diff", "--no-color"]);
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        command.arg("--").arg(path);
    }
    match command.output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        Ok(output) => format!("git diff 失败：{}", String::from_utf8_lossy(&output.stderr)),
        Err(error) => format!("无法执行 git: {error}"),
    }
}

async fn api_diff(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let workspace = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let workspace = &workspace;
    // 结构化解析必须基于完整 patch（截断会切断 hunk），展示用的 patch 才截断。
    let raw = git_diff_text(workspace, query.path.as_deref());
    let patch = truncate_middle(&raw, 200_000);
    let diffs = parse_unified_diff(&raw);

    let mut stat_cmd = runtime::sandbox::quiet_command("git");
    stat_cmd
        .arg("-C")
        .arg(workspace)
        .args(["diff", "--numstat"]);
    let mut files = Vec::new();
    if let Ok(output) = stat_cmd.output() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.split('\t');
            let adds = parts.next().unwrap_or_default();
            let dels = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            files.push(json!({
                "path": path,
                "added": adds.parse::<i64>().unwrap_or(0),
                "removed": dels.parse::<i64>().unwrap_or(0),
            }));
        }
    }

    let mut status_cmd = runtime::sandbox::quiet_command("git");
    status_cmd
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain", "--untracked-files=all"]);
    let mut untracked = Vec::new();
    if let Ok(output) = status_cmd.output() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(path) = line.strip_prefix("?? ") {
                untracked.push(path.trim().to_string());
            }
        }
    }

    // 结构化 hunk：前端按块渲染「采纳/丢弃」，index 即数组下标。
    let hunks: Vec<Value> = diffs
        .iter()
        .flat_map(|file| {
            file.hunks.iter().enumerate().map(move |(index, hunk)| {
                json!({
                    "path": file.path,
                    "index": index,
                    "old_start": hunk.old_start,
                    "old_count": hunk.old_count,
                    "new_start": hunk.new_start,
                    "new_count": hunk.new_count,
                    "section": hunk.section,
                    "fingerprint": hunk.fingerprint(),
                    "header": format!(
                        "@@ -{},{} +{},{} @@ {}",
                        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count, hunk.section
                    ),
                    "lines": hunk.lines,
                })
            })
        })
        .collect();

    Json(json!({
        "patch": patch,
        "files": files,
        "untracked": untracked,
        "hunks": hunks,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DiscardHunkBody {
    path: String,
    /// 来自 GET /api/workspace/diff 的 `hunks[].fingerprint`。
    fingerprint: String,
    /// 来自 GET /api/workspace/diff 的 `hunks[].index`；仅用于定位，最终以 fingerprint 为准。
    index: usize,
    /// 读取时的 mtime（毫秒）；不匹配说明文件在展示后被改动。
    expected_mtime_ms: Option<u64>,
    workspace: Option<String>,
}

/// 丢弃单个 hunk（= `git checkout -p` 对该块答 no）：把工作区文件里这一块改回 HEAD 的样子。
///
/// 双重校验，任一不符即整块拒绝而非猜测位置：
/// - `fingerprint` 必须与当前 diff 中该 `index` 处 hunk 一致 → 证明讨论的是同一块；
/// - 文件对应行内容必须仍等于该 hunk 的新侧 → 证明文件未被并发改动。
async fn api_diff_discard(
    State(state): State<AppState>,
    Query(query): Query<PathQuery>,
    Json(body): Json<DiscardHunkBody>,
) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let root = match resolve_workspace(
        &state,
        body.workspace.as_deref().or(query.workspace.as_deref()),
    ) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let absolute = match resolve_in_workspace(&root, &body.path) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };

    let diffs = parse_unified_diff(&git_diff_text(&root, Some(&body.path)));
    let Some(hunk) = diffs
        .iter()
        .find(|file| file.path == body.path)
        .and_then(|file| file.hunks.get(body.index))
    else {
        return (
            StatusCode::CONFLICT,
            "该 hunk 已不存在（文件可能已被修改或还原），请刷新后重试",
        )
            .into_response();
    };
    if hunk.fingerprint() != body.fingerprint {
        return (StatusCode::CONFLICT, "hunk 内容已变化，请刷新后重试").into_response();
    }
    if let Some(expected) = body.expected_mtime_ms {
        if let Some(actual) = modified_ms(&absolute) {
            if actual != expected {
                return (
                    StatusCode::CONFLICT,
                    format!("文件已被修改（{actual} != {expected}），请刷新后重试"),
                )
                    .into_response();
            }
        }
    }

    let Ok(content) = std::fs::read_to_string(&absolute) else {
        return (StatusCode::NOT_FOUND, "文件不存在或不是文本文件").into_response();
    };
    let reverted = match revert_hunk(&content, hunk) {
        Ok(text) => text,
        Err(message) => return (StatusCode::CONFLICT, message).into_response(),
    };
    match atomic_write(&absolute, &reverted) {
        Ok(()) => Json(json!({
            "ok": true,
            "path": body.path,
            "mtime_ms": modified_ms(&absolute),
        }))
        .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn api_goal(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let root = match resolve_workspace(&state, query.workspace.as_deref()) {
        Ok(root) => root,
        Err((code, message)) => return (code, message).into_response(),
    };
    let path = root.join(".claw").join("goal.json");
    let goal: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    Json(json!({ "goal": goal })).into_response()
}

#[derive(Deserialize)]
struct ApprovalBody {
    id: String,
    allow: bool,
}

/// 用户在 Web 端对越权工具的批准/拒绝。
async fn api_approval(
    State(state): State<AppState>,
    UrlPath(session_id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<ApprovalBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let pending = {
        let map = state
            .approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(&body.id).cloned()
    };
    let Some(pending) = pending else {
        return (StatusCode::NOT_FOUND, "审批请求已失效").into_response();
    };
    if pending.session != session_id {
        return (StatusCode::NOT_FOUND, "审批请求不属于该会话").into_response();
    }
    match pending.sender.send(body.allow) {
        Ok(()) => Json(json!({ "id": body.id, "allow": body.allow })).into_response(),
        Err(_) => (StatusCode::CONFLICT, "审批请求已结束").into_response(),
    }
}

/// Web 端维护的待办清单（sidecar JSON），发送消息时会作为上下文注入。
async fn api_todos_get(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let (_path, workspace) = match resolve_session(&state, &id) {
        Ok(found) => found,
        Err((code, message)) => return (code, message).into_response(),
    };
    Json(json!({ "session": id, "items": load_web_todos(&workspace, &id) })).into_response()
}

async fn api_todos_set(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<TodosBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let (_session_path, workspace) = match resolve_session(&state, &id) {
        Ok(found) => found,
        Err((code, message)) => return (code, message).into_response(),
    };
    let items: Vec<Value> = body
        .items
        .iter()
        .take(100)
        .filter_map(|item| {
            let text = item.text.trim();
            if text.is_empty() {
                return None;
            }
            Some(json!({ "text": truncate_chars(text, 500), "done": item.done }))
        })
        .collect();
    let path = web_todos_path(&workspace, &id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match atomic_write(&path, &json!({ "items": items }).to_string()) {
        Ok(()) => Json(json!({ "session": id, "items": items })).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn api_tasks(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let path = match resolve_session(&state, &id) {
        Ok((path, _workspace)) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    let session = match runtime::Session::load_from_path(&path) {
        Ok(session) => session,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };

    let mut todos: Option<Vec<Value>> = None;
    let mut tasks: Vec<Value> = Vec::new();
    let mut crons: Vec<Value> = Vec::new();
    let mut plan_mode: Option<bool> = None;

    'outer: for message in session.messages.iter().rev() {
        for block in message.blocks.iter().rev() {
            let runtime::ContentBlock::ToolUse { name, input, .. } = block else {
                continue;
            };
            match name.as_str() {
                "TodoWrite" if todos.is_none() => {
                    if let Ok(value) = serde_json::from_str::<Value>(input) {
                        if let Some(list) = value["todos"].as_array() {
                            todos = Some(list.clone());
                        }
                    }
                }
                "EnterPlanMode" if plan_mode.is_none() => plan_mode = Some(true),
                "ExitPlanMode" if plan_mode.is_none() => plan_mode = Some(false),
                "TaskCreate" | "TaskUpdate" if tasks.len() < 20 => tasks.push(json!({
                    "tool": name,
                    "detail": truncate_middle(input, 240),
                })),
                "CronCreate" | "CronDelete" if crons.len() < 20 => crons.push(json!({
                    "tool": name,
                    "detail": truncate_middle(input, 240),
                })),
                _ => {}
            }
            if todos.is_some() && plan_mode.is_some() && tasks.len() >= 20 && crons.len() >= 20 {
                break 'outer;
            }
        }
    }

    Json(json!({
        "session": id,
        "plan_mode": plan_mode,
        "todos": todos.unwrap_or_default(),
        "tasks": tasks,
        "crons": crons,
    }))
    .into_response()
}

async fn api_meta(State(state): State<AppState>, Query(auth): Query<AuthQuery>) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "git_sha": crate::GIT_SHA,
        "workspace": state.workspace.display().to_string(),
    }))
    .into_response()
}

fn web_settings_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".claw").join("web-settings.json"))
}

fn load_web_settings() -> Value {
    let mut settings = json!({
        "theme": "dark",
        "permission": "workspace-write",
        "panel_width": 380,
        "panel_open": true,
        "model": "",
        "panel_tab": "files",
        "panel_side": "right",
        "sidebar_collapsed": false,
    });
    if let Some(path) = web_settings_path() {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
                for (key, value) in map {
                    settings[&key] = value;
                }
            }
        }
    }
    settings
}

async fn api_web_settings_get(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    Json(load_web_settings()).into_response()
}

async fn api_web_settings_set(
    State(state): State<AppState>,
    Query(auth): Query<AuthQuery>,
    Json(body): Json<WebSettingsBody>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let mut settings = load_web_settings();
    if let Some(theme) = body.theme {
        if theme == "dark" || theme == "light" {
            settings["theme"] = json!(theme);
        }
    }
    if let Some(permission) = body.permission {
        if PERMISSION_MODES.contains(&permission.as_str()) {
            settings["permission"] = json!(permission);
        }
    }
    if let Some(width) = body.panel_width {
        settings["panel_width"] = json!(width.clamp(260, 720));
    }
    if let Some(model) = body.model {
        let model = model.trim();
        if model.chars().count() <= 100 {
            settings["model"] = json!(model);
        }
    }
    if let Some(open) = body.panel_open {
        settings["panel_open"] = json!(open);
    }
    if let Some(tab) = body.panel_tab {
        if matches!(
            tab.as_str(),
            "files" | "diff" | "tasks" | "preview" | "attachments" | "terminal"
        ) {
            settings["panel_tab"] = json!(tab);
        }
    }
    if let Some(side) = body.panel_side {
        if side == "left" || side == "right" {
            settings["panel_side"] = json!(side);
        }
    }
    if let Some(collapsed) = body.sidebar_collapsed {
        settings["sidebar_collapsed"] = json!(collapsed);
    }
    if let Some(path) = web_settings_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, settings.to_string());
    }
    Json(settings).into_response()
}

fn authorize(state: &AppState, auth: &AuthQuery) -> bool {
    auth.t.as_deref() == Some(state.token.as_str())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "缺少或无效的访问令牌").into_response()
}

fn list_sessions(
    dir: &Path,
    workspace: &Path,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    // 先只做元数据扫描并排序截断，再对入选的少数会话读标题——
    // 避免为列出 50 条会话而完整解析上百个大 JSONL。
    //
    // 归属工作区以“会话文件所在的 store”为准，而不是回读 `session_meta` 里的
    // `workspace_root`：后者是创建时写下的路径字符串，Windows 上同一目录可能
    // 留下 `d:\foo` / `D:\foo` 两种拼写，前端按字符串分组会把一个工作区拆成
    // 两个同名分组。
    let workspace_label = workspace.display().to_string();
    let mut entries: Vec<(PathBuf, u64, u64)> = session_files(dir)
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            // 只有 session_meta 头、从未产生消息的空会话不进入列表：它们没有
            // 标题也没有内容，只会在侧栏留下一条以会话 id 命名的空记录。
            if is_header_only_session(&path, metadata.len()) {
                return None;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |elapsed| elapsed.as_secs());
            Some((path, modified, metadata.len()))
        })
        .collect();
    entries.sort_by_key(|(_, modified, _)| std::cmp::Reverse(*modified));
    let total = entries.len();
    let titles = load_session_titles(workspace);

    let sessions: Vec<Value> = entries
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|(path, modified, size)| {
            let id = path.file_stem().and_then(|stem| stem.to_str())?.to_string();
            let title = titles
                .get(&id)
                .cloned()
                .unwrap_or_else(|| session_title(&path));
            Some(json!({
                "id": id,
                "modified": modified,
                "size": size,
                "title": title,
                "workspace": workspace_label.clone(),
            }))
        })
        .collect();
    Ok(json!({
        "sessions": sessions,
        "total": total,
        "offset": offset,
        "limit": limit,
    }))
}

fn session_titles_path(workspace: &Path) -> PathBuf {
    workspace.join(".claw").join("session-titles.json")
}

fn load_session_titles(workspace: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(session_titles_path(workspace))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|object| {
            object
                .into_iter()
                .filter_map(|(key, value)| value.as_str().map(|title| (key, title.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn save_session_title(workspace: &Path, id: &str, title: &str) -> Result<(), String> {
    let mut titles = load_session_titles(workspace);
    titles.insert(id.to_string(), title.to_string());
    let path = session_titles_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let value = Value::Object(
        titles
            .into_iter()
            .map(|(key, title)| (key, Value::String(title)))
            .collect(),
    );
    atomic_write(&path, &value.to_string()).map_err(|error| error.to_string())
}

fn remove_session_title(workspace: &Path, id: &str) {
    let mut titles = load_session_titles(workspace);
    if titles.remove(id).is_none() {
        return;
    }
    let path = session_titles_path(workspace);
    let value = Value::Object(
        titles
            .into_iter()
            .map(|(key, title)| (key, Value::String(title)))
            .collect(),
    );
    let _ = atomic_write(&path, &value.to_string());
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_session_files(dir, 0, &mut files);
    files
}

/// 只认真正的会话文件：首行必须是 `session_meta` 记录。
/// 同目录还会存在 `*-punchback.jsonl` 之类的旁路文件，它们不属于会话。
fn is_session_file(path: &Path) -> bool {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 512];
    let Ok(read) = file.read(&mut head) else {
        return false;
    };
    let text = String::from_utf8_lossy(&head[..read]);
    text.contains("\"type\":\"session_meta\"") || text.contains("\"type\": \"session_meta\"")
}

/// 只有 `session_meta` 头、从未产生任何消息的会话（典型：点了“新建任务”之后
/// 没发消息就离开）。这类文件没有标题也没有内容，列进侧栏只会得到一条以会话
/// id 命名的空记录。
///
/// 正常会话会随首条消息增长到远大于 4 KiB：先按体积快速排除，再确认第一行之后
/// 没有内容，避免为了过滤空会话去读全部 JSONL。
fn is_header_only_session(path: &Path, size: u64) -> bool {
    const HEADER_PROBE_LIMIT: u64 = 4096;
    if size > HEADER_PROBE_LIMIT {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    match text.split_once('\n') {
        Some((_header, rest)) => rest.trim().is_empty(),
        // 没有换行同样是“只有一行”：只要不是空文件就判定为只有会话头。
        None => !text.trim().is_empty(),
    }
}

fn collect_session_files(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 3;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_session_files(&path, depth + 1, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some(PRIMARY_SESSION_EXTENSION)
            && is_session_file(&path)
        {
            files.push(path);
        }
    }
}

fn session_path(dir: &Path, id: &str) -> Result<PathBuf, (StatusCode, String)> {
    let valid = !id.is_empty()
        && !id.contains("..")
        && !id.contains('/')
        && !id.contains('\\')
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if !valid {
        return Err((StatusCode::BAD_REQUEST, "无效的会话 ID".to_string()));
    }
    session_files(dir)
        .into_iter()
        .find(|path| path.file_stem().and_then(|stem| stem.to_str()) == Some(id))
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("会话 {id} 不存在")))
}

/// 只读会话头部与第一条用户消息，避免为列表完整解析大文件。
fn session_title(path: &Path) -> String {
    use std::io::{BufRead, BufReader};

    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let reader = BufReader::new(file);
    let mut title = String::new();
    let mut budget: usize = 96 * 1024;

    for line in reader.lines().take(60) {
        let Ok(line) = line else {
            break;
        };
        if line.len() > budget {
            break;
        }
        budget -= line.len();
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if title.is_empty() && value["message"]["role"] == "user" {
            if let Some(blocks) = value["message"]["blocks"].as_array() {
                let text = blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !text.trim().is_empty() {
                    title = truncate_chars(&text, 80);
                }
            }
        }
        if !title.is_empty() {
            break;
        }
    }
    title
}

fn session_view(path: &Path) -> Result<Value, String> {
    let session = runtime::Session::load_from_path(path).map_err(|error| error.to_string())?;
    let messages: Vec<Value> = session
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                runtime::MessageRole::System => "system",
                runtime::MessageRole::User => "user",
                runtime::MessageRole::Assistant => "assistant",
                runtime::MessageRole::Tool => "tool",
            };
            // 保持原始 block 顺序：思考 → 文本 → 工具调用 → 工具结果。
            let mut blocks = Vec::new();
            for block in &message.blocks {
                match block {
                    runtime::ContentBlock::Text { text } => blocks.push(json!({
                        "kind": "text",
                        "text": truncate_middle(text, 12_000),
                    })),
                    runtime::ContentBlock::Thinking { thinking, .. } => blocks.push(json!({
                        "kind": "thinking",
                        "text": truncate_middle(thinking, 4_000),
                    })),
                    runtime::ContentBlock::ToolUse { name, input, .. } => blocks.push(json!({
                        "kind": "tool_use",
                        "name": name,
                        "input": truncate_middle(input, 2_000),
                    })),
                    runtime::ContentBlock::ToolResult {
                        tool_name,
                        output,
                        is_error,
                        ..
                    } => blocks.push(json!({
                        "kind": "tool_result",
                        "name": tool_name,
                        "output": truncate_middle(output, 4_000),
                        "is_error": is_error,
                    })),
                }
            }
            json!({
                "role": role,
                "ts": message.created_at_ms,
                "blocks": blocks,
            })
        })
        .collect();

    let mut stats_turns = 0u64;
    let mut stats_steps = 0u64;
    let mut stats_input = 0u64;
    let mut stats_output = 0u64;
    let mut stats_cache_read = 0u64;
    let mut stats_first_ts = 0u64;
    let mut stats_last_ts = 0u64;
    for message in &session.messages {
        if matches!(message.role, runtime::MessageRole::User) {
            stats_turns += 1;
        }
        if matches!(message.role, runtime::MessageRole::Assistant) {
            stats_steps += message
                .blocks
                .iter()
                .filter(|block| matches!(block, runtime::ContentBlock::ToolUse { .. }))
                .count() as u64;
        }
        if let Some(usage) = message.usage {
            stats_input += u64::from(usage.input_tokens);
            stats_output += u64::from(usage.output_tokens);
            stats_cache_read += u64::from(usage.cache_read_input_tokens);
        }
        if message.created_at_ms > 0 {
            if stats_first_ts == 0 {
                stats_first_ts = message.created_at_ms;
            }
            stats_last_ts = message.created_at_ms;
        }
    }
    let stats_total = stats_input + stats_output;
    let stats_cache_pct = stats_cache_read
        .saturating_mul(100)
        .checked_div(stats_input + stats_cache_read)
        .unwrap_or(0);

    Ok(json!({
        "id": session.session_id,
        "model": session.model,
        "workspace": session
            .workspace_root
            .as_ref()
            .map(|path| path.display().to_string()),
        "created_at_ms": session.created_at_ms,
        "messages": messages,
        "message_count": session.messages.len(),
        "stats": {
            "turns": stats_turns,
            "steps": stats_steps,
            "input_tokens": stats_input,
            "output_tokens": stats_output,
            "total_tokens": stats_total,
            "cache_read_tokens": stats_cache_read,
            "cache_hit_pct": stats_cache_pct,
            "duration_ms": stats_last_ts.saturating_sub(stats_first_ts),
        },
    }))
}

/// 中间截断：保留头尾，避免长工具输出把 SSE 帧撑爆。
fn truncate_middle(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let head_len = limit * 3 / 4;
    let tail_len = limit - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n…（截断 {} 字符）…\n{tail}", count - limit)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Web 热升级：helper 通过 `CLAW_WEB_TOKEN` 传入旧 token，新进程复用。
/// 只接受 32 位十六进制，避免环境变量被塞入怪值。
fn web_token_from_env() -> Option<String> {
    std::env::var("CLAW_WEB_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| token.len() == 32 && token.chars().all(|ch| ch.is_ascii_hexdigit()))
}

/// Web 热升级：写请求 → spawn helper → 通知页面 → 退出旧进程。
///
/// 新进程由 helper 以 `serve --port <同端口>` 启动，并继承旧 token；页面上的
/// EventSource 会在服务恢复后自动重连，随后前端轮询 `/api/meta` 成功即 reload。
fn begin_web_upgrade(
    workspace: &Path,
    port: u16,
    token: &str,
    session_id: &str,
    events: &EventHub,
) {
    let old_pid = std::process::id();
    let request = crate::session_mgr::UpgradeRequest {
        session_id: session_id.to_string(),
        plan_id: None,
        requested_at_ms: now_ms() as i64,
        from_version: crate::version_label(),
        old_pid,
        web_port: Some(port),
        web_token: Some(token.to_string()),
    };
    if let Err(error) = crate::session_mgr::write_upgrade_request(workspace, &request) {
        events.emit(
            session_id,
            json!({
                "kind": "error",
                "message": format!("热升级请求写入失败：{error}"),
            }),
        );
        return;
    }
    if let Err(error) = crate::upgrade_helper::spawn(workspace) {
        crate::session_mgr::clear_upgrade_request(workspace);
        events.emit(
            session_id,
            json!({
                "kind": "error",
                "message": format!("热升级助手启动失败：{error}"),
            }),
        );
        return;
    }

    events.emit(
        session_id,
        json!({
            "kind": "upgrading",
            "port": port,
            "message": "正在热升级：重建并重启 Web 服务，页面会自动恢复…",
        }),
    );
    // 给 SSE 一点时间把升级通知刷到浏览器，再写 exited 让 helper 开始迁移。
    std::thread::sleep(Duration::from_millis(700));
    let _ = crate::session_mgr::write_upgrade_exited(workspace, old_pid);
    std::process::exit(0);
}

/// 禁止监听 socket 被后续子进程继承（Windows 热升级必需）。
#[cfg(windows)]
fn make_listener_non_inheritable(listener: &tokio::net::TcpListener) {
    use std::os::windows::io::AsRawSocket;

    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    let socket = listener.as_raw_socket();
    // SAFETY: socket 来自存活的 TcpListener；只清除继承位，不关闭句柄。
    unsafe {
        let _ = SetHandleInformation(socket as *mut core::ffi::c_void, HANDLE_FLAG_INHERIT, 0);
    }
}

#[cfg(not(windows))]
fn make_listener_non_inheritable(_listener: &tokio::net::TcpListener) {}

fn generate_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let pid = u128::from(std::process::id());
    format!("{:032x}", nanos ^ (pid << 96))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        // quiet_command:打开浏览器不应在 Web 服务后面闪一个控制台窗口。
        let _ = runtime::sandbox::quiet_command("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_32_hex_chars() {
        let token = generate_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn session_path_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            session_path(dir.path(), "../etc/passwd").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            session_path(dir.path(), "a/b").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            session_path(dir.path(), "").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        // 合法 id 但不存在：NOT_FOUND，便于前端区分"非法"与"已删除"。
        assert_eq!(
            session_path(dir.path(), "ok-session_1").unwrap_err().0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn session_listing_finds_nested_jsonl_files() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("fingerprint");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("session-1-0.jsonl"),
            "{\"type\":\"session_meta\",\"session_id\":\"session-1-0\"}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        std::fs::write(
            dir.path().join("session-1-0-punchback.jsonl"),
            "{\"kind\":\"punchback\"}\n",
        )
        .unwrap();
        let files = session_files(dir.path());
        assert_eq!(files.len(), 1, "旁路 jsonl 不应被当作会话");
        assert_eq!(
            files[0].file_stem().and_then(|stem| stem.to_str()),
            Some("session-1-0")
        );
    }

    /// 会话归属必须以“文件所在的 store”为准：`session_meta` 里的
    /// `workspace_root` 是创建时写下的字符串，可能大小写/写法不同（Windows
    /// 盘符 `d:` 与 `D:`）。回读它会让前端按字符串分组时把一个工作区拆成
    /// 两个同名分组。
    #[test]
    fn session_listing_labels_sessions_with_store_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("fingerprint");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let recorded_roots = [
            dir.path().display().to_string(),
            r"D:\stale\claw-code-src".to_string(),
        ];
        for (index, workspace_root) in recorded_roots.iter().enumerate() {
            let id = format!("session-{index}-0");
            let meta = serde_json::json!({
                "type": "session_meta",
                "version": 1,
                "session_id": id.as_str(),
                "workspace_root": workspace_root,
            });
            let message = serde_json::json!({
                "type": "message",
                "message": { "role": "user", "blocks": [{ "text": "你好" }] },
            });
            std::fs::write(
                sessions_dir.join(format!("{id}.jsonl")),
                format!("{meta}\n{message}\n"),
            )
            .unwrap();
        }

        let value = list_sessions(&sessions_dir, dir.path(), 0, 10).unwrap();
        let sessions = value["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2, "两条会话都应进入列表");
        let expected = dir.path().display().to_string();
        for session in sessions {
            assert_eq!(
                session["workspace"].as_str(),
                Some(expected.as_str()),
                "workspace 必须取 store root，而不是 session_meta 里的字符串"
            );
        }
    }

    /// “新建任务”后没发消息就离开会留下只有 `session_meta` 头的文件：不进入
    /// 列表，否则侧栏会多出一条以会话 id 命名的空记录。
    #[test]
    fn session_listing_skips_header_only_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("fingerprint");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let empty_path = sessions_dir.join("session-empty-0.jsonl");
        // 真实形状的会话头本身就 ≥ 64 字节：旧的“< 64 字节”过滤挡不住它。
        let empty_meta = serde_json::json!({
            "type": "session_meta",
            "version": 1,
            "session_id": "session-empty-0",
            "model": "deepseek-v4-flash",
            "created_at_ms": 1,
            "updated_at_ms": 1,
            "workspace_root": dir.path().display().to_string(),
        });
        std::fs::write(&empty_path, format!("{empty_meta}\n")).unwrap();
        assert!(
            std::fs::metadata(&empty_path).unwrap().len() > 64,
            "测试要覆盖旧阈值挡不住的真实头部体积"
        );

        let real_meta = serde_json::json!({
            "type": "session_meta",
            "version": 1,
            "session_id": "session-real-0",
            "model": "deepseek-v4-flash",
            "created_at_ms": 2,
            "updated_at_ms": 2,
            "workspace_root": dir.path().display().to_string(),
        });
        let first_message = serde_json::json!({
            "type": "message",
            "message": { "role": "user", "blocks": [{ "text": "第一条消息" }] },
        });
        std::fs::write(
            sessions_dir.join("session-real-0.jsonl"),
            format!("{real_meta}\n{first_message}\n"),
        )
        .unwrap();

        let value = list_sessions(&sessions_dir, dir.path(), 0, 10).unwrap();
        let sessions = value["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "空会话不应进入列表");
        assert_eq!(value["total"].as_u64(), Some(1), "total 也不应计入空会话");
        assert_eq!(sessions[0]["id"], "session-real-0");
        assert_eq!(sessions[0]["title"], "第一条消息");
    }

    #[test]
    fn truncate_chars_respects_utf8_boundaries() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("中文测试", 2), "中文…");
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let text = "abcdefghij";
        assert_eq!(truncate_middle(text, 100), text);
        let cut = truncate_middle(text, 4);
        assert!(cut.starts_with("abc"), "head kept: {cut}");
        assert!(cut.ends_with('j'), "tail kept: {cut}");
        assert!(cut.contains("截断"), "marker present: {cut}");
    }

    #[test]
    fn workspace_path_resolution_blocks_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "hi").unwrap();
        assert!(resolve_in_workspace(dir.path(), "ok.txt").is_ok());
        assert!(resolve_in_workspace(dir.path(), "../outside.txt").is_err());
        assert!(resolve_in_workspace(dir.path(), "").is_err());
    }

    #[test]
    fn file_kind_classifies_common_types() {
        assert_eq!(file_kind(Path::new("a/b.png")), "image");
        assert_eq!(file_kind(Path::new("a/b.pdf")), "pdf");
        assert_eq!(file_kind(Path::new("a/b.rs")), "text");
        assert_eq!(file_kind(Path::new("a/b.md")), "markdown");
        assert_eq!(file_kind(Path::new("a/b.bin")), "binary");
    }

    #[test]
    fn atomic_write_replaces_content_and_reports_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        std::fs::write(&path, "old").unwrap();
        let before = modified_ms(&path).expect("mtime");
        atomic_write(&path, "new content").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
        assert!(modified_ms(&path).expect("mtime") >= before);
    }

    #[test]
    fn web_todos_roundtrip_and_inject_into_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let id = "session-1-0";
        let path = web_todos_path(dir.path(), id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        atomic_write(
            &path,
            &serde_json::json!({
                "items": [
                    { "text": "写测试", "done": false },
                    { "text": "跑测试", "done": true }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let items = load_web_todos(dir.path(), id);
        assert_eq!(items.len(), 2);

        let prompt = prompt_with_web_todos(dir.path(), id, "继续");
        assert!(prompt.contains("- [ ] 写测试"), "prompt: {prompt}");
        assert!(prompt.contains("- [x] 跑测试"), "prompt: {prompt}");
        assert!(prompt.ends_with("继续"));
    }

    #[test]
    fn acquire_turn_lock_times_out_instead_of_blocking_forever() {
        let lock = std::sync::Arc::new(std::sync::Mutex::new(()));
        let shared = std::sync::Arc::clone(&lock);
        let held = lock.lock().unwrap();
        let started = std::time::Instant::now();
        let result = super::acquire_turn_lock(&shared, std::time::Duration::from_millis(200));
        assert!(result.is_err(), "卡住的 turn 不应让后续 turn 无限等待");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(held);
        assert!(super::acquire_turn_lock(&shared, std::time::Duration::from_millis(200)).is_ok());
    }

    /// 一个真实形状的 patch：两文件、多 hunk，含 section 标题。
    /// 用 concat! 而非 `\` 续行——续行会吞掉行首空格，破坏上下文行的 ` ` 前缀。
    const SAMPLE_PATCH: &str = concat!(
        "diff --git a/src/a.rs b/src/a.rs\n",
        "index 1111111..2222222 100644\n",
        "--- a/src/a.rs\n",
        "+++ b/src/a.rs\n",
        "@@ -1,3 +1,4 @@ fn main() {\n",
        " let a = 1;\n",
        "-let b = 2;\n",
        "+let b = 3;\n",
        "+let c = 4;\n",
        " let d = 5;\n",
        "diff --git a/src/b.rs b/src/b.rs\n",
        "index 3333333..4444444 100644\n",
        "--- a/src/b.rs\n",
        "+++ b/src/b.rs\n",
        "@@ -10,2 +10,2 @@\n",
        "-old\n",
        "+new\n",
        " tail\n",
    );

    /// 单 hunk、new_start=1，用于验证末尾换行状态。
    const SMALL_PATCH: &str = concat!(
        "diff --git a/n.txt b/n.txt\n",
        "--- a/n.txt\n",
        "+++ b/n.txt\n",
        "@@ -1,2 +1,2 @@\n",
        "-old\n",
        "+new\n",
        " tail\n",
    );

    #[test]
    fn parse_unified_diff_groups_hunks_by_file() {
        let files = super::parse_unified_diff(SAMPLE_PATCH);
        assert_eq!(files.len(), 2, "两个文件");
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[1].hunks.len(), 1);
        let hunk = &files[0].hunks[0];
        assert_eq!((hunk.old_start, hunk.old_count), (1, 3));
        assert_eq!((hunk.new_start, hunk.new_count), (1, 4));
        assert_eq!(hunk.section, "fn main() {");
        // 正文行数 = 上下文 2 + 删除 1 + 新增 2。
        assert_eq!(hunk.lines.len(), 5);
    }

    #[test]
    fn parse_unified_diff_ignores_trailing_newline_artifact() {
        // 末尾换行不能让最后一个 hunk 多出一条空白上下文行。
        let files = super::parse_unified_diff(SAMPLE_PATCH);
        let last = &files[1].hunks[0];
        assert_eq!(last.lines.len(), 3, "lines: {:?}", last.lines);
        assert!(!last.lines.iter().any(|line| line.is_empty()));
    }

    #[test]
    fn parse_unified_diff_without_trailing_newline() {
        let text = SAMPLE_PATCH.trim_end_matches('\n');
        let files = super::parse_unified_diff(text);
        assert_eq!(files[1].hunks[0].lines.len(), 3);
    }

    #[test]
    fn hunk_sides_split_by_prefix() {
        let files = super::parse_unified_diff(SAMPLE_PATCH);
        let hunk = &files[0].hunks[0];
        // 新侧 = 上下文 + 新增（文件当前应有内容）。
        assert_eq!(
            hunk.new_side(),
            vec!["let a = 1;", "let b = 3;", "let c = 4;", "let d = 5;"]
        );
        // 旧侧 = 上下文 + 删除（还原后应有内容）。
        assert_eq!(
            hunk.old_side(),
            vec!["let a = 1;", "let b = 2;", "let d = 5;"]
        );
    }

    #[test]
    fn fingerprint_tracks_content_changes() {
        let files = super::parse_unified_diff(SAMPLE_PATCH);
        let first = files[0].hunks[0].fingerprint();
        assert_eq!(first, files[0].hunks[0].fingerprint(), "同内容指纹稳定");
        assert_ne!(first, files[1].hunks[0].fingerprint(), "不同 hunk 指纹不同");

        let mutated = SAMPLE_PATCH.replace("+let c = 4;", "+let c = 9;");
        let files2 = super::parse_unified_diff(&mutated);
        assert_ne!(first, files2[0].hunks[0].fingerprint(), "改内容必须改指纹");
    }

    #[test]
    fn revert_hunk_restores_old_side() {
        let files = super::parse_unified_diff(SAMPLE_PATCH);
        let hunk = &files[0].hunks[0];
        let current = "let a = 1;\nlet b = 3;\nlet c = 4;\nlet d = 5;\n";
        let reverted = super::revert_hunk(current, hunk).expect("应能还原");
        assert_eq!(reverted, "let a = 1;\nlet b = 2;\nlet d = 5;\n");
    }

    #[test]
    fn revert_hunk_rejects_when_file_drifted() {
        let files = super::parse_unified_diff(SAMPLE_PATCH);
        let hunk = &files[0].hunks[0];
        // 第 2 行已被改成 99：与 hunk 新侧不符，必须拒绝而不是猜位置。
        let drifted = "let a = 1;\nlet b = 99;\nlet c = 4;\nlet d = 5;\n";
        let error = super::revert_hunk(drifted, hunk).unwrap_err();
        assert!(error.contains("已变化"), "error: {error}");
    }

    #[test]
    fn revert_hunk_rejects_out_of_range() {
        let files = super::parse_unified_diff(SAMPLE_PATCH);
        let hunk = &files[0].hunks[0];
        let error = super::revert_hunk("let a = 1;\n", hunk).unwrap_err();
        assert!(error.contains("越界"), "error: {error}");
    }

    #[test]
    fn revert_hunk_preserves_trailing_newline_state() {
        // 用 new_start=1 的小 patch；SAMPLE_PATCH 第二块从第 10 行开始，不适用于 2 行文件。
        let files = super::parse_unified_diff(SMALL_PATCH);
        let hunk = &files[0].hunks[0];
        // 无尾随换行的文件，还原后也不应凭空多出换行。
        let reverted = super::revert_hunk("new\ntail", hunk).expect("应能还原");
        assert_eq!(reverted, "old\ntail");
    }

    #[test]
    fn revert_hunk_handles_crlf_working_tree() {
        // Windows 工作树是 CRLF、git diff 输出是 LF：比较必须归一，写回必须保留 CRLF。
        let files = super::parse_unified_diff(SMALL_PATCH);
        let hunk = &files[0].hunks[0];
        let reverted = super::revert_hunk("new\r\ntail\r\n", hunk).expect("CRLF 文件应能还原");
        assert_eq!(reverted, "old\r\ntail\r\n");
    }

    // ---------------- @ 引用展开 ----------------

    fn write_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn parse_mention_handles_all_three_forms() {
        let plain = super::parse_mention("@src/a.rs").expect("纯路径");
        assert_eq!(
            (plain.path.as_str(), plain.start, plain.end),
            ("src/a.rs", 1, usize::MAX)
        );

        let single = super::parse_mention("@src/a.rs:7").expect("单行");
        assert_eq!(
            (single.path.as_str(), single.start, single.end),
            ("src/a.rs", 7, 7)
        );

        let range = super::parse_mention("@src/a.rs:12-40").expect("区间");
        assert_eq!(
            (range.path.as_str(), range.start, range.end),
            ("src/a.rs", 12, 40)
        );
    }

    #[test]
    fn parse_mention_rejects_bad_ranges() {
        // 倒序、0 行、非数字一律不认作区间，整体回退为路径（不吞掉原文）。
        assert_eq!(
            super::parse_mention("@a.rs:40-12").unwrap().path,
            "a.rs:40-12"
        );
        assert_eq!(super::parse_mention("@a.rs:0").unwrap().path, "a.rs:0");
        assert_eq!(super::parse_mention("@a.rs:x-y").unwrap().path, "a.rs:x-y");
        assert!(super::parse_mention("@").is_none());
    }

    #[test]
    fn expand_mentions_inlines_numbered_lines() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/a.rs", "l1\nl2\nl3\nl4\nl5\n");
        let out = super::expand_mentions(dir.path(), "看这里 @src/a.rs:2-3 再说");
        assert!(
            out.contains("[引用 src/a.rs 第 2-3 行（文件共 5 行）]"),
            "out: {out}"
        );
        assert!(out.contains("2| l2"), "out: {out}");
        assert!(out.contains("3| l3"), "out: {out}");
        // 未在引用范围内的行不得出现。
        assert!(!out.contains("l4"), "out: {out}");
        // 引用之外的原文保留。
        assert!(out.contains("看这里 "), "out: {out}");
        assert!(out.contains(" 再说"), "out: {out}");
    }

    #[test]
    fn expand_mentions_without_range_reads_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "n.txt", "a\nb\n");
        let out = super::expand_mentions(dir.path(), "@n.txt");
        assert!(out.contains("1| a"), "out: {out}");
        assert!(out.contains("2| b"), "out: {out}");
    }

    #[test]
    fn expand_mentions_clamps_and_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=500).map(|i| format!("line{i}\n")).collect();
        write_file(dir.path(), "big.txt", &body);
        // 请求 1-400，单次展开上限 200 行，必须截断并说明还有多少行。
        let out = super::expand_mentions(dir.path(), "@big.txt:1-400");
        assert!(
            out.contains("[引用 big.txt 第 1-200 行（文件共 500 行）]"),
            "out head: {}",
            &out[..120.min(out.len())]
        );
        assert!(
            out.contains("该引用还有 200 行未展开"),
            "tail: {}",
            &out[out.len().saturating_sub(120)..]
        );
    }

    #[test]
    fn expand_mentions_clamps_range_beyond_eof() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "n.txt", "a\nb\n");
        let out = super::expand_mentions(dir.path(), "@n.txt:2-999");
        assert!(out.contains("第 2-2 行（文件共 2 行）"), "out: {out}");
    }

    #[test]
    fn expand_mentions_ignores_unknown_and_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        // 不存在的文件：保留原文，不注入空块。
        let missing = super::expand_mentions(dir.path(), "@nope/x.txt");
        assert_eq!(missing, "@nope/x.txt");

        // 路径穿越必须被 resolve_in_workspace 拦下，原文保留。
        let escape = super::expand_mentions(dir.path(), "@../../etc/passwd");
        assert_eq!(escape, "@../../etc/passwd");
    }

    #[test]
    fn expand_mentions_leaves_email_and_quoted_at_alone() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.txt", "x\n");
        // 邮箱中间的 @ 左侧是字母，不构成引用边界。
        assert_eq!(super::expand_mentions(dir.path(), "me@a.txt"), "me@a.txt");
        // 引号内的 @ 仍算引用起点（左侧是边界字符），这是刻意行为：粘贴路径时好用。
        assert!(super::expand_mentions(dir.path(), "\"@a.txt\"").contains("1| x"));
    }

    #[test]
    fn expand_mentions_caps_count_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.txt", "x\n");
        // 超过 MAX_MENTIONS 的部分不再展开，但原文全部保留。
        let input = "@a.txt ".repeat(super::MAX_MENTIONS + 5);
        let out = super::expand_mentions(dir.path(), &input);
        let expansions = out.matches("[引用 a.txt").count();
        assert_eq!(expansions, super::MAX_MENTIONS, "expansions={expansions}");
        assert_eq!(out.matches("@a.txt").count(), 5, "剩余原文应保留");
    }

    /// 回归：中文 Windows 上旧脚本用 [Console]::Out 写出 GBK 字节，Rust 端按
    /// UTF-8 解析后路径变成 `D:\������Ŀ`，前端报“工作区不存在”。脚本现在固定
    /// 写 UTF-8，这里锁住解码约定。
    #[cfg(windows)]
    #[test]
    fn picker_output_decodes_utf8_paths() {
        let path =
            parse_picker_output("D:\\测试项目\r\n".as_bytes()).expect("非空输出应解析出路径");
        assert_eq!(path.to_string_lossy(), "D:\\测试项目");
        assert!(parse_picker_output(b"").is_none(), "空输出=用户取消");
        assert!(
            parse_picker_output(b" \r\n\t").is_none(),
            "纯空白也视为取消"
        );
    }
}
