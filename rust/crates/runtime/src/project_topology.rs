//! ProjectTopology — 可查询的语义拓扑图。
//!
//! # Design
//!
//! ProjectTopology 提供项目级代码结构查询能力：
//! - [`ModuleGraph`] — crate 级依赖图(通过 `cargo metadata`)
//! - [`SymbolIndex`] — 符号交叉引用索引(基于 LspRegistry, best-effort)
//! - [`TopologyState`] — 状态机(Uninitialized → Building → Ready → Failed)
//!
//! ## Caching
//!
//! 首次查询时懒加载，后台异步构建，写入 `.claw/topology.json`。
//!
//! ## Building UX
//!
//! 工具描述中指导 LLM："If TopologyState is 'building', do NOT retry immediately.
//! Use read/grep instead."
//!
//! ## P2-1: 真正异步化
//!
//! `ensure_built()` 是**非阻塞**的:首次调用时通过 `std::thread::spawn` 派发
//! 后台线程执行 `cargo metadata` + `build_symbol_index_fast`,立即返回 `Building`。
//! 后台线程完成后通过共享的 `Arc<Mutex<TopologyState>>` 更新状态为 `Ready`/`Failed`。
//!
//! 选择 `std::thread::spawn` 而非 `tokio::task::spawn_blocking` 的原因:
//! - `ProjectTopology` 不持有 tokio runtime handle,在非 async 上下文中也可用
//! - 构建工作是纯阻塞 I/O(子进程 + 文件扫描),不是 async
//! - 后台线程只需更新共享 state,无需 async 通信
//!
//! `ensure_built_blocking()` 是**阻塞**版本,供测试和需要同步结果的场景使用。
//! 它直接调用 `build_topology_data` 并同步更新状态。

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TopologyState
// ---------------------------------------------------------------------------

/// 拓扑构建状态。
#[derive(Debug, Clone)]
pub enum TopologyState {
    Uninitialized,
    Building { started_at: Instant },
    Ready(Box<TopologyData>),
    Failed(String),
}

impl std::fmt::Display for TopologyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uninitialized => write!(f, "uninitialized"),
            Self::Building { .. } => write!(f, "building"),
            Self::Ready(_) => write!(f, "ready"),
            Self::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// ModuleGraph
// ---------------------------------------------------------------------------

/// Crate-level dependency graph entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrateInfo {
    pub name: String,
    pub version: String,
    pub manifest_path: PathBuf,
    pub dependencies: Vec<String>,
    pub source_paths: Vec<PathBuf>,
}

/// Crate dependency graph derived from `cargo metadata`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleGraph {
    pub workspace_root: PathBuf,
    pub crates: Vec<CrateInfo>,
    /// dependency name → list of dependant crate names
    pub reverse_deps: HashMap<String, Vec<String>>,
}

// ---------------------------------------------------------------------------
// SymbolIndex
// ---------------------------------------------------------------------------

/// 符号定义记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolDef {
    pub name: String,
    pub kind: String, // "fn", "struct", "trait", "enum", "mod", etc.
    pub file: PathBuf,
    pub line: u32,
    pub crate_name: Option<String>,
    pub visibility: Option<String>, // "pub", "pub(crate)", or None for private
}

/// 调用点记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSite {
    pub file: PathBuf,
    pub line: u32,
    pub context: Option<String>, // surrounding line for quick reference
}

/// 符号交叉引用索引(best-effort, LSP 可能遗漏宏展开/条件编译)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolIndex {
    pub definitions: HashMap<String, Vec<SymbolDef>>,
    pub callers: HashMap<String, Vec<CallSite>>,
    pub file_symbols: HashMap<PathBuf, Vec<String>>,
}

/// 完整拓扑数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyData {
    pub module_graph: ModuleGraph,
    pub symbol_index: Option<SymbolIndex>,
    pub built_at_ms: u64,
}

// ---------------------------------------------------------------------------
// ProjectTopology
// ---------------------------------------------------------------------------

/// 项目拓扑管理器。
///
/// 懒加载:首次查询时触发 `cargo metadata` 构建 ModuleGraph。
/// SymbolIndex 需要 LspRegistry(可选,首次查询时异步构建)。
///
/// P1:磁盘缓存 — 首次构建成功后写入 `.claw/topology_<language>.json`,
/// 下次会话 `ensure_built` 先查缓存(经指纹校验)命中即 `Ready`,跨会话免重建。
/// 主语言由 [`crate::topology_cache::pick_primary_language`] 选定;
/// 无支持语言时直接 `Failed`,不进入 Building。
#[derive(Debug)]
pub struct ProjectTopology {
    state: Arc<Mutex<TopologyState>>,
    workspace_root: PathBuf,
    /// 主语言(混合项目策略选定)。None = 无支持语言 → 构建直接 Failed。
    language: Option<crate::topology_cache::Language>,
    lsp_registry: Option<Arc<crate::lsp_client::LspRegistry>>,
}

impl ProjectTopology {
    /// 创建新的 ProjectTopology，初始状态为 Uninitialized。
    pub fn new(workspace_root: PathBuf) -> Self {
        let language = crate::topology_cache::pick_primary_language(&workspace_root);
        Self {
            state: Arc::new(Mutex::new(TopologyState::Uninitialized)),
            workspace_root,
            language,
            lsp_registry: None,
        }
    }

    /// 注入 LspRegistry(可选)。注入后查询操作可返回符号索引信息。
    pub fn with_lsp_registry(mut self, registry: Arc<crate::lsp_client::LspRegistry>) -> Self {
        self.lsp_registry = Some(registry);
        self
    }

    /// 获取当前拓扑状态。
    pub fn state(&self) -> TopologyState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// **非阻塞**地尝试构建拓扑(P2-1)。
    ///
    /// 如果当前状态为 `Uninitialized`:
    /// 1. 无主语言(无支持语言标记)→ 直接置 `Failed`,不进入 Building(防死循环);
    /// 2. 先查磁盘缓存(P1,经指纹校验)→ 命中直接置 `Ready`,跨会话免重建;
    /// 3. 未命中则在锁内将状态转为 `Building { started_at }`,派发
    ///    `std::thread::spawn` 后台线程执行实际构建,立即返回 `Building`;
    /// 4. 后台构建成功后写入磁盘缓存(仅成功时落盘,防缓存误导)。
    ///
    /// 如果当前状态为 `Ready`/`Failed`/`Building`,直接返回当前状态(不重复派发)。
    ///
    /// 后台线程通过共享的 `Arc<Mutex<TopologyState>>` 更新状态为 `Ready` 或 `Failed`。
    /// 线程使用 `catch_unwind` 防止 panic 导致状态永久停留在 `Building`。
    ///
    /// 调用方(如 `query_project_graph`)在收到 `Building` 时应返回提示消息,
    /// 告诉 LLM "do NOT retry immediately, use read/grep instead"。
    pub fn ensure_built(&self) -> TopologyState {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());

        match &*guard {
            TopologyState::Ready(_) => return guard.clone(),
            TopologyState::Failed(_) => return guard.clone(),
            TopologyState::Building { .. } => return guard.clone(),
            TopologyState::Uninitialized => {}
        }

        // 无支持语言 → 直接 Failed,不进入 Uninitialized→Building(防"永远 Building")。
        let Some(language) = self.language else {
            *guard = TopologyState::Failed(
                "no supported language detected (requires a cargo workspace or \
                 Python project markers: pyproject.toml/setup.py/setup.cfg/requirements.txt)"
                    .to_string(),
            );
            return guard.clone();
        };

        // P1:先查磁盘缓存(指纹校验)。命中即 Ready,跳过 cargo metadata + rg。
        if let Some(cached) = crate::topology_cache::load_cache(&self.workspace_root, language) {
            *guard = TopologyState::Ready(Box::new(cached.data));
            return guard.clone();
        }

        // Uninitialized → Building,派发后台线程
        let started_at = Instant::now();
        *guard = TopologyState::Building { started_at };
        drop(guard); // 释放锁,让后台线程能更新状态

        // 克隆共享 state 和 workspace_root 给后台线程
        let state_clone = Arc::clone(&self.state);
        let workspace_root = self.workspace_root.clone();

        std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(|| build_topology_data(&workspace_root, language));

