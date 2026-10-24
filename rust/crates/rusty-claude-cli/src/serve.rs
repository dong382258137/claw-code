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
use axum::http::{header, StatusCode};
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

#[derive(Deserialize)]
struct PathQuery {
    t: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
struct WebSettingsBody {
    theme: Option<String>,
    permission: Option<String>,
    panel_width: Option<u32>,
}

#[derive(Deserialize)]
struct WriteBody {
    path: String,
    content: String,
    /// 读取时的 mtime（毫秒）；不匹配说明文件在编辑期间被改动。
    expected_mtime_ms: Option<u64>,
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
            .route("/api/workspace/files", get(api_files))
            .route("/api/workspace/file", get(api_file).post(api_file_write))
            .route("/api/workspace/raw", get(api_raw))
            .route("/api/workspace/diff", get(api_diff))
            .route("/api/workspace/goal", get(api_goal))
            .route("/api/workspace/plans", get(api_plans))
            .route("/api/attachments", get(api_attachments))
            .route("/api/meta", get(api_meta))
            .route("/api/session/{id}/tasks", get(api_tasks))
            .route(
                "/api/web-settings",
                get(api_web_settings_get).post(api_web_settings_set),
            )
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
    let mut files = Vec::new();
    collect_workspace_entries(&state.workspace, &state.workspace, 0, &mut files);
    Json(json!({
        "root": state.workspace.display().to_string(),
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
    let path = match resolve_in_workspace(&state.workspace, rel) {
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
    let path = match resolve_in_workspace(&state.workspace, &body.path) {
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
    let dir = state.workspace.join(".claw").join("plans");
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
async fn api_attachments(
    State(state): State<AppState>,
    Query(query): Query<PathQuery>,
) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let mut items = Vec::new();
    let mut files = Vec::new();
    collect_workspace_entries(&state.workspace, &state.workspace, 0, &mut files);
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

    let paste_dir = state.workspace.join(".claw").join("paste-cache");
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

    Json(json!({ "items": items, "pastes": pastes })).into_response()
}

async fn api_raw(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let Some(rel) = query.path.as_deref() else {
        return (StatusCode::BAD_REQUEST, "缺少 path").into_response();
    };
    let path = match resolve_in_workspace(&state.workspace, rel) {
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

async fn api_diff(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let workspace = &state.workspace;
    let mut patch_cmd = std::process::Command::new("git");
    patch_cmd
        .arg("-C")
        .arg(workspace)
        .arg("diff")
        .arg("--no-color");
    if let Some(path) = query.path.as_deref().filter(|path| !path.is_empty()) {
        patch_cmd.arg("--").arg(path);
    }
    let patch = match patch_cmd.output() {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            truncate_middle(&text, 200_000)
        }
        Ok(output) => format!("git diff 失败: {}", String::from_utf8_lossy(&output.stderr)),
        Err(error) => format!("无法执行 git: {error}"),
    };

    let mut stat_cmd = std::process::Command::new("git");
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

    let mut status_cmd = std::process::Command::new("git");
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

    Json(json!({
        "patch": patch,
        "files": files,
        "untracked": untracked,
    }))
    .into_response()
}

async fn api_goal(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    if !authorize_path(&state, &query) {
        return unauthorized();
    }
    let path = state.workspace.join(".claw").join("goal.json");
    let goal: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    Json(json!({ "goal": goal })).into_response()
}

async fn api_tasks(
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
}
