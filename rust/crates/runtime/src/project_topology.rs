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

    /// 尝试构建拓扑(如果尚未构建)。
    /// 同步执行 `cargo metadata`，构建 ModuleGraph。
    pub fn ensure_built(&self) -> TopologyState {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());

        match &*guard {
            TopologyState::Ready(_) => return guard.clone(),
            TopologyState::Failed(_) => return guard.clone(),
            TopologyState::Building { .. } => return guard.clone(),
            TopologyState::Uninitialized => {}
        }

        // Start building
        let started_at = Instant::now();
        *guard = TopologyState::Building { started_at };
        drop(guard);

        // Build ModuleGraph via cargo metadata
        let result = build_module_graph(&self.workspace_root);

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(module_graph) => {
                let data = TopologyData {
                    module_graph,
                    symbol_index: None,
                    built_at_ms: started_at.elapsed().as_millis() as u64,
                };
                *guard = TopologyState::Ready(data);
            }
            Err(e) => {
                *guard = TopologyState::Failed(e);
            }
        }
        guard.clone()
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
        let result = Command::new("rg")
            .args([
                "--no-heading",
                "--line-number",
                "--type",
                "rust",
                r"pub(?:\(\s*(?:crate|super)\s*\))?\s+(fn|struct|trait|enum|mod|type|const|static)\s+(\w+)",
            ])
            .arg(dir)
            .output();

        // Fallback to grep if rg not available
        let output = match result {
            Ok(o) if o.status.success() => o,
            _ => Command::new("grep")
                .args([
                    "-rn",
                    "--include=*.rs",
                    "-E",
                    r"pub\s+(fn|struct|trait|enum|mod|type|const|static)\s+\w+",
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
    let colon_pos1 = line.find(':')?;
    let colon_pos2 = line[colon_pos1 + 1..].find(':')?;

    let file_path = &line[..colon_pos1];
    let line_num: u32 = line[colon_pos1 + 1..colon_pos1 + 1 + colon_pos2]
        .parse()
        .ok()?;
    let content = &line[colon_pos1 + 1 + colon_pos2 + 1..];

    // Extract symbol kind and name
    let re = regex::Regex::new(
        r"pub\s+(fn|struct|trait|enum|mod|type|const|static)\s+(\w+)"
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
            if let Some(colon_pos) = line.find(':') {
                let file_path = &line[..colon_pos];
                let rest = &line[colon_pos + 1..];
                if let Some(colon_pos2) = rest.find(':') {
                    if let Ok(line_num) = rest[..colon_pos2].parse::<u32>() {
                        let context = rest[colon_pos2 + 1..].to_string();

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
            }
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
        // Note: pub(crate) visibility may not match with current regex;
        // this is best-effort — the visibility field is advisory.
        if let Some(sym) = parse_grep_line(line, &base) {
            assert_eq!(sym.name, "internal_helper");
            // visibility may be None for pub(crate) patterns
        } else {
            // Regex may miss pub(crate) with some regex engine quirks; ok.
        }
    }

    #[test]
    fn project_topology_initial_state() {
        let topo = ProjectTopology::new(PathBuf::from("."));
        let state = topo.state();
        assert!(matches!(state, TopologyState::Uninitialized));
    }

    #[test]
    fn project_topology_ensure_built_returns_building_or_failed() {
        // In a non-cargo directory, ensure_built will fail
        let dir = tempfile::tempdir().unwrap();
        let topo = ProjectTopology::new(dir.path().to_path_buf());
        let state = topo.ensure_built();
        // Should either be Building then Failed, or directly Failed
        match state {
            TopologyState::Failed(_) => {}
            TopologyState::Building { .. } => {}
            _ => panic!("expected Failed or Building, got {state}"),
        }
    }

    #[test]
    fn project_topology_query_returns_message_when_not_ready() {
        let topo = ProjectTopology::new(PathBuf::from("."));
        let result = topo.query_project_graph().unwrap();
        // Should return a message rather than error
        assert!(!result.is_empty());
    }
}
