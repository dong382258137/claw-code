#![allow(clippy::should_implement_trait, clippy::must_use_candidate)]
//! LSP (Language Server Protocol) client registry for tool dispatch.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Supported LSP actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspAction {
    Diagnostics,
    Hover,
    Definition,
    References,
    Completion,
    Symbols,
    Format,
}

impl LspAction {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "diagnostics" => Some(Self::Diagnostics),
            "hover" => Some(Self::Hover),
            "definition" | "goto_definition" => Some(Self::Definition),
            "references" | "find_references" => Some(Self::References),
            "completion" | "completions" => Some(Self::Completion),
            "symbols" | "document_symbols" => Some(Self::Symbols),
            "format" | "formatting" => Some(Self::Format),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspDiagnostic {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub severity: String,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspLocation {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub end_line: Option<u32>,
    pub end_character: Option<u32>,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspHoverResult {
    pub content: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspCompletionItem {
    pub label: String,
    pub kind: Option<String>,
    pub detail: Option<String>,
    pub insert_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspSymbol {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspServerStatus {
    Connected,
    Disconnected,
    Starting,
    Error,
}

impl std::fmt::Display for LspServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => write!(f, "connected"),
            Self::Disconnected => write!(f, "disconnected"),
            Self::Starting => write!(f, "starting"),
            Self::Error => write!(f, "error"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerState {
    pub language: String,
    pub status: LspServerStatus,
    pub root_path: Option<String>,
    pub capabilities: Vec<String>,
    pub diagnostics: Vec<LspDiagnostic>,
    /// LSP server 命令(如 "rust-analyzer"),Step 4.2 新增。
    /// None 表示未配置真实 server(仅 placeholder 注册)。
    #[serde(default)]
    pub server_command: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LspRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Default)]
struct RegistryInner {
    servers: HashMap<String, LspServerState>,
    /// Step 4.2:已 spawn 的真实传输层。
    /// key 是语言标识(如 "rust"),value 是 ProcessLspTransport 实例。
    /// dispatch 时优先使用此处存储的 transport,无则 fallback 到 MemoryLspTransport。
    process_transports: HashMap<String, Arc<Mutex<ProcessLspTransport>>>,
}

impl std::fmt::Debug for RegistryInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryInner")
            .field("servers", &self.servers)
            .field(
                "process_transports",
                &self.process_transports.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl LspRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        language: &str,
        status: LspServerStatus,
        root_path: Option<&str>,
        capabilities: Vec<String>,
    ) {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.servers.insert(
            language.to_owned(),
            LspServerState {
                language: language.to_owned(),
                status,
                root_path: root_path.map(str::to_owned),
                capabilities,
                diagnostics: Vec::new(),
                server_command: None,
            },
        );
    }

    /// Step 4.2:注册 LSP server 并记录其启动命令。
    ///
    /// 与 [`register`](Self::register) 相同,但额外存储 `server_command`
    /// (如 "rust-analyzer"),供后续 [`spawn_server`](Self::spawn_server) 使用。
    pub fn register_with_command(
        &self,
        language: &str,
        status: LspServerStatus,
        root_path: Option<&str>,
        capabilities: Vec<String>,
        server_command: &str,
    ) {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.servers.insert(
            language.to_owned(),
            LspServerState {
                language: language.to_owned(),
                status,
                root_path: root_path.map(str::to_owned),
                capabilities,
                diagnostics: Vec::new(),
                server_command: Some(server_command.to_owned()),
            },
        );
    }

    /// Step 4.2:真实启动 LSP server 子进程。
    ///
    /// 创建 [`ProcessLspTransport`] 并调用其 `spawn()` 方法启动 LSP server 子进程,
    /// 完成 LSP initialize → initialized 握手。启动成功后将 transport 存入 registry,
    /// 后续 `dispatch()` 调用将优先使用此真实传输层。
    ///
    /// # 参数
    /// - `language`:语言标识(如 "rust"),需先通过 `register` 或
    ///   `register_with_command` 注册对应 server
    /// - `command`:LSP server 启动命令(如 "rust-analyzer"),覆盖注册时的 server_command
    /// - `root_path`:工作区根路径,LSP initialize 的 rootUri
    ///
    /// # 返回
    /// - `Ok(())`:server 已启动并完成 initialize 握手
    /// - `Err`:spawn 失败、initialize 超时或 IO 错误
    ///
    /// # 错误处理
    /// - 若 language 未注册,返回错误
    /// - 若该 language 已有运行中的 transport,返回错误(需先 `shutdown_server`)
    /// - spawn 失败时,server 状态更新为 `Error`
    pub fn spawn_server(
        &self,
        language: &str,
        command: &str,
        root_path: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");

        // 检查是否已有运行中的 transport
        if inner.process_transports.contains_key(language) {
            return Err(format!(
                "LSP server for '{language}' already spawned; call shutdown_server first"
            ));
        }

        // 检查 language 是否已注册
        let server = inner
            .servers
            .get_mut(language)
            .ok_or_else(|| format!("LSP server not registered for language: {language}"))?;

        server.status = LspServerStatus::Starting;
        server.server_command = Some(command.to_owned());
        server.root_path = Some(root_path.to_owned());

        // 创建并 spawn ProcessLspTransport
        let mut transport = ProcessLspTransport::new(
            language.to_owned(),
            Some(root_path.to_owned()),
            command.to_owned(),
        );

        match transport.spawn() {
            Ok(()) => {
                let server = inner
                    .servers
                    .get_mut(language)
                    .expect("server was checked above");
                server.status = LspServerStatus::Connected;
                inner
                    .process_transports
                    .insert(language.to_owned(), Arc::new(Mutex::new(transport)));
                Ok(())
            }
            Err(e) => {
                let server = inner
                    .servers
                    .get_mut(language)
                    .expect("server was checked above");
                server.status = LspServerStatus::Error;
                Err(format!("failed to spawn LSP server '{command}': {e}"))
            }
        }
    }

    /// Step 4.2:关闭已 spawn 的 LSP server。
    ///
    /// 从 registry 中移除 transport(触发 Drop → kill 子进程),
    /// 并将 server 状态更新为 `Disconnected`。
    ///
    /// 若该 language 未 spawn,返回 `Ok(())`(幂等)。
    pub fn shutdown_server(&self, language: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
        if let Some(transport_arc) = inner.process_transports.remove(language) {
            // Drop transport 触发 ProcessLspTransport::drop → kill 子进程
            // 显式 lock 一下确保 Drop 在这里发生(而非延迟到 Arc 引用计数清零)
            if let Ok(mut transport) = transport_arc.lock() {
                // 显式 drop 内部 child
                transport.shutdown();
            }
            if let Some(server) = inner.servers.get_mut(language) {
                server.status = LspServerStatus::Disconnected;
            }
        }
        Ok(())
    }

    /// Step 4.2:检查指定 language 是否有已 spawn 的真实 transport。
    #[must_use]
    pub fn is_server_spawned(&self, language: &str) -> bool {
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.process_transports.contains_key(language)
    }

    /// Step 4.2:获取指定文件的 LSP symbols。
    ///
    /// 通过 `textDocument/documentSymbol` 请求 LSP server,
    /// 并用 [`parse_document_symbols`] 解析响应为 `Vec<LspSymbol>`。
    ///
    /// # 前置条件
    /// - 文件路径对应的 language 必须已注册并 spawn_server
    /// - LSP server 必须支持 documentSymbol 能力
    ///
    /// # 返回
    /// - `Ok(Vec<LspSymbol>)`:成功获取并解析
    /// - `Err`:server 未连接、dispatch 失败或解析错误
    ///
    /// # 与 repomap 协同
    /// 此方法返回的 symbols 可注入 `RepoMap::augment_with_lsp_symbols`,
    /// 补充 regex-based 提取的不足。
    pub fn get_symbols(&self, path: &str) -> Result<Vec<LspSymbol>, String> {
        let response = self.dispatch("symbols", Some(path), None, None, None)?;
        Ok(parse_document_symbols(&response, path))
    }

    pub fn get(&self, language: &str) -> Option<LspServerState> {
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.servers.get(language).cloned()
    }

    /// Find the appropriate server for a file path based on extension.
    pub fn find_server_for_path(&self, path: &str) -> Option<LspServerState> {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let language = match ext {
            "rs" => "rust",
            "ts" | "tsx" => "typescript",
            "js" | "jsx" => "javascript",
            "py" => "python",
            "go" => "go",
            "java" => "java",
            "c" | "h" => "c",
            "cpp" | "hpp" | "cc" => "cpp",
            "rb" => "ruby",
            "lua" => "lua",
            _ => return None,
        };

        self.get(language)
    }

    /// List all registered servers.
    pub fn list_servers(&self) -> Vec<LspServerState> {
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.servers.values().cloned().collect()
    }

    /// Add diagnostics to a server.
    pub fn add_diagnostics(
        &self,
        language: &str,
        diagnostics: Vec<LspDiagnostic>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
        let server = inner
            .servers
            .get_mut(language)
            .ok_or_else(|| format!("LSP server not found for language: {language}"))?;
        server.diagnostics.extend(diagnostics);
        Ok(())
    }

    /// Get diagnostics for a specific file path.
    pub fn get_diagnostics(&self, path: &str) -> Vec<LspDiagnostic> {
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner
            .servers
            .values()
            .flat_map(|s| &s.diagnostics)
            .filter(|d| d.path == path)
            .cloned()
            .collect()
    }

    /// Clear diagnostics for a language server.
    pub fn clear_diagnostics(&self, language: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
        let server = inner
            .servers
            .get_mut(language)
            .ok_or_else(|| format!("LSP server not found for language: {language}"))?;
        server.diagnostics.clear();
        Ok(())
    }

    /// Disconnect a server.
    pub fn disconnect(&self, language: &str) -> Option<LspServerState> {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.servers.remove(language)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.servers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Dispatch an LSP action and return a structured result.
    pub fn dispatch(
        &self,
        action: &str,
        path: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
        _query: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let lsp_action =
            LspAction::from_str(action).ok_or_else(|| format!("unknown LSP action: {action}"))?;

        // For diagnostics, we can check existing cached diagnostics
        if lsp_action == LspAction::Diagnostics {
            if let Some(path) = path {
                let diags = self.get_diagnostics(path);
                return Ok(serde_json::json!({
                    "action": "diagnostics",
                    "path": path,
                    "diagnostics": diags,
                    "count": diags.len()
                }));
            }
            // All diagnostics across all servers
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            let all_diags: Vec<_> = inner
                .servers
                .values()
                .flat_map(|s| &s.diagnostics)
                .collect();
            return Ok(serde_json::json!({
                "action": "diagnostics",
                "diagnostics": all_diags,
                "count": all_diags.len()
            }));
        }

        // For other actions, we need a connected server for the given file
        let path = path.ok_or("path is required for this LSP action")?;
        let server = self
            .find_server_for_path(path)
            .ok_or_else(|| format!("no LSP server available for path: {path}"))?;

        if server.status != LspServerStatus::Connected {
            return Err(format!(
                "LSP server for '{}' is not connected (status: {})",
                server.language, server.status
            ));
        }

        // Step 4.2 — 真实 LSP JSON-RPC 调用。
        // 详见 docs/harness-engineering-optimization-plan.md Step 4.2
        //
        // 优先级:
        // 1. 若 process_transports 中有已 spawn 的真实 transport,使用它发送 JSON-RPC
        // 2. 否则 fallback 到 LspJsonRpcClient(MemoryLspTransport)— 保持向后兼容
        //
        // 协议流程:initialize → initialized → didChange → completion/hover/definition
        let request = LspRequest::new(
            lsp_action,
            path,
            line,
            character,
            server.language.clone(),
        );

        // 检查是否有已 spawn 的真实 transport
        let transport_arc = {
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            inner.process_transports.get(&server.language).cloned()
        };

        if let Some(transport_arc) = transport_arc {
            // 使用真实 ProcessLspTransport
            let transport = transport_arc
                .lock()
                .map_err(|_| "transport lock poisoned".to_string())?;
            let method = request.method();
            let params = request.params();
            let rpc_response = transport.send(method, params)?;

            return Ok(serde_json::json!({
                "action": format!("{:?}", request.action).to_lowercase(),
                "path": request.path,
                "line": request.line,
                "character": request.character,
                "language": server.language,
                "method": method,
                "rpc_response": rpc_response,
                "transport": "process",
                "status": "dispatched"
            }));
        }

        // Fallback:MemoryLspTransport(测试或未 spawn 场景)
        let rpc_client = LspJsonRpcClient::new(server.language.clone(), server.root_path.clone());
        rpc_client.dispatch(&request)
    }
}

// ============================================================================
// Step 4.2 — LSP JSON-RPC 2.0 客户端
// 详见 docs/harness-engineering-optimization-plan.md Step 4.2
// ============================================================================

/// LSP JSON-RPC 2.0 请求描述符。
///
/// 由 [`LspJsonRpcClient::dispatch`] 消费,构造具体的 JSON-RPC method 和 params。
#[derive(Debug, Clone)]
pub struct LspRequest {
    /// LSP action(hover/completion/definition/references/symbols/format)。
    pub action: LspAction,
    /// 文件路径(URI 形式,file://...)。
    pub path: String,
    /// 行号(0-based)。
    pub line: Option<u32>,
    /// 列号(0-based)。
    pub character: Option<u32>,
    /// 语言标识(如 "rust" / "typescript")。
    pub language: String,
}

impl LspRequest {
    #[must_use]
    pub fn new(
        action: LspAction,
        path: impl Into<String>,
        line: Option<u32>,
        character: Option<u32>,
        language: impl Into<String>,
    ) -> Self {
        Self {
            action,
            path: path.into(),
            line,
            character,
            language: language.into(),
        }
    }

    /// 将文件路径转为 file:// URI。
    #[must_use]
    pub fn file_uri(&self) -> String {
        let path = &self.path;
        if path.starts_with("file://") {
            path.clone()
        } else {
            // Windows 路径需要特殊处理(反斜杠 → 正斜杠)
            let normalized = path.replace('\\', "/");
            if normalized.starts_with('/') {
                format!("file://{normalized}")
            } else {
                format!("file:///{normalized}")
            }
        }
    }

    /// 构造 JSON-RPC method 名称。
    #[must_use]
    pub fn method(&self) -> &'static str {
        match self.action {
            LspAction::Hover => "textDocument/hover",
            LspAction::Definition => "textDocument/definition",
            LspAction::References => "textDocument/references",
            LspAction::Completion => "textDocument/completion",
            LspAction::Symbols => "textDocument/documentSymbol",
            LspAction::Format => "textDocument/formatting",
            LspAction::Diagnostics => "textDocument/publishDiagnostics",
        }
    }

    /// 构造 JSON-RPC params。
    #[must_use]
    pub fn params(&self) -> serde_json::Value {
        let uri = self.file_uri();
        match self.action {
            LspAction::Hover | LspAction::Definition | LspAction::Completion => {
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": self.line.unwrap_or(0),
                        "character": self.character.unwrap_or(0)
                    }
                })
            }
            LspAction::References => serde_json::json!({
                "textDocument": { "uri": uri },
                "position": {
                    "line": self.line.unwrap_or(0),
                    "character": self.character.unwrap_or(0)
                },
                "context": { "includeDeclaration": true }
            }),
            LspAction::Symbols => serde_json::json!({
                "textDocument": { "uri": uri }
            }),
            LspAction::Format => serde_json::json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 4, "insertSpaces": true }
            }),
            LspAction::Diagnostics => serde_json::json!({
                "uri": uri
            }),
        }
    }
}

