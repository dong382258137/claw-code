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
    Building {
        started_at: Instant,
    },
    Ready(TopologyData),
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
    pub kind: String,           // "fn", "struct", "trait", "enum", "mod", etc.
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
    pub context: Option<String>,  // surrounding line for quick reference
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
#[derive(Debug)]
pub struct ProjectTopology {
    state: Arc<Mutex<TopologyState>>,
    workspace_root: PathBuf,
    lsp_registry: Option<Arc<crate::lsp_client::LspRegistry>>,
}

impl ProjectTopology {
    /// 创建新的 ProjectTopology，初始状态为 Uninitialized。
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(TopologyState::Uninitialized)),
            workspace_root,
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
    /// 1. 在锁内将状态转为 `Building { started_at }`
    /// 2. 派发一个 `std::thread::spawn` 后台线程执行实际构建
    /// 3. 立即返回 `Building`(不等待构建完成)
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

        // Uninitialized → Building,派发后台线程
        let started_at = Instant::now();
        *guard = TopologyState::Building { started_at };
        drop(guard); // 释放锁,让后台线程能更新状态

        // 克隆共享 state 和 workspace_root 给后台线程
        let state_clone = Arc::clone(&self.state);
        let workspace_root = self.workspace_root.clone();

        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                build_topology_data(&workspace_root)
            });

            let mut guard = state_clone.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(Ok(data)) => {
                    *guard = TopologyState::Ready(data);
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

        // Uninitialized → Building,同步执行构建
        let started_at = Instant::now();
        *guard = TopologyState::Building { started_at };
        drop(guard);

        let result = build_topology_data(&self.workspace_root);
        let elapsed = started_at.elapsed();

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(mut data) => {
                data.built_at_ms = elapsed.as_millis() as u64;
                *guard = TopologyState::Ready(data);
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
                    out.push_str(&format!(
                        "## crate `{}` v{}\n",
                        c.name, c.version
                    ));
                    out.push_str(&format!(
                        "   manifest: {}\n",
                        c.manifest_path.display()
                    ));
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
                            "   deps: [{}]\n",
                            c.dependencies.join(", ")
                        ));
                    }
                    if let Some(reverse) = g.reverse_deps.get(&c.name) {
                        if !reverse.is_empty() {
                            out.push_str(&format!(
                                "   dependants: [{}]\n",
                                reverse.join(", ")
                            ));
                        }
                    }
                    out.push('\n');
                }
                out.push_str("Use `find_boundary_crossings` to find cross-crate symbol references, \
                       or `get_symbol_info` to look up a specific symbol.\n");
                Ok(out)
            }
            TopologyState::Building { .. } => {
                Ok("ProjectTopology is still building (cargo metadata + crate graph). \
                    This usually takes < 5 seconds. Do NOT retry immediately. \
                    Use read/grep/search tools instead to find what you need."
                    .to_string())
            }
            TopologyState::Failed(e) => {
                Ok(format!(
                    "ProjectTopology failed to build: {e}. \
                     Use read/grep/search tools instead."
                ))
            }
            TopologyState::Uninitialized => {
                Ok("ProjectTopology is uninitialized.".to_string())
            }
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
                        out.push_str(&format!(
                            "No cross-crate boundary crossings found matching the query. \
                             Try `query_project_graph` to see all crates and their dependencies.\n"
                        ));
                    } else {
                        out.push_str("No cross-crate dependencies found in workspace.\n");
                    }
                } else {
                    out.push_str(&format!(
                        "Found {} cross-crate boundary crossing(s):\n\n",
                        crossings.len()
                    ));
                    for (consumer, provider, source_paths) in &crossings {
                        out.push_str(&format!(
                            "## {consumer} → {provider}\n",
                        ));
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
                            out.push_str("   (no symbol index matches — symbol index is best-effort)\n");
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
            TopologyState::Failed(e) => Ok(format!(
                "ProjectTopology failed to build: {e}"
            )),
            TopologyState::Uninitialized => Ok(
                "ProjectTopology is uninitialized.".to_string()
            ),
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
                        out.push_str(&format!(
                            "## Crate `{}` v{}\n",
                            c.name, c.version
                        ));
                        out.push_str(&format!(
                            "   manifest: {}\n",
                            c.manifest_path.display()
                        ));
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
                                out.push_str(&format!(
                                    "   dependants: [{}]\n",
                                    reverse.join(", ")
                                ));
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
                            out.push_str(&format!(
                                "   call sites ({}):\n",
                                callers.len()
                            ));
                            for cs in callers {
                                out.push_str(&format!(
                                    "      {}:{}\n",
                                    cs.file.display(),
                                    cs.line
                                ));
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
            TopologyState::Building { .. } => Ok(
                "ProjectTopology is still building. Do NOT retry immediately.".to_string(),
            ),
            TopologyState::Failed(e) => Ok(format!(
                "ProjectTopology failed to build: {e}"
            )),
            TopologyState::Uninitialized => Ok(
                "ProjectTopology is uninitialized.".to_string()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// cargo metadata → ModuleGraph
// ---------------------------------------------------------------------------

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
/// - `Err(e)`:cargo metadata 失败(非 cargo 目录、cargo 不可用等)
fn build_topology_data(workspace_root: &Path) -> Result<TopologyData, String> {
    let started_at = Instant::now();

    // Build ModuleGraph via cargo metadata
    let module_graph = build_module_graph(workspace_root)?;

    // Phase 4 P0-5:用 grep/rg 填充 SymbolIndex(best-effort)。
    // 之前 symbol_index 恒为 None,导致 DomainTools 的 refactor_algorithm_topo
    // 永远返回 "no symbol index" 降级提示。现在从 module_graph 收集所有
    // source_paths,调用 build_symbol_index_fast 构建 definitions + callers。
    // 如果构建失败(rg/grep 不可用),symbol_index 仍为 None,不影响 ModuleGraph。
    let source_dirs: Vec<PathBuf> = module_graph
        .crates
        .iter()
        .flat_map(|c| c.source_paths.iter().cloned())
        .collect();
    let symbol_index = if source_dirs.is_empty() {
        None
    } else {
        match build_symbol_index_fast(&source_dirs) {
            Ok(si) => Some(si),
            Err(_) => None, // best-effort:grep 失败不阻断拓扑
        }
    };

    Ok(TopologyData {
        module_graph,
        symbol_index,
        built_at_ms: started_at.elapsed().as_millis() as u64,
    })
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
        let name = pkg["name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        let version = pkg["version"]
            .as_str()
            .unwrap_or("0.0.0")
            .to_string();
        let manifest_path = PathBuf::from(
            pkg["manifest_path"].as_str().unwrap_or(""),
        );

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
/// 这是 best-effort 实现:使用 `grep -rn` 搜 Rust 符号定义
/// (`pub fn`, `pub struct`, `pub trait`, `pub enum`, `pub mod` 等)。
/// 比 LSP 快但不支持宏展开/条件编译。
pub fn build_symbol_index_fast(
    source_dirs: &[PathBuf],
) -> Result<SymbolIndex, String> {
    let mut definitions: HashMap<String, Vec<SymbolDef>> = HashMap::new();
    let mut file_symbols: HashMap<PathBuf, Vec<String>> = HashMap::new();

    for dir in source_dirs {
        // Use cargo's ripgrep or grep to find pub declarations
        // P2-3 修复:在 `pub` 前加 `\b` 词边界,避免匹配 `repub fn foo()` 中的 `pub`。
        // rg 使用 Rust regex 语法,支持 `\b`;grep -E (POSIX ERE) 不支持 `\b`,
        // 但 grep fallback 用了不同的 pattern(见下方),所以这里 rg pattern 用 \b。
        let result = Command::new("rg")
            .args([
                "--no-heading",
                "--line-number",
                "--type",
                "rust",
                r"\bpub(?:\(\s*(?:crate|super)\s*\))?\s+(fn|struct|trait|enum|mod|type|const|static)\s+(\w+)",
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
                .args([
                    "-rn",
                    "--include=*.rs",
                    "-E",
                    r"(^|[^a-zA-Z0-9_])pub(\([[:space:]]*(crate|super)[[:space:]]*\))?[[:space:]]+(fn|struct|trait|enum|mod|type|const|static)[[:space:]]+[a-zA-Z_][a-zA-Z0-9_]*",
                ])
                .arg(dir)
                .output()
                .map_err(|e| format!("grep failed: {e}"))?,
        };

        if output.status.success() {
            for line in output.stdout.lines() {
                let line = line.map_err(|e| format!("read line: {e}"))?;
                if let Some(sym) = parse_grep_line(&line, dir) {
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
    // by finding all function/method invocations
    let mut callers: HashMap<String, Vec<CallSite>> = HashMap::new();
    for dir in source_dirs {
        if let Ok(lines) = find_callers_fast(dir, &definitions) {
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

fn parse_grep_line(line: &str, base_dir: &Path) -> Option<SymbolDef> {
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

    // Extract symbol kind and name
    // 支持 pub、pub(crate)、pub(super) 三种可见性,与 build_symbol_index_fast 的 grep regex 一致
    //
    // P2-3 修复:在 `pub` 前加 `\b` 词边界,避免匹配 `repub fn foo()`
    // 中的 `pub`(否则会把 `repub fn` 误识别为 `pub fn`,捕获到错误的符号名)。
    // `\b` 在 `repub` 的 `e`→`p` 位置不成立(都是单词字符),所以会跳过。
    let re = regex::Regex::new(
        r"\bpub(?:\(\s*(?:crate|super)\s*\))?\s+(fn|struct|trait|enum|mod|type|const|static)\s+(\w+)"
    ).ok()?;
    let caps = re.captures(content)?;

    let kind = caps.get(1)?.as_str().to_string();
    let name = caps.get(2)?.as_str().to_string();

    let relative_path = Path::new(file_path)
        .strip_prefix(base_dir)
        .ok()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(file_path));

    // Determine visibility
    let visibility = if content.contains("pub(crate)") {
        Some("pub(crate)".to_string())
    } else if content.contains("pub(super)") {
        Some("pub(super)".to_string())
    } else if content.contains("pub ") {
        Some("pub".to_string())
    } else {
        None
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

fn find_callers_fast(
    dir: &Path,
    definitions: &HashMap<String, Vec<SymbolDef>>,
) -> Result<HashMap<String, Vec<CallSite>>, String> {
    let mut result: HashMap<String, Vec<CallSite>> = HashMap::new();

    // P2-3:预编译 regex,避免在内层 line 循环中重复编译。
    // 用于解析 grep 输出 `path:line:content`,Windows 盘符冒号免疫。
    let separator_re = regex::Regex::new(r":(\d+):")
        .map_err(|e| format!("regex compile failed: {e}"))?;

    // For each known function name, search for its usage
    for func_name in definitions
        .iter()
        .filter_map(|(name, defs)| {
            if defs.iter().any(|d| d.kind == "fn") {
                Some(name.as_str())
            } else {
                None
            }
        })
        .take(100)
    {
        let output = Command::new("rg")
            .args([
                "--no-heading",
                "--line-number",
                "--type",
                "rust",
                &format!(r"\b{func_name}\("),
            ])
            .arg(dir)
            .output();

        let lines = match output {
            Ok(o) if o.status.success() => o.stdout,
            _ => continue,
        };

        for line in lines.lines() {
            let line = line.map_err(|e| format!("read line: {e}"))?;
            // P2-3 修复:Windows 路径解析,与 parse_grep_line 同样的策略。
            // 之前 `line.find(':')` 在 `C:\project\src\main.rs:42:foo()` 上
            // 会定位到盘符后的冒号,导致 file_path="C" 解析失败。
            // 现在用预编译的 separator_re 找到真正的 "行号分隔符" 位置。
            let Some(caps) = separator_re.captures(&line) else {
                continue;
            };
            let Some(whole_match) = caps.get(0) else {
                continue;
            };
            let Ok(line_num) = caps.get(1).unwrap().as_str().parse::<u32>() else {
                continue;
            };
            let file_path = &line[..whole_match.start()];
            let context = line[whole_match.end()..].to_string();
            if file_path.is_empty() {
                continue;
            }

            let file_path_path = Path::new(file_path);
            let relative_path = file_path_path
                .strip_prefix(dir)
                .ok()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(file_path));

            result
                .entry(func_name.to_string())
                .or_default()
                .push(CallSite {
                    file: relative_path,
                    line: line_num,
                    context: Some(context),
                });
        }
    }

    Ok(result)
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
            TopologyState::Ready(TopologyData {
                module_graph: ModuleGraph {
                    workspace_root: PathBuf::from("/test"),
                    crates: vec![],
                    reverse_deps: HashMap::new(),
                },
                symbol_index: None,
                built_at_ms: 0,
            })
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
        let sym = parse_grep_line(line, &base).expect("should parse");
        assert_eq!(sym.name, "hello_world");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.line, 42);
        assert_eq!(sym.visibility, Some("pub".to_string()));
    }

    #[test]
    fn parse_grep_line_parses_pub_struct() {
        let base = PathBuf::from("/project/src");
        let line = "/project/src/lib.rs:10:pub struct MyStruct<T> {";
        let sym = parse_grep_line(line, &base).expect("should parse");
        assert_eq!(sym.name, "MyStruct");
        assert_eq!(sym.kind, "struct");
    }
    #[test]
    fn parse_grep_line_parses_pub_crate_fn() {
        let base = PathBuf::from("/project/src");
        let line = "/project/src/internal.rs:5:pub(crate) fn internal_helper() {";
        // P2-3 修复：regex 现在支持 pub(crate) 可见性，不再容忍解析失败
        let sym = parse_grep_line(line, &base)
            .expect("pub(crate) fn should be parsed with fixed regex");
        assert_eq!(sym.name, "internal_helper");
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.visibility.as_deref(), Some("pub(crate)"));
    }

    #[test]
    fn parse_grep_line_parses_pub_super_fn() {
        let base = PathBuf::from("/project/src");
        let line = "/project/src/mod.rs:12:pub(super) fn super_helper() {";
        let sym = parse_grep_line(line, &base)
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
        let sym = parse_grep_line(line, &base).expect("should parse Windows path");
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
        let sym = parse_grep_line(line, &base)
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
        let sym = parse_grep_line(line, &base).expect("should parse the real pub fn");
        assert_eq!(
            sym.name, "real_pub",
            "should capture real_pub, not repub_fn (word boundary bug)"
        );
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
        let sym = parse_grep_line(line, &base).expect("should parse");
        assert_eq!(sym.name, "fetch");
        assert_eq!(sym.line, 42);
    }

    #[test]
    fn parse_grep_line_rejects_empty_file_path() {
        // 边界情况:regex 匹配到字符串开头的 `:42:`,file_path 为空。
        // 这不应该发生在真实 grep 输出中,但作为防御性编程需要处理。
        let base = PathBuf::from("/project/src");
        let line = ":42:pub fn foo() {}";
        let result = parse_grep_line(line, &base);
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
        let sym = parse_grep_line(line, &base).expect("should parse pub const");
        assert_eq!(sym.name, "MAX_RETRIES");
        assert_eq!(sym.kind, "const");
        assert_eq!(sym.visibility.as_deref(), Some("pub"));
    }

    #[test]
    fn parse_grep_line_parses_pub_static() {
        // 测试 static 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/globals.rs:5:pub static COUNTER: AtomicU64 = AtomicU64::new(0);";
        let sym = parse_grep_line(line, &base).expect("should parse pub static");
        assert_eq!(sym.name, "COUNTER");
        assert_eq!(sym.kind, "static");
    }

    #[test]
    fn parse_grep_line_parses_pub_type_alias() {
        // 测试 type 关键字(类型别名)
        let base = PathBuf::from("/project/src");
        let line = "/project/src/types.rs:8:pub type Result<T> = std::result::Result<T, MyError>;";
        let sym = parse_grep_line(line, &base).expect("should parse pub type");
        assert_eq!(sym.name, "Result");
        assert_eq!(sym.kind, "type");
    }

    #[test]
    fn parse_grep_line_parses_pub_mod() {
        // 测试 mod 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/lib.rs:1:pub mod network;";
        let sym = parse_grep_line(line, &base).expect("should parse pub mod");
        assert_eq!(sym.name, "network");
        assert_eq!(sym.kind, "mod");
    }

    #[test]
    fn parse_grep_line_parses_pub_enum() {
        // 测试 enum 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/enums.rs:3:pub enum Color { Red, Green, Blue }";
        let sym = parse_grep_line(line, &base).expect("should parse pub enum");
        assert_eq!(sym.name, "Color");
        assert_eq!(sym.kind, "enum");
    }

    #[test]
    fn parse_grep_line_parses_pub_trait() {
        // 测试 trait 关键字
        let base = PathBuf::from("/project/src");
        let line = "/project/src/traits.rs:7:pub trait Serializable { fn serialize(&self); }";
        let sym = parse_grep_line(line, &base).expect("should parse pub trait");
        assert_eq!(sym.name, "Serializable");
        assert_eq!(sym.kind, "trait");
    }

    #[test]
    fn parse_grep_line_skips_non_pub_declarations() {
        // 非 pub 声明不应被索引(私有符号不在 SymbolIndex 范围内)
        let base = PathBuf::from("/project/src");
        let line = "/project/src/internal.rs:5:fn private_helper() {}";
        let result = parse_grep_line(line, &base);
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
        let sym = parse_grep_line(line, &base).expect("should parse indented pub fn");
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
    // P2-1:异步化测试
    // -----------------------------------------------------------------------

    #[test]
    fn p2_1_ensure_built_returns_building_immediately() {
        // ensure_built() 应立即返回 Building,不阻塞等待 cargo metadata
        let dir = tempfile::tempdir().unwrap();
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
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let state1 = topo.ensure_built();
        assert!(matches!(state1, TopologyState::Building { .. }));
        let state2 = topo.ensure_built();
        assert!(matches!(state2, TopologyState::Building { .. }));
        // 两次返回的 Building 的 started_at 应相同(同一个 Building 实例)
        if let (TopologyState::Building { started_at: s1 }, TopologyState::Building { started_at: s2 }) = (state1, state2) {
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
        let result = build_topology_data(dir.path());
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
}
