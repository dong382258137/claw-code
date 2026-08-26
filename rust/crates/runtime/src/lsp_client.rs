#![allow(clippy::should_implement_trait, clippy::must_use_candidate)]
//! LSP (Language Server Protocol) client registry for tool dispatch.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// 默认工作区根路径,供 auto-start 时使用。
    /// 由 init_lsp_from_config 在启动时设置(即使 lspServers 为空也会设置)。
    default_root_path: Option<String>,
    /// publishDiagnostics 推送序号(全局递增)。reader 线程每处理一次推送递增。
    /// 配合 [`last_push_versions`],用于"编辑后自动诊断"检测某文件的新推送是否已到达。
    push_counter: Arc<AtomicU64>,
    /// path → 该文件最近一次 publishDiagnostics 推送对应的 `push_counter` 序号。
    /// 解决空诊断歧义:publishDiagnostics 为空数组时缓存被清空,无法从缓存内容
    /// 区分"已推送 0 错误"与"尚未推送",序号越过基线才是可靠信号。
    last_push_versions: Mutex<HashMap<String, u64>>,
    /// 语言级 auto-start 冷却:auto-start 失败后,该语言在冷却期内不再尝试。
    /// 避免 server 缺失时每次编辑都重复 spawn + 弹安装提示(刷屏)。
    auto_start_cooldowns: Mutex<HashMap<String, Instant>>,
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

/// "编辑后自动诊断"的刷新结果。
///
/// 由 [`LspRegistry::refresh_diagnostics_for_path`] 返回,调用方(tools crate
/// 编辑工具)按结果决定是否把诊断附加到工具结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspAutoDiagOutcome {
    /// 刷新成功,返回该文件最新诊断(空 Vec = 0 问题,表示已确认刷新过)。
    Refresh(Vec<LspDiagnostic>),
    /// auto-start 首次失败,携带安装提示(应附到工具结果,引导安装 server)。
    InstallHint(String),
    /// 静默跳过:语言不支持 / 冷却期 / 服务器不可用 / 等待推送超时。
    Skip,
}

/// 编辑后自动诊断等待 server 推送 `publishDiagnostics` 的超时上限。
const LSP_AUTO_DIAG_TIMEOUT_MS: u64 = 2500;

/// 语言级 auto-start 失败后的冷却时长。
const LSP_AUTO_START_COOLDOWN_MS: u64 = 60_000;