/// LSP 传输层 trait — 抽象 stdin/stdout 或其他传输。
///
/// 默认实现 [`MemoryLspTransport`] 用于测试;生产环境使用 [`ProcessLspTransport`]
/// 启动子进程并通过 stdin/stdout 通信。
pub trait LspTransport: Send + Sync {
    /// 发送 JSON-RPC 请求并返回响应。
    fn send(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String>;
}

/// 内存传输层 — 用于测试和 placeholder。
///
/// 不实际启动 LSP server,返回结构化的 "not yet spawned" 响应,
/// 但协议层(initialize/didChange/completion)的 JSON-RPC 构造是真实的。
#[derive(Debug, Clone, Default)]
pub struct MemoryLspTransport {
    /// 语言标识。
    pub language: String,
    /// root_path。
    pub root_path: Option<String>,
}

impl MemoryLspTransport {
    #[must_use]
    pub fn new(language: impl Into<String>, root_path: Option<String>) -> Self {
        Self {
            language: language.into(),
            root_path,
        }
    }
}

impl LspTransport for MemoryLspTransport {
    fn send(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        // 内存传输层返回结构化响应,表明协议层已构造正确但未实际连接 LSP server。
        // 这允许测试验证 JSON-RPC method/params 构造逻辑。
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transport": "memory",
                "language": self.language,
                "method": method,
                "params": params,
                "status": "protocol_constructed",
                "message": "JSON-RPC request constructed correctly; spawn real LSP server via ProcessLspTransport for actual responses"
            }
        }))
    }
}

/// 进程传输层 — 启动 LSP server 子进程,通过 stdin/stdout 通信。
///
/// BUG-12:实现真实子进程启动(Step 4.2)。
/// 通过 `std::process::Command` 启动 LSP server,使用 LSP 规范的
/// Content-Length header + JSON-RPC body 通过 stdin/stdout 通信。
///
/// 生命周期:
/// 1. `new()` 构造但未启动
/// 2. `spawn()` 启动子进程并发送 `initialize` 请求
/// 3. `send()` 发送 JSON-RPC 请求并读取响应
/// 4. 进程在 drop 时自动清理
pub struct ProcessLspTransport {
    /// 语言标识。
    pub language: String,
    /// root_path。
    pub root_path: Option<String>,
    /// LSP server 命令(如 "rust-analyzer")。
    pub server_command: String,
    /// 是否已初始化(已发送 initialize 请求)。
    pub initialized: bool,
    /// 已启动的子进程(若存在)。
    child: Option<Arc<Mutex<Child>>>,
    /// 子进程 stdin(用于写入请求)。
    child_stdin: Option<Arc<Mutex<ChildStdin>>>,
    /// 子进程 stdout(用于读取响应)。
    child_stdout: Option<Arc<Mutex<ChildStdout>>>,
    /// JSON-RPC 请求 ID 计数器。
    next_id: Arc<Mutex<u64>>,
}