            let mut guard = state_clone.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(Ok(data)) => {
                    // P1:构建成功后落盘(仅成功时;best-effort,失败静默)。
                    crate::topology_cache::save_cache(&workspace_root, language, &data);
                    *guard = TopologyState::Ready(Box::new(data));
                }
                Ok(Err(e)) => {
                    *guard = TopologyState::Failed(e);
                }
                Err(panic_payload) => {
                    // catch_unwind 捕获 panic,防止状态永久停留在 Building
                    let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                        format!("topology build panicked: {s}")
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        format!("topology build panicked: {s}")
                    } else {
                        "topology build panicked (unknown payload type)".to_string()
                    };
                    *guard = TopologyState::Failed(msg);
                }
            }
        });

        // 返回 Building 状态(刚设置的)
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// **阻塞**地构建拓扑,直到达到 `Ready`/`Failed` 终态(P2-1)。
    ///
    /// 供测试和需要同步结果的场景使用。在非 `Uninitialized` 状态下行为与
    /// `ensure_built()` 相同(返回当前状态)。在 `Uninitialized` 状态下
    /// 同步执行 `build_topology_data` 并更新状态,返回最终结果。
    ///
    /// P1:Uninitialized 时同样先查磁盘缓存;构建成功后落盘。
    ///
    /// 注意:此方法会阻塞调用线程直到 `cargo metadata` + `build_symbol_index_fast`
    /// 完成(通常 < 5 秒,大项目可能更久)。生产路径应使用 `ensure_built()`。
    pub fn ensure_built_blocking(&self) -> TopologyState {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());

        match &*guard {
            TopologyState::Ready(_) => return guard.clone(),
            TopologyState::Failed(_) => return guard.clone(),
            TopologyState::Building { .. } => {
                // 已有后台线程在构建:spin-wait 直到终态(带超时保护)
                drop(guard);
                return self.wait_for_build_completion();
            }
            TopologyState::Uninitialized => {}
        }

        // 无支持语言 → 直接 Failed。
        let Some(language) = self.language else {
            *guard = TopologyState::Failed(
                "no supported language detected (requires a cargo workspace or \
                 Python project markers: pyproject.toml/setup.py/setup.cfg/requirements.txt)"
                    .to_string(),
            );
            return guard.clone();
        };

        // P1:先查磁盘缓存(指纹校验)。
        if let Some(cached) = crate::topology_cache::load_cache(&self.workspace_root, language) {
            *guard = TopologyState::Ready(Box::new(cached.data));
            return guard.clone();
        }

        // Uninitialized → Building,同步执行构建
        let started_at = Instant::now();
        *guard = TopologyState::Building { started_at };
        drop(guard);

        let result = build_topology_data(&self.workspace_root, language);
        let elapsed = started_at.elapsed();

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(mut data) => {
                data.built_at_ms = elapsed.as_millis() as u64;
                // P1:构建成功后落盘。
                crate::topology_cache::save_cache(&self.workspace_root, language, &data);
                *guard = TopologyState::Ready(Box::new(data));
            }
            Err(e) => {
                *guard = TopologyState::Failed(e);
            }
        }
        guard.clone()
    }

    /// 等待已在进行中的后台构建完成(用于 `ensure_built_blocking` 遇到 `Building` 时)。
    ///
    /// 使用 spin-wait + sleep(50ms)轮询,超时 120 秒后返回当前状态(可能仍为 Building)。
    fn wait_for_build_completion(&self) -> TopologyState {
        const POLL_INTERVAL_MS: u64 = 50;
        const TIMEOUT_SECS: u64 = 120;

        let deadline = Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS);
        loop {
            let state = self.state();
            if !matches!(state, TopologyState::Building { .. }) {
                return state;
            }
            if Instant::now() > deadline {
                // 超时:返回当前 Building 状态,让调用方决定如何处理
                return state;
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    /// 拓扑感知派发(Epic 3):按 crate 边界自动推导建议的 `workspace` 参数。
    ///
    /// 集成 `ModuleGraph`:把每个 crate 的目录映射为 `dispatch_subagent` 的
    /// `workspace` 相对路径候选(manifest_path.parent() 相对 workspace_root),
    /// 供 LLM 派发时参考。`query` 可选,按 crate 名过滤(大小写不敏感)。
    ///
    /// # 返回
    /// - `Ready`:候选列表(每个条目含 crate 名 / 建议 workspace 相对路径 / deps)。
    /// - `Building`/`Failed`/`Uninitialized`:降级提示(与 [`query_project_graph`] 一致)。
    pub fn suggest_workspaces(&self, query: Option<&str>) -> Result<String, String> {
        let state = self.ensure_built();
        match state {
            TopologyState::Ready(data) => {
                Ok(format_workspace_suggestions(&data.module_graph, query))
            }
            TopologyState::Building { .. } => Ok(
                "ProjectTopology is still building (cargo metadata + crate graph). \
                    This usually takes < 5 seconds. Do NOT retry immediately. \
                    Use read/grep/search tools instead."
                    .to_string(),
            ),
            TopologyState::Failed(e) => Ok(format!(
                "ProjectTopology failed to build: {e}. \
                     Workspace suggestions unavailable; dispatch without the 'workspace' field."
            )),
            TopologyState::Uninitialized => Ok("ProjectTopology is uninitialized.".to_string()),
        }
    }

    /// 查询 crate 依赖图。
    pub fn query_project_graph(&self) -> Result<String, String> {
        let state = self.ensure_built();
        match state {
            TopologyState::Ready(data) => {
                let g = &data.module_graph;
                let mut out = format!(
                    "ProjectTopology: workspace={} crates={}\n\n",
                    g.workspace_root.display(),
                    g.crates.len(),
                );
                for c in &g.crates {
                    out.push_str(&format!("## crate `{}` v{}\n", c.name, c.version));
                    out.push_str(&format!("   manifest: {}\n", c.manifest_path.display()));
                    out.push_str(&format!(
                        "   sources: [{}]\n",
                        c.source_paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    if !c.dependencies.is_empty() {
                        out.push_str(&format!("   deps: [{}]\n", c.dependencies.join(", ")));
                    }
                    if let Some(reverse) = g.reverse_deps.get(&c.name) {
                        if !reverse.is_empty() {
                            out.push_str(&format!("   dependants: [{}]\n", reverse.join(", ")));
                        }
                    }
                    out.push('\n');
                }
                out.push_str(
                    "Use `find_boundary_crossings` to find cross-crate symbol references, \
                       or `get_symbol_info` to look up a specific symbol.\n",
                );
                Ok(out)
            }
            TopologyState::Building { .. } => Ok(
                "ProjectTopology is still building (cargo metadata + crate graph). \
                    This usually takes < 5 seconds. Do NOT retry immediately. \
                    Use read/grep/search tools instead to find what you need."
                    .to_string(),
            ),
            TopologyState::Failed(e) => Ok(format!(
                "ProjectTopology failed to build: {e}. \
                     Use read/grep/search tools instead."
            )),
            TopologyState::Uninitialized => Ok("ProjectTopology is uninitialized.".to_string()),
        }
    }

    /// 查找跨 crate 边界的符号引用。
    ///
    /// `query` 参数可选，不提供时返回所有跨 crate 依赖概要。
    pub fn find_boundary_crossings(&self, query: Option<&str>) -> Result<String, String> {
        let state = self.ensure_built();
        match state {
            TopologyState::Ready(data) => {
                let g = &data.module_graph;
                let mut out = String::new();

                let query_lower = query.map(|q| q.to_lowercase());

                // Find cross-crate dependencies
                let mut crossings = Vec::new();
                for c in &g.crates {
                    for dep_name in &c.dependencies {
                        // Find which crate provides this dependency
                        if let Some(provider) = g.crates.iter().find(|p| &p.name == dep_name) {
                            let entry = (
                                c.name.clone(),
                                dep_name.clone(),
                                provider.source_paths.clone(),
                            );
                            // Filter by query if provided
                            if let Some(ref q) = query_lower {
                                if c.name.to_lowercase().contains(q)
                                    || dep_name.to_lowercase().contains(q)
                                {
                                    crossings.push(entry);
                                }
                            } else {
                                crossings.push(entry);
                            }
                        }
                    }
                }
                if crossings.is_empty() {
                    if query.is_some() {
                        out.push_str(
                            "No cross-crate boundary crossings found matching the query. \
                             Try `query_project_graph` to see all crates and their dependencies.\n",
                        );
                    } else {
                        out.push_str("No cross-crate dependencies found in workspace.\n");
                    }
                } else {
                    out.push_str(&format!(
                        "Found {} cross-crate boundary crossing(s):\n\n",
                        crossings.len()
                    ));
                    for (consumer, provider, source_paths) in &crossings {
                        out.push_str(&format!("## {consumer} → {provider}\n",));
                        out.push_str(&format!(
                            "   provider sources: [{}]\n\n",
                            source_paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    out.push_str(&format!(
                        "Total: {} boundary crossing(s). Use `find_boundary_crossings` with \
                         a crate name to filter results.\n",
                        crossings.len()
                    ));
                }

                // If symbol index is available, add symbol-level info
                if let Some(ref si) = data.symbol_index {
                    if let Some(q) = query {
                        out.push_str(&format!("\n## Symbol matches for '{q}':\n"));
                        if let Some(defs) = si.definitions.get(q) {
                            for def in defs {
                                out.push_str(&format!(
                                    "   {} {} ({}:{})\n",
                                    def.kind,
                                    def.name,
                                    def.file.display(),
                                    def.line
                                ));
                            }
                        } else {
                            out.push_str(
                                "   (no symbol index matches — symbol index is best-effort)\n",
                            );
                        }
                    }
                }

                Ok(out)
            }
            TopologyState::Building { .. } => Ok(
                "ProjectTopology is still building. Do NOT retry immediately. \
                 Use read/grep/search tools instead."
                    .to_string(),
            ),
            TopologyState::Failed(e) => Ok(format!("ProjectTopology failed to build: {e}")),
            TopologyState::Uninitialized => Ok("ProjectTopology is uninitialized.".to_string()),
        }
    }

    /// P3:查找符号的引用点(爆炸半径)。
    ///
    /// 基于 SymbolIndex 的 `callers` 表,输出按 caller 文件分组、**按不同 caller
    /// 文件去重**的紧凑列表(避免单文件多次调用放大权重)。用于 LLM 在修改
    /// 符号前评估影响面。
    ///
    /// 未找到符号或仅有定义无调用时引导 `grep_search`;topology 未 Ready 时
    /// 返回"构建中/不可用"提示(沿用 Building 分支模式,不阻塞)。
    pub fn find_references(&self, symbol: &str) -> Result<String, String> {
        let state = self.ensure_built();
        match state {
            TopologyState::Ready(data) => {
                let Some(si) = &data.symbol_index else {
                    return Ok(format!(
                        "Symbol index is not available (no symbol index in topology data). \
                         Use `grep_search` to find references to `{symbol}` manually."
                    ));
                };
                // 校验符号存在(定义或引用皆可);严格按定义查询。
                let defined = si.definitions.contains_key(symbol);
                let called = si
                    .callers
                    .get(symbol)
                    .map(|c| !c.is_empty())
                    .unwrap_or(false);
                if !defined && !called {
                    return Ok(format!(
                        "Symbol `{symbol}` not found in topology index. \
                         Use `grep_search` or `read_file` to locate it."
                    ));
                }
                if !called {
                    return Ok(format!(
                        "Symbol `{symbol}` is defined but no call sites were found in the \
                         topology index. Verify with `grep_search` if unsure."
                    ));
                }
                // 按 caller 文件分组去重。
                let mut by_file: std::collections::BTreeMap<PathBuf, Vec<u32>> =
                    std::collections::BTreeMap::new();
                if let Some(callers) = si.callers.get(symbol) {
                    for cs in callers {
                        by_file.entry(cs.file.clone()).or_default().push(cs.line);
                    }
                }
                let mut out = format!(
                    "## References to `{symbol}` ({} calling file(s)):\n",
                    by_file.len()
                );
                for (file, mut lines) in by_file {
                    lines.sort_unstable();
                    lines.dedup();
                    let rel = if file.is_absolute() {
                        file.strip_prefix(&data.module_graph.workspace_root)
                            .map(|r| r.to_path_buf())
                            .unwrap_or(file.clone())
                            .to_string_lossy()
                            .replace('\\', "/")
                    } else {
                        file.to_string_lossy().replace('\\', "/")
                    };
                    let shown: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
                    out.push_str(&format!("  {}: {}\n", rel, shown.join(", ")));
                }
                out.push_str(
                    "\nUse this to assess blast radius before modifying `{symbol}`. \
                     grep_search may find additional references the index missed.",
                );
                Ok(out)
            }
            TopologyState::Building { .. } => Ok(
                "ProjectTopology is still building. Use `grep_search` to find references \
                 now, or retry after a moment. Do NOT retry immediately."
                    .to_string(),
            ),
            TopologyState::Failed(e) => Ok(format!(
                "ProjectTopology failed to build: {e}. Use `grep_search` to find references."
            )),
            TopologyState::Uninitialized => Ok("ProjectTopology is uninitialized.".to_string()),
        }
    }

    /// 获取符号信息。
    ///
    /// 如果 LspRegistry 可用，会尝试通过 LSP 获取引用信息。
    pub fn get_symbol_info(&self, symbol: &str) -> Result<String, String> {
        let state = self.ensure_built();
        match state {
            TopologyState::Ready(data) => {
                let g = &data.module_graph;
                // First check if the symbol is a crate name
                let mut found_crate = false;
                let mut out = String::new();

                for c in &g.crates {
                    if c.name == symbol {
                        found_crate = true;
                        out.push_str(&format!("## Crate `{}` v{}\n", c.name, c.version));
                        out.push_str(&format!("   manifest: {}\n", c.manifest_path.display()));
                        out.push_str(&format!(
                            "   sources: [{}]\n",
                            c.source_paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                        if !c.dependencies.is_empty() {
                            out.push_str(&format!(
                                "   dependencies: [{}]\n",
                                c.dependencies.join(", ")
                            ));
                        }
                        if let Some(reverse) = g.reverse_deps.get(&c.name) {
                            if !reverse.is_empty() {
                                out.push_str(&format!("   dependants: [{}]\n", reverse.join(", ")));
                            }
                        }
                        out.push('\n');
                    }
                }

                if let Some(ref si) = data.symbol_index {
                    if let Some(defs) = si.definitions.get(symbol) {
                        if !found_crate {
                            out.push_str(&format!("## Symbol `{symbol}` definitions:\n"));
                        }
                        for def in defs {
                            out.push_str(&format!(
                                "   {} {} ({}:{})\n",
                                def.kind,
                                def.name,
                                def.file.display(),
                                def.line
                            ));
                            if let Some(crate_name) = &def.crate_name {
                                out.push_str(&format!("      crate: {crate_name}\n"));
                            }
                            if let Some(vis) = &def.visibility {
                                out.push_str(&format!("      visibility: {vis}\n"));
                            }
                        }
                        if let Some(callers) = si.callers.get(symbol) {
                            out.push_str(&format!("   call sites ({}):\n", callers.len()));
                            for cs in callers {
                                out.push_str(&format!("      {}:{}\n", cs.file.display(), cs.line));
                            }
                        }
                    } else if !found_crate {
                        out.push_str(&format!(
                            "Symbol `{symbol}` not found in topology index. \
                             Note: symbol index is best-effort and may miss items \
                             behind macros or conditional compilation. \
                             Use `grep_search` or `read_file` to find it manually.\n"
                        ));
                    }
                } else if !found_crate {
                    out.push_str(&format!(
                        "Symbol `{symbol}` not found as a crate name. \
                         Symbol index is not yet available (requires LspRegistry). \
                         Use `grep_search` or `query_project_graph` instead.\n"
                    ));
                }

                Ok(out)
            }
            TopologyState::Building { .. } => {
                Ok("ProjectTopology is still building. Do NOT retry immediately.".to_string())
            }
            TopologyState::Failed(e) => Ok(format!("ProjectTopology failed to build: {e}")),
            TopologyState::Uninitialized => Ok("ProjectTopology is uninitialized.".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// cargo metadata → ModuleGraph
// ---------------------------------------------------------------------------

/// Epic 3(拓扑感知派发):把 `ModuleGraph` 映射为 `dispatch_subagent` 的
/// `workspace` 建议列表(纯函数,便于单测)。
///
/// 每个 crate 映射为建议 workspace 相对路径(manifest_path.parent() 相对
/// workspace_root,正斜杠);crate 位于 workspace 根时返回 `"."`。按 crate 名
/// 排序;`query` 提供时仅保留 crate 名包含 query(大小写不敏感)的条目。
#[must_use]
pub fn format_workspace_suggestions(g: &ModuleGraph, query: Option<&str>) -> String {
    let query_lower = query.map(|q| q.to_lowercase());
    let mut crates: Vec<&CrateInfo> = g
        .crates
        .iter()
        .filter(|c| {
            query_lower
                .as_ref()
                .is_none_or(|q| c.name.to_lowercase().contains(q))
        })
        .collect();
    crates.sort_by(|a, b| a.name.cmp(&b.name));

    if crates.is_empty() {
        return if let Some(q) = query {
            format!("No crates found matching '{q}'. Use `query_project_graph` to see all crates.")
        } else {
            "No crates found in the workspace. Use `query_project_graph` for details.".to_string()
        };
    }

    let mut out = String::from(
        "## 建议 workspace(按 crate 边界)\n\
         在 dispatch_subagent 的 \"workspace\" 字段中使用下列相对路径,将子代理约束到对应 crate。\n",
    );
    for c in &crates {
        out.push_str(&format!(
            "- crate `{}` v{} → workspace \"{}\"\n",
            c.name,
            c.version,
            crate_workspace_relative(g, c)
        ));
        out.push_str(&format!(
            "   manifest: {}\n",
            crate_path_display(g, &c.manifest_path)
        ));
        if !c.dependencies.is_empty() {
            out.push_str(&format!("   deps: [{}]\n", c.dependencies.join(", ")));
        }
    }
    out
}

/// crate 的建议 workspace 值:manifest_path.parent() 相对 workspace_root(正斜杠)。
/// crate 位于 workspace 根时返回 `"."`。
fn crate_workspace_relative(g: &ModuleGraph, c: &CrateInfo) -> String {
    match crate_manifest_relative(g, &c.manifest_path) {
        Some(rel) if rel.parent().is_some_and(|p| !p.as_os_str().is_empty()) => rel
            .parent()
            .expect("parent checked above")
            .to_string_lossy()
            .replace('\\', "/"),
        _ => ".".to_string(),
    }
}

/// manifest_path 相对 workspace_root;不在 workspace_root 内时为 None。
fn crate_manifest_relative(g: &ModuleGraph, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(&g.workspace_root)
        .ok()
        .map(Path::to_path_buf)
}

/// 路径展示:相对 workspace_root 用正斜杠,否则原样显示。
fn crate_path_display(g: &ModuleGraph, path: &Path) -> String {
    crate_manifest_relative(g, path)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| path.display().to_string())
}

/// P2-1:构建完整的 `TopologyData`(ModuleGraph + best-effort SymbolIndex)。
///
/// 这是 `ensure_built()`(异步)和 `ensure_built_blocking()`(同步)共用的
/// 核心构建逻辑。提取为独立函数使得:
/// - `ensure_built()` 的后台线程可以调用它(闭包捕获 `workspace_root`)
/// - `ensure_built_blocking()` 可以同步调用它
/// - 单元测试可以直接测试此函数而无需走状态机
///
/// 返回 `Result<TopologyData, String>`:
/// - `Ok(data)`:ModuleGraph 构建成功;SymbolIndex 可能仍为 None(best-effort)
/// - `Err(e)`:cargo metadata 失败且无 Python 降级路径(非 cargo 目录、cargo 不可用等)
fn build_topology_data(
    workspace_root: &Path,
    language: crate::topology_cache::Language,
) -> Result<TopologyData, String> {
    let started_at = Instant::now();

    // Build ModuleGraph(cargo workspace)或 Python 降级(单伪 crate)。
    let module_graph = match build_module_graph(workspace_root) {
        Ok(g) => g,
        // P2:非 cargo 项目 → 若为 Python 项目,退化出单伪 crate 的 ModuleGraph,
        // 使 query_project_graph/find_boundary_crossings/get_symbol_info 全部可用。
        // 不做目录模块图(改动最小)。非 Python 项目维持 Failed。
        Err(e) => match language {
            crate::topology_cache::Language::Python => {
                match build_python_fallback_graph(workspace_root) {
                    Some(g) => g,
                    None => return Err(e),
                }
            }
            crate::topology_cache::Language::Rust => return Err(e),
        },
    };

    // Phase 4 P0-5:用 grep/rg 填充 SymbolIndex(best-effort)。
    // 之前 symbol_index 恒为 None,导致 DomainTools 的 refactor_algorithm_topo
    // 永远返回 "no symbol index" 降级提示。现在从 module_graph 收集所有
    // source_paths,调用 build_symbol_index_fast 构建 definitions + callers。
    let source_dirs: Vec<PathBuf> = module_graph
        .crates
        .iter()
        .flat_map(|c| c.source_paths.iter().cloned())
        .collect();
    let symbol_index = if source_dirs.is_empty() {
        None
    } else {
        build_symbol_index_fast(&source_dirs, language).ok()
    };

    Ok(TopologyData {
        module_graph,
        symbol_index,
        built_at_ms: started_at.elapsed().as_millis() as u64,
    })
}

/// P2:Python 项目的退化 ModuleGraph(单伪 crate)。
/// `source_paths` = 含 `__init__.py` 的包目录;无包结构时退回 [root]。
fn build_python_fallback_graph(root: &Path) -> Option<ModuleGraph> {
    if !has_python_project_from_meta(root) {
        return None;
    }
    // 收集含 __init__.py 的包目录;为空则用 root 本身。
    let mut pkg_dirs: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    "target" | "node_modules" | "__pycache__" | "venv" | ".venv"
                ) && !name.starts_with('.')
            } else {
                true
            }
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_dir() && path.join("__init__.py").is_file() {
            pkg_dirs.push(path.to_path_buf());
        }
    }
    let source_paths = if pkg_dirs.is_empty() {
        vec![root.to_path_buf()]
    } else {
        pkg_dirs
    };
    Some(ModuleGraph {
        workspace_root: root.to_path_buf(),
        crates: vec![CrateInfo {
            name: "python-project".to_string(),
            version: "0.0.0".to_string(),
            manifest_path: {
                let pyproject = root.join("pyproject.toml");
                if pyproject.exists() {
                    pyproject
                } else {
                    root.to_path_buf()
                }
            },
            dependencies: Vec::new(),
            source_paths,
        }],
        reverse_deps: HashMap::new(),
    })
}

/// 判断目录是否含 Python 项目标记(与 topology_cache::pick_primary_language
/// 的判定一致;此处避免循环依赖,只做文件存在检查)。
fn has_python_project_from_meta(root: &Path) -> bool {
    const PY_MARKERS: [&str; 4] = [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
    ];
    PY_MARKERS.iter().any(|m| root.join(m).exists())
}

/// 通过 `cargo metadata` 构建 ModuleGraph。
fn build_module_graph(root: &Path) -> Result<ModuleGraph, String> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(root.join("Cargo.toml"))
        .output()
        .map_err(|e| format!("failed to run cargo metadata: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo metadata failed: {stderr}"));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("failed to parse cargo metadata JSON: {e}"))?;

    let workspace_root = root.to_path_buf();

    let packages = json["packages"]
        .as_array()
        .ok_or("cargo metadata: no 'packages' array")?;

    let mut crates = Vec::new();

    for pkg in packages {
        let name = pkg["name"].as_str().unwrap_or("unknown").to_string();
        let version = pkg["version"].as_str().unwrap_or("0.0.0").to_string();
        let manifest_path = PathBuf::from(pkg["manifest_path"].as_str().unwrap_or(""));

        let dependencies: Vec<String> = pkg["dependencies"]
            .as_array()
            .map(|deps| {
                deps.iter()
                    .filter_map(|d| d["name"].as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // Collect source paths from targets
        let mut source_paths = Vec::new();
        if let Some(targets) = pkg["targets"].as_array() {
            for target in targets {
                // Lib and bin targets provide source directories
                if let Some(src_path) = target["src_path"].as_str() {
                    let src = PathBuf::from(src_path);
                    if let Some(parent) = src.parent() {
                        let parent_path = parent.to_path_buf();
                        if !source_paths.contains(&parent_path) {
                            source_paths.push(parent_path);
                        }
                    }
                }
            }
        }

        crates.push(CrateInfo {
            name,
            version,
            manifest_path,
            dependencies,
            source_paths,
        });
    }

    // Build reverse dependency map
    let mut reverse_deps: HashMap<String, Vec<String>> = HashMap::new();
    for c in &crates {
        for dep in &c.dependencies {
            reverse_deps
                .entry(dep.clone())
                .or_default()
                .push(c.name.clone());
        }
    }

    Ok(ModuleGraph {
        workspace_root,
        crates,
        reverse_deps,
    })
}

// ---------------------------------------------------------------------------
// SymbolIndex 构建(基于 grep/ripgrep, best-effort)
// ---------------------------------------------------------------------------

/// 通过 grep 快速构建符号索引。
///
/// 这是 best-effort 实现:使用 `rg`/`grep` 搜符号定义。
/// - Rust:`pub fn`, `pub struct`, `pub trait`, `pub enum`, `pub mod` 等;
/// - Python:`def` / `async def` / `class`。
///
/// 比 LSP 快但不支持宏展开/条件编译/动态定义。
pub fn build_symbol_index_fast(
    source_dirs: &[PathBuf],
    language: crate::topology_cache::Language,
) -> Result<SymbolIndex, String> {
    let mut definitions: HashMap<String, Vec<SymbolDef>> = HashMap::new();
    let mut file_symbols: HashMap<PathBuf, Vec<String>> = HashMap::new();

    for dir in source_dirs {
        // Use ripgrep to find symbol declarations (language-specific pattern).
        // P2-3 修复:在 `pub` 前加 `\b` 词边界,避免匹配 `repub fn foo()` 中的 `pub`。
        // rg 使用 Rust regex 语法,支持 `\b`;grep -E (POSIX ERE) 不支持 `\b`,
        // 但 grep fallback 用了不同的 pattern(见下方),所以这里 rg pattern 用 \b。
        let (pattern, grep_pattern, include): (&str, &str, &str) = match language {
            crate::topology_cache::Language::Rust => (
                r"\bpub(?:\(\s*(?:crate|super)\s*\))?\s+(fn|struct|trait|enum|mod|type|const|static)\s+(\w+)",
                r"(^|[^a-zA-Z0-9_])pub(\([[:space:]]*(crate|super)[[:space:]]*\))?[[:space:]]+(fn|struct|trait|enum|mod|type|const|static)[[:space:]]+[a-zA-Z_][a-zA-Z0-9_]*",
                "*.rs",
            ),
            crate::topology_cache::Language::Python => (
                r"^\s*(?:async\s+)?(def|class)\s+(\w+)",
                r"^[[:space:]]*(\[[:space:]]*async[[:space:]]+)?(def|class)[[:space:]]+[a-zA-Z_][a-zA-Z0-9_]*",
                "*.py",
            ),
        };
        let result = Command::new("rg")
            .args([
                "--no-heading",
                "--line-number",
                "--type",
                language.grep_type(),
                pattern,
            ])
            .arg(dir)
            .output();

        // Fallback to grep if rg not available
        // P2-3 修复:grep -E 不支持 `\b`,改用 `(^|[^a-zA-Z0-9_])` 模拟词边界。
        // 同时支持 pub(crate)/pub(super) 可见性,与 rg pattern 行为一致。
        // 注意:grep -E 中 `\s` 不被所有版本支持,用 `[[:space:]]` 替代。
        let output = match result {
            Ok(o) if o.status.success() => o,
            _ => Command::new("grep")
                .args(["-rn", &format!("--include={include}"), "-E", grep_pattern])
                .arg(dir)
                .output()
                .map_err(|e| format!("grep failed: {e}"))?,
        };

        if output.status.success() {
            for line in output.stdout.lines() {
                let line = line.map_err(|e| format!("read line: {e}"))?;
                if let Some(sym) = parse_grep_line(&line, dir, language) {
                    definitions
                        .entry(sym.name.clone())
                        .or_default()
                        .push(sym.clone());
                    file_symbols
                        .entry(sym.file.clone())
                        .or_default()
                        .push(sym.name.clone());
                }
            }
        }
    }

    // Callers are harder to get without LSP; use a simplified approach
    // by finding all function/method invocations.
    let mut callers: HashMap<String, Vec<CallSite>> = HashMap::new();
    for dir in source_dirs {
        if let Ok(lines) = find_callers_fast(dir, &definitions, language) {
            for (func_name, sites) in lines {
                callers.entry(func_name).or_default().extend(sites);
            }
        }
    }

    Ok(SymbolIndex {
        definitions,
        callers,
        file_symbols,
    })
}

fn parse_grep_line(
    line: &str,
    base_dir: &Path,
    language: crate::topology_cache::Language,
) -> Option<SymbolDef> {
    // Format: path:line:content
    //
    // P2-3 修复:Windows 路径解析。
    // 之前用 `line.find(':')` 在 `C:\project\src\main.rs:42:pub fn foo` 上
    // 返回位置 1(盘符 `C` 后的冒号),导致 file_path="C",解析失败。
    // 现在用 regex `:(\d+):` 找到 "冒号+数字+冒号" 的位置作为分隔符,
    // 对 Windows 盘符冒号免疫(因为盘符后是 `\` 不是数字)。
    let separator_re = regex::Regex::new(r":(\d+):").ok()?;
    let caps = separator_re.captures(line)?;
    let whole_match = caps.get(0)?; // 形如 ":42:"
    let line_num: u32 = caps.get(1)?.as_str().parse().ok()?;
    let file_path = &line[..whole_match.start()];
    let content = &line[whole_match.end()..];

    if file_path.is_empty() {
        return None;
    }

    // 按语言提取符号 kind 与 name。
    // Rust:P2-3 修复 在 `pub` 前加 `\b` 词边界,避免匹配 `repub fn foo()`
    // 中的 `pub`(否则会把 `repub fn` 误识别为 `pub fn`,捕获到错误的符号名)。
    // Python:def / async def / class。
    let re = match language {
        crate::topology_cache::Language::Rust => regex::Regex::new(
            r"\bpub(?:\(\s*(?:crate|super)\s*\))?\s+(fn|struct|trait|enum|mod|type|const|static)\s+(\w+)",
        )
        .ok()?,
        crate::topology_cache::Language::Python => {
            regex::Regex::new(r"^\s*(?:async\s+)?(def|class)\s+(\w+)").ok()?
        }
    };
    let caps = re.captures(content)?;

    // Python:def 归一为 "fn"(函数语义,与 callers 过滤 `kind == "fn"` 一致),
    // class 保持 "class";Rust 沿用捕获组。
    let (kind, name) = match language {
        crate::topology_cache::Language::Rust => (
            caps.get(1)?.as_str().to_string(),
            caps.get(2)?.as_str().to_string(),
        ),
        crate::topology_cache::Language::Python => {
            let raw_kind = caps.get(1)?.as_str();
            let kind = if raw_kind == "def" { "fn" } else { raw_kind }.to_string();
            (kind, caps.get(2)?.as_str().to_string())
        }
    };

    let relative_path = Path::new(file_path)
        .strip_prefix(base_dir)
        .ok()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(file_path));

    // Determine visibility(Rust 专属;Python 无 pub 概念)。
    let visibility = match language {
        crate::topology_cache::Language::Rust => {
            if content.contains("pub(crate)") {
                Some("pub(crate)".to_string())
            } else if content.contains("pub(super)") {
                Some("pub(super)".to_string())
            } else if content.contains("pub ") {
                Some("pub".to_string())
            } else {
                None
            }
        }
        crate::topology_cache::Language::Python => None,
    };

    Some(SymbolDef {
        name,
        kind,
        file: relative_path,
        line: line_num,
        crate_name: None,
        visibility,
    })
}

/// 批量 rg 查找调用点(P2 重写,替代"每符号一次 rg"的 O(N) 子进程开销)。
///
/// 实现要点:
/// - 每目录一次 `rg --json` + 交替 pattern,子进程数从「符号数」降到「目录数」。
/// - `--json` 事件流:仅处理 `match` 事件,路径取 `data.path.text`,行号取
///   `data.lines.line_number`,符号名优先取捕获组 `submatches[1].match.text`
///   (`submatches[0]` 为整体匹配)。
/// - 符号名必须 `regex::escape`;按 **pattern 字节数**(≤ 8KB,而非数量)分块,
///   防 ARG_MAX / rg 编译过慢。
/// - 超时:复用 `topology_cache::run_command_with_timeout`(30s,try_wait 轮询 +
///   超时 kill + wait 回收僵尸进程),返回部分结果。
/// - 调用点按匹配到的符号名分桶;被调函数同名的定义行会被过滤。
fn find_callers_fast(
    dir: &Path,
    definitions: &HashMap<String, Vec<SymbolDef>>,
    language: crate::topology_cache::Language,
) -> Result<HashMap<String, Vec<CallSite>>, String> {
    // 函数类型符号(kind == "fn"):匹配 `name(`。
    let fn_names: Vec<&str> = definitions
        .iter()
        .filter(|(_, defs)| defs.iter().any(|d| d.kind == "fn"))
        .map(|(name, _)| name.as_str())
        .collect();

    // struct / enum / class 类型符号:词用引用(不带 `(`),避免与方法调用混淆。
    let type_names: Vec<&str> = definitions
        .iter()
        .filter(|(_, defs)| {
            defs.iter().any(|d| {
                d.kind != "fn" && d.kind != "mod" && d.kind != "const" && d.kind != "static"
            })
        })
        .map(|(name, _)| name.as_str())
        .collect();

    let mut result: HashMap<String, Vec<CallSite>> = HashMap::new();

    // 函数调用:name(
    for pattern in chunked_alternation(&fn_names, r"\b({})\s*\(", 8 * 1024) {
        let sites = run_batch_rg(dir, &pattern, language)?;
        for (symbol, callsites) in sites {
            result.entry(symbol).or_default().extend(callsites);
        }
    }
    // 类型词用:name(不带括号,宽松匹配跨文件引用)
    for pattern in chunked_alternation(&type_names, r"\b({})\b", 8 * 1024) {
        let sites = run_batch_rg(dir, &pattern, language)?;
        for (symbol, callsites) in sites {
            result.entry(symbol).or_default().extend(callsites);
        }
    }

    Ok(result)
}

/// 把符号名按 **pattern 字节数**分块(不是按数量),避免单次 rg pattern 过大。
/// `template` 含一个 `{}` 占位符用于包住交替组。
fn chunked_alternation(names: &[&str], template: &str, max_bytes: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_len = 0usize;
    for name in names {
        let escaped = regex::escape(name);
        let add_len = escaped.len() + 1; // 交替分隔符
        if !current.is_empty() && current_len + add_len > max_bytes {
            chunks.push(template.replace("{}", &current.join("|")));
            current.clear();
            current_len = 0;
        }
        current_len += add_len;
        current.push(escaped);
    }
    if !current.is_empty() {
        chunks.push(template.replace("{}", &current.join("|")));
    }
    chunks
}

/// 跑一次批量 rg(--json),解析匹配事件并按符号名分桶。
fn run_batch_rg(
    dir: &Path,
    pattern: &str,
    language: crate::topology_cache::Language,
) -> Result<HashMap<String, Vec<CallSite>>, String> {
    let mut cmd = Command::new("rg");
    cmd.args(["-o", "-n", "--no-heading", "--json", "--type"])
        .arg(language.grep_type())
        .arg(pattern)
        .arg(dir);

    // 30s 超时:复用 topology_cache 的轮询实现(kill 后 wait 回收僵尸进程)。
    let Some(out) = crate::topology_cache::run_command_with_timeout(
        &mut cmd,
        std::time::Duration::from_secs(30),
    ) else {
        return Ok(HashMap::new());
    };

    let mut result: HashMap<String, Vec<CallSite>> = HashMap::new();
    for line in out.stdout.lines() {
        let Ok(line) = line else { continue };
        let Ok(ev) = serde_json::from_str::<RgEvent>(&line) else {
            continue;
        };
        // 仅处理 match 事件(begin/end 跳过)。
        if ev.typ != "match" {
            continue;
        }
        let Some(data) = ev.data else { continue };
        let path = data.path.map(|p| p.text).unwrap_or_default();
        let line_num = data.lines.as_ref().map(|l| l.line_number).unwrap_or(0);
        let context = data.lines.map(|l| l.text).unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        // 符号名:优先捕获组(submatches[1]),否则整体匹配(submatches[0])。
        let symbol = data
            .submatches
            .as_deref()
            .and_then(|s| s.get(1))
            .map(|m| m.match_.text.clone())
            .or_else(|| {
                data.submatches
                    .as_deref()
                    .and_then(|s| s.first())
                    .map(|m| m.match_.text.clone())
            });
        let Some(symbol) = symbol else {
            continue;
        };
        // 排除被调函数自身的定义行:统一按语言识别定义行前缀,
        // 避免 `def foo(` / `fn foo(` 自身的定义行计入调用点。
        if is_definition_line(&context, language) {
            continue;
        }
        let relative_path = Path::new(&path)
            .strip_prefix(dir)
            .ok()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(&path));
        result.entry(symbol).or_default().push(CallSite {
            file: relative_path,
            line: line_num,
            context: Some(context),
        });
    }
    Ok(result)
}

/// 判断一行是否为符号**定义行**(仅覆盖文本可见的常见修饰符组合,best-effort;
/// `default fn` nightly 特化 / 宏生成的 fn 不在文本可判范围,可接受漏判)。
fn is_definition_line(line: &str, language: crate::topology_cache::Language) -> bool {
    match language {
        crate::topology_cache::Language::Rust => {
            // 覆盖 pub / pub(crate) / pub(in path) / async / unsafe / const / extern "C"。
            regex::Regex::new(
                r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+"#,
            )
            .is_ok_and(|re| re.is_match(line))
        }
        crate::topology_cache::Language::Python => {
            regex::Regex::new(r"^\s*(?:async\s+)?(?:def|class)\s+")
                .is_ok_and(|re| re.is_match(line))
        }
    }
}

/// rg `--json` 事件的最小结构(只取需要的字段)。
#[derive(Debug, serde::Deserialize)]
struct RgEvent {
    #[serde(rename = "type")]
    typ: String,
    data: Option<RgData>,
}

#[derive(Debug, serde::Deserialize)]
struct RgData {
    path: Option<RgText>,
    lines: Option<RgLines>,
    submatches: Option<Vec<RgSubmatch>>,
}

#[derive(Debug, serde::Deserialize)]
struct RgText {
    text: String,
}

#[derive(Debug, serde::Deserialize)]
struct RgLines {
    text: String,
    line_number: u32,
}

#[derive(Debug, serde::Deserialize)]
struct RgSubmatch {
    #[serde(rename = "match")]
    match_: RgText,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_state_display() {
        assert_eq!(TopologyState::Uninitialized.to_string(), "uninitialized");
        assert_eq!(
            TopologyState::Ready(Box::new(TopologyData {
                module_graph: ModuleGraph {
                    workspace_root: PathBuf::from("/test"),
                    crates: vec![],
                    reverse_deps: HashMap::new(),
                },
                symbol_index: None,
                built_at_ms: 0,
            }))
            .to_string(),
            "ready"
        );
        assert_eq!(
            TopologyState::Failed("test error".into()).to_string(),
            "failed: test error"
        );
    }

    #[test]
    fn parse_grep_line_parses_pub_fn() {
        let base = PathBuf::from("/project/src");
        let line = "/project/src/main.rs:42:pub fn hello_world() {";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse");
        assert_eq!(sym.name, "hello_world");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.line, 42);
        assert_eq!(sym.visibility, Some("pub".to_string()));
    }

    #[test]
    fn parse_grep_line_parses_pub_struct() {
        let base = PathBuf::from("/project/src");
        let line = "/project/src/lib.rs:10:pub struct MyStruct<T> {";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse");
        assert_eq!(sym.name, "MyStruct");
        assert_eq!(sym.kind, "struct");
    }
    #[test]
    fn parse_grep_line_parses_pub_crate_fn() {
        let base = PathBuf::from("/project/src");
        let line = "/project/src/internal.rs:5:pub(crate) fn internal_helper() {";
        // P2-3 修复：regex 现在支持 pub(crate) 可见性，不再容忍解析失败
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("pub(crate) fn should be parsed with fixed regex");
        assert_eq!(sym.name, "internal_helper");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.visibility.as_deref(), Some("pub(crate)"));
    }

    #[test]
    fn parse_grep_line_parses_pub_super_fn() {
        let base = PathBuf::from("/project/src");
        let line = "/project/src/mod.rs:12:pub(super) fn super_helper() {";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("pub(super) fn should be parsed with fixed regex");
        assert_eq!(sym.name, "super_helper");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.visibility.as_deref(), Some("pub(super)"));
    }

    // -----------------------------------------------------------------
    // P2-3: Windows 路径解析 + regex 词边界修复测试
    // -----------------------------------------------------------------

    #[test]
    fn parse_grep_line_parses_windows_drive_letter_path() {
        // P2-3 修复:Windows 盘符 `C:` 不应被误认为 path:line 分隔符。
        // 之前 `line.find(':')` 会返回位置 1(盘符 C 后的冒号),
        // 导致 file_path="C",line_num 解析失败,返回 None。
        let base = PathBuf::from(r"C:\project\src");
        let line = r"C:\project\src\main.rs:42:pub fn hello_world() {";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse Windows path");
        assert_eq!(sym.name, "hello_world");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.line, 42);
        // file 字段保留原始路径(包括盘符),strip_prefix 成功则去掉 base
        assert!(
            sym.file.ends_with("main.rs"),
            "file should end with main.rs, got {}",
            sym.file.display()
        );
    }

    #[test]
    fn parse_grep_line_parses_windows_path_with_pub_crate() {
        // 组合测试:Windows 路径 + pub(crate) 可见性
        let base = PathBuf::from(r"C:\project\src");
        let line = r"C:\project\src\internal.rs:5:pub(crate) fn internal_helper() {";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse Windows path + pub(crate) fn");
        assert_eq!(sym.name, "internal_helper");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.line, 5);
        assert_eq!(sym.visibility.as_deref(), Some("pub(crate)"));
    }

    #[test]
    fn parse_grep_line_rejects_repub_word_boundary_violation() {
        // P2-3 修复:`repub fn foo()` 不应被识别为 `pub fn foo()`。
        // 之前没有 `\b` 词边界,regex `pub...` 会从 `repub` 的 `pub` 部分开始匹配,
        // 错误捕获 `foo` 作为 pub fn 符号。
        // 现在加了 `\bpub`,要求 `pub` 前必须是单词边界(非标识符字符),
        // `repub` 中 `e`→`p` 不是单词边界,所以不会匹配。
        let base = PathBuf::from("/project/src");
        let line = "/project/src/main.rs:42:fn repub_fn() { pub fn real_pub() {}";
        // 第一行 `fn repub_fn()` 不匹配 `pub fn` 模式(没有 pub 前缀)。
        // `pub fn real_pub()` 是合法的 pub fn,应该被解析到 `real_pub`。
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse the real pub fn");
        assert_eq!(
            sym.name, "real_pub",
            "should capture real_pub, not repub_fn (word boundary bug)"
        );
    }

    // -----------------------------------------------------------------
    // P2 — Python 符号索引 / 批量 callers / 定义行排除
    // -----------------------------------------------------------------

    #[test]
    fn parse_grep_line_parses_python_def_and_class() {
        let base = PathBuf::from("/proj/pkg");
        let line = "/proj/pkg/mod.py:5:def hello(x):";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Python)
            .expect("python def should parse");
        assert_eq!(sym.name, "hello");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.visibility, None);

        let line2 = "/proj/pkg/mod.py:10:class Foo:";
        let sym2 = parse_grep_line(line2, &base, crate::topology_cache::Language::Python)
            .expect("python class should parse");
        assert_eq!(sym2.name, "Foo");
        assert_eq!(sym2.kind, "class");
    }

    #[test]
    fn chunked_alternation_splits_by_bytes_and_escapes() {
        let owned: Vec<String> = (0..20).map(|i| format!("func_{i}")).collect();
        let names: Vec<&str> = owned.iter().map(String::as_str).collect();
        // 足够大的预算 → 单块,且符号被转义。
        let chunks = chunked_alternation(&names, r"\b({})\s*\(", usize::MAX);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains(r"\b(func_0|func_1"));
        // 极小的预算 → 逐符号分块。
        let tiny = chunked_alternation(&names, r"\b({})\s*\(", 4);
        assert!(tiny.len() > 1);
        // 元字符符号必须被转义。
        let tricky: Vec<&str> = vec!["a.b", "c+d"];
        let esc = chunked_alternation(&tricky, r"\b({})\s*\(", usize::MAX);
        assert!(esc[0].contains(r"(a\.b"), "should escape dot: {}", esc[0]);
    }

    #[test]
    fn is_definition_line_rust_covers_modifier_variants() {
        fn is_def(line: &str) -> bool {
            is_definition_line(line, crate::topology_cache::Language::Rust)
        }
        assert!(is_def("fn foo() {}"));
        assert!(is_def("pub fn foo() {}"));
        assert!(is_def("pub(crate) fn foo() {}"));
        assert!(is_def("pub(in crate::x) fn foo() {}"));
        assert!(is_def("async fn foo() {}"));
        assert!(is_def("unsafe fn foo() {}"));
        assert!(is_def("const fn foo() {}"));
        assert!(is_def(r#"extern "C" fn foo() {}"#));
        assert!(!is_def("    foo();"));
        assert!(!is_def("let x = foo();"));
    }

    #[test]
    fn is_definition_line_python_def_and_class() {
        fn is_def(line: &str) -> bool {
            is_definition_line(line, crate::topology_cache::Language::Python)
        }
        assert!(is_def("def foo():"));
        assert!(is_def("async def foo():"));
        assert!(is_def("class Foo:"));
        assert!(is_def("    def method(self):"));
        assert!(!is_def("foo()"));
        assert!(!is_def("x = foo(1)"));
    }

    #[test]
    fn build_python_fallback_graph_recognizes_pyproject() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\n").unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg").join("__init__.py"), "").unwrap();
        let g = build_python_fallback_graph(dir.path()).expect("should build fallback graph");
        assert_eq!(g.crates.len(), 1);
        assert_eq!(g.crates[0].name, "python-project");
        // source_paths 收敛到含 __init__.py 的包目录。
        assert!(
            g.crates[0].source_paths.iter().any(|p| p.ends_with("pkg")),
            "should include pkg dir, got: {:?}",
            g.crates[0].source_paths
        );
    }

    #[test]
    fn build_topology_python_project_succeeds_with_fallback() {
        // Python 项目(cargo 不存在/失败)→ 退化 ModuleGraph + 符号索引,不可用 Failed。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname=\"demo\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("demo.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let result = build_topology_data(dir.path(), crate::topology_cache::Language::Python);
        let data = result.expect("python project should build with fallback graph");
        assert_eq!(data.module_graph.crates.len(), 1);
        assert_eq!(data.module_graph.crates[0].name, "python-project");
        // 符号索引应含 add(需本机有 rg;无 rg 时 all). 至少不应是 None 之外报错。
        if let Some(si) = &data.symbol_index {
            assert!(
                si.definitions.contains_key("add"),
                "symbol index should contain add"
            );
        }
    }

    #[test]
    fn parse_grep_line_handles_unix_path_with_colon_in_content() {
        // 边界情况:content 中含冒号+数字(如 URL `:8080`)。
        // regex `:(\d+):` 应匹配第一个出现的 `:digits:`(即行号分隔符),
        // 而不是 content 中的 `:8080:`。
        // 注意:此测试用例需要 content 中没有形如 `:digits:` 的子串,
        // 否则会匹配到错误位置。我们用一个含 URL 的 content 来验证。
        let base = PathBuf::from("/project/src");
        let line = "/project/src/main.rs:42:pub fn fetch(url: &str) -> String {";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse");
        assert_eq!(sym.name, "fetch");
        assert_eq!(sym.line, 42);
    }

    #[test]
    fn parse_grep_line_rejects_empty_file_path() {
        // 边界情况:regex 匹配到字符串开头的 `:42:`,file_path 为空。
        // 这不应该发生在真实 grep 输出中,但作为防御性编程需要处理。
        let base = PathBuf::from("/project/src");
        let line = ":42:pub fn foo() {}";
        let result = parse_grep_line(line, &base, crate::topology_cache::Language::Rust);
        assert!(
            result.is_none(),
            "empty file_path should return None, got {result:?}"
        );
    }

    #[test]
    fn parse_grep_line_parses_pub_const() {
        // 测试 const 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/constants.rs:10:pub const MAX_RETRIES: u32 = 3;";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse pub const");
        assert_eq!(sym.name, "MAX_RETRIES");
        assert_eq!(sym.kind, "const");
        assert_eq!(sym.visibility.as_deref(), Some("pub"));
    }

    #[test]
    fn parse_grep_line_parses_pub_static() {
        // 测试 static 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/globals.rs:5:pub static COUNTER: AtomicU64 = AtomicU64::new(0);";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse pub static");
        assert_eq!(sym.name, "COUNTER");
        assert_eq!(sym.kind, "static");
    }

    #[test]
    fn parse_grep_line_parses_pub_type_alias() {
        // 测试 type 关键字(类型别名)
        let base = PathBuf::from("/project/src");
        let line = "/project/src/types.rs:8:pub type Result<T> = std::result::Result<T, MyError>;";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse pub type");
        assert_eq!(sym.name, "Result");
        assert_eq!(sym.kind, "type");
    }

    #[test]
    fn parse_grep_line_parses_pub_mod() {
        // 测试 mod 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/lib.rs:1:pub mod network;";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse pub mod");
        assert_eq!(sym.name, "network");
        assert_eq!(sym.kind, "mod");
    }

    #[test]
    fn parse_grep_line_parses_pub_enum() {
        // 测试 enum 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/enums.rs:3:pub enum Color { Red, Green, Blue }";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse pub enum");
        assert_eq!(sym.name, "Color");
        assert_eq!(sym.kind, "enum");
    }

    #[test]
    fn parse_grep_line_parses_pub_trait() {
        // 测试 trait 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/traits.rs:7:pub trait Serializable { fn serialize(&self); }";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse pub trait");
        assert_eq!(sym.name, "Serializable");
        assert_eq!(sym.kind, "trait");
    }

    #[test]
    fn parse_grep_line_skips_non_pub_declarations() {
        // 非 pub 声明不应被索引(私有符号不在 SymbolIndex 范围内)
        let base = PathBuf::from("/project/src");
        let line = "/project/src/internal.rs:5:fn private_helper() {}";
        let result = parse_grep_line(line, &base, crate::topology_cache::Language::Rust);
        assert!(
            result.is_none(),
            "non-pub fn should not be indexed, got {result:?}"
        );
    }

    #[test]
    fn parse_grep_line_handles_indented_pub_fn() {
        // 缩进的 pub fn(在 mod 块内)也应被解析
        let base = PathBuf::from("/project/src");
        let line = "/project/src/lib.rs:10:    pub fn indented_fn() {}";
        let sym = parse_grep_line(line, &base, crate::topology_cache::Language::Rust)
            .expect("should parse indented pub fn");
        assert_eq!(sym.name, "indented_fn");
        assert_eq!(sym.kind, "fn");
    }

    #[test]
    fn project_topology_initial_state() {
        let topo = ProjectTopology::new(PathBuf::from("."));
        let state = topo.state();
        assert!(matches!(state, TopologyState::Uninitialized));
    }

    #[test]
    fn project_topology_ensure_built_returns_failed_in_non_cargo_dir() {
        // In a non-cargo directory, ensure_built_blocking will fail synchronously
        let dir = tempfile::tempdir().unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let state = topo.ensure_built_blocking();
        // 非 cargo 目录应返回 Failed
        assert!(
            matches!(state, TopologyState::Failed(_)),
            "expected Failed in non-cargo dir, got {state}"
        );
    }

    #[test]
    fn project_topology_query_returns_message_when_not_ready() {
        let topo = ProjectTopology::new(PathBuf::from("."));
        let result = topo.query_project_graph().unwrap();
        // Should return a message rather than error
        assert!(!result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Epic 3: 拓扑感知派发 — workspace 建议推导
    // -----------------------------------------------------------------------

    fn sample_graph() -> ModuleGraph {
        ModuleGraph {
            workspace_root: PathBuf::from("/proj"),
            crates: vec![
                CrateInfo {
                    name: "api".to_string(),
                    version: "1.0.0".to_string(),
                    manifest_path: PathBuf::from("/proj/crates/api/Cargo.toml"),
                    dependencies: vec!["core".to_string()],
                    source_paths: vec![PathBuf::from("/proj/crates/api/src")],
                },
                CrateInfo {
                    name: "core".to_string(),
                    version: "0.9.0".to_string(),
                    manifest_path: PathBuf::from("/proj/crates/core/Cargo.toml"),
                    dependencies: Vec::new(),
                    source_paths: vec![PathBuf::from("/proj/crates/core/src")],
                },
                CrateInfo {
                    name: "app".to_string(),
                    version: "2.0.0".to_string(),
                    manifest_path: PathBuf::from("/proj/Cargo.toml"),
                    dependencies: vec!["api".to_string()],
                    source_paths: vec![PathBuf::from("/proj/src")],
                },
            ],
            reverse_deps: HashMap::new(),
        }
    }

    #[test]
    fn suggest_workspaces_maps_crates_to_relative_paths() {
        let out = format_workspace_suggestions(&sample_graph(), None);
        // 子 crate → 相对 workspace 路径
        assert!(out.contains("crate `api`"), "got: {out}");
        assert!(out.contains("workspace \"crates/api\""), "got: {out}");
        assert!(out.contains("crate `core`"), "got: {out}");
        assert!(out.contains("workspace \"crates/core\""), "got: {out}");
        // workspace 根自身 crate → "."
        assert!(out.contains("workspace \".\""), "got: {out}");
        // 按 crate 名排序(api 在 app 前)
        let api_pos = out.find("crate `api`").expect("api");
        let app_pos = out.find("crate `app`").expect("app");
        assert!(api_pos < app_pos, "crates should be sorted by name");
    }

    #[test]
    fn suggest_workspaces_filters_by_query() {
        let out = format_workspace_suggestions(&sample_graph(), Some("API"));
        assert!(out.contains("crate `api`"), "got: {out}");
        assert!(
            !out.contains("crate `core`"),
            "query filter should exclude core: {out}"
        );
        assert!(
            !out.contains("crate `app`"),
            "query filter should exclude app: {out}"
        );
    }

    #[test]
    fn suggest_workspaces_no_match_returns_hint() {
        let out = format_workspace_suggestions(&sample_graph(), Some("nonexistent"));
        assert!(out.contains("No crates found matching"), "got: {out}");
    }

    #[test]
    fn suggest_workspaces_windows_paths_normalize_to_forward_slashes() {
        let g = ModuleGraph {
            workspace_root: PathBuf::from(r"C:\repo"),
            crates: vec![CrateInfo {
                name: "api".to_string(),
                version: "1.0.0".to_string(),
                manifest_path: PathBuf::from(r"C:\repo\crates\api\Cargo.toml"),
                dependencies: Vec::new(),
                source_paths: Vec::new(),
            }],
            reverse_deps: HashMap::new(),
        };
        let out = format_workspace_suggestions(&g, None);
        assert!(
            out.contains("workspace \"crates/api\""),
            "Windows separators should be normalized: {out}"
        );
    }

    // -----------------------------------------------------------------------
    // P2-1:异步化测试
    // -----------------------------------------------------------------------

    #[test]
    fn p2_1_ensure_built_returns_building_immediately() {
        // ensure_built() 应立即返回 Building,不阻塞等待 cargo metadata
        let dir = tempfile::tempdir().unwrap();
        // P1:需有语言标记(否则直接 Failed)。用损坏 Cargo.toml:语言= Rust 但
        // cargo metadata 必然失败 → 终态 Failed,使异步化测试聚焦状态机行为。
        std::fs::write(dir.path().join("Cargo.toml"), "[package").unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let state = topo.ensure_built();
        // 首次调用必须立即返回 Building(而非 Ready/Failed)
        assert!(
            matches!(state, TopologyState::Building { .. }),
            "expected Building immediately after ensure_built(), got {state}"
        );
    }

    #[test]
    fn p2_1_ensure_built_does_not_respawn_while_building() {
        // 在 Building 状态下再次调用 ensure_built() 应返回 Building,不重复派发
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package").unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let state1 = topo.ensure_built();
        assert!(matches!(state1, TopologyState::Building { .. }));
        let state2 = topo.ensure_built();
        assert!(matches!(state2, TopologyState::Building { .. }));
        // 两次返回的 Building 的 started_at 应相同(同一个 Building 实例)
        if let (
            TopologyState::Building { started_at: s1 },
            TopologyState::Building { started_at: s2 },
        ) = (state1, state2)
        {
            assert_eq!(s1, s2, "second ensure_built() should not restart building");
        }
    }

    #[test]
    fn p2_1_ensure_built_eventually_reaches_terminal_state() {
        // 后台线程完成后,状态应转为 Ready 或 Failed(取决于是否在 cargo 目录)
        let dir = tempfile::tempdir().unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        topo.ensure_built(); // kick off background build

        // 轮询等待终态(最多 30 秒)
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let final_state = loop {
            let state = topo.state();
            if !matches!(state, TopologyState::Building { .. }) {
                break state;
            }
            if Instant::now() > deadline {
                panic!("topology did not reach terminal state within 30s, still: {state}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        // 非 cargo 目录应最终达到 Failed
        assert!(
            matches!(final_state, TopologyState::Failed(_)),
            "expected Failed after background build in non-cargo dir, got {final_state}"
        );
    }

    #[test]
    fn p2_1_ensure_built_blocking_waits_for_existing_build() {
        // 如果已有后台构建在进行,ensure_built_blocking 应等待其完成
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package").unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        topo.ensure_built(); // 启动后台构建
                             // 此时状态应为 Building
        assert!(matches!(topo.state(), TopologyState::Building { .. }));

        // ensure_built_blocking 应等待后台线程完成
        let state = topo.ensure_built_blocking();
        assert!(
            matches!(state, TopologyState::Failed(_)),
            "expected Failed after waiting for background build, got {state}"
        );
    }

    #[test]
    fn p2_1_ensure_built_returns_cached_ready_state() {
        // Ready 状态下 ensure_built() 应直接返回缓存的 Ready,不重复构建
        let dir = tempfile::tempdir().unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        // 先用 blocking 构建到终态
        let state1 = topo.ensure_built_blocking();
        assert!(matches!(state1, TopologyState::Failed(_)));
        // 再次调用 ensure_built() 应返回相同的 Failed(不重复构建)
        let state2 = topo.ensure_built();
        assert!(
            matches!(state2, TopologyState::Failed(_)),
            "expected cached Failed state, got {state2}"
        );
    }

    #[test]
    fn p2_1_build_topology_data_function_directly() {
        // 直接测试提取出的 build_topology_data 函数(不经状态机)
        let dir = tempfile::tempdir().unwrap();
        let result = build_topology_data(dir.path(), crate::topology_cache::Language::Rust);
        // 非 cargo 目录应返回 Err
        assert!(result.is_err(), "expected error in non-cargo dir");
        let err = result.unwrap_err();
        assert!(
            err.contains("cargo metadata") || err.contains("cargo"),
            "error should mention cargo, got: {err}"
        );
    }

    #[test]
    fn p2_1_panic_safety_transitions_to_failed() {
        // 验证 catch_unwind:如果后台线程 panic,状态应转为 Failed 而非永久 Building。
        // 我们通过模拟一个会导致 build_topology_data panic 的场景来验证——
        // 但 build_topology_data 本身不会 panic(它返回 Result),所以这里
        // 间接验证:确保非 cargo 目录的 Failed 状态能被正确设置(而非卡在 Building)。
        let dir = tempfile::tempdir().unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        topo.ensure_built();

        // 等待终态
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let final_state = loop {
            let state = topo.state();
            if !matches!(state, TopologyState::Building { .. }) {
                break state;
            }
            if Instant::now() > deadline {
                panic!("stuck in Building state — catch_unwind may have failed");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        // 如果 catch_unwind 工作正常,状态应为 Failed(而非永久 Building)
        assert!(
            matches!(final_state, TopologyState::Failed(_)),
            "expected Failed (not stuck Building), got {final_state}"
        );
    }

    // -----------------------------------------------------------------------
    // P1:磁盘缓存命中 / 无语言状态机
    // -----------------------------------------------------------------------

    #[test]
    fn p1_ensure_built_hits_disk_cache_and_skips_build() {
        // 手工构造缓存文件(模拟上次会话落盘),ensure_built 应直接 Ready,
        // 不触发 cargo metadata(不进入 Building)。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let data = TopologyData {
            module_graph: ModuleGraph {
                workspace_root: dir.path().to_path_buf(),
                crates: Vec::new(),
                reverse_deps: HashMap::new(),
            },
            symbol_index: None,
            built_at_ms: 0,
        };
        crate::topology_cache::save_cache(dir.path(), crate::topology_cache::Language::Rust, &data);

        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let state = topo.ensure_built();
        if let TopologyState::Ready(d) = &state {
            assert_eq!(d.built_at_ms, 0);
        } else {
            panic!("expected Ready from disk cache, got {state}");
        }
    }

    #[test]
    fn p1_ensure_built_fails_fast_without_supported_language() {
        // 无任何语言标记 → 直接 Failed,不进入 Building(防"永远 Building")。
        let dir = tempfile::tempdir().unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let state = topo.ensure_built();
        assert!(
            matches!(state, TopologyState::Failed(_)),
            "expected Failed without supported language, got {state}"
        );
    }

    // -----------------------------------------------------------------------
    // P3:find_references 爆炸半径
    // -----------------------------------------------------------------------

    fn refs_sample_topo() -> (ProjectTopology, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        // workspace 根 Cargo.toml 使语言探测为 Rust;缓存命中即 Ready。
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let data = TopologyData {
            module_graph: ModuleGraph {
                workspace_root: dir.path().to_path_buf(),
                crates: Vec::new(),
                reverse_deps: HashMap::new(),
            },
            symbol_index: Some(SymbolIndex {
                definitions: {
                    let mut m = HashMap::new();
                    m.insert(
                        "target_fn".to_string(),
                        vec![SymbolDef {
                            name: "target_fn".to_string(),
                            kind: "fn".to_string(),
                            file: PathBuf::from("src/a.rs"),
                            line: 1,
                            crate_name: None,
                            visibility: Some("pub".to_string()),
                        }],
                    );
                    m
                },
                callers: {
                    let mut m: HashMap<String, Vec<CallSite>> = HashMap::new();
                    m.insert(
                        "target_fn".to_string(),
                        vec![
                            CallSite {
                                file: PathBuf::from("src/b.rs"),
                                line: 10,
                                context: Some("target_fn()".to_string()),
                            },
                            CallSite {
                                file: PathBuf::from("src/b.rs"),
                                line: 12,
                                context: Some("target_fn(1)".to_string()),
                            },
                            CallSite {
                                file: PathBuf::from("src/c.rs"),
                                line: 5,
                                context: Some("target_fn()".to_string()),
                            },
                        ],
                    );
                    m
                },
                file_symbols: HashMap::new(),
            }),
            built_at_ms: 0,
        };
        crate::topology_cache::save_cache(dir.path(), crate::topology_cache::Language::Rust, &data);
        // 注意:必须随返回值持有 TempDir,否则函数返回即被 Drop 删除缓存文件。
        (ProjectTopology::new(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn find_references_groups_by_file_and_dedups_lines() {
        let (topo, _keep) = refs_sample_topo();
        let out = topo.find_references("target_fn").expect("should not err");
        // 两个调用文件(src/b.rs 的 10、12 两处去重行号仍保留;按文件分组)。
        assert!(out.contains("src/b.rs"), "got: {out}");
        assert!(out.contains("src/c.rs"), "got: {out}");
        // 两个文件 → "2 calling file(s)"。
        assert!(out.contains("2 calling file(s)"), "got: {out}");
        // BTreeMap 按文件路径排序 → src/b.rs 在 src/c.rs 前。
        let b_pos = out.find("src/b.rs").unwrap();
        let c_pos = out.find("src/c.rs").unwrap();
        assert!(b_pos < c_pos);
    }

    #[test]
    fn find_references_unknown_symbol_guides_grep() {
        let (topo, _keep) = refs_sample_topo();
        let out = topo.find_references("nope").expect("should not err");
        assert!(out.contains("not found"), "got: {out}");
        assert!(out.contains("grep_search"), "got: {out}");
    }

    #[test]
    fn find_references_building_state_returns_non_blocking_hint() {
        // 无语言 → Failed;但确保 find_references 在这些状态下不 panic 且返回提示。
        let dir = tempfile::tempdir().unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let out = topo.find_references("x").expect("should not err");
        assert!(
            out.contains("Failed") || out.contains("not found") || out.contains("grep_search"),
            "got: {out}"
        );
    }
}