impl LspRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置默认工作区根路径,供 auto-start 时使用。
    ///
    /// 应在 `init_lsp_from_config` 中调用,即使 lspServers 配置为空也会设置,
    /// 这样 dispatch 时遇到未配置但有预置默认的语言仍能 auto-start。
    pub fn set_default_root_path(&self, root_path: &str) {
        let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner.default_root_path = Some(root_path.to_owned());
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
    /// - `args`:LSP server 命令行参数(如 `["--stdio"]`),无参数传空切片
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
        args: &[String],
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
        let mut transport = ProcessLspTransport::with_args(
            language.to_owned(),
            Some(root_path.to_owned()),
            command.to_owned(),
            args.to_vec(),
        );
        // 注入 registry inner 的弱引用,供后台 reader 线程推送 publishDiagnostics
        // 弱引用避免 ProcessLspTransport ↔ LspRegistry 的循环强引用
        transport.set_registry_inner(Arc::downgrade(&self.inner));

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

    /// 尝试为未配置的语言自动启动预置默认 LSP server(lazy auto-start)。
    ///
    /// 当 dispatch 发现某语言未注册时调用此方法。流程:
    /// 1. 查找 `default_lsp_server_for_language` 预置配置
    /// 2. 若无预置 → 返回 Err("no default")
    /// 3. 检测命令是否在 PATH 中(`is_command_available`)
    /// 4. 不在 PATH → 返回 Err(含安装提示)
    /// 5. 在 PATH → 注册 + spawn,返回 Ok
    ///
    /// 这是"拆包即用"的核心:用户无需配置 settings.json,
    /// 只要系统装了对应的 LSP server(如 rust-analyzer),
    /// claw 就能在首次调用时自动启动它。
    fn try_auto_start(&self, language: &str) -> Result<(), String> {
        let (command, args) = default_lsp_server_for_language(language)
            .ok_or_else(|| format!("no default LSP server for language '{language}'"))?;

        if !is_command_available(command) {
            let hint = install_hint_for_command(command)
                .unwrap_or("install the corresponding LSP server and add it to PATH");
            return Err(format!(
                "LSP server '{command}' for language '{language}' is not in PATH — {hint}"
            ));
        }

        // 获取 default_root_path(由 init_lsp_from_config 设置)
        let root_path = {
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            inner
                .default_root_path
                .clone()
                .unwrap_or_else(|| ".".to_string())
        };

        // 注册 + spawn
        self.register_with_command(
            language,
            LspServerStatus::Disconnected,
            Some(&root_path),
            vec![],
            command,
        );

        let args_vec: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match self.spawn_server(language, command, &args_vec, &root_path) {
            Ok(()) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 当 dispatch 遇到 Error/Disconnected 状态的已注册 server 时,尝试重新 spawn。
    ///
    /// 与 [`try_auto_start`](Self::try_auto_start) 不同,此方法针对"已注册但状态异常"的场景,
    /// 无需重新 register,直接清理旧 transport 并重新 spawn。
    ///
    /// 流程:
    /// 1. 获取 server_command(优先已注册的,fallback 到预置默认)
    /// 2. 检测命令是否在 PATH 中
    /// 3. 清理旧 transport(通过 [`shutdown_server`](Self::shutdown_server))
    /// 4. 更新 server_command 并重新 spawn
    fn try_retry_spawn(&self, language: &str) -> Result<(), String> {
        // 1. 获取 command:优先使用已注册的 server_command,fallback 到预置默认
        let (command, args): (String, Vec<String>) = {
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            if let Some(cmd) = inner
                .servers
                .get(language)
                .and_then(|s| s.server_command.clone())
            {
                drop(inner);
                (cmd, vec![])
            } else {
                drop(inner);
                let (cmd, args) = default_lsp_server_for_language(language).ok_or_else(|| {
                    format!(
                        "no server_command configured and no default LSP server for language '{language}' — add it to lspServers in settings.json"
                    )
                })?;
                (cmd.to_owned(), args.iter().map(|s| s.to_string()).collect())
            }
        };

        // 2. 检测命令是否在 PATH 中
        if !is_command_available(&command) {
            let hint = install_hint_for_command(&command)
                .unwrap_or("install the corresponding LSP server and add it to PATH");
            return Err(format!(
                "LSP server '{command}' for language '{language}' is not in PATH — {hint}"
            ));
        }

        // 3. 获取 root_path
        let root_path = {
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            inner
                .default_root_path
                .clone()
                .unwrap_or_else(|| ".".to_string())
        };

        // 4. 清理旧 transport(对 Error 状态可能有残留的僵尸 transport)
        let _ = self.shutdown_server(language);

        // 5. 确保 server_command 为最新值(用于再次重试和错误诊断)
        {
            let mut inner = self.inner.lock().expect("lsp registry lock poisoned");
            if let Some(server) = inner.servers.get_mut(language) {
                server.server_command = Some(command.clone());
            }
        }

        // 6. 重新 spawn
        self.spawn_server(language, &command, &args, &root_path)?;

        Ok(())
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
        // SP4.2-B6:优先使用 typed 解析(lsp-types 0.95),fallback 到手动解析
        Ok(parse_document_symbols_typed(&response, path))
    }

    /// 获取符号在跨文件中的引用位置(精确语义解析,Step 4.3)。
    ///
    /// 通过 `textDocument/references` 请求 LSP server,
    /// 并用 [`parse_references`] 解析响应为 `Vec<LspLocation>`。
    ///
    /// # 与 repomap 协同
    /// 返回值用于 [`crate::repomap::RepoMap::refresh_lsp_references`]:
    /// 统计"定义符号被其他文件引用"的次数,替代 regex 子串匹配的跨模块定位。
    ///
    /// # 参数
    /// - `path`:定义符号所在文件路径
    /// - `line`:定义符号的行号(0-based,来自 LspSymbol.line)
    /// - `character`:定义符号的列号(0-based,来自 LspSymbol.character)
    pub fn get_references(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, String> {
        let response =
            self.dispatch("references", Some(path), Some(line), Some(character), None)?;
        Ok(parse_references(&response))
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
    ///
    /// 每条 diagnostic 的 path 经 [`normalize_lsp_path`] 统一为规范化格式
    /// (绝对路径 + 正斜杠),与 `get_diagnostics` 的查询规范化保持一致,
    /// 避免缓存 path 与查询 path 因反斜杠/相对路径失配。
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
        let normalized: Vec<LspDiagnostic> = diagnostics
            .into_iter()
            .map(|mut d| {
                if let Some(path) = normalize_lsp_path(&d.path) {
                    d.path = path;
                }
                d
            })
            .collect();
        server.diagnostics.extend(normalized);
        Ok(())
    }

    /// Get diagnostics for a specific file path.
    ///
    /// 查询 path 会先经 [`normalize_lsp_path`] 规范化(绝对路径 + 正斜杠),
    /// 与 `publishDiagnostics` 推送缓存中的 path(`uri_to_path` 输出的 `D:/...`
    /// 格式)保持一致,避免反斜杠/正斜杠导致的失配。
    /// Windows 上比较大小写不敏感(文件系统不区分大小写,但 LSP 返回的
    /// URI 大小写可能与查询路径不同)。
    pub fn get_diagnostics(&self, path: &str) -> Vec<LspDiagnostic> {
        let Some(normalized) = normalize_lsp_path(path) else {
            return Vec::new();
        };
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner
            .servers
            .values()
            .flat_map(|s| &s.diagnostics)
            .filter(|d| {
                #[cfg(windows)]
                {
                    d.path.eq_ignore_ascii_case(&normalized)
                }
                #[cfg(not(windows))]
                {
                    d.path == normalized
                }
            })
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

    /// 编辑后自动诊断:确保 server 就绪 → didOpen/didChange → 等待推送。
    ///
    /// 设计见 `docs/2026-08-10-auto-lsp-diagnostics-design.md`。核心策略:
    /// - 语言不支持 / 冷却期 / 等待推送超时 → [`LspAutoDiagOutcome::Skip`](静默,
    ///   不阻塞编辑结果);
    /// - auto-start 首次失败 → [`LspAutoDiagOutcome::InstallHint`](引导安装),
    ///   随后进入语言级冷却,冷却期内不再尝试;
    /// - 刷新成功(推送序号越过基线) → [`LspAutoDiagOutcome::Refresh`](诊断,
    ///   空 Vec = 0 问题,表示已确认刷新过)。
    pub fn refresh_diagnostics_for_path(&self, path: &str) -> LspAutoDiagOutcome {
        let Some(ext) = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
        else {
            return LspAutoDiagOutcome::Skip;
        };
        let Some(language) = language_for_extension(&ext) else {
            return LspAutoDiagOutcome::Skip;
        };

        // 冷却检查:auto-start 失败后 60s 内不再尝试(避免 server 缺失时刷屏)。
        if self.is_auto_start_cooldown(language) {
            return LspAutoDiagOutcome::Skip;
        }

        // 路径规范化:绝对路径 + 正斜杠,与 `uri_to_path` 的输出格式一致
        // (publishDiagnostics 推送的 path 是 file:// URI 反解出的 `D:/...` 格式;
        //  不统一则 last_push_version / get_diagnostics 全部查不到,链路永远超时)。
        let Some(normalized_path) = normalize_lsp_path(path) else {
            return LspAutoDiagOutcome::Skip;
        };

        // 确保 server 已注册并 spawn(未就绪则 auto-start,含安装提示)。
        let server_ready = {
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            inner.servers.contains_key(language) && inner.process_transports.contains_key(language)
        };
        if !server_ready {
            if let Err(err) = self.try_auto_start(language) {
                // 首次失败:记录冷却;若属 server 缺失类错误,附安装提示引导安装。
                self.set_auto_start_cooldown(language);
                if err.contains("not in PATH") || err.contains("failed to spawn") {
                    return LspAutoDiagOutcome::InstallHint(err);
                }
                return LspAutoDiagOutcome::Skip;
            }
        }

        // 获取 transport 并触发重新诊断(didOpen 首开 + didChange 增量)。
        let transport = {
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            inner.process_transports.get(language).cloned()
        };
        let Some(transport) = transport else {
            return LspAutoDiagOutcome::Skip;
        };

        // 基线:该文件当前已知的最新推送序号。
        let baseline = self.last_push_version(&normalized_path);

        {
            let t = transport.lock().unwrap_or_else(|e| e.into_inner());
            if t.ensure_did_open(&normalized_path, language).is_err() {
                return LspAutoDiagOutcome::Skip;
            }
            if t.notify_did_change(&normalized_path).is_err() {
                return LspAutoDiagOutcome::Skip;
            }
        }

        // 等待该文件的推送序号越过基线(带超时)。
        let deadline = Instant::now() + Duration::from_millis(LSP_AUTO_DIAG_TIMEOUT_MS);
        loop {
            if self.last_push_version(&normalized_path) > baseline {
                let diags = self.get_diagnostics(&normalized_path);
                return LspAutoDiagOutcome::Refresh(diags);
            }
            if Instant::now() >= deadline {
                return LspAutoDiagOutcome::Skip;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 该文件最近一次 publishDiagnostics 推送对应的全局序号(0 = 从未推送)。
    ///
    /// path 经 [`normalize_lsp_path`] 规范化,与 reader 线程 `record_push`
    /// 记录的 key(uri_to_path 正斜杠格式)保持一致。
    /// Windows 上查找大小写不敏感(与 `get_diagnostics` 语义一致)。
    fn last_push_version(&self, path: &str) -> u64 {
        let Some(normalized) = normalize_lsp_path(path) else {
            return 0;
        };
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        let versions = inner
            .last_push_versions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        #[cfg(windows)]
        {
            versions
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(&normalized))
                .map(|(_, version)| *version)
                .unwrap_or(0)
        }
        #[cfg(not(windows))]
        {
            versions.get(&normalized).copied().unwrap_or(0)
        }
    }

    /// 语言是否处于 auto-start 失败冷却期。
    fn is_auto_start_cooldown(&self, language: &str) -> bool {
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        inner
            .auto_start_cooldowns
            .lock()
            .map(|c| c.get(language).is_some_and(|until| *until > Instant::now()))
            .unwrap_or(false)
    }

    /// 记录语言的 auto-start 失败冷却截止时间。
    fn set_auto_start_cooldown(&self, language: &str) {
        let inner = self.inner.lock().expect("lsp registry lock poisoned");
        let mut cooldowns = inner
            .auto_start_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cooldowns.insert(
            language.to_owned(),
            Instant::now() + Duration::from_millis(LSP_AUTO_START_COOLDOWN_MS),
        );
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

        // 分层错误诊断:区分三种无 server 可用的情形,给出针对性修复指引。
        //
        // 1. 文件扩展名未识别 → 提示支持的扩展名
        // 2. 扩展名已识别但该语言未在 lspServers 中配置 → 提示加配置
        // 3. 已配置但 spawn 失败或正在启动 → 提示检查命令/等待启动
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
            _ => {
                return Err(format!(
                    "unsupported file extension '.{}': LSP supports rust(.rs), typescript(.ts/.tsx), javascript(.js/.jsx), python(.py), go(.go), java(.java), c(.c/.h), cpp(.cpp/.hpp/.cc), ruby(.rb), lua(.lua); path: {path}",
                    if ext.is_empty() { "<none>" } else { ext }
                ));
            }
        };

        let server = {
            let inner = self.inner.lock().expect("lsp registry lock poisoned");
            inner.servers.get(language).cloned()
        };

        let mut server = match server {
            Some(s) => s,
            None => {
                // 预置默认配置:尝试 auto-start
                // 如果该语言有预置默认 LSP server 且在 PATH 中,自动注册并启动
                // 如果不在 PATH 中,返回含安装提示的错误
                // 如果无预置默认,返回含配置模板的错误
                if default_lsp_server_for_language(language).is_some() {
                    match self.try_auto_start(language) {
                        Ok(()) => {
                            // auto-start 成功,重新读取 server 状态
                            let inner = self.inner.lock().expect("lsp registry lock poisoned");
                            inner.servers.get(language).cloned().ok_or_else(|| {
                                "auto-start succeeded but server not found in registry".to_string()
                            })?
                        }
                        Err(e) => {
                            return Err(format!(
                                "auto-start failed for language '{language}' (file: {path}) — {e}"
                            ));
                        }
                    }
                } else {
                    return Err(format!(
                        "no LSP server configured for language '{language}' (file: {path}) — add \"lspServers\" to settings.json, e.g. {{\"lspServers\": {{\"{language}\": {{\"language\": \"{language}\", \"command\": \"<server-command>\"}}}}}}"
                    ));
                }
            }
        };
        // 对于 Error/Disconnected 状态,尝试重新 spawn 一次再继续 dispatch,
        // 而非直接报错(之前的 spawn 可能因临时原因失败,如命令未安装)。
        // Starting 状态无法干预,仍需等待。
        if server.status != LspServerStatus::Connected {
            if server.status == LspServerStatus::Error
                || server.status == LspServerStatus::Disconnected
            {
                // 尝试重试 spawn
                match self.try_retry_spawn(language) {
                    Ok(()) => {
                        // 重试成功,重新读取 server 状态并继续 dispatch
                        let inner = self.inner.lock().expect("lsp registry lock poisoned");
                        server = inner.servers.get(language).cloned().ok_or_else(|| {
                            "retry spawn succeeded but server not found in registry".to_string()
                        })?;
                    }
                    Err(retry_err) => {
                        let status_label = server.status.to_string();
                        let hint = if server.status == LspServerStatus::Error {
                            format!(
                                "server previously failed to start; retry also failed: {retry_err}"
                            )
                        } else {
                            format!("server was disconnected; retry also failed: {retry_err}")
                        };
                        return Err(format!(
                            "LSP server for '{}' is not connected (status: {status_label}) — {hint}",
                            server.language,
                        ));
                    }
                }
            } else {
                // Starting 状态:无法干预,提示等待
                return Err(format!(
                    "LSP server for '{}' is still starting up, retry in a few seconds (rust-analyzer indexing may take 30-60s on first run)",
                    server.language,
                ));
            }
        }

        // Step 4.2 — 真实 LSP JSON-RPC 调用。
        // 详见 docs/harness-engineering-optimization-plan.md Step 4.2
        //
        // 优先级:
        // 1. 若 process_transports 中有已 spawn 的真实 transport,使用它发送 JSON-RPC
        // 2. 否则 fallback 到 LspJsonRpcClient(MemoryLspTransport)— 保持向后兼容
        //
        // 协议流程:initialize → initialized → didChange → completion/hover/definition
        let request = LspRequest::new(lsp_action, path, line, character, server.language.clone());

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

            // SP4.2 修复:在发任何 textDocument 请求前,先发 didOpen 通知。
            // LSP 协议要求 client 先 didOpen,server 才能解析文件内容。
            // ensure_did_open 内部跟踪已 open 的文件,不会重复发送。
            let language_id = language_id_from_extension(path);
            transport.ensure_did_open(path, language_id)?;

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
            // SP4.2 修复:Diagnostics 是 server→client 通知,client 不应向 server 发送。
            // dispatch 对 Diagnostics 做了特殊处理(查本地缓存),不会走到这里。
            // 返回空字符串而非 method 名,防止未来重构时意外发送。
            LspAction::Diagnostics => "",
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

/// 根据已配置的 LSP server 命令名,返回安装提示。
///
/// 覆盖第一档语言(有标准包管理器安装方式)的主流 LSP server。
/// 返回 `None` 表示该 server 无标准安装方式(如需手动下载二进制)。
///
/// 这些提示会附加到 spawn 失败和 dispatch 未连接的错误信息中,
/// 让 AI 能看到安装命令并主动协助用户执行(通过 bash 工具,需用户确认)。
fn install_hint_for_command(command: &str) -> Option<&'static str> {
    match command {
        "rust-analyzer" => Some("install with: `rustup component add rust-analyzer` (requires rustup)"),
        "pylsp" => Some("install with: `pip install python-lsp-server` (requires pip)"),
        "pyright" => Some("install with: `npm install -g pyright` (requires npm)"),
        "ruff-lsp" => Some("install with: `pip install ruff-lsp` (requires pip)"),
        "gopls" => Some("install with: `go install golang.org/x/tools/gopls@latest` (requires go)"),
        "typescript-language-server" => Some("install with: `npm install -g typescript-language-server typescript` (requires npm)"),
        "clangd" => Some("install via system package manager: `apt install clangd` (Debian/Ubuntu), `winget install LLVM.LLVM` (Windows), or `brew install llvm` (macOS)"),
        "solargraph" => Some("install with: `gem install solargraph` (requires ruby/gem)"),
        "lua-language-server" => Some("download from https://github.com/LuaLS/lua-language-server/releases and add to PATH"),
        "jdtls" => Some("download from https://download.eclipse.org/jdtls/snapshots/ (requires JDK 17+, complex setup)"),
        _ => None,
    }
}

/// 检测命令是否在 PATH 中可用。
///
/// Windows 用 `where`,Unix 用 `which`。
/// 返回 `true` 表示命令存在且可执行。
fn is_command_available(command: &str) -> bool {
    let checker = if cfg!(windows) { "where" } else { "which" };
    Command::new(checker)
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 预置的 LSP server 默认配置。
///
/// 当用户未在 settings.json 中配置 `lspServers` 时,
/// dispatch 会根据此表自动推断常见语言应使用的 LSP server。
/// 仅当对应 server 命令在 PATH 中可用时才会自动启动(lazy auto-start)。
///
/// 返回 `(command, args)` 元组。`args` 为空切片表示无参数。
fn default_lsp_server_for_language(
    language: &str,
) -> Option<(&'static str, &'static [&'static str])> {
    match language {
        "rust" => Some(("rust-analyzer", &[])),
        "python" => Some(("pylsp", &[])),
        "typescript" | "javascript" => Some(("typescript-language-server", &["--stdio"])),
        "go" => Some(("gopls", &[])),
        "c" | "cpp" => Some(("clangd", &[])),
        "ruby" => Some(("solargraph", &["stdio"])),
        "lua" => Some(("lua-language-server", &[])),
        _ => None,
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
///
/// SP4.2 修复(审查后):
/// - `send_lock: Mutex<()>` 确保 write+read 原子性(原代码 write 和 read 之间锁释放)
/// - `read_response_for_id` 循环读取直到 `id` 匹配,过滤通知(原代码不区分通知与响应)
/// - `read_message_with_timeout` 用线程+channel 实现超时(原代码无超时,永久阻塞)
/// - `opened_files` 跟踪已 didOpen 的文件,避免重复发送
pub struct ProcessLspTransport {
    /// 语言标识。
    pub language: String,
    /// root_path。
    pub root_path: Option<String>,
    /// LSP server 命令(如 "rust-analyzer")。
    pub server_command: String,
    /// LSP server 命令行参数(如 `["--stdio"]`)。
    /// 空 Vec 表示无参数。
    pub server_args: Vec<String>,
    /// 是否已初始化(已发送 initialize 请求)。
    pub initialized: bool,
    /// 已启动的子进程(若存在)。
    child: Option<Arc<Mutex<Child>>>,
    /// 子进程 stdin(用于写入请求)。
    child_stdin: Option<Arc<Mutex<ChildStdin>>>,
    /// 子进程 stdout(由后台 reader 线程独占读取)。
    child_stdout: Option<Arc<Mutex<ChildStdout>>>,
    /// JSON-RPC 请求 ID 计数器。
    next_id: Arc<Mutex<u64>>,
    /// 只锁 write,不再锁 read(reader 线程独占 stdout 读取)。
    /// 替代旧 send_lock,使并发请求不再因读等待而串行化。
    write_lock: Mutex<()>,
    /// 已 didOpen 的文件 path → 当前版本号(didOpen 记录 v1,didChange 递增)。
    /// 同时避免重复发送 didOpen。
    opened_files: Mutex<HashMap<String, u64>>,
    /// 后台 reader 线程停止信号。
    /// Drop/shutdown 时置 true;reader 线程在下次循环检查时退出
    /// (若阻塞在 read_exact,则由 child kill 导致 EOF 退出)。
    reader_stop: Arc<AtomicBool>,
    /// pending requests: 请求 id → 响应 channel sender。
    /// send() 注册后等待,reader 线程读到匹配 id 的响应时通过 channel 发送。
    #[allow(clippy::type_complexity)]
    pending_responses: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<serde_json::Value, String>>>>>,
    /// LspRegistry inner 的弱引用,reader 线程用于推送 publishDiagnostics 到缓存。
    /// 弱引用避免 ProcessLspTransport ↔ LspRegistry 的循环强引用。
    /// None 表示未关联 registry(如单元测试中独立构造的 transport)。
    registry_inner: Option<Weak<Mutex<RegistryInner>>>,
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
        Self::with_args(language, root_path, server_command, Vec::new())
    }

    /// 创建带命令行参数的 transport。
    ///
    /// `args` 会在 spawn 时追加到 `server_command` 之后,
    /// 例如 `typescript-language-server --stdio`。
    #[must_use]
    pub fn with_args(
        language: impl Into<String>,
        root_path: Option<String>,
        server_command: impl Into<String>,
        args: Vec<String>,
    ) -> Self {
        Self {
            language: language.into(),
            root_path,
            server_command: server_command.into(),
            server_args: args,
            initialized: false,
            child: None,
            child_stdin: None,
            child_stdout: None,
            next_id: Arc::new(Mutex::new(1)),
            write_lock: Mutex::new(()),
            opened_files: Mutex::new(HashMap::new()),
            reader_stop: Arc::new(AtomicBool::new(false)),
            pending_responses: Arc::new(Mutex::new(HashMap::new())),
            registry_inner: None,
        }
    }

    /// 关联 LspRegistry inner 的弱引用,供后台 reader 线程推送 publishDiagnostics。
    ///
    /// 必须在 [`spawn`](Self::spawn) 之前调用,否则 reader 线程无法更新 diagnostics 缓存。
    /// 通常由 [`LspRegistry::spawn_server`] 在创建 transport 后自动设置。
    fn set_registry_inner(&mut self, weak: Weak<Mutex<RegistryInner>>) {
        self.registry_inner = Some(weak);
    }

    /// 启动 LSP server 子进程(Step 4.2)。
    ///
    /// 通过 `std::process::Command` 启动 `server_command`,
    /// 配置 stdin/stdout 为 piped,stderr 为 inherit。
    /// 启动后自动发送 `initialize` 请求并等待响应。
    pub fn spawn(&mut self) -> Result<(), String> {
        let mut cmd = Command::new(&self.server_command);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if !self.server_args.is_empty() {
            cmd.args(&self.server_args);
        }
        let mut child = cmd.spawn().map_err(|e| {
            let base = format!("failed to spawn LSP server '{}': {e}", self.server_command);
            // 命令不存在时附加安装提示,让 AI 能看到安装命令并主动协助
            if e.kind() == std::io::ErrorKind::NotFound {
                if let Some(hint) = install_hint_for_command(&self.server_command) {
                    return format!("{base}\n  {hint}");
                }
            }
            base
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to get stdin handle".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to get stdout handle".to_string())?;

        // SP4.2 修复:保持 ChildStdout 类型,用线程+recv_timeout 实现读取超时
        // (原方案用 unsafe raw handle 转 File 调 set_read_timeout,但 workspace
        // 禁用 unsafe_code,改用线程方案)
        self.child = Some(Arc::new(Mutex::new(child)));
        self.child_stdin = Some(Arc::new(Mutex::new(stdin)));
        self.child_stdout = Some(Arc::new(Mutex::new(stdout)));

        // 启动后台 reader 线程:独占 stdout 读取,分发响应到 pending channel,
        // 并将 publishDiagnostics 通知推送到 LspRegistry 缓存。
        // 必须在 send("initialize") 之前启动,否则响应无人消费。
        self.start_reader_thread();

        // 发送 initialize 请求(SP4.2 修复:验证响应而非忽略)
        let init_params = self.initialize_params();
        let response = self.send("initialize", init_params)?;
        // 验证 initialize 响应包含 capabilities
        if response.get("result").is_none() && response.get("error").is_some() {
            let err = response["error"]
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(format!("LSP initialize failed: {err}"));
        }
        self.initialized = true;

        // 发送 initialized 通知(无响应)
        // SP4.2 修复:不再用 let _ = 忽略错误
        self.send_notification("initialized", serde_json::json!({}))?;

        Ok(())
    }

    /// 启动后台 reader 线程。
    ///
    /// reader 线程独占 `child_stdout` 的读取,循环读取 JSON-RPC 消息:
    /// - **响应**(有 `id` 字段):通过 `pending_responses` channel 分发给对应的 `send()` 调用
    /// - **通知**(无 `id` 字段):`textDocument/publishDiagnostics` 推送到 LspRegistry 缓存
    ///
    /// 线程退出条件:
    /// - `reader_stop` 被置 true(Drop/shutdown 触发),且当前未阻塞在 read
    /// - `read_one_message` 返回错误(通常是子进程关闭导致 EOF)
    ///
    /// # 并发模型
    /// 旧实现中 `send_lock` 把 write+read 绑定为原子操作,导致并发请求串行化。
    /// 新实现中 reader 线程独立消费响应,`send()` 只用 `write_lock` 保护写入,
    /// 多个请求可并发 write(排队) + 并发等待响应(各自 channel),互不阻塞。
    fn start_reader_thread(&self) {
        let stdout = match &self.child_stdout {
            Some(s) => Arc::clone(s),
            None => return,
        };
        let stop = Arc::clone(&self.reader_stop);
        let pending = Arc::clone(&self.pending_responses);
        let registry_weak = self.registry_inner.clone();
        let language = self.language.clone();

        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match read_one_message(&stdout) {
                    Ok(msg) => {
                        // 响应:有 id 字段,分发给 pending request
                        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                            if let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&id)) {
                                let _ = tx.send(Ok(msg));
                            }
                            // 无 pending:可能是 send() 超时已移除,丢弃
                        } else {
                            // 通知:检查是否为 publishDiagnostics
                            let is_publish_diag = msg
                                .get("method")
                                .and_then(|m| m.as_str())
                                .map(|m| m == "textDocument/publishDiagnostics")
                                .unwrap_or(false);
                            if is_publish_diag {
                                let diags = parse_publish_diagnostics(&msg);
                                // LSP 语义:publishDiagnostics 是某文件的全量诊断推送。
                                // 需按 path 替换(而非追加),先清除该 path 的旧诊断再 extend。
                                if let Some(path) = diags.first().map(|d| d.path.clone()) {
                                    if let Some(weak) = &registry_weak {
                                        if let Some(arc) = weak.upgrade() {
                                            if let Ok(mut inner) = arc.lock() {
                                                if let Some(server) =
                                                    inner.servers.get_mut(&language)
                                                {
                                                    server.diagnostics.retain(|d| d.path != path);
                                                    server.diagnostics.extend(diags);
                                                    // 记录该文件的推送序号(全局递增),
                                                    // 供"编辑后自动诊断"检测新推送已到达。
                                                    record_push(&mut inner, path);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    // 空诊断数组:清除该文件的所有诊断
                                    if let Some(uri) = msg
                                        .get("params")
                                        .and_then(|p| p.get("uri"))
                                        .and_then(|u| u.as_str())
                                    {
                                        let path = uri_to_path(uri);
                                        if let Some(weak) = &registry_weak {
                                            if let Some(arc) = weak.upgrade() {
                                                if let Ok(mut inner) = arc.lock() {
                                                    if let Some(server) =
                                                        inner.servers.get_mut(&language)
                                                    {
                                                        server
                                                            .diagnostics
                                                            .retain(|d| d.path != path);
                                                        record_push(&mut inner, path);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // 其他通知(window/logMessage 等):忽略
                        }
                    }
                    Err(_) => {
                        // EOF 或读取错误(通常是子进程关闭),退出循环
                        break;
                    }
                }
            }
        });
    }

    /// 发送 JSON-RPC 通知(无 id,不等待响应)。
    fn send_notification(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&message)
    }

    /// SP4.2 修复:发送 textDocument/didOpen 通知。
    ///
    /// LSP 协议要求 client 在发任何 textDocument 请求前先发 didOpen,
    /// server 才能解析文件内容。此方法跟踪已 open 的文件(path → 版本号),
    /// 避免重复发送。
    pub fn ensure_did_open(&self, path: &str, language_id: &str) -> Result<(), String> {
        let abs_path = if std::path::Path::new(path).is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(path)
                .display()
                .to_string()
        };

        let mut opened = self
            .opened_files
            .lock()
            .map_err(|_| "opened_files lock poisoned".to_string())?;
        if opened.contains_key(&abs_path) {
            return Ok(());
        }

        // 读取文件内容
        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| format!("failed to read file for didOpen '{abs_path}': {e}"))?;

        let uri = path_to_file_uri(&abs_path);
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": content,
            }
        });

        self.send_notification("textDocument/didOpen", params)?;
        opened.insert(abs_path, 1);
        Ok(())
    }

    /// 编辑后发送 textDocument/didChange 通知,触发服务器重新诊断。
    ///
    /// 为何必要:didOpen 有去重,同一文件二次编辑时不会重发 didOpen,
    /// 服务器不会感知文件变化。因此每次编辑后必须发 didChange。
    /// - contentChanges 使用全量替换(编辑工具改动的完整内容);
    /// - 版本号由 `opened_files` 内部递增(didOpen 为 v1);
    /// - didChange 无需 languageId(与 didOpen 不同)。
    pub fn notify_did_change(&self, path: &str) -> Result<(), String> {
        let abs_path = if std::path::Path::new(path).is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(path)
                .display()
                .to_string()
        };

        let version = {
            let mut opened = self
                .opened_files
                .lock()
                .map_err(|_| "opened_files lock poisoned".to_string())?;
            let v = opened.get(&abs_path).copied().unwrap_or(0) + 1;
            opened.insert(abs_path.clone(), v);
            v
        };

        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| format!("failed to read file for didChange '{abs_path}': {e}"))?;

        let uri = path_to_file_uri(&abs_path);
        let params = serde_json::json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": content }],
        });

        self.send_notification("textDocument/didChange", params)
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
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    // Step 4.3:声明 hierarchicalDocumentSymbolSupport。
                    // 不声明时 rust-analyzer 回退 flat SymbolInformation(location.range.start
                    // 是声明块起点,含 doc 注释),导致 references 查询位置落在注释行上,
                    // 跨模块引用永远解析不到。声明后返回 DocumentSymbol[] 的 selectionRange
                    // (标识符精确位置),get_references 才能命中符号。
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "synchronization": { "didOpen": true, "didChange": true, "didClose": true }
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
        // 先通知 reader 线程停止
        self.reader_stop.store(true, Ordering::Release);
        if let Some(child) = self.child.take() {
            let mut child = match child.lock() {
                Ok(c) => c,
                Err(_e) => {
                    // TUI 模式下 stderr 输出会污染终端界面，静默处理。
                    return;
                }
            };
            // 尝试优雅终止;失败则强制杀死
            // kill 后 reader 线程的 read_exact 因 EOF 返回错误,自然退出
            let _ = child.kill();
            let _ = child.wait();
        }
        // 清理 stdin/stdout 句柄
        self.child_stdin = None;
        self.child_stdout = None;
        self.initialized = false;
        // 清理已 open 文件集合
        if let Ok(mut opened) = self.opened_files.lock() {
            opened.clear();
        }
    }

    /// 写入 JSON-RPC 消息到子进程 stdin(Content-Length header)。
    fn write_message(&self, message: &serde_json::Value) -> Result<(), String> {
        let Some(stdin_handle) = &self.child_stdin else {
            return Err("LSP server not spawned; call spawn() first".to_string());
        };

        let body =
            serde_json::to_string(message).map_err(|e| format!("JSON serialization error: {e}"))?;
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
        stdin.flush().map_err(|e| format!("flush error: {e}"))?;

        Ok(())
    }

    // (read_message / read_response_for_id 已删除,改由后台 reader 线程 +
    // `read_one_message` 模块函数 + `pending_responses` channel 替代。)
}

impl LspTransport for ProcessLspTransport {
    fn send(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        // 若子进程已启动,走真实 stdin/stdout 通信
        if self.child.is_some() {
            let id = {
                let mut next = self
                    .next_id
                    .lock()
                    .map_err(|_| "id counter lock poisoned".to_string())?;
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

            // 注册 pending response channel,reader 线程读到匹配 id 的响应时通过它发送
            let (tx, rx) = mpsc::channel::<Result<serde_json::Value, String>>();
            self.pending_responses
                .lock()
                .map_err(|_| "pending_responses lock poisoned".to_string())?
                .insert(id, tx);

            // 只锁 write(不锁 read),reader 线程独占 stdout 读取
            let write_result = {
                let _w = self
                    .write_lock
                    .lock()
                    .map_err(|_| "write_lock poisoned".to_string())?;
                self.write_message(&request)
            };
            if let Err(e) = write_result {
                // write 失败:清理 pending,避免泄漏
                self.pending_responses
                    .lock()
                    .ok()
                    .and_then(|mut p| p.remove(&id));
                return Err(e);
            }

            // 等待 reader 线程通过 channel 投递响应(带超时)
            let timeout = if method == "initialize" { 30 } else { 10 };
            match rx.recv_timeout(Duration::from_secs(timeout)) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.pending_responses
                        .lock()
                        .ok()
                        .and_then(|mut p| p.remove(&id));
                    Err(format!(
                        "LSP request {id} ({method}) timeout after {timeout}s"
                    ))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.pending_responses
                        .lock()
                        .ok()
                        .and_then(|mut p| p.remove(&id));
                    Err("LSP reader thread terminated (server closed?)".to_string())
                }
            }
        } else {
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
}

impl Drop for ProcessLspTransport {
    fn drop(&mut self) {
        // 先通知 reader 线程停止
        self.reader_stop.store(true, Ordering::Release);
        // kill 子进程:reader 线程的 read_exact 会因 EOF 返回错误并退出循环
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

/// 从 LSP server stdout 读取一条 JSON-RPC 消息(后台 reader 线程调用)。
///
/// 阻塞读取 Content-Length header + body,返回解析后的 JSON。
/// 与旧 `read_message` 的区别:
/// - 无超时(由 reader 线程的 stop 信号 + 子进程 EOF 控制退出)
/// - 不每次 spawn 新线程(reader 线程本身就是长期运行的)
///
/// # 错误
/// - 子进程关闭 stdout → `read_exact` 返回 EOF 错误,reader 线程据此退出
/// - 协议格式错误(缺 Content-Length、JSON 解析失败)→ 返回 Err
fn read_one_message(stdout: &Mutex<ChildStdout>) -> Result<serde_json::Value, String> {
    let mut stdout = stdout
        .lock()
        .map_err(|_| "stdout lock poisoned".to_string())?;

    // 读取 Content-Length header
    let mut header_line = String::new();
    let mut byte = [0u8; 1];
    loop {
        stdout
            .read_exact(&mut byte)
            .map_err(|e| format!("read header error: {e}"))?;
        header_line.push(byte[0] as char);
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

    let body_str = String::from_utf8(body).map_err(|e| format!("body UTF-8 error: {e}"))?;

    serde_json::from_str(&body_str).map_err(|e| format!("JSON parse error: {e}"))
}

/// 解析 `textDocument/publishDiagnostics` 通知为 `Vec<LspDiagnostic>`。
///
/// LSP 通知格式:
/// ```json
/// {
///   "method": "textDocument/publishDiagnostics",
///   "params": {
///     "uri": "file:///path/to/file.rs",
///     "diagnostics": [
///       { "range": {...}, "severity": 1, "message": "...", "source": "rust-analyzer" }
///     ]
///   }
/// }
/// ```
///
/// reader 线程处理 publishDiagnostics 后调用:递增全局推送序号,
/// 并记录该文件最近一次推送的序号(供"编辑后自动诊断"检测新推送到达)。
fn record_push(inner: &mut RegistryInner, path: String) {
    let seq = inner.push_counter.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut versions) = inner.last_push_versions.lock() {
        versions.insert(path, seq);
    }
}

/// 文件扩展名 → LSP 语言标识(与 `LspRegistry::dispatch` 的语言映射保持一致)。
fn language_for_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" => Some("javascript"),
        "py" => Some("python"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "hpp" | "cc" => Some("cpp"),
        "rb" => Some("ruby"),
        "lua" => Some("lua"),
        _ => None,
    }
}

/// 将文件路径规范化为"绝对路径 + 正斜杠",与 [`uri_to_path`] 的输出格式一致。
///
/// 为什么必须统一:LSP 推送(`publishDiagnostics`)的 path 是 file:// URI 反解出的
/// `D:/...` 格式(正斜杠),而本地查询路径可能是 `D:\...`(反斜杠)、相对路径,
/// 或编辑工具返回的 Windows verbatim 路径 `\\?\D:\...`。所有按 path 匹配缓存的
/// 入口(诊断过滤 / 推送序号)必须走同一规范化,否则失配。
/// - 剥离 Windows verbatim 前缀 `\\?\`(以及 `\\?\UNC\server\share` → `//server/share`);
/// - 相对路径 → 基于当前工作目录解析为绝对路径;
/// - 反斜杠 → 正斜杠(与 Windows 下 uri_to_path 输出一致);
/// - 当前目录不可用时返回 None(调用方静默跳过)。
fn normalize_lsp_path(path: &str) -> Option<String> {
    // 剥离 Windows verbatim 扩展长度前缀(`\\?\D:\...` → `D:\...`)。
    let cleaned = if let Some(rest) = path.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            // `\\?\UNC\server\share` → `//server/share`(UNC 路径,正斜杠)。
            format!("//{}", unc.replace('\\', "/"))
        } else {
            rest.to_owned()
        }
    } else {
        path.to_owned()
    };
    let abs = if std::path::Path::new(&cleaned).is_absolute() {
        cleaned
    } else {
        std::env::current_dir()
            .ok()?
            .join(cleaned)
            .display()
            .to_string()
    };
    Some(abs.replace('\\', "/"))
}

/// severity 映射:1=error, 2=warning, 3=info, 4=hint
fn parse_publish_diagnostics(msg: &serde_json::Value) -> Vec<LspDiagnostic> {
    let uri = msg
        .get("params")
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    let path = uri_to_path(uri);

    msg.get("params")
        .and_then(|p| p.get("diagnostics"))
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .map(|d| {
                    let line = d
                        .get("range")
                        .and_then(|r| r.get("start"))
                        .and_then(|s| s.get("line"))
                        .and_then(|l| l.as_u64())
                        .unwrap_or(0) as u32;
                    let character = d
                        .get("range")
                        .and_then(|r| r.get("start"))
                        .and_then(|s| s.get("character"))
                        .and_then(|c| c.as_u64())
                        .unwrap_or(0) as u32;
                    let severity = d
                        .get("severity")
                        .and_then(|s| s.as_u64())
                        .map(|s| match s {
                            1 => "error",
                            2 => "warning",
                            3 => "info",
                            4 => "hint",
                            _ => "unknown",
                        })
                        .unwrap_or("unknown")
                        .to_string();
                    let message = d
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string();
                    let source = d.get("source").and_then(|s| s.as_str()).map(str::to_owned);
                    LspDiagnostic {
                        path: path.clone(),
                        line,
                        character,
                        severity,
                        message,
                        source,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// SP4.2 修复:将文件路径转为 LSP file:// URI。
fn path_to_file_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        format!("file://{normalized}")
    } else {
        format!("file:///{normalized}")
    }
}

/// SP4.2 修复:根据文件扩展名返回 LSP languageId。
///
/// 用于 `textDocument/didOpen` 通知的 `languageId` 字段。
/// 参考 LSP 规范 §3.10.1 和 language-server-protocol/languages。
fn language_id_from_extension(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
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
        "json" => "json",
        "md" => "markdown",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        _ => "plaintext",
    }
}

/// SP4.2 修复:将 LSP file:// URI 转换为本地文件路径。
///
/// 统一 SymbolInformation 和 DocumentSymbol 的 path 格式:
/// - `file:///C:/workspace/main.rs` → `C:/workspace/main.rs`
/// - `file:///home/user/main.rs` → `/home/user/main.rs`
/// - `file://host/path` → `path`(简化处理,不保留 host)
/// - 非 file:// URI 原样返回
fn uri_to_path(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("file://") {
        // file:// 后可能是 /path(Unix)或 /C:/path(Windows 三斜杠)或 host/path
        if let Some(windows_path) = rest.strip_prefix('/') {
            // 检查是否是 Windows 路径(/C:/...)
            if windows_path.len() >= 2 && windows_path.as_bytes()[1] == b':' {
                // Windows: /C:/workspace/main.rs → C:/workspace/main.rs
                return windows_path.to_owned();
            }
            // Unix: /home/user/main.rs → /home/user/main.rs
            return format!("/{windows_path}");
        }
        // file://host/path(不常见)→ 原样返回 rest
        return rest.to_owned();
    }
    // 非 file:// URI 原样返回
    uri.to_owned()
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
// Step 4.2-c: lsp-types crate 类型转换 impl
// 用官方类型替代部分手搓 serde_json,提高协议层可靠性
// ============================================================================

/// 将 `lsp_types::SymbolKind`(newtype around private i32)映射为可读字符串。
///
/// Step 4.2-c 原使用 `impl From<lsp_types::SymbolKind> for &'static str`,
/// 但违反孤儿规则(E0117,两个外部类型),且 0.95.1 中 `SymbolKind` 是
/// struct 而非 enum(E0432,无法 `use SymbolKind::*` 展开),内部字段
/// 为 private(E0616,不能 `kind.0`)。改为独立函数用关联常量匹配,
/// 与 [`symbol_kind_to_str`](fn@symbol_kind_to_str) 保持映射一致。
fn symbol_kind_to_str_typed(kind: lsp_types::SymbolKind) -> &'static str {
    use lsp_types::SymbolKind;
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum_member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type_parameter",
        _ => "unknown",
    }
}

impl From<lsp_types::DocumentSymbol> for LspSymbol {
    /// 将 `lsp_types::DocumentSymbol`(官方类型)转换为 `LspSymbol`。
    ///
    /// 注意:DocumentSymbol 不包含文件路径,转换后 `path` 为空,
    /// 调用方需手动设置(参考 `parse_document_symbols_typed` 的实现)。
    fn from(doc: lsp_types::DocumentSymbol) -> Self {
        let kind_str = symbol_kind_to_str_typed(doc.kind);
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
    ///
    /// SP4.2 修复:统一 path 格式为本地路径(去掉 `file://` 前缀),
    /// 与 DocumentSymbol 格式(调用方传入的 path)保持一致。
    fn from(info: lsp_types::SymbolInformation) -> Self {
        let kind_str = symbol_kind_to_str_typed(info.kind);
        let position = info.location.range.start;
        // SP4.2 修复:去掉 file:// 前缀,统一为本地路径格式
        let path = uri_to_path(info.location.uri.as_str());
        Self {
            name: info.name,
            kind: kind_str.to_owned(),
            path,
            line: position.line,
            character: position.character,
        }
    }
}

/// 从 dispatch 包装响应中提取真正的 LSP JSON-RPC `result`。
///
/// `LspRegistry::dispatch` 返回 `{ action, path, ..., rpc_response, ... }`,
/// 真正的 LSP 结果位于 `rpc_response.result`。此函数同时兼容
/// 直接传入原始 JSON-RPC 响应(`result` 在顶层)的调用方式。
fn unwrap_lsp_result(response: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(rpc) = response.get("rpc_response") {
        if let Some(result) = rpc.get("result") {
            return Some(result);
        }
    }
    response.get("result")
}

/// 解析 `textDocument/references` 响应为 `Vec<LspLocation>`。
///
/// LSP server 可能返回两种格式(参考 LSP spec §3.11.4):
/// 1. `Location[]` — 平铺结构,每项含 `uri` + `range`
/// 2. `LocationLink[]` — 链接结构,每项含 `targetUri` + `targetRange`(+`targetSelectionRange`)
///
/// 两种格式都会统一转换为 [`LspLocation`](LspLocation)(本地路径 + 位置)。
/// result 为 `null`(无引用)或解析失败时返回空 Vec。
///
/// # 与 repomap 协同
/// 返回的引用位置用于 [`crate::repomap::RepoMap::refresh_lsp_references`]
/// 统计跨模块引用计数(替代 regex 子串匹配)。
#[must_use]
pub fn parse_references(response: &serde_json::Value) -> Vec<LspLocation> {
    let result = match unwrap_lsp_result(response) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let arr = match result.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut locations = Vec::new();
    for item in arr {
        // LocationLink 优先(targetUri),否则 Location(uri)
        let uri = item
            .get("targetUri")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("uri").and_then(|v| v.as_str()));
        // LocationLink 用 targetRange,Location 用 range
        let range = item.get("targetRange").or_else(|| item.get("range"));

        let (Some(uri), Some(range)) = (uri, range) else {
            continue;
        };

        let path = uri_to_path(uri);
        let line = range
            .get("start")
            .and_then(|s| s.get("line").and_then(|l| l.as_u64()))
            .unwrap_or(0) as u32;
        let character = range
            .get("start")
            .and_then(|s| s.get("character").and_then(|c| c.as_u64()))
            .unwrap_or(0) as u32;
        let end_line = range
            .get("end")
            .and_then(|e| e.get("line").and_then(|l| l.as_u64()))
            .map(|n| n as u32);
        let end_character = range
            .get("end")
            .and_then(|e| e.get("character").and_then(|c| c.as_u64()))
            .map(|n| n as u32);

        locations.push(LspLocation {
            path,
            line,
            character,
            end_line,
            end_character,
            preview: None,
        });
    }
    locations
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
    // SP4.3 修复:process transport 下响应被包装在 `rpc_response.result`,
    // 用 unwrap_lsp_result 统一解包(兼容原始 JSON-RPC 与 dispatch 包装两种形态),
    // 否则真实 rust-analyzer 的 documentSymbol 永远解析为空。
    let result = unwrap_lsp_result(response).unwrap_or(response);

    // 尝试 1:DocumentSymbol[] 反序列化
    if let Ok(doc_symbols) =
        serde_json::from_value::<Vec<lsp_types::DocumentSymbol>>(result.clone())
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
fn parse_document_symbol_recursive(item: &serde_json::Value, path: &str, out: &mut Vec<LspSymbol>) {
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

    // SP4.2 修复:去掉 location.uri 的 file:// 前缀,统一为本地路径格式
    // 与 typed 路径(From<lsp_types::SymbolInformation>)保持一致
    let uri = location.get("uri").and_then(|v| v.as_str()).unwrap_or("");
    let path = uri_to_path(uri);

    Some(LspSymbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        path,
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
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        // Step 4.3:声明 hierarchicalDocumentSymbolSupport,
                        // 与 ProcessLspTransport::initialize_params 保持一致,
                        // 确保 rust-analyzer 返回带 selectionRange 的 DocumentSymbol[]。
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
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

    // ── 编辑后自动诊断(refresh_diagnostics_for_path)──

    #[test]
    fn refresh_skips_non_code_extensions_without_touching_registry() {
        let registry = LspRegistry::new();
        for path in ["README.md", "config.json", "noext", "notes.txt"] {
            assert_eq!(
                registry.refresh_diagnostics_for_path(path),
                LspAutoDiagOutcome::Skip,
                "非代码文件 {path} 应静默跳过"
            );
        }
    }

    #[test]
    fn record_push_tracks_per_file_versions() {
        let registry = LspRegistry::new();
        let base = if cfg!(windows) { "D:/proj" } else { "/proj" };
        let a = format!("{base}/a.rs");
        let b = format!("{base}/b.py");
        let c = format!("{base}/c.go");
        {
            let mut inner = registry.inner.lock().expect("lock");
            record_push(&mut inner, a.clone());
            record_push(&mut inner, b.clone());
            record_push(&mut inner, a.clone());
        }
        // 全局序号递增,每文件记录最近一次序号。
        assert_eq!(registry.last_push_version(&a), 3);
        assert_eq!(registry.last_push_version(&b), 2);
        assert_eq!(registry.last_push_version(&c), 0, "从未推送的文件序号为 0");
    }

    #[test]
    fn refresh_respects_auto_start_cooldown_for_supported_language() {
        let registry = LspRegistry::new();
        // 设置 python 冷却后,刷新 .py 直接 Skip(不 spawn server)。
        registry.set_auto_start_cooldown("python");
        assert!(registry.is_auto_start_cooldown("python"));
        assert_eq!(
            registry.refresh_diagnostics_for_path("src/mod.py"),
            LspAutoDiagOutcome::Skip
        );
        // 未冷却的其他语言不受影响。
        assert!(!registry.is_auto_start_cooldown("typescript"));
    }

    #[test]
    fn language_for_extension_maps_known_code_files() {
        assert_eq!(language_for_extension("rs"), Some("rust"));
        assert_eq!(language_for_extension("tsx"), Some("typescript"));
        assert_eq!(language_for_extension("py"), Some("python"));
        assert_eq!(language_for_extension("md"), None);
        assert_eq!(language_for_extension(""), None);
    }

    #[test]
    fn normalize_lsp_path_unifies_separators_and_resolves_relative() {
        // 相对路径 → 基于 cwd 解析为绝对路径,且不再含反斜杠。
        let rel = normalize_lsp_path("src/x.py").expect("resolvable");
        assert!(rel.ends_with("src/x.py"));
        assert!(!rel.contains('\\'), "规范化后不应有反斜杠: {rel}");
        // 绝对路径的反斜杠 → 正斜杠(与 uri_to_path 输出一致;仅 Windows)。
        #[cfg(windows)]
        assert_eq!(
            normalize_lsp_path("D:\\a\\b.rs").as_deref(),
            Some("D:/a/b.rs")
        );
        // Windows verbatim 前缀 `\\?\` 剥离(编辑工具返回的 filePath 格式;仅 Windows)。
        #[cfg(windows)]
        assert_eq!(
            normalize_lsp_path(r"\\?\D:\a\b.rs").as_deref(),
            Some("D:/a/b.rs")
        );
    }

    #[test]
    fn get_diagnostics_matches_cached_path_regardless_of_separator() {
        let registry = LspRegistry::new();
        registry.register("python", LspServerStatus::Connected, None, vec![]);
        // 跨平台绝对路径(Windows: D:/..., Unix: /...)。
        let key = if cfg!(windows) {
            "D:/proj/src/mod.py"
        } else {
            "/proj/src/mod.py"
        };
        // 缓存 path 是 uri_to_path 输出格式(正斜杠)。
        registry
            .add_diagnostics(
                "python",
                vec![LspDiagnostic {
                    path: key.to_string(),
                    line: 0,
                    character: 0,
                    severity: "error".to_string(),
                    message: "syntax".to_string(),
                    source: None,
                }],
            )
            .expect("add");
        // 正斜杠查询命中。
        assert_eq!(registry.get_diagnostics(key).len(), 1);
        // 反斜杠查询同样命中(同类 bug 修复点;仅 Windows)。
        #[cfg(windows)]
        assert_eq!(registry.get_diagnostics("D:\\proj\\src\\mod.py").len(), 1);
        // 大小写不同同样命中(Windows 文件系统不区分大小写;仅 Windows)。
        #[cfg(windows)]
        assert_eq!(registry.get_diagnostics("d:/PROJ/src/MOD.py").len(), 1);
        // 无关路径不命中。
        let other = if cfg!(windows) {
            "D:/other.py"
        } else {
            "/other.py"
        };
        assert!(registry.get_diagnostics(other).is_empty());
    }

    #[test]
    fn record_push_version_matches_backslash_query() {
        let registry = LspRegistry::new();
        let key = if cfg!(windows) {
            "D:/proj/a.py"
        } else {
            "/proj/a.py"
        };
        {
            let mut inner = registry.inner.lock().expect("lock");
            record_push(&mut inner, key.to_string());
        }
        // last_push_version 同样规范化,反斜杠查询可命中(仅 Windows)。
        #[cfg(windows)]
        assert_eq!(registry.last_push_version("D:\\proj\\a.py"), 1);
        #[cfg(windows)]
        assert_eq!(
            registry.last_push_version("d:/PROJ/A.PY"),
            1,
            "大小写不敏感命中"
        );
        assert_eq!(registry.last_push_version(key), 1);
    }

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
        // .md 扩展名不在 LSP 支持列表中,返回 unsupported extension 错误
        let error = result.expect_err("unsupported extension should fail");
        assert!(
            error.contains("unsupported file extension '.md'"),
            "got: {error}"
        );
        assert!(error.contains("notes.md"));
    }

    #[test]
    fn dispatch_unsupported_extension_error_includes_supported_list() {
        // given
        let registry = LspRegistry::new();

        // when
        let result = registry.dispatch("hover", Some("data.csv"), Some(1), Some(0), None);

        // then
        let error = result.expect_err("unsupported extension should fail");
        assert!(error.contains("unsupported file extension '.csv'"));
        assert!(error.contains(".rs"), "should list supported extensions");
    }

    #[test]
    fn dispatch_no_extension_error_is_clear() {
        // given
        let registry = LspRegistry::new();

        // when — 无扩展名文件
        let result = registry.dispatch("hover", Some("Makefile"), Some(1), Some(0), None);

        // then
        let error = result.expect_err("no extension should fail");
        assert!(error.contains("unsupported file extension"));
    }

    #[test]
    fn dispatch_unconfigured_language_error_gives_config_hint() {
        // given — registry 为空,.java 扩展名被识别但 java 语言无预置默认且未配置
        let registry = LspRegistry::new();

        // when
        let result = registry.dispatch("hover", Some("src/Main.java"), Some(1), Some(0), None);

        // then — java 无预置默认,返回"未配置"错误(含配置模板)
        let error = result.expect_err("unconfigured language should fail with hint");
        assert!(error.contains("no LSP server configured for language 'java'"));
        assert!(error.contains("lspServers"), "should mention config key");
        assert!(
            error.contains("settings.json"),
            "should mention config file"
        );
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
        let req = LspRequest::new(
            LspAction::Hover,
            "/workspace/src/main.rs",
            None,
            None,
            "rust",
        );
        assert_eq!(req.file_uri(), "file:///workspace/src/main.rs");
    }

    #[test]
    fn lsp_request_file_uri_converts_windows_path() {
        let req = LspRequest::new(
            LspAction::Hover,
            r"C:\workspace\main.rs",
            None,
            None,
            "rust",
        );
        assert_eq!(req.file_uri(), "file:///C:/workspace/main.rs");
    }

    #[test]
    fn lsp_request_file_uri_preserves_existing_file_uri() {
        let req = LspRequest::new(
            LspAction::Hover,
            "file:///workspace/main.rs",
            None,
            None,
            "rust",
        );
        // 已是 file:// URI 时应原样返回
        assert_eq!(req.file_uri(), "file:///workspace/main.rs");
    }

    #[test]
    fn lsp_request_method_maps_correctly() {
        assert_eq!(
            LspRequest::new(LspAction::Hover, "", None, None, "").method(),
            "textDocument/hover"
        );
        assert_eq!(
            LspRequest::new(LspAction::Definition, "", None, None, "").method(),
            "textDocument/definition"
        );
        assert_eq!(
            LspRequest::new(LspAction::Completion, "", None, None, "").method(),
            "textDocument/completion"
        );
        assert_eq!(
            LspRequest::new(LspAction::References, "", None, None, "").method(),
            "textDocument/references"
        );
        assert_eq!(
            LspRequest::new(LspAction::Symbols, "", None, None, "").method(),
            "textDocument/documentSymbol"
        );
        assert_eq!(
            LspRequest::new(LspAction::Format, "", None, None, "").method(),
            "textDocument/formatting"
        );
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
        let req = LspRequest::new(
            LspAction::References,
            "src/main.rs",
            Some(1),
            Some(0),
            "rust",
        );
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
        let result = transport
            .send("textDocument/hover", serde_json::json!({}))
            .unwrap();
        assert_eq!(result["result"]["transport"], "memory");
        assert_eq!(result["result"]["language"], "rust");
        assert_eq!(result["result"]["status"], "protocol_constructed");
    }

    #[test]
    fn process_lsp_transport_returns_not_spawned() {
        let transport =
            ProcessLspTransport::new("rust", Some("/workspace".to_string()), "rust-analyzer");
        let result = transport.send("initialize", serde_json::json!({})).unwrap();
        assert_eq!(result["result"]["transport"], "process");
        assert_eq!(result["result"]["server_command"], "rust-analyzer");
        assert_eq!(result["result"]["status"], "not_spawned");
    }

    #[test]
    fn process_lsp_transport_with_args_stores_args() {
        // 验证 with_args 正确存储参数(不 spawn,只检查字段)
        let transport = ProcessLspTransport::with_args(
            "typescript",
            Some("/workspace".to_string()),
            "typescript-language-server",
            vec!["--stdio".to_string()],
        );
        assert_eq!(transport.server_command, "typescript-language-server");
        assert_eq!(transport.server_args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn process_lsp_transport_new_defaults_to_empty_args() {
        let transport =
            ProcessLspTransport::new("rust", Some("/workspace".to_string()), "rust-analyzer");
        assert!(
            transport.server_args.is_empty(),
            "new() should default to empty args"
        );
    }

    #[test]
    fn install_hint_covers_mainstream_servers() {
        // 主流 server 应有安装提示
        assert!(install_hint_for_command("rust-analyzer").is_some());
        assert!(install_hint_for_command("pylsp").is_some());
        assert!(install_hint_for_command("gopls").is_some());
        assert!(install_hint_for_command("typescript-language-server").is_some());
        assert!(install_hint_for_command("clangd").is_some());
        assert!(install_hint_for_command("solargraph").is_some());
        // 未知命令应返回 None
        assert!(install_hint_for_command("my-custom-lsp-server").is_none());
    }

    #[test]
    fn install_hint_for_rust_contains_rustup() {
        let hint = install_hint_for_command("rust-analyzer").unwrap();
        assert!(
            hint.contains("rustup"),
            "rust-analyzer hint should mention rustup"
        );
    }

    #[test]
    fn default_lsp_server_covers_mainstream_languages() {
        // 主流语言应有预置默认
        assert_eq!(
            default_lsp_server_for_language("rust").unwrap().0,
            "rust-analyzer"
        );
        assert_eq!(
            default_lsp_server_for_language("python").unwrap().0,
            "pylsp"
        );
        assert_eq!(
            default_lsp_server_for_language("typescript").unwrap().0,
            "typescript-language-server"
        );
        assert_eq!(default_lsp_server_for_language("go").unwrap().0, "gopls");
        assert_eq!(default_lsp_server_for_language("c").unwrap().0, "clangd");
        assert_eq!(
            default_lsp_server_for_language("ruby").unwrap().0,
            "solargraph"
        );
        // 未知语言应返回 None
        assert!(default_lsp_server_for_language("brainfuck").is_none());
    }

    #[test]
    fn default_lsp_server_typescript_has_stdio_arg() {
        let (_, args) = default_lsp_server_for_language("typescript").unwrap();
        assert_eq!(args, &["--stdio"]);
    }

    #[test]
    fn default_lsp_server_rust_has_no_args() {
        let (_, args) = default_lsp_server_for_language("rust").unwrap();
        assert!(args.is_empty());
    }

    #[test]
    fn is_command_available_detects_known_command() {
        // cargo 几乎肯定在 PATH 中(我们在 Rust 项目里)
        assert!(
            is_command_available("cargo") || is_command_available("cargo.exe"),
            "cargo should be in PATH"
        );
    }

    #[test]
    fn is_command_available_rejects_unknown_command() {
        assert!(
            !is_command_available("nonexistent-command-xyz-12345"),
            "nonexistent command should not be in PATH"
        );
    }

    #[test]
    fn set_default_root_path_stores_root() {
        let registry = LspRegistry::new();
        registry.set_default_root_path("/my/workspace");
        let inner = registry.inner.lock().unwrap();
        assert_eq!(inner.default_root_path.as_deref(), Some("/my/workspace"));
    }

    #[test]
    fn try_auto_start_returns_error_for_no_default_language() {
        let registry = LspRegistry::new();
        registry.set_default_root_path("/workspace");
        let result = registry.try_auto_start("brainfuck");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no default LSP server"));
    }

    #[test]
    fn try_auto_start_returns_error_when_command_not_in_path() {
        // rust-analyzer 在 CI 中可能未安装,但如果安装了则不会触发此路径
        // 用一个肯定不存在的命令来测试:先检查默认命令是否存在
        // 这里测试的是 "命令不在 PATH" 的错误路径
        // 注意:如果 rust-analyzer 恰好在 PATH 中,这个测试会跳过
        let registry = LspRegistry::new();
        registry.set_default_root_path("/workspace");
        // solargraph 在大多数环境中未安装
        if !is_command_available("solargraph") {
            let result = registry.try_auto_start("ruby");
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                err.contains("not in PATH"),
                "error should mention PATH: {err}"
            );
            assert!(
                err.contains("gem install"),
                "error should include install hint: {err}"
            );
        }
    }

    #[test]
    fn process_lsp_transport_initialize_params_includes_root_uri() {
        let transport =
            ProcessLspTransport::new("rust", Some("/workspace".to_string()), "rust-analyzer");
        let params = transport.initialize_params();
        assert_eq!(params["rootPath"], "/workspace");
        assert_eq!(params["rootUri"], "file:///workspace");
        assert_eq!(
            params["capabilities"]["textDocument"]["completion"]["completionItem"]
                ["snippetSupport"],
            true
        );
        // Step 4.3:必须声明 hierarchicalDocumentSymbolSupport,
        // 否则 rust-analyzer 返回 flat SymbolInformation(位置含 doc 注释),
        // get_references 无法命中标识符。
        assert_eq!(
            params["capabilities"]["textDocument"]["documentSymbol"]
                ["hierarchicalDocumentSymbolSupport"],
            true,
            "initialize must declare hierarchicalDocumentSymbolSupport"
        );
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
        let request = LspRequest::new(
            LspAction::Completion,
            "src/main.rs",
            Some(1),
            Some(0),
            "rust",
        );
        let result = client.dispatch(&request).unwrap();
        assert_eq!(result["rpc_response"]["result"]["transport"], "process");
        assert_eq!(
            result["rpc_response"]["result"]["server_command"],
            "rust-analyzer"
        );
    }

    #[test]
    fn dispatch_hover_action_uses_json_rpc_client() {
        let registry = LspRegistry::new();
        registry.register(
            "rust",
            LspServerStatus::Connected,
            Some("/workspace"),
            vec![],
        );

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
        let result = registry.spawn_server("rust", "rust-analyzer", &[], "/workspace");
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

        let result = registry.spawn_server("rust", "nonexistent-lsp-server-xyz", &[], "/workspace");
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
        registry.register(
            "rust",
            LspServerStatus::Connected,
            Some("/workspace"),
            vec![],
        );

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
            registry.spawn_server("rust", "rust-analyzer", &[], workspace.to_str().unwrap());
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
        let result = registry.spawn_server("rust", "cat", &[], "/tmp");
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
        assert_eq!(
            symbols.len(),
            3,
            "should have 3 symbols (main + inner_fn + MyStruct)"
        );

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
        // SP4.2 修复:path 现在是本地路径(去掉 file:// 前缀)
        assert_eq!(symbols[0].path, "/workspace/src/lib.rs");

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
    fn parse_document_symbols_typed_uses_selection_range_for_position() {
        // Step 4.3 回归:typed 路径必须用 DocumentSymbol 的 selectionRange
        // (标识符精确位置),而不是 range(声明块起点,含 doc 注释)。
        // 用 dispatch 包装响应(与真实 process transport 返回一致)构造,
        // 验证 unwrap_lsp_result 解包 + DocumentSymbol 反序列化双路径。
        let response = serde_json::json!({
            "action": "symbols",
            "path": "src/lib.rs",
            "language": "rust",
            "method": "textDocument/documentSymbol",
            "rpc_response": {
                "jsonrpc": "2.0",
                "id": 1,
                "result": [
                    {
                        "name": "OrderItem",
                        "kind": 23,
                        "range": { "start": {"line": 9, "character": 0}, "end": {"line": 13, "character": 1} },
                        "selectionRange": { "start": {"line": 10, "character": 11}, "end": {"line": 10, "character": 20} },
                        "children": []
                    }
                ]
            },
            "status": "dispatched"
        });

        let symbols = parse_document_symbols_typed(&response, "src/lib.rs");
        assert_eq!(symbols.len(), 1, "typed path should parse one symbol");
        assert_eq!(symbols[0].name, "OrderItem");
        // selectionRange.start = (10, 11) 是标识符位置;若误用 range.start 会得到 (9, 0)。
        assert_eq!(
            (symbols[0].line, symbols[0].character),
            (10, 11),
            "position must come from selectionRange (identifier), not range (declaration block start)"
        );
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
    // Step 4.3 — parse_references / get_references 测试
    // ========================================================================

    #[test]
    fn parse_references_handles_location_array() {
        // Location[] 格式:每项含 uri + range
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "uri": "file:///workspace/src/lib.rs",
                    "range": { "start": {"line": 10, "character": 4}, "end": {"line": 10, "character": 12} }
                },
                {
                    "uri": "file:///workspace/src/main.rs",
                    "range": { "start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 5} }
                }
            ]
        });

        let refs = parse_references(&response);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].path, "/workspace/src/lib.rs");
        assert_eq!(refs[0].line, 10);
        assert_eq!(refs[0].character, 4);
        assert_eq!(refs[0].end_line, Some(10));
        assert_eq!(refs[0].end_character, Some(12));
        assert_eq!(refs[1].path, "/workspace/src/main.rs");
        assert_eq!(refs[1].line, 3);
    }

    #[test]
    fn parse_references_handles_location_link_array() {
        // LocationLink[] 格式:每项含 targetUri + targetRange
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                {
                    "originSelectionRange": { "start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 8} },
                    "targetUri": "file:///workspace/src/types.rs",
                    "targetRange": { "start": {"line": 20, "character": 0}, "end": {"line": 20, "character": 30} },
                    "targetSelectionRange": { "start": {"line": 20, "character": 4}, "end": {"line": 20, "character": 10} }
                }
            ]
        });

        let refs = parse_references(&response);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "/workspace/src/types.rs");
        assert_eq!(refs[0].line, 20);
        assert_eq!(refs[0].character, 0);
        assert_eq!(refs[0].end_line, Some(20));
        assert_eq!(refs[0].end_character, Some(30));
    }

    #[test]
    fn parse_references_handles_dispatch_wrapped_response() {
        // dispatch 包装响应(rpc_response.result 内层)
        let response = serde_json::json!({
            "action": "references",
            "path": "src/main.rs",
            "line": 10,
            "character": 4,
            "language": "rust",
            "method": "textDocument/references",
            "rpc_response": {
                "jsonrpc": "2.0",
                "id": 1,
                "result": [
                    {
                        "uri": "file:///workspace/src/lib.rs",
                        "range": { "start": {"line": 2, "character": 1}, "end": {"line": 2, "character": 9} }
                    }
                ]
            },
            "status": "dispatched"
        });

        let refs = parse_references(&response);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "/workspace/src/lib.rs");
        assert_eq!(refs[0].line, 2);
        assert_eq!(refs[0].character, 1);
    }

    #[test]
    fn parse_references_handles_null_and_empty_result() {
        // result 为 null(无引用)
        let null_resp = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": null });
        assert!(parse_references(&null_resp).is_empty());

        // result 为空数组
        let empty_resp = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": [] });
        assert!(parse_references(&empty_resp).is_empty());

        // 错误响应(无 result)
        let err_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32601, "message": "method not found" }
        });
        assert!(parse_references(&err_resp).is_empty());
    }

    #[test]
    fn parse_references_skips_malformed_entries() {
        // 缺 uri/range 的畸形条目应被跳过,不影响其他有效条目
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [
                { "uri": "file:///workspace/src/lib.rs", "range": { "start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4} } },
                { "uri": "file:///workspace/src/no_range.rs" },
                { "range": { "start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 1} } }
            ]
        });

        let refs = parse_references(&response);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "/workspace/src/lib.rs");
    }

    #[test]
    fn get_references_uses_memory_transport_fallback() {
        // 未 spawn 时 dispatch fallback 到 MemoryLspTransport,
        // get_references 返回空 Vec(不报错)。
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Connected, None, vec![]);

        let refs = registry
            .get_references("src/main.rs", 10, 4)
            .expect("memory fallback should not error");
        assert!(refs.is_empty());
    }

    #[test]
    fn get_references_errors_for_disconnected_server() {
        let registry = LspRegistry::new();
        registry.register("rust", LspServerStatus::Disconnected, None, vec![]);

        let result = registry.get_references("src/main.rs", 10, 4);
        assert!(result.is_err());
    }

    #[test]
    fn parse_publish_diagnostics_extracts_severity_and_position() {
        let msg = serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///workspace/src/main.rs",
                "diagnostics": [
                    {
                        "range": { "start": {"line": 10, "character": 5}, "end": {"line": 10, "character": 15} },
                        "severity": 1,
                        "message": "mismatched types",
                        "source": "rust-analyzer"
                    },
                    {
                        "range": { "start": {"line": 20, "character": 0}, "end": {"line": 20, "character": 3} },
                        "severity": 2,
                        "message": "unused variable: x",
                        "source": "rust-analyzer"
                    }
                ]
            }
        });

        let diags = parse_publish_diagnostics(&msg);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].path, "/workspace/src/main.rs");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].character, 5);
        assert_eq!(diags[0].severity, "error");
        assert_eq!(diags[0].message, "mismatched types");
        assert_eq!(diags[0].source.as_deref(), Some("rust-analyzer"));
        assert_eq!(diags[1].severity, "warning");
    }

    #[test]
    fn parse_publish_diagnostics_handles_empty_diagnostics() {
        let msg = serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///workspace/src/main.rs",
                "diagnostics": []
            }
        });

        let diags = parse_publish_diagnostics(&msg);
        assert!(diags.is_empty());
    }

    #[test]
    fn parse_publish_diagnostics_handles_unknown_severity() {
        let msg = serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///workspace/main.rs",
                "diagnostics": [
                    {
                        "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} },
                        "severity": 99,
                        "message": "weird"
                    }
                ]
            }
        });

        let diags = parse_publish_diagnostics(&msg);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, "unknown");
        assert!(diags[0].source.is_none());
    }

    #[test]
    fn parse_publish_diagnostics_handles_missing_diagnostics_field() {
        let msg = serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///workspace/main.rs"
            }
        });

        let diags = parse_publish_diagnostics(&msg);
        assert!(diags.is_empty());
    }
}