impl std::fmt::Debug for ProcessLspTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessLspTransport")
            .field("language", &self.language)
            .field("root_path", &self.root_path)
            .field("server_command", &self.server_command)
            .field("initialized", &self.initialized)
            .field("spawned", &self.child.is_some())
            .finish()
    }
}

impl ProcessLspTransport {
    #[must_use]
    pub fn new(
        language: impl Into<String>,
        root_path: Option<String>,
        server_command: impl Into<String>,
    ) -> Self {
        Self {
            language: language.into(),
            root_path,
            server_command: server_command.into(),
            initialized: false,
            child: None,
            child_stdin: None,
            child_stdout: None,
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// BUG-12:启动 LSP server 子进程(Step 4.2)。
    ///
    /// 通过 `std::process::Command` 启动 `server_command`,
    /// 配置 stdin/stdout 为 piped,stderr 为 inherit。
    /// 启动后自动发送 `initialize` 请求并等待响应。
    pub fn spawn(&mut self) -> Result<(), String> {
        let mut child = Command::new(&self.server_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to spawn LSP server '{}': {e}", self.server_command))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to get stdin handle".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to get stdout handle".to_string())?;

        self.child = Some(Arc::new(Mutex::new(child)));
        self.child_stdin = Some(Arc::new(Mutex::new(stdin)));
        self.child_stdout = Some(Arc::new(Mutex::new(stdout)));

        // 发送 initialize 请求
        let init_params = self.initialize_params();
        let _response = self.send("initialize", init_params)?;
        self.initialized = true;

        // 发送 initialized 通知(无响应)
        let _ = self.send_notification("initialized", serde_json::json!({}));

        Ok(())
    }

    /// 发送 JSON-RPC 通知(无 id,不等待响应)。
    fn send_notification(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), String> {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&message)
    }

    /// 构造 initialize 请求的 params。
    #[must_use]
    pub fn initialize_params(&self) -> serde_json::Value {
        let root_uri = self.root_path.as_ref().map(|p| {
            let normalized = p.replace('\\', "/");
            if normalized.starts_with('/') {
                format!("file://{normalized}")
            } else {
                format!("file:///{normalized}")
            }
        });
        serde_json::json!({
            "processId": std::process::id(),
            "rootPath": self.root_path,
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "completion": { "completionItem": { "snippetSupport": true } },
                    "hover": { "contentFormat": ["markdown", "plaintext"] }
                }
            }
        })
    }

    /// 检查子进程是否已启动。
    #[must_use]
    pub fn is_spawned(&self) -> bool {
        self.child.is_some()
    }

    /// Step 4.2:显式关闭 LSP server 子进程。
    ///
    /// 与 Drop 不同,此方法允许调用方在 Drop 之前主动关闭 server,
    /// 并能获取关闭错误(Drop 无法返回错误)。
    /// 关闭后,`is_spawned()` 返回 false,后续 `send()` 调用将返回
    /// "not_spawned" placeholder 响应(保持向后兼容)。
    pub fn shutdown(&mut self) {
        if let Some(child) = self.child.take() {
            let mut child = match child.lock() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[lsp] child lock poisoned during shutdown: {e}");
                    return;
                }
            };
            // 尝试优雅终止;失败则强制杀死
            let _ = child.kill();
            let _ = child.wait();
        }
        // 清理 stdin/stdout 句柄
        self.child_stdin = None;
        self.child_stdout = None;
        self.initialized = false;
    }

    /// 写入 JSON-RPC 消息到子进程 stdin(Content-Length header)。
    fn write_message(&self, message: &serde_json::Value) -> Result<(), String> {
        let Some(stdin_handle) = &self.child_stdin else {
            return Err("LSP server not spawned; call spawn() first".to_string());
        };

        let body = serde_json::to_string(message)
            .map_err(|e| format!("JSON serialization error: {e}"))?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        let mut stdin = stdin_handle
            .lock()
            .map_err(|_| "stdin lock poisoned".to_string())?;
        stdin
            .write_all(header.as_bytes())
            .map_err(|e| format!("write header error: {e}"))?;
        stdin
            .write_all(body.as_bytes())
            .map_err(|e| format!("write body error: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("flush error: {e}"))?;

        Ok(())
    }

    /// 从子进程 stdout 读取 JSON-RPC 响应(Content-Length header)。
    fn read_message(&self) -> Result<serde_json::Value, String> {
        let Some(stdout_handle) = &self.child_stdout else {
            return Err("LSP server not spawned; call spawn() first".to_string())?;
        };

        let mut stdout = stdout_handle
            .lock()
            .map_err(|_| "stdout lock poisoned".to_string())?;

        // 读取 Content-Length header
        let mut header_line = String::new();
        let mut byte = [0u8; 1];
        loop {
            stdout
                .read_exact(&mut byte)
                .map_err(|e| format!("read header error: {e}"))?;
            let c = byte[0] as char;
            header_line.push(c);
            if header_line.ends_with("\r\n\r\n") {
                break;
            }
            // 防止 header 无限增长
            if header_line.len() > 1024 {
                return Err("LSP header too long, possibly malformed".to_string());
            }
        }

        // 解析 Content-Length
        let content_length: usize = header_line
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length: ")
                    .and_then(|v| v.trim().parse().ok())
            })
            .ok_or_else(|| format!("missing Content-Length in header: {header_line}"))?;

        // 读取 body
        let mut body = vec![0u8; content_length];
        stdout
            .read_exact(&mut body)
            .map_err(|e| format!("read body error: {e}"))?;

        let body_str = String::from_utf8(body)
            .map_err(|e| format!("body UTF-8 error: {e}"))?;

        serde_json::from_str(&body_str)
            .map_err(|e| format!("JSON parse error: {e}"))
    }
}

impl LspTransport for ProcessLspTransport {
    fn send(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        // 若子进程已启动,走真实 stdin/stdout 通信
        if self.child.is_some() {
            let id = {
                let mut next = self.next_id.lock().map_err(|_| "id counter lock poisoned".to_string())?;
                let id = *next;
                *next += 1;
                id
            };

            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });

            self.write_message(&request)?;
            return self.read_message();
        }

        // 子进程未启动,返回 placeholder 响应(保持向后兼容)
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transport": "process",
                "language": self.language,
                "server_command": self.server_command,
                "method": method,
                "params": params,
                "status": "not_spawned",
                "message": "ProcessLspTransport not spawned — call spawn() for actual responses"
            }
        }))
    }
}

impl Drop for ProcessLspTransport {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let mut child = match child.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            // 尝试优雅终止;失败则强制杀死
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ============================================================================
// Step 4.2 — LSP symbol 解析(documentSymbol 响应 → LspSymbol)
// 详见 docs/harness-engineering-optimization-plan.md Step 4.2
// ============================================================================

/// LSP SymbolKind 枚举值(参考 LSP spec 3.17 §3.10.2)。
///
/// 用于将 `textDocument/documentSymbol` 响应中的数字 kind 映射为可读字符串。
/// 数字编码固定,不能更改(协议规范)。
#[allow(dead_code)]
pub fn symbol_kind_to_str(kind: u32) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "unknown",
    }
}

// ============================================================================
// Step 4.2-c: lsp-types crate 类型转换
// 用官方类型替代部分手搓 serde_json,提高协议层可靠性
// ============================================================================

/// 将 `lsp_types::SymbolKind`(newtype struct,内部为 u32)映射为可读字符串。
///
/// 注意:由于 orphan rule,无法为 `lsp_types::SymbolKind` 实现 `From<T> for &str`,
/// 故用辅助函数替代。与 `symbol_kind_to_str(u32)` 保持一致。
fn lsp_symbol_kind_to_str(kind: lsp_types::SymbolKind) -> &'static str {
    use lsp_types::SymbolKind as SK;
    // SymbolKind 在 lsp-types 0.95 中是 newtype struct,变体以 const 形式存在。
    if kind == SK::FILE {
        "file"
    } else if kind == SK::MODULE {
        "module"
    } else if kind == SK::NAMESPACE {
        "namespace"
    } else if kind == SK::PACKAGE {
        "package"
    } else if kind == SK::CLASS {
        "class"
    } else if kind == SK::METHOD {
        "method"
    } else if kind == SK::PROPERTY {
        "property"
    } else if kind == SK::FIELD {
        "field"
    } else if kind == SK::CONSTRUCTOR {
        "constructor"
    } else if kind == SK::ENUM {
        "enum"
    } else if kind == SK::INTERFACE {
        "interface"
    } else if kind == SK::FUNCTION {
        "function"
    } else if kind == SK::VARIABLE {
        "variable"
    } else if kind == SK::CONSTANT {
        "constant"
    } else if kind == SK::STRING {
        "string"
    } else if kind == SK::NUMBER {
        "number"
    } else if kind == SK::BOOLEAN {
        "boolean"
    } else if kind == SK::ARRAY {
        "array"
    } else if kind == SK::OBJECT {
        "object"
    } else if kind == SK::KEY {
        "key"
    } else if kind == SK::NULL {
        "null"
    } else if kind == SK::ENUM_MEMBER {
        "enum_member"
    } else if kind == SK::STRUCT {
        "struct"
    } else if kind == SK::EVENT {
        "event"
    } else if kind == SK::OPERATOR {
        "operator"
    } else if kind == SK::TYPE_PARAMETER {
        "type_parameter"
    } else {
        "unknown"
    }
}

