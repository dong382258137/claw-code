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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::session_mgr::PRIMARY_SESSION_EXTENSION;

const INDEX_HTML: &str = include_str!("serve/index.html");
const MAX_SESSIONS: usize = 50;
const STREAM_INTERVAL: Duration = Duration::from_millis(900);

#[derive(Clone)]
struct AppState {
    token: String,
    /// 当前工作区的会话目录：`<workspace>/.claw/sessions/<fingerprint>/`。
    sessions: PathBuf,
    /// 工作区根，子进程的 cwd。
    workspace: PathBuf,
    /// 正在跑 turn 的子进程（同一会话同时只允许一个写入者）。
    running: Arc<Mutex<HashMap<String, std::process::Child>>>,
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
}

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
        Some(path) => path,
        None => default_workspace(),
    };
    let store = runtime::SessionStore::from_cwd(&cwd)?;
    let sessions = store.sessions_dir().to_path_buf();
    let token = generate_token();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let state = AppState {
            token: token.clone(),
            sessions,
            workspace: cwd.clone(),
            running: Arc::new(Mutex::new(HashMap::new())),
        };
        let app = Router::new()
            .route("/", get(index))
            .route("/api/sessions", get(api_sessions))
            .route("/api/session/{id}", get(api_session))
            .route("/api/session/{id}/stream", get(api_stream))
            .route("/api/session/{id}/prompt", post(api_prompt))
            .route("/api/session/{id}/interrupt", post(api_interrupt))
            .route("/api/session/{id}/status", get(api_status))
            .with_state(state);

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("端口 {port} 不可用（{error}），改用系统分配端口。");
                tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?
            }
        };
        let actual_port = listener.local_addr()?.port();
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
    [
        ".git",
        ".claw",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
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

async fn api_sessions(State(state): State<AppState>, Query(auth): Query<AuthQuery>) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    match list_sessions(&state.sessions) {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn api_session(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let path = match session_path(&state.sessions, &id) {
        Ok(path) => path,
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
    let path = match session_path(&state.sessions, &id) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
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

/// 写入通道：在既有会话上跑一轮。通过子进程 `claw -p` 复用完整 turn 逻辑，
/// 子进程用 `CLAW_RESUME_SESSION` 指回同一个 JSONL，SSE 会自然跟随其变化。
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
        return (StatusCode::BAD_REQUEST, "prompt text is empty").into_response();
    }
    let path = match session_path(&state.sessions, &id) {
        Ok(path) => path,
        Err((code, message)) => return (code, message).into_response(),
    };
    let permission = body.permission.as_deref().unwrap_or("workspace-write");
    if !PERMISSION_MODES.contains(&permission) {
        return (
            StatusCode::BAD_REQUEST,
            format!("unsupported permission mode: {permission}"),
        )
            .into_response();
    }

    {
        let running = state
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if running.contains_key(&id) {
            return (StatusCode::CONFLICT, "session is busy with another turn").into_response();
        }
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    let mut command = std::process::Command::new(exe);
    command
        .arg("--permission-mode")
        .arg(permission)
        .arg("-p")
        .arg(&body.text)
        .current_dir(&state.workspace)
        .env("CLAW_RESUME_SESSION", &path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    let pid = child.id();
    {
        let mut running = state
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        running.insert(id.clone(), child);
    }
    spawn_reaper(Arc::clone(&state.running), id.clone());

    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "running", "pid": pid, "session": id })),
    )
        .into_response()
}

/// 后台回收已退出的子进程，避免 `running` 表无限增长。
fn spawn_reaper(running: Arc<Mutex<HashMap<String, std::process::Child>>>, id: String) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(400));
        let mut guard = running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let finished = match guard.get_mut(&id) {
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            None => true,
        };
        if finished {
            guard.remove(&id);
            break;
        }
    });
}

async fn api_interrupt(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    Query(auth): Query<AuthQuery>,
) -> Response {
    if !authorize(&state, &auth) {
        return unauthorized();
    }
    let interrupted = {
        let mut running = state
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match running.get_mut(&id) {
            Some(child) => {
                let _ = child.kill();
                let _ = child.wait();
                true
            }
            None => false,
        }
    };
    if interrupted {
        (
            StatusCode::OK,
            Json(json!({ "status": "interrupted", "session": id })),
        )
            .into_response()
    } else {
        (StatusCode::CONFLICT, "no running turn for this session").into_response()
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
    let running = {
        let guard = state
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.contains_key(&id)
    };
    Json(json!({ "session": id, "running": running })).into_response()
}

fn authorize(state: &AppState, auth: &AuthQuery) -> bool {
    auth.t.as_deref() == Some(state.token.as_str())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response()
}

fn list_sessions(dir: &Path) -> Result<Value, String> {
    // 先只做元数据扫描并排序截断，再对入选的少数会话读标题——
    // 避免为列出 50 条会话而完整解析上百个大 JSONL。
    let mut entries: Vec<(PathBuf, u64, u64)> = session_files(dir)
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            // 只有 header 的空会话（例如中断的恢复流程留下）不进入列表。
            if metadata.len() < 512 {
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
    entries.truncate(MAX_SESSIONS);

    let sessions: Vec<Value> = entries
        .into_iter()
        .filter_map(|(path, modified, size)| {
            let id = path.file_stem().and_then(|stem| stem.to_str())?.to_string();
            let (title, workspace) = session_summary(&path);
            Some(json!({
                "id": id,
                "modified": modified,
                "size": size,
                "title": title,
                "workspace": workspace,
            }))
        })
        .collect();
    Ok(json!(sessions))
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
        return Err((StatusCode::BAD_REQUEST, "invalid session id".to_string()));
    }
    session_files(dir)
        .into_iter()
        .find(|path| path.file_stem().and_then(|stem| stem.to_str()) == Some(id))
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("session {id} not found")))
}

/// 只读会话头部与第一条用户消息，避免为列表完整解析大文件。
fn session_summary(path: &Path) -> (String, String) {
    use std::io::{BufRead, BufReader};

    let fallback_workspace = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();

    let Ok(file) = std::fs::File::open(path) else {
        return (String::new(), fallback_workspace);
    };
    let reader = BufReader::new(file);
    let mut workspace = String::new();
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
        if workspace.is_empty() && value["type"] == "session_meta" {
            workspace = value["workspace_root"]
                .as_str()
                .unwrap_or_default()
                .to_string();
        }
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
        if !workspace.is_empty() && !title.is_empty() {
            break;
        }
    }
    if workspace.is_empty() {
        workspace = fallback_workspace;
    }
    (title, workspace)
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
                "blocks": blocks,
            })
        })
        .collect();

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
        let _ = std::process::Command::new("cmd")
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
}