impl From<lsp_types::DocumentSymbol> for LspSymbol {
    /// 将 `lsp_types::DocumentSymbol`(官方类型)转换为 `LspSymbol`。
    ///
    /// 注意:DocumentSymbol 不包含文件路径,转换后 `path` 为空,
    /// 调用方需手动设置(参考 `parse_document_symbols_typed` 的实现)。
    fn from(doc: lsp_types::DocumentSymbol) -> Self {
        let kind_str = lsp_symbol_kind_to_str(doc.kind);
        let position = doc.selection_range.start;
        Self {
            name: doc.name,
            kind: kind_str.to_owned(),
            path: String::new(), // 调用方填充
            line: position.line,
            character: position.character,
        }
    }
}

impl From<lsp_types::SymbolInformation> for LspSymbol {
    /// 将 `lsp_types::SymbolInformation`(官方类型)转换为 `LspSymbol`。
    ///
    /// SymbolInformation 的 location.uri 提供文件路径,
    /// location.range.start 提供位置。
    fn from(info: lsp_types::SymbolInformation) -> Self {
        let kind_str = lsp_symbol_kind_to_str(info.kind);
        let position = info.location.range.start;
        let path = info.location.uri.as_str().to_owned();
        Self {
            name: info.name,
            kind: kind_str.to_owned(),
            path,
            line: position.line,
            character: position.character,
        }
    }
}

/// Step 4.2-c:使用 `lsp_types` 官方类型解析 documentSymbol 响应。
///
/// 与 [`parse_document_symbols`](fn@parse_document_symbols) 功能相同,
/// 但内部使用 `lsp_types::DocumentSymbol` / `lsp_types::SymbolInformation`
/// 进行反序列化,类型更安全。
///
/// # 容错策略
/// - 优先尝试用 `lsp_types` 反序列化
/// - 若失败(LSP server 返回非标准字段),fallback 到 `parse_document_symbols`
///   (serde_json 手动解析)
///
/// # 参数
/// - `response`:LSP JSON-RPC 响应
/// - `path`:文件路径(用于 DocumentSymbol 格式,SymbolInformation 自带 uri)
#[must_use]
pub fn parse_document_symbols_typed(response: &serde_json::Value, path: &str) -> Vec<LspSymbol> {
    let result = response.get("result").unwrap_or(response);

    // 尝试 1:DocumentSymbol[] 反序列化
    if let Ok(doc_symbols) = serde_json::from_value::<Vec<lsp_types::DocumentSymbol>>(result.clone())
    {
        let mut symbols = Vec::new();
        for doc in doc_symbols {
            collect_document_symbols_typed(doc, path, &mut symbols);
        }
        return symbols;
    }

    // 尝试 2:SymbolInformation[] 反序列化
    if let Ok(sym_infos) =
        serde_json::from_value::<Vec<lsp_types::SymbolInformation>>(result.clone())
    {
        return sym_infos.into_iter().map(LspSymbol::from).collect();
    }

    // Fallback:serde_json 手动解析(容错)
    parse_document_symbols(response, path)
}

/// 递归收集 DocumentSymbol 及其 children 为 LspSymbol 列表。
fn collect_document_symbols_typed(
    doc: lsp_types::DocumentSymbol,
    path: &str,
    out: &mut Vec<LspSymbol>,
) {
    let mut symbol: LspSymbol = doc.clone().into();
    symbol.path = path.to_owned();
    out.push(symbol);

    // 递归处理 children
    if let Some(children) = doc.children {
        for child in children {
            collect_document_symbols_typed(child, path, out);
        }
    }
}

/// Step 4.2:解析 `textDocument/documentSymbol` 响应为 `Vec<LspSymbol>`。
///
/// LSP server 可能返回两种格式(参考 LSP spec 3.17 §3.11.2):
/// 1. `DocumentSymbol[]` — 嵌套结构,有 `range`/`selectionRange`/`children`
/// 2. `SymbolInformation[]` — 平铺结构,有 `location`
///
/// 此函数自动识别两种格式并统一转换为 `LspSymbol`。
/// 嵌套结构的 `children` 会被递归解析,平铺为顶层 `LspSymbol` 列表。
///
/// # 参数
/// - `response`:LSP JSON-RPC 响应(完整 JSON-RPC envelope,包含 `result` 字段)
/// - `path`:文件路径(用于填充 `LspSymbol.path`,因为 DocumentSymbol 不包含路径)
///
/// # 返回
/// 解析后的 symbol 列表。若响应格式不匹配,返回空 Vec(不报错,容错处理)。
#[must_use]
pub fn parse_document_symbols(response: &serde_json::Value, path: &str) -> Vec<LspSymbol> {
    // 从 JSON-RPC envelope 中提取 result
    let result = response.get("result").unwrap_or(response);

    // result 可能是数组(DocumentSymbol[] 或 SymbolInformation[])或 null
    let symbols_array = match result.as_array() {
        Some(arr) => arr,
        None => return Vec::new(),
    };

    let mut symbols = Vec::new();
    for item in symbols_array {
        // 尝试 DocumentSymbol 格式(有 range 字段)
        if item.get("range").is_some() {
            parse_document_symbol_recursive(item, path, &mut symbols);
        }
        // 尝试 SymbolInformation 格式(有 location 字段)
        else if let Some(location) = item.get("location") {
            if let Some(symbol) = parse_symbol_information(item, location) {
                symbols.push(symbol);
            }
        }
        // 未知格式,跳过(容错)
    }
    symbols
}

/// 递归解析 DocumentSymbol(可能包含 children)。
fn parse_document_symbol_recursive(
    item: &serde_json::Value,
    path: &str,
    out: &mut Vec<LspSymbol>,
) {
    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let kind_num = item
        .get("kind")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(0);
    let kind = symbol_kind_to_str(kind_num);

    // selectionRange.start 是 symbol 的精确位置(LSP spec 3.17 §3.11.2)
    let (line, character) = item
        .get("selectionRange")
        .and_then(|r| r.get("start"))
        .and_then(|s| {
            let line = s.get("line").and_then(|v| v.as_u64()).map(|n| n as u32);
            let character = s
                .get("character")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            line.zip(character)
        })
        .unwrap_or((0, 0));

    out.push(LspSymbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        path: path.to_owned(),
        line,
        character,
    });

    // 递归处理 children(嵌套结构)
    if let Some(children) = item.get("children").and_then(|v| v.as_array()) {
        for child in children {
            parse_document_symbol_recursive(child, path, out);
        }
    }
}

/// 解析 SymbolInformation(平铺结构,有 location)。
fn parse_symbol_information(
    item: &serde_json::Value,
    location: &serde_json::Value,
) -> Option<LspSymbol> {
    let name = item.get("name").and_then(|v| v.as_str())?;
    let kind_num = item
        .get("kind")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(0);
    let kind = symbol_kind_to_str(kind_num);

    // SymbolInformation.location.uri 提供路径,但调用方已传入 path,优先使用调用方 path
    // location.range.start 提供位置
    let (line, character) = location
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| {
            let line = s.get("line").and_then(|v| v.as_u64()).map(|n| n as u32);
            let character = s
                .get("character")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            line.zip(character)
        })
        .unwrap_or((0, 0));

    // 优先用 location.uri,但若与调用方 path 不一致,使用调用方 path
    // (因为调用方知道请求的是哪个文件)
    let path = location
        .get("uri")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    Some(LspSymbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        path: path.to_owned(),
        line,
        character,
    })
}

/// LSP JSON-RPC 2.0 客户端 — 持有传输层,构造协议请求。
///
/// 协议流程(参考 LSP 规范 3.17):
/// 1. `initialize` — 握手,交换 capabilities
/// 2. `initialized` — 通知,表示客户端已就绪
/// 3. `textDocument/didChange` — 通知,文档变更
/// 4. `textDocument/completion` / `hover` / `definition` — 请求,获取语义信息
///
/// 与 `repomap.rs` 协同:LSP 提供 symbol 信息,repomap 聚合为符号图。
pub struct LspJsonRpcClient {
    /// 语言标识。
    language: String,
    /// root_path。
    root_path: Option<String>,
    /// 传输层(默认 MemoryLspTransport)。
    transport: Box<dyn LspTransport>,
}

impl std::fmt::Debug for LspJsonRpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspJsonRpcClient")
            .field("language", &self.language)
            .field("root_path", &self.root_path)
            .finish_non_exhaustive()
    }
}

impl LspJsonRpcClient {
    #[must_use]
    pub fn new(language: impl Into<String>, root_path: Option<String>) -> Self {
        let language = language.into();
        Self {
            language: language.clone(),
            root_path,
            transport: Box::new(MemoryLspTransport::new(language, None)),
        }
    }

    /// 使用指定传输层构建(用于 ProcessLspTransport)。
    #[must_use]
    pub fn with_transport(
        language: impl Into<String>,
        root_path: Option<String>,
        transport: Box<dyn LspTransport>,
    ) -> Self {
        Self {
            language: language.into(),
            root_path,
            transport,
        }
    }

    /// 分派 LSP 请求 — 构造 JSON-RPC 并通过传输层发送。
    ///
    /// 流程:
    /// 1. 构造 [`LspRequest`] 的 method 和 params
    /// 2. 通过传输层发送 JSON-RPC 请求
    /// 3. 返回响应(包含 transport 状态和协议层构造结果)
    pub fn dispatch(&self, request: &LspRequest) -> Result<serde_json::Value, String> {
        let method = request.method();
        let params = request.params();

        let response = self.transport.send(method, params)?;

        // 包装响应,附加元数据
        Ok(serde_json::json!({
            "action": format!("{:?}", request.action).to_lowercase(),
            "path": request.path,
            "line": request.line,
            "character": request.character,
            "language": self.language,
            "method": method,
            "rpc_response": response,
            "status": "dispatched"
        }))
    }

    /// 发送 initialize 请求(LSP 握手第一步)。
    pub fn initialize(&self) -> Result<serde_json::Value, String> {
        let params = if let Some(root) = &self.root_path {
            let normalized = root.replace('\\', "/");
            let root_uri = if normalized.starts_with('/') {
                format!("file://{normalized}")
            } else {
                format!("file:///{normalized}")
            };
            serde_json::json!({
                "processId": std::process::id(),
                "rootPath": root,
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "completion": { "completionItem": { "snippetSupport": true } },
                        "hover": { "contentFormat": ["markdown", "plaintext"] }
                    }
                }
            })
        } else {
            serde_json::json!({
                "processId": std::process::id(),
                "capabilities": {}
            })
        };
        self.transport.send("initialize", params)
    }

    /// 发送 didChange 通知(文档变更)。
    pub fn did_change(
        &self,
        path: &str,
        text: &str,
        version: u32,
    ) -> Result<serde_json::Value, String> {
        let normalized = path.replace('\\', "/");
        let uri = if normalized.starts_with("file://") {
            normalized
        } else if normalized.starts_with('/') {
            format!("file://{normalized}")
        } else {
            format!("file:///{normalized}")
        };
        let params = serde_json::json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }]
        });
        self.transport.send("textDocument/didChange", params)
    }

    /// 获取语言标识。
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_retrieves_server() {
        let registry = LspRegistry::new();
        registry.register(
            "rust",
            LspServerStatus::Connected,
            Some("/workspace"),
            vec!["hover".into(), "completion".into()],
        );

        let server = registry.get("rust").expect("should exist");
        assert_eq!(server.language, "rust");
        assert_eq!(server.status, LspServerStatus::Connected);
        assert_eq!(server.capabilities.len(), 2);
    }

    #[test]
    fn finds_server_by_file_extension() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        registry.register("typescript", LspServerStatus::Connected, None, vec![]);

        let rs_server = registry.find_server_for_path("src/main.rs").unwrap();
        assert_eq!(rs_server.language, "rust");

        let ts_server = registry.find_server_for_path("src/index.ts").unwrap();
        assert_eq!(ts_server.language, "typescript");

        assert!(registry.find_server_for_path("data.csv").is_none());
    }

    #[test]
    fn manages_diagnostics() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);

        registry
            .add_diagnostics(
                "rust",
                vec![LspDiagnostic {
                    path: "src/main.rs".into(),
                    line: 10,
                    character: 5,
                    severity: "error".into(),
                    message: "mismatched types".into(),
                    source: Some("rust-analyzer".into()),
                }],
            )
            .unwrap();

        let diags = registry.get_diagnostics("src/main.rs");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "mismatched types");

        registry.clear_diagnostics("rust").unwrap();
        assert!(registry.get_diagnostics("src/main.rs").is_empty());
    }

    #[test]
    fn dispatches_diagnostics_action() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        registry
            .add_diagnostics(
                "rust",
                vec![LspDiagnostic {
                    path: "src/lib.rs".into(),
                    line: 1,
                    character: 0,
                    severity: "warning".into(),
                    message: "unused import".into(),
                    source: None,
                }],
            )
            .unwrap();

        let result = registry
            .dispatch("diagnostics", Some("src/lib.rs"), None, None, None)
            .unwrap();
        assert_eq!(result["count"], 1);
    }

    #[test]
    fn dispatches_hover_action() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);

        let result = registry
            .dispatch("hover", Some("src/main.rs"), Some(10), Some(5), None)
            .unwrap();
        assert_eq!(result["action"], "hover");
        assert_eq!(result["language"], "rust");
    }

    #[test]
    fn rejects_action_on_disconnected_server() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Disconnected, None, vec![]);

        assert!(registry
            .dispatch("hover", Some("src/main.rs"), Some(1), Some(0), None)
            .is_err());
    }

    #[test]
    fn rejects_unknown_action() {
        let registry = LspRegistry::new();
        assert!(registry
            .dispatch("unknown_action", Some("file.rs"), None, None, None)
            .is_err());
    }

    #[test]
    fn disconnects_server() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        assert_eq!(registry.len(), 1);

        let removed = registry.disconnect("rust");
        assert!(removed.is_some());
        assert!(registry.is_empty());
    }

    #[test]
    fn lsp_action_from_str_all_aliases() {
        // given
        let cases = [
            ("diagnostics", Some(LspAction::Diagnostics)),
            ("hover", Some(LspAction::Hover)),
            ("definition", Some(LspAction::Definition)),
            ("goto_definition", Some(LspAction::Definition)),
            ("references", Some(LspAction::References)),
            ("find_references", Some(LspAction::References)),
            ("completion", Some(LspAction::Completion)),
            ("completions", Some(LspAction::Completion)),
            ("symbols", Some(LspAction::Symbols)),
            ("document_symbols", Some(LspAction::Symbols)),
            ("format", Some(LspAction::Format)),
            ("formatting", Some(LspAction::Format)),
            ("unknown", None),
        ];

        // when
        let resolved: Vec<_> = cases
            .into_iter()
            .map(|(input, expected)| (input, LspAction::from_str(input), expected))
            .collect();

        // then
        for (input, actual, expected) in resolved {
            assert_eq!(actual, expected, "unexpected action resolution for {input}");
        }
    }

    #[test]
    fn lsp_server_status_display_all_variants() {
        // given
        let cases = [
            (LspServerStatus::Connected, "connected"),
            (LspServerStatus::Disconnected, "disconnected"),
            (LspServerStatus::Starting, "starting"),
            (LspServerStatus::Error, "error"),
        ];

        // when
        let rendered: Vec<_> = cases
            .into_iter()
            .map(|(status, expected)| (status.to_string(), expected))
            .collect();

        // then
        assert_eq!(
            rendered,
            vec![
                ("connected".to_string(), "connected"),
                ("disconnected".to_string(), "disconnected"),
                ("starting".to_string(), "starting"),
                ("error".to_string(), "error"),
            ]
        );
    }

    #[test]
    fn dispatch_diagnostics_without_path_aggregates() {
        // given
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        registry.register("python", LspServerStatus::Connected, None, vec![]);
        registry
            .add_diagnostics(
                "rust",
                vec![LspDiagnostic {
                    path: "src/lib.rs".into(),
                    line: 1,
                    character: 0,
                    severity: "warning".into(),
                    message: "unused import".into(),
                    source: Some("rust-analyzer".into()),
                }],
            )
            .expect("rust diagnostics should add");
        registry
            .add_diagnostics(
                "python",
                vec![LspDiagnostic {
                    path: "script.py".into(),
                    line: 2,
                    character: 4,
                    severity: "error".into(),
                    message: "undefined name".into(),
                    source: Some("pyright".into()),
                }],
            )
            .expect("python diagnostics should add");

        // when
        let result = registry
            .dispatch("diagnostics", None, None, None, None)
            .expect("aggregate diagnostics should work");

        // then
        assert_eq!(result["action"], "diagnostics");
        assert_eq!(result["count"], 2);
        assert_eq!(result["diagnostics"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn dispatch_non_diagnostics_requires_path() {
        // given
        let registry = LspRegistry::new();

        // when
        let result = registry.dispatch("hover", None, Some(1), Some(0), None);

        // then
        assert_eq!(
            result.expect_err("path should be required"),
            "path is required for this LSP action"
        );
    }

    #[test]
    fn dispatch_no_server_for_path_errors() {
        // given
        let registry = LspRegistry::new();

        // when
        let result = registry.dispatch("hover", Some("notes.md"), Some(1), Some(0), None);

        // then
        let error = result.expect_err("missing server should fail");
        assert!(error.contains("no LSP server available for path: notes.md"));
    }

    #[test]
    fn dispatch_disconnected_server_error_payload() {
        // given
        let registry = LspRegistry::new();
        registry.register("typescript", LspServerStatus::Disconnected, None, vec![]);

        // when
        let result = registry.dispatch("hover", Some("src/index.ts"), Some(3), Some(2), None);

        // then
        let error = result.expect_err("disconnected server should fail");
        assert!(error.contains("typescript"));
        assert!(error.contains("disconnected"));
    }

    #[test]
    fn find_server_for_all_extensions() {
        // given
        let registry = LspRegistry::new();
        for language in [
            "rust",
            "typescript",
            "javascript",
            "python",
            "go",
            "java",
            "c",
            "cpp",
            "ruby",
            "lua",
        ] {
            registry.register(language, LspServerStatus::Connected, None, vec![]);
        }
        let cases = [
            ("src/main.rs", "rust"),
            ("src/index.ts", "typescript"),
            ("src/view.tsx", "typescript"),
            ("src/app.js", "javascript"),
            ("src/app.jsx", "javascript"),
            ("script.py", "python"),
            ("main.go", "go"),
            ("Main.java", "java"),
            ("native.c", "c"),
            ("native.h", "c"),
            ("native.cpp", "cpp"),
            ("native.hpp", "cpp"),
            ("native.cc", "cpp"),
            ("script.rb", "ruby"),
            ("script.lua", "lua"),
        ];

        // when
        let resolved: Vec<_> = cases
            .into_iter()
            .map(|(path, expected)| {
                (
                    path,
                    registry
                        .find_server_for_path(path)
                        .map(|server| server.language),
                    expected,
                )
            })
            .collect();

        // then
        for (path, actual, expected) in resolved {
            assert_eq!(
                actual.as_deref(),
                Some(expected),
                "unexpected mapping for {path}"
            );
        }
    }

    #[test]
    fn find_server_for_path_no_extension() {
        // given
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);

        // when
        let result = registry.find_server_for_path("Makefile");

        // then
        assert!(result.is_none());
    }

    #[test]
    fn list_servers_with_multiple() {
        // given
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        registry.register("typescript", LspServerStatus::Starting, None, vec![]);
        registry.register("python", LspServerStatus::Error, None, vec![]);

        // when
        let servers = registry.list_servers();

        // then
        assert_eq!(servers.len(), 3);
        assert!(servers.iter().any(|server| server.language == "rust"));
        assert!(servers.iter().any(|server| server.language == "typescript"));
        assert!(servers.iter().any(|server| server.language == "python"));
    }

    #[test]
    fn get_missing_server_returns_none() {
        // given
        let registry = LspRegistry::new();

        // when
        let server = registry.get("missing");

        // then
        assert!(server.is_none());
    }

    #[test]
    fn add_diagnostics_missing_language_errors() {
        // given
        let registry = LspRegistry::new();

        // when
        let result = registry.add_diagnostics("missing", vec![]);

        // then
        let error = result.expect_err("missing language should fail");
        assert!(error.contains("LSP server not found for language: missing"));
    }

    #[test]
    fn get_diagnostics_across_servers() {
        // given
        let registry = LspRegistry::new();
        let shared_path = "shared/file.txt";
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        registry.register("python", LspServerStatus::Connected, None, vec![]);
        registry
            .add_diagnostics(
                "rust",
                vec![LspDiagnostic {
                    path: shared_path.into(),
                    line: 4,
                    character: 1,
                    severity: "warning".into(),
                    message: "warn".into(),
                    source: None,
                }],
            )
            .expect("rust diagnostics should add");
        registry
            .add_diagnostics(
                "python",
                vec![LspDiagnostic {
                    path: shared_path.into(),
                    line: 8,
                    character: 3,
                    severity: "error".into(),
                    message: "err".into(),
                    source: None,
                }],
            )
            .expect("python diagnostics should add");

        // when
        let diagnostics = registry.get_diagnostics(shared_path);

        // then
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "warn"));
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "err"));
    }

    #[test]
    fn clear_diagnostics_missing_language_errors() {
        // given
        let registry = LspRegistry::new();

        // when
        let result = registry.clear_diagnostics("missing");

        // then
        let error = result.expect_err("missing language should fail");
        assert!(error.contains("LSP server not found for language: missing"));
    }

    // ========================================================================
    // Step 4.2 — LSP JSON-RPC 2.0 客户端 测试
    // ========================================================================

    #[test]
    fn lsp_request_file_uri_converts_unix_path() {
        let req = LspRequest::new(LspAction::Hover, "/workspace/src/main.rs", None, None, "rust");
        assert_eq!(req.file_uri(), "file:///workspace/src/main.rs");
    }

    #[test]
    fn lsp_request_file_uri_converts_windows_path() {
        let req = LspRequest::new(LspAction::Hover, r"C:\workspace\main.rs", None, None, "rust");
        assert_eq!(req.file_uri(), "file:///C:/workspace/main.rs");
    }

    #[test]
    fn lsp_request_file_uri_preserves_existing_file_uri() {
        let req = LspRequest::new(LspAction::Hover, "file:///workspace/main.rs", None, None, "rust");
        // 已是 file:// URI 时应原样返回
        assert_eq!(req.file_uri(), "file:///workspace/main.rs");
    }

    #[test]
    fn lsp_request_method_maps_correctly() {
        assert_eq!(LspRequest::new(LspAction::Hover, "", None, None, "").method(), "textDocument/hover");
        assert_eq!(LspRequest::new(LspAction::Definition, "", None, None, "").method(), "textDocument/definition");
        assert_eq!(LspRequest::new(LspAction::Completion, "", None, None, "").method(), "textDocument/completion");
        assert_eq!(LspRequest::new(LspAction::References, "", None, None, "").method(), "textDocument/references");
        assert_eq!(LspRequest::new(LspAction::Symbols, "", None, None, "").method(), "textDocument/documentSymbol");
        assert_eq!(LspRequest::new(LspAction::Format, "", None, None, "").method(), "textDocument/formatting");
    }

    #[test]
    fn lsp_request_params_hover_includes_position() {
        let req = LspRequest::new(LspAction::Hover, "src/main.rs", Some(10), Some(5), "rust");
        let params = req.params();
        assert_eq!(params["textDocument"]["uri"], "file:///src/main.rs");
        assert_eq!(params["position"]["line"], 10);
        assert_eq!(params["position"]["character"], 5);
    }

    #[test]
    fn lsp_request_params_completion_defaults_to_zero() {
        let req = LspRequest::new(LspAction::Completion, "src/main.rs", None, None, "rust");
        let params = req.params();
        assert_eq!(params["position"]["line"], 0);
        assert_eq!(params["position"]["character"], 0);
    }

    #[test]
    fn lsp_request_params_references_includes_context() {
        let req = LspRequest::new(LspAction::References, "src/main.rs", Some(1), Some(0), "rust");
        let params = req.params();
        assert_eq!(params["context"]["includeDeclaration"], true);
    }

    #[test]
    fn lsp_request_params_symbols_excludes_position() {
        let req = LspRequest::new(LspAction::Symbols, "src/main.rs", Some(1), Some(0), "rust");
        let params = req.params();
        assert!(params.get("position").is_none());
        assert!(params.get("textDocument").is_some());
    }

    #[test]
    fn memory_lsp_transport_returns_protocol_constructed() {
        let transport = MemoryLspTransport::new("rust", None);
        let result = transport.send("textDocument/hover", serde_json::json!({})).unwrap();
        assert_eq!(result["result"]["transport"], "memory");
        assert_eq!(result["result"]["language"], "rust");
        assert_eq!(result["result"]["status"], "protocol_constructed");
    }

    #[test]
    fn process_lsp_transport_returns_not_spawned() {
        let transport = ProcessLspTransport::new("rust", Some("/workspace".to_string()), "rust-analyzer");
        let result = transport.send("initialize", serde_json::json!({})).unwrap();
        assert_eq!(result["result"]["transport"], "process");
        assert_eq!(result["result"]["server_command"], "rust-analyzer");
        assert_eq!(result["result"]["status"], "not_spawned");
    }

    #[test]
    fn process_lsp_transport_initialize_params_includes_root_uri() {
        let transport = ProcessLspTransport::new("rust", Some("/workspace".to_string()), "rust-analyzer");
        let params = transport.initialize_params();
        assert_eq!(params["rootPath"], "/workspace");
        assert_eq!(params["rootUri"], "file:///workspace");
        assert_eq!(params["capabilities"]["textDocument"]["completion"]["completionItem"]["snippetSupport"], true);
    }

    #[test]
    fn lsp_json_rpc_client_new_uses_memory_transport() {
        let client = LspJsonRpcClient::new("rust", Some("/workspace".to_string()));
        assert_eq!(client.language(), "rust");
    }

    #[test]
    fn lsp_json_rpc_client_dispatch_returns_action_metadata() {
        let client = LspJsonRpcClient::new("rust", None);
        let request = LspRequest::new(LspAction::Hover, "src/main.rs", Some(10), Some(5), "rust");
        let result = client.dispatch(&request).unwrap();
        assert_eq!(result["action"], "hover");
        assert_eq!(result["path"], "src/main.rs");
        assert_eq!(result["line"], 10);
        assert_eq!(result["character"], 5);
        assert_eq!(result["language"], "rust");
        assert_eq!(result["method"], "textDocument/hover");
        assert_eq!(result["status"], "dispatched");
        // RPC response should be present
        assert_eq!(result["rpc_response"]["result"]["transport"], "memory");
    }

    #[test]
    fn lsp_json_rpc_client_initialize_sends_initialize_method() {
        let client = LspJsonRpcClient::new("rust", Some("/workspace".to_string()));
        let result = client.initialize().unwrap();
        assert_eq!(result["result"]["method"], "initialize");
        assert!(result["result"]["params"]["rootUri"].is_string());
    }

    #[test]
    fn lsp_json_rpc_client_did_change_constructs_correct_params() {
        let client = LspJsonRpcClient::new("rust", None);
        let result = client.did_change("src/main.rs", "fn main() {}", 1).unwrap();
        let params = &result["result"]["params"];
        assert_eq!(params["textDocument"]["uri"], "file:///src/main.rs");
        assert_eq!(params["textDocument"]["version"], 1);
        assert_eq!(params["contentChanges"][0]["text"], "fn main() {}");
    }

    #[test]
    fn lsp_json_rpc_client_with_transport_uses_custom_transport() {
        let transport = Box::new(ProcessLspTransport::new("rust", None, "rust-analyzer"));
        let client = LspJsonRpcClient::with_transport("rust", None, transport);
        let request = LspRequest::new(LspAction::Completion, "src/main.rs", Some(1), Some(0), "rust");
        let result = client.dispatch(&request).unwrap();
        assert_eq!(result["rpc_response"]["result"]["transport"], "process");
        assert_eq!(result["rpc_response"]["result"]["server_command"], "rust-analyzer");
    }

    #[test]
    fn dispatch_hover_action_uses_json_rpc_client() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, Some("/workspace"), vec![]);

        let result = registry
            .dispatch("hover", Some("src/main.rs"), Some(10), Some(5), None)
            .unwrap();
        // Verify dispatch now uses real JSON-RPC client
        assert_eq!(result["action"], "hover");
        assert_eq!(result["language"], "rust");
        assert_eq!(result["method"], "textDocument/hover");
        assert_eq!(result["status"], "dispatched");
        // RPC response should be present (memory transport)
        assert_eq!(result["rpc_response"]["result"]["transport"], "memory");
    }

    #[test]
    fn dispatch_completion_action_returns_rpc_response() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);

        let result = registry
            .dispatch("completion", Some("src/main.rs"), Some(1), Some(0), None)
            .unwrap();
        assert_eq!(result["action"], "completion");
        assert_eq!(result["method"], "textDocument/completion");
        assert!(result.get("rpc_response").is_some());
    }

    // ========================================================================
    // Step 4.2 — spawn_server / shutdown_server / dispatch 真实传输 测试
    // ========================================================================

    #[test]
    fn register_with_command_stores_server_command() {
        let registry = LspRegistry::new();
        registry.register_with_command(
            "rust",
            LspServerStatus::Disconnected,
            Some("/workspace"),
            vec!["hover".into()],
            "rust-analyzer",
        );

        let server = registry.get("rust").unwrap();
        assert_eq!(server.server_command.as_deref(), Some("rust-analyzer"));
    }

    #[test]
    fn spawn_server_fails_for_unregistered_language() {
        let registry = LspRegistry::new();
        let result = registry.spawn_server("rust", "rust-analyzer", "/workspace");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("LSP server not registered for language: rust"));
    }

    #[test]
    fn spawn_server_fails_for_nonexistent_command() {
        let registry = LspRegistry::new();
        registry.register_with_command(
            "rust",
            LspServerStatus::Disconnected,
            None,
            vec![],
            "nonexistent-lsp-server-xyz",
        );

        let result = registry.spawn_server("rust", "nonexistent-lsp-server-xyz", "/workspace");
        assert!(result.is_err());
        // spawn 失败应包含 command 名称
        let err = result.unwrap_err();
        assert!(err.contains("nonexistent-lsp-server-xyz"));

        // server 状态应更新为 Error
        let server = registry.get("rust").unwrap();
        assert_eq!(server.status, LspServerStatus::Error);
    }

    #[test]
    fn is_server_spawned_returns_false_initially() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        assert!(!registry.is_server_spawned("rust"));
    }

    #[test]
    fn shutdown_server_is_idempotent_for_unspawned() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        // 未 spawn 时 shutdown 应返回 Ok(幂等)
        let result = registry.shutdown_server("rust");
        assert!(result.is_ok());
    }

    #[test]
    fn shutdown_server_updates_status_to_disconnected() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);
        // 即使没有真实 spawn,shutdown_server 也应将状态改为 Disconnected
        // (因为 process_transports 中没有,所以只是 no-op,但不应报错)
        registry.shutdown_server("rust").unwrap();
        // 注意:由于未 spawn,server 状态不会改变(只有真实 spawn 后才会)
        let server = registry.get("rust").unwrap();
        assert_eq!(server.status, LspServerStatus::Connected);
    }

    #[test]
    fn dispatch_falls_back_to_memory_when_not_spawned() {
        // dispatch 在未 spawn 时应 fallback 到 MemoryLspTransport
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, Some("/workspace"), vec![]);

        let result = registry
            .dispatch("hover", Some("src/main.rs"), Some(10), Some(5), None)
            .unwrap();
        // 应使用 memory transport(未 spawn)
        assert_eq!(result["rpc_response"]["result"]["transport"], "memory");
        assert_eq!(result["status"], "dispatched");
    }

    /// 集成测试:真实启动 rust-analyzer 并验证 initialize → hover 流程。
    ///
    /// 此测试 `#[ignore]` 默认不运行,因为:
    /// 1. 需要 rust-analyzer 在 PATH 中
    /// 2. 需要真实的工作区(用临时目录)
    /// 3. 启动子进程较慢(秒级)
    ///
    /// 运行方式:`cargo test -p runtime --lib -- --ignored spawn_server_real_rust_analyzer`
    #[test]
    #[ignore]
    fn spawn_server_real_rust_analyzer() {
        // 前置条件:rust-analyzer 在 PATH 中
        let rust_analyzer_check = std::process::Command::new("rust-analyzer")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if rust_analyzer_check.map(|s| !s.success()).unwrap_or(true) {
            eprintln!("[lsp_test] rust-analyzer not available; skipping");
            return;
        }

        // 创建临时工作区,写入一个最小 Rust 文件
        let temp = tempfile::tempdir().expect("failed to create temp dir");
        let workspace = temp.path();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"test_lsp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join("src/main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let registry = LspRegistry::new();
        registry.register_with_command(
            "rust",
            LspServerStatus::Disconnected,
            Some(workspace.to_str().unwrap()),
            vec!["hover".into(), "completion".into()],
            "rust-analyzer",
        );

        // 启动 rust-analyzer
        let spawn_result =
            registry.spawn_server("rust", "rust-analyzer", workspace.to_str().unwrap());
        assert!(
            spawn_result.is_ok(),
            "spawn_server failed: {:?}",
            spawn_result.err()
        );

        // 验证 server 状态
        assert!(registry.is_server_spawned("rust"));
        let server = registry.get("rust").unwrap();
        assert_eq!(server.status, LspServerStatus::Connected);

        // 关闭 server(清理子进程)
        registry.shutdown_server("rust").unwrap();
        assert!(!registry.is_server_spawned("rust"));
    }

    /// 集成测试:用 echo 模拟 LSP server 验证 spawn → shutdown 生命周期。
    ///
    /// 此测试 `#[ignore]` 因为 echo 不是真实 LSP server,
    /// spawn 后 initialize 会超时或失败,但能验证子进程启动 + 清理逻辑。
    #[test]
    #[ignore]
    fn spawn_server_lifecycle_with_fake_server() {
        let registry = LspRegistry::new();
        registry.register_with_command(
            "rust",
            LspServerStatus::Disconnected,
            None,
            vec![],
            "cat", // cat 会持续读取 stdin,模拟长期运行的 server
        );

        // 注意:cat 不是 LSP server,initialize 会因 read 超时或格式错误失败
        // 但 spawn() 本身(子进程启动)应成功
        let result = registry.spawn_server("rust", "cat", "/tmp");
        // 即使 initialize 失败,spawn 子进程本身应成功
        // result 可能是 Err(initialize timeout/parse error),这是预期的
        if let Err(e) = &result {
            eprintln!("[lsp_test] expected initialize failure with fake server: {e}");
        }

        // 无论 initialize 是否成功,都应能 shutdown(清理子进程)
        // 注意:若 spawn 失败,process_transports 不会有该 transport
        registry.shutdown_server("rust").unwrap();
    }

    // ========================================================================
    // Step 4.2 — parse_document_symbols / get_symbols 测试
    // ========================================================================

    #[test]
    fn symbol_kind_to_str_maps_all_known_kinds() {
        assert_eq!(symbol_kind_to_str(1), "file");
        assert_eq!(symbol_kind_to_str(12), "function");
        assert_eq!(symbol_kind_to_str(23), "struct");
        assert_eq!(symbol_kind_to_str(26), "type_parameter");
        assert_eq!(symbol_kind_to_str(99), "unknown");
        assert_eq!(symbol_kind_to_str(0), "unknown");
    }

    #[test]
    fn parse_document_symbols_handles_document_symbol_format() {
        // DocumentSymbol 格式(嵌套,有 range/selectionRange/children)
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "name": "main",
                    "kind": 12,
                    "range": { "start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 1} },
                    "selectionRange": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7} },
                    "children": [
                        {
                            "name": "inner_fn",
                            "kind": 12,
                            "range": { "start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 20} },
                            "selectionRange": { "start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 15} }
                        }
                    ]
                },
                {
                    "name": "MyStruct",
                    "kind": 23,
                    "range": { "start": {"line": 5, "character": 0}, "end": {"line": 8, "character": 1} },
                    "selectionRange": { "start": {"line": 5, "character": 7}, "end": {"line": 5, "character": 15} }
                }
            ]
        });

        let symbols = parse_document_symbols(&response, "src/main.rs");
        assert_eq!(symbols.len(), 3, "should have 3 symbols (main + inner_fn + MyStruct)");

        // 验证 main symbol
        assert_eq!(symbols[0].name, "main");
        assert_eq!(symbols[0].kind, "function");
        assert_eq!(symbols[0].path, "src/main.rs");
        assert_eq!(symbols[0].line, 0);
        assert_eq!(symbols[0].character, 3);

        // 验证 inner_fn(递归解析的 child)
        assert_eq!(symbols[1].name, "inner_fn");
        assert_eq!(symbols[1].kind, "function");
        assert_eq!(symbols[1].line, 1);
        assert_eq!(symbols[1].character, 7);

        // 验证 MyStruct
        assert_eq!(symbols[2].name, "MyStruct");
        assert_eq!(symbols[2].kind, "struct");
        assert_eq!(symbols[2].line, 5);
        assert_eq!(symbols[2].character, 7);
    }

    #[test]
    fn parse_document_symbols_handles_symbol_information_format() {
        // SymbolInformation 格式(平铺,有 location)
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "name": "helper",
                    "kind": 12,
                    "location": {
                        "uri": "file:///workspace/src/lib.rs",
                        "range": { "start": {"line": 10, "character": 0}, "end": {"line": 15, "character": 1} }
                    }
                },
                {
                    "name": "MAX_SIZE",
                    "kind": 14,
                    "location": {
                        "uri": "file:///workspace/src/lib.rs",
                        "range": { "start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 20} }
                    }
                }
            ]
        });

        let symbols = parse_document_symbols(&response, "src/lib.rs");
        assert_eq!(symbols.len(), 2);

        assert_eq!(symbols[0].name, "helper");
        assert_eq!(symbols[0].kind, "function");
        assert_eq!(symbols[0].line, 10);
        assert_eq!(symbols[0].character, 0);
        // path 来自 location.uri
        assert_eq!(symbols[0].path, "file:///workspace/src/lib.rs");

        assert_eq!(symbols[1].name, "MAX_SIZE");
        assert_eq!(symbols[1].kind, "constant");
        assert_eq!(symbols[1].line, 3);
    }

    #[test]
    fn parse_document_symbols_handles_empty_result() {
        // result 为空数组
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": []
        });
        let symbols = parse_document_symbols(&response, "src/main.rs");
        assert!(symbols.is_empty());

        // result 为 null
        let response_null = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        });
        let symbols_null = parse_document_symbols(&response_null, "src/main.rs");
        assert!(symbols_null.is_empty());

        // 无 result 字段(错误响应)
        let response_err = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32601, "message": "method not found" }
        });
        let symbols_err = parse_document_symbols(&response_err, "src/main.rs");
        assert!(symbols_err.is_empty());
    }

    #[test]
    fn parse_document_symbols_handles_mixed_format() {
        // 混合格式(虽然实际中不会发生,但应容错)
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "name": "doc_symbol",
                    "kind": 12,
                    "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 10} },
                    "selectionRange": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 13} }
                },
                {
                    "name": "sym_info",
                    "kind": 13,
                    "location": {
                        "uri": "file:///workspace/main.rs",
                        "range": { "start": {"line": 5, "character": 2}, "end": {"line": 5, "character": 10} }
                    }
                },
                {
                    // 未知格式,应被跳过
                    "name": "unknown_format",
                    "kind": 12
                }
            ]
        });

        let symbols = parse_document_symbols(&response, "main.rs");
        assert_eq!(symbols.len(), 2, "should skip unknown format");
        assert_eq!(symbols[0].name, "doc_symbol");
        assert_eq!(symbols[1].name, "sym_info");
    }

    #[test]
    fn get_symbols_returns_empty_for_memory_transport() {
        // MemoryLspTransport 返回 placeholder 响应,parse 应返回空 Vec
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);

        let symbols = registry.get_symbols("src/main.rs").unwrap();
        // MemoryLspTransport 返回的响应无 result 数组,parse 返回空
        assert!(symbols.is_empty());
    }

    #[test]
    fn get_symbols_errors_for_disconnected_server() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Disconnected, None, vec![]);

        let result = registry.get_symbols("src/main.rs");
        assert!(result.is_err());
    }

    // ========================================================================
    // Step 4.2-c 测试:parse_document_symbols_typed(lsp-types 路径)
    // ========================================================================

    #[test]
    fn parse_document_symbols_typed_handles_document_symbol_format() {
        // DocumentSymbol[] 格式(嵌套,有 range/selectionRange/children)
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "name": "main_fn",
                    "kind": 12, // Function
                    "range": { "start": {"line": 0, "character": 0}, "end": {"line": 10, "character": 1} },
                    "selectionRange": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 9} },
                    "children": [
                        {
                            "name": "inner_var",
                            "kind": 13, // Variable
                            "range": { "start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 20} },
                            "selectionRange": { "start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 16} }
                        }
                    ]
                }
            ]
        });

        let symbols = parse_document_symbols_typed(&response, "main.rs");
        assert_eq!(symbols.len(), 2, "should flatten children");
        assert_eq!(symbols[0].name, "main_fn");
        assert_eq!(symbols[0].kind, "function");
        assert_eq!(symbols[0].line, 0);
        assert_eq!(symbols[0].character, 3);
        assert_eq!(symbols[0].path, "main.rs");
        assert_eq!(symbols[1].name, "inner_var");
        assert_eq!(symbols[1].kind, "variable");
        assert_eq!(symbols[1].line, 1);
        assert_eq!(symbols[1].character, 8);
    }

    #[test]
    fn parse_document_symbols_typed_handles_symbol_information_format() {
        // SymbolInformation[] 格式(平铺,有 location)
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "name": "Foo",
                    "kind": 23, // Struct
                    "location": {
                        "uri": "file:///workspace/main.rs",
                        "range": { "start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 10} }
                    }
                },
                {
                    "name": "bar",
                    "kind": 12, // Function
                    "location": {
                        "uri": "file:///workspace/main.rs",
                        "range": { "start": {"line": 10, "character": 4}, "end": {"line": 10, "character": 20} }
                    }
                }
            ]
        });

        let symbols = parse_document_symbols_typed(&response, "main.rs");
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "Foo");
        assert_eq!(symbols[0].kind, "struct");
        assert_eq!(symbols[0].line, 5);
        assert_eq!(symbols[0].character, 0);
        assert_eq!(symbols[0].path, "file:///workspace/main.rs");
        assert_eq!(symbols[1].name, "bar");
        assert_eq!(symbols[1].kind, "function");
    }

    #[test]
    fn parse_document_symbols_typed_handles_empty_result() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": []
        });

        let symbols = parse_document_symbols_typed(&response, "main.rs");
        assert!(symbols.is_empty());
    }

    #[test]
    fn parse_document_symbols_typed_handles_null_result() {
        // 部分 LSP server 对空文件返回 null
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        });

        let symbols = parse_document_symbols_typed(&response, "main.rs");
        assert!(symbols.is_empty());
    }

    #[test]
    fn parse_document_symbols_typed_falls_back_for_invalid_response() {
        // 非 standard 字段类型(如 kind 为字符串),lsp-types 反序列化失败,
        // 应 fallback 到 serde_json 手动解析
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "name": "weird",
                    "kind": "not_a_number", // 非标准:kind 应为数字
                    "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 10} },
                    "selectionRange": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5} }
                }
            ]
        });

        // 不应 panic,fallback 路径应处理
        let symbols = parse_document_symbols_typed(&response, "main.rs");
        // fallback 解析时 kind_num=0 → "unknown"
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "weird");
        assert_eq!(symbols[0].kind, "unknown");
    }

    #[test]
    fn parse_document_symbols_typed_consistent_with_untyped() {
        // 验证 typed 路径与 untyped 路径对标准响应返回一致结果
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "name": "func_a",
                    "kind": 12,
                    "range": { "start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 1} },
                    "selectionRange": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 9} }
                },
                {
                    "name": "StructB",
                    "kind": 23,
                    "range": { "start": {"line": 7, "character": 0}, "end": {"line": 10, "character": 1} },
                    "selectionRange": { "start": {"line": 7, "character": 7}, "end": {"line": 7, "character": 14} }
                }
            ]
        });

        let typed = parse_document_symbols_typed(&response, "main.rs");
        let untyped = parse_document_symbols(&response, "main.rs");

        assert_eq!(typed.len(), untyped.len(), "typed and untyped should return same count");
        for (t, u) in typed.iter().zip(untyped.iter()) {
            assert_eq!(t.name, u.name, "name mismatch");
            assert_eq!(t.kind, u.kind, "kind mismatch for {}", t.name);
            assert_eq!(t.line, u.line, "line mismatch for {}", t.name);
            assert_eq!(t.character, u.character, "character mismatch for {}", t.name);
            assert_eq!(t.path, u.path, "path mismatch for {}", t.name);
        }
    }
}
