//! Simplified repository code map.
//!
//! Generates a token-budgeted overview of Rust definitions across a project
//! using regex-based extraction and reference counting. Intended for system
//! prompt injection so the model has a high-level view of the codebase
//! without consuming the full file contents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde::{Deserialize, Serialize};

const DEFAULT_MAX_TOKENS: usize = 1024;
const CACHE_TTL_SECS: u64 = 60;

/// RepoMap 磁盘缓存 schema 版本。序列化语义变化时递增(如 P4 semantic_importance
/// 改变 Definition 序列化语义 → bump 到 2,旧缓存作废重建)。
pub const REPOMAP_CACHE_SCHEMA_VERSION: u32 = 2;

/// Step 4.3:单文件一次引用解析最多查询的 symbol 数量。
/// 防止 monorepo 中文件 symbol 过多导致 LSP 调用爆炸。
const MAX_REF_SYMBOLS_PER_FILE: usize = 16;
/// B2:渲染时单文件最多输出的 symbol/definition 条数 —— 防止符号密集型文件
/// 独占 token 预算(1024 tokens 被少数文件占满,其余文件被挤出地图)。
/// 语义重要性排序下,前 8 个通常是该文件的核心公开符号。
const MAX_RENDER_SYMBOLS_PER_FILE: usize = 8;

#[derive(Debug, Clone)]
pub struct RepoMap {
    root: PathBuf,
    max_tokens: usize,
    cache: HashMap<PathBuf, CachedFileMap>,
    cache_time: Option<SystemTime>,
    /// Step 4.3:LSP references 解析出的跨模块引用计数。
    /// key 是"定义所在文件"(绝对路径),value 是该文件符号被其他文件引用的次数。
    /// 仅在 LSP 可用时填充,优先级高于 regex 子串匹配的 importance。
    lsp_importance: HashMap<PathBuf, usize>,
    /// P4:基于 ProjectTopology SymbolIndex 的语义重要性(cross-caller 文件去重数)。
    /// 与 lsp_importance 并列,优先级 lsp > semantic > regex。
    semantic_importance: HashMap<PathBuf, usize>,
    /// P1/性能:内存 cache 与磁盘缓存是否不一致。仅在 `refresh_cache_if_stale`
    /// 实际发生重解析/增删时置位,`save_disk` 据此跳过无变化的全量写盘
    /// (避免每次启动都把 ~1MB JSON 序列化重写一遍)。
    dirty: bool,
}

/// 单个文件的缓存条目(mtime 变化时增量重解析)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFileMap {
    definitions: Vec<Definition>,
    references: Vec<String>,
    /// UNIX_EPOCH 起的毫秒数(自定义 serde,规避 SystemTime 在 JSON 下的序列化不确定性)。
    #[serde(with = "serde_mtime_ms")]
    mtime: SystemTime,
    /// Step 4.2:从 LSP 获取的 symbol 信息(可选)。
    /// 若存在,render 时优先使用 LSP symbols(语义准确),
    /// 否则 fallback 到 regex 提取的 definitions。
    /// 磁盘缓存不落盘(LSP 数据易过期),加载后走 regex 路径,由运行时重新注入。
    #[serde(skip_serializing, default)]
    lsp_symbols: Vec<crate::lsp_client::LspSymbol>,
    /// Step 4.3:该文件是否已尝试过 LSP references 解析。
    /// 用于区分"LSP 不可用(fallback regex)"与"LSP 可用但符号无跨文件引用"。
    #[serde(skip_serializing, default)]
    lsp_refs_resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub kind: DefinitionKind,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefinitionKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Type,
    /// P2:Python 类(Rust 项目不会产生该变体)。
    Class,
}

/// RepoMap 支持的源语言(P1 仅 Rust;P2 扩展 Python)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
}

impl Lang {
    /// 识别文件扩展名对应的语言;不支持 → None(跳过该文件)。
    fn from_path(path: &Path) -> Option<Lang> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Some(Lang::Rust),
            Some("py") => Some(Lang::Python),
            _ => None,
        }
    }
}

impl RepoMap {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            max_tokens: DEFAULT_MAX_TOKENS,
            cache: HashMap::new(),
            cache_time: None,
            lsp_importance: HashMap::new(),
            semantic_importance: HashMap::new(),
            dirty: false,
        }
    }

    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// 当前支持的源语言列表(磁盘缓存按此校验;范围变化 → 缓存作废重建)。
    /// P2:支持 Rust + Python。
    fn supported_languages() -> Vec<String> {
        vec!["rust".to_string(), "python".to_string()]
    }

    /// P1:从 `.claw/repomap_cache.json` 加载磁盘缓存(best-effort)。
    ///
    /// 校验:逃生阀 / 大小守卫 / schema / root / 语言范围。加载后将 `cache_time`
    /// 置为过期,使首次 `render()` 必然执行一次 walkdir mtime 校验遍历:
    /// ① mtime 变化文件增量重扫;② 新增文件被发现;③ 删除文件被清理。
    /// 返回 false 表示未加载(调用方继续走全量构建路径)。
    #[must_use]
    pub fn load_disk(&mut self) -> bool {
        if crate::topology_cache::cache_disabled("repomap") {
            return false;
        }
        let root_norm = crate::topology_cache::normalize_root(&self.root);
        let path = root_norm.join(".claw").join("repomap_cache.json");
        let size_ok = std::fs::metadata(&path)
            .map(|m| crate::topology_cache::cache_within_limit("repomap", m.len()))
            .unwrap_or(false);
        if !size_ok {
            return false;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            return false;
        };
        let Ok(cached) = serde_json::from_str::<RepoMapCache>(&content) else {
            return false;
        };
        if cached.schema_version != REPOMAP_CACHE_SCHEMA_VERSION {
            return false;
        }
        if cached.root != root_norm.to_string_lossy() {
            return false;
        }
        if cached.languages != Self::supported_languages() {
            return false;
        }
        self.cache = cached.files;
        // 强制首次 render 走 mtime 校验遍历(增量重扫 + 新文件发现 + 删除清理)。
        self.cache_time = Some(SystemTime::now() - Duration::from_secs(CACHE_TTL_SECS + 1));
        true
    }

    /// P1:将当前内存缓存落盘到 `.claw/repomap_cache.json`(best-effort,失败静默)。
    ///
    /// 跨平台写盘:写唯一 tmp(带 pid)→ rename;Windows 上 rename 无法覆盖已存在
    /// 目标时降级为 remove 目标再 rename;仍失败则丢弃(接受竞态,最坏下次重建)。
    /// 性能:仅当 `dirty`(缓存实际发生变化)时写盘,无变化启动(纯 mtime 校验通过)
    /// 直接跳过 ~1MB JSON 序列化重写。
    pub fn save_disk(&self) {
        if crate::topology_cache::cache_disabled("repomap") {
            return;
        }
        if !self.dirty {
            return;
        }
        let root_norm = crate::topology_cache::normalize_root(&self.root);
        let claw_dir = root_norm.join(".claw");
        if std::fs::create_dir_all(&claw_dir).is_err() {
            return;
        }
        let final_path = claw_dir.join("repomap_cache.json");
        let tmp = claw_dir.join(format!("repomap_cache.{}.tmp", std::process::id()));
        let cache = RepoMapCache {
            schema_version: REPOMAP_CACHE_SCHEMA_VERSION,
            languages: Self::supported_languages(),
            root: root_norm.to_string_lossy().to_string(),
            files: self.cache.clone(),
        };
        let Ok(json) = serde_json::to_string(&cache) else {
            return;
        };
        // 写侧网关：RepoMap 在超限策略下放弃落盘（Topology 优先持久化）。
        if !crate::topology_cache::cache_within_limit("repomap", json.len() as u64) {
            return;
        }
        if std::fs::write(&tmp, json).is_err() {
            return;
        }
        if std::fs::rename(&tmp, &final_path).is_err() {
            let _ = std::fs::remove_file(&final_path);
            let _ = std::fs::rename(&tmp, &final_path);
        }
    }

    pub fn render(&mut self) -> String {
        let t0 = cache_verbose_timer();
        self.refresh_cache_if_stale();
        cache_verbose_log("render.refresh", &t0);
        let importance = self.calculate_importance();
        cache_verbose_log("render.importance", &t0);
        let mut ranked: Vec<(PathBuf, usize)> = importance.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let out = self.build_map_with_budget(&ranked);
        cache_verbose_log("render.budget", &t0);
        out
    }

    /// SP4.2-B5:render with LSP symbols augmentation.
    ///
    /// 先 `refresh_cache_if_stale`(regex 提取 + 保留已有 LSP symbols),
    /// 再 `refresh_lsp_symbols`(从 LSP server 获取最新 symbols),
    /// 最后 render。
    ///
    /// 若 registry 中无已 spawn 的 server,退化为普通 `render()`。
    pub fn render_with_lsp(&mut self, registry: &crate::lsp_client::LspRegistry) -> String {
        self.refresh_cache_if_stale();
        self.refresh_lsp_symbols(registry);
        // Step 4.3:LSP references 跨模块引用解析(语义重要性,优先于 regex 子串匹配)。
        // best-effort:无可用 LSP server 时内部自动 fallback,不影响 render。
        self.refresh_lsp_references(registry);
        let importance = self.calculate_importance();
        let mut ranked: Vec<(PathBuf, usize)> = importance.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.build_map_with_budget(&ranked)
    }

    #[must_use]
    pub fn extract_definitions(content: &str) -> Vec<Definition> {
        Self::extract_definitions_lang(content, Lang::Rust)
    }

    /// P2:按语言提取定义(Rust 或 Python)。
    ///
    /// best-effort 边界(注释声明,测试只覆盖该子集):不支持装饰器、多行定义、
    /// 相对导入、import 别名(as)、条件导入、动态定义、嵌套函数/类。
    pub fn extract_definitions_lang(content: &str, lang: Lang) -> Vec<Definition> {
        let mut defs = Vec::new();
        let re = match lang {
            Lang::Rust => DEFINITION_REGEXES.get_or_init(init_definition_regexes),
            Lang::Python => PY_DEFINITION_REGEXES.get_or_init(init_py_definition_regexes),
        };
        for (kind, re) in re {
            for caps in re.captures_iter(content) {
                if let Some(name_match) = caps.get(1) {
                    let line = content[..name_match.start()].matches('\n').count() + 1;
                    defs.push(Definition {
                        name: name_match.as_str().to_string(),
                        kind: *kind,
                        line,
                    });
                }
            }
        }
        defs
    }

    #[must_use]
    pub fn extract_references(content: &str) -> Vec<String> {
        Self::extract_references_lang(content, Lang::Rust)
    }

    /// P2:按语言提取引用(Rust 或 Python)。
    /// Python 引用:`from x import y` / `import x`(别名 as 不支持,best-effort)。
    pub fn extract_references_lang(content: &str, lang: Lang) -> Vec<String> {
        let mut refs = Vec::new();
        let re = match lang {
            Lang::Rust => REFERENCE_REGEXES.get_or_init(init_reference_regexes),
            Lang::Python => PY_REFERENCE_REGEXES.get_or_init(init_py_reference_regexes),
        };
        for re in re {
            for caps in re.captures_iter(content) {
                if let Some(m) = caps.get(1) {
                    refs.push(m.as_str().to_string());
                }
            }
        }
        refs
    }

    /// Step 4.2:用 LSP symbols 增强 cache 中指定文件的 symbol 信息。
    ///
    /// LSP 提供的 symbols 比 regex 提取更准确(语义级别),
    /// 且能识别 regex 难以处理的场景(如宏生成的定义、impl 块内的方法)。
    ///
    /// # 参数
    /// - `path`:文件路径(绝对路径或相对 root 的路径)
    /// - `symbols`:`LspRegistry::get_symbols()` 返回的 symbol 列表
    ///
    /// # 行为
    /// - 若 path 不在 cache 中,创建一个空条目(仅含 LSP symbols,无 regex definitions)
    /// - 若 path 已在 cache 中,替换其 `lsp_symbols` 字段
    /// - 后续 `render()` 时,若 `lsp_symbols` 非空,优先渲染 LSP symbols
    ///
    /// # 与 LSP 协同
    /// 典型流程:
    /// 1. `LspRegistry::spawn_server("rust", "rust-analyzer", root)`
    /// 2. 对每个文件:`let symbols = registry.get_symbols(path)?;`
    /// 3. `repomap.augment_with_lsp_symbols(path, symbols)`
    /// 4. `repomap.render()` — 输出包含 LSP symbols 的 repomap
    pub fn augment_with_lsp_symbols(
        &mut self,
        path: &Path,
        symbols: Vec<crate::lsp_client::LspSymbol>,
    ) {
        // 转换为绝对路径(cache 使用绝对路径作为 key)
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // 尝试获取文件 mtime(若文件不存在,使用 UNIX_EPOCH)
        let mtime = std::fs::metadata(&abs_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);

        self.cache
            .entry(abs_path)
            .and_modify(|cached| {
                cached.lsp_symbols = symbols.clone();
            })
            .or_insert_with(|| CachedFileMap {
                definitions: Vec::new(),
                references: Vec::new(),
                mtime,
                lsp_symbols: symbols,
                lsp_refs_resolved: false,
            });
    }

    /// Step 4.2:检查指定文件是否有 LSP symbols 增强。
    #[must_use]
    pub fn has_lsp_symbols(&self, path: &Path) -> bool {
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        self.cache
            .get(&abs_path)
            .map(|c| !c.lsp_symbols.is_empty())
            .unwrap_or(false)
    }

    #[must_use]
    pub fn calculate_importance(&self) -> HashMap<PathBuf, usize> {
        let mut importance: HashMap<PathBuf, usize> = HashMap::new();

        // Step 4.3:若 LSP references 已解析,优先使用语义引用计数。
        // LSP 能精确区分跨模块引用(regex 的 substring contains 会误匹配
        // Foo 与 FooBar、注释/字符串中的同名标识符)。
        // lsp_importance 在 refresh_lsp_references 中为全部文件初始化了 0,
        // 因此非空即表示 LSP 解析可用。
        if !self.lsp_importance.is_empty() {
            return self.lsp_importance.clone();
        }

        // P4:否则若已注入 SymbolIndex 语义重要性,其次使用之。
        // 优先级 lsp > semantic > regex。
        if !self.semantic_importance.is_empty() {
            return self.semantic_importance.clone();
        }

        // regex 路径(O(N) 倒排,替代原 O(N²) 子串匹配)。
        // 语义:def.name 作为**完整标识符段**出现在其他文件的引用串中即计 1 次
        // 引用。与原 `ref_str.contains(def.name)` 相比:
        // ① 消除 O(N²)(建段倒排一次遍历,查询 O(total defs));
        // ② 修复 `Foo` 误匹配 `FooBar`(段精确匹配);
        // ③ 注释/字符串中的同名标识符不再误计。
        // 兼容性:`use b::func_b` → 段 {b, func_b} 精确命中 def.name=func_b ✓。
        let mut seg_index: HashMap<&str, Vec<PathBuf>> = HashMap::new();
        for (path, cached) in &self.cache {
            for ref_str in &cached.references {
                for seg in split_identifiers(ref_str) {
                    seg_index.entry(seg).or_default().push(path.clone());
                }
            }
        }
        for (path, cached) in &self.cache {
            let mut count = 0usize;
            for def in &cached.definitions {
                if let Some(files) = seg_index.get(def.name.as_str()) {
                    // 排除自身文件后的引用方文件数。
                    count += files.iter().filter(|f| *f != path).count();
                }
            }
            importance.insert(path.clone(), count);
        }
        importance
    }

    /// P4:注入 ProjectTopology SymbolIndex 的语义重要性。
    ///
    /// 统计"被不同 caller 文件引用的次数"(按 `caller.file → def.file` 映射,
    /// 跨调用者文件**去重**计数,避免单文件多次调用放大权重),存入
    /// `semantic_importance`。`calculate_importance` 因此在缺少 LSP 时优先使用
    /// 语义计数,替代 regex 子串匹配(`Foo` 误匹配 `FooBar` / 注释/字符串)。
    ///
    /// 注:此方法仅供 best-effort 排序增强;若读到的 SymbolIndex 略过期(来自
    /// P1 磁盘缓存,未校验指纹),只影响排序权重,不影响查询正确性。
    pub fn augment_with_symbol_index(
        &mut self,
        symbol_index: &crate::project_topology::SymbolIndex,
    ) {
        let mut semantic: HashMap<PathBuf, usize> = HashMap::new();
        for (symbol, callers) in &symbol_index.callers {
            // 找到该符号的定义所在文件(取首个定义;SymbolDef.file 为相对路径)。
            let Some(def_file) = symbol_index.definitions.get(symbol).and_then(|d| d.first())
            else {
                continue;
            };
            let def_abs = self.root.join(&def_file.file);
            // 跨调用者文件去重(不在定义文件内的 caller 文件计数)。
            let mut caller_files: std::collections::HashSet<PathBuf> =
                std::collections::HashSet::new();
            for cs in callers {
                let caller_abs = if cs.file.is_absolute() {
                    cs.file.clone()
                } else {
                    self.root.join(&cs.file)
                };
                if caller_abs == def_abs {
                    continue;
                }
                caller_files.insert(caller_abs);
            }
            let count = caller_files.len();
            if count > 0 {
                *semantic.entry(def_abs).or_default() += count;
            }
        }
        // 非空才启用(与 lsp_importance 语义一致:空表 = 该路径不可用)。
        if !semantic.is_empty() {
            self.semantic_importance = semantic;
        }
    }

    fn build_map_with_budget(&self, ranked: &[(PathBuf, usize)]) -> String {
        let mut sorted = ranked.to_vec();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        if sorted.is_empty() {
            return String::new();
        }

        // Binary search for the maximum number of files whose rendered form
        // fits within the token budget.
        let mut lo: usize = 0;
        let mut hi: usize = sorted.len();
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            let candidate = self.render_files(&sorted[..mid]);
            if estimate_tokens(&candidate) <= self.max_tokens {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }

        if lo == 0 {
            // Even a single file exceeds budget; emit nothing rather than a
            // truncated entry that would mislead the model.
            return String::new();
        }

        self.render_files(&sorted[..lo])
    }

    fn render_files(&self, files: &[(PathBuf, usize)]) -> String {
        let mut out = String::new();
        for (path, refs) in files {
            let rel = path
                .strip_prefix(&self.root)
                .unwrap_or(path)
                .display()
                .to_string();
            out.push_str(&format!("{} (refs: {})\n", rel, refs));
            if let Some(cached) = self.cache.get(path) {
                // Step 4.2:若 lsp_symbols 非空,优先渲染 LSP symbols(语义更准确)
                if !cached.lsp_symbols.is_empty() {
                    for symbol in cached.lsp_symbols.iter().take(MAX_RENDER_SYMBOLS_PER_FILE) {
                        out.push_str(&format!(
                            "  {} {} (L:{}:{})\n",
                            symbol.kind, symbol.name, symbol.line, symbol.character
                        ));
                    }
                } else {
                    // Fallback:regex 提取的 definitions。带行号——同名符号
                    // (如多个 `fn new`/`fn fmt`)无行号时无区分度,曾被评为
                    // "重复裸函数名,信噪比低"(2026-09-17 系统提示词评估 P2)。
                    for def in cached.definitions.iter().take(MAX_RENDER_SYMBOLS_PER_FILE) {
                        let kind_str = match def.kind {
                            DefinitionKind::Function => "fn",
                            DefinitionKind::Struct => "struct",
                            DefinitionKind::Enum => "enum",
                            DefinitionKind::Trait => "trait",
                            DefinitionKind::Impl => "impl",
                            DefinitionKind::Module => "mod",
                            DefinitionKind::Const => "const",
                            DefinitionKind::Type => "type",
                            DefinitionKind::Class => "class",
                        };
                        out.push_str(&format!("  {} {} (L:{})\n", kind_str, def.name, def.line));
                    }
                }
            }
            out.push('\n');
        }
        out
    }

    pub fn refresh_cache_if_stale(&mut self) {
        let now = SystemTime::now();
        let needs_refresh = match self.cache_time {
            None => true,
            Some(t) => now
                .duration_since(t)
                .map(|d| d.as_secs() > CACHE_TTL_SECS)
                .unwrap_or(true),
        };
        if !needs_refresh {
            return;
        }

        // 本次遍历前缓存为空 → 全新构建(必写盘);否则只有发生重解析才置 dirty。
        let had_cache = !self.cache.is_empty();

        // 自写 READDIR 遍历替代 walkdir:walkdir 对**每个条目**调用 file_type()
        // (Windows 上每次≈一次 syscall,大仓库 1400+ 条目即数百 ms)。
        // 此处按文件名先做零 syscall 过滤(目录名/扩展名),非目标文件完全跳过;
        // 每个目标条目至多一次 fs::metadata(可同时取类型+mtime)。
        let walk_t = cache_verbose_timer();
        let walked = walk_source_files(&self.root);
        cache_verbose_log("render.walk", &walk_t);
        for (path, mtime) in walked {
            let Some(lang) = Lang::from_path(&path) else {
                continue;
            };

            let needs_reparse = match self.cache.get(&path) {
                None => true,
                // 统一毫秒精度比较:磁盘缓存经 serde_mtime_ms 序列化为毫秒,
                // 而 walk_source_files 返回纳秒级 SystemTime,直接 `!=` 会因
                // 截断导致缓存永远被视为过期 → 每次启动全量重解析。
                Some(cached) => mtime_ms(&cached.mtime) != mtime_ms(&mtime),
            };
            if !needs_reparse {
                continue;
            }
            self.dirty = true;

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let definitions = Self::extract_definitions_lang(&content, lang);
            let references = Self::extract_references_lang(&content, lang);

            // SP4.2-B5:re-parse 时保留已有的 LSP symbols(避免 refresh 清空)
            // LSP symbols 由 augment_with_lsp_symbols 单独注入,refresh_cache_if_stale
            // 不负责重新获取 LSP symbols(那需要调用 LSP server,开销大且可能阻塞)。
            // 保留策略:文件 mtime 变化时,regex 提取结果更新,但 LSP symbols 保留
            // (略有过期,但比清空好;下次 augment_with_lsp_symbols 调用会刷新)。
            let existing_lsp_symbols = self
                .cache
                .get(&path)
                .map(|c| c.lsp_symbols.clone())
                .unwrap_or_default();

            let existing_lsp_refs_resolved = self
                .cache
                .get(&path)
                .map(|c| c.lsp_refs_resolved)
                .unwrap_or(false);

            self.cache.insert(
                path,
                CachedFileMap {
                    definitions,
                    references,
                    mtime,
                    lsp_symbols: existing_lsp_symbols,
                    lsp_refs_resolved: existing_lsp_refs_resolved,
                },
            );
        }

        // Drop cache entries for files that no longer exist on disk.
        let before_retain = self.cache.len();
        self.cache.retain(|p, _| p.exists());
        if self.cache.len() != before_retain {
            self.dirty = true;
        }
        // 全新构建(此前无缓存)也视为 dirty,保证写盘。
        if !had_cache && !self.cache.is_empty() {
            self.dirty = true;
        }

        self.cache_time = Some(now);
    }

    /// SP4.2-B5:从 LSP registry 刷新所有缓存文件的 LSP symbols。
    ///
    /// 遍历 cache 中每个文件,若对应语言有已 spawn 的 LSP server,
    /// 调用 `get_symbols` 获取语义 symbols 并注入 cache。
    ///
    /// 这是 best-effort 操作:单个文件获取失败不阻断其他文件,
    /// 失败的文件保持原有 lsp_symbols(可能为空)。
    ///
    /// # 参数
    /// - `registry`:LSP registry(需已 spawn 对应语言的 server)
    ///
    /// # 返回
    /// 成功刷新的文件数量
    pub fn refresh_lsp_symbols(&mut self, registry: &crate::lsp_client::LspRegistry) -> usize {
        let mut refreshed = 0usize;
        let paths: Vec<PathBuf> = self.cache.keys().cloned().collect();

        for path in paths {
            let path_str = match path.to_str() {
                Some(s) => s.to_owned(),
                None => continue,
            };

            // 检查该文件是否有对应的 LSP server
            if registry.find_server_for_path(&path_str).is_none() {
                continue;
            }

            match registry.get_symbols(&path_str) {
                Ok(symbols) if !symbols.is_empty() => {
                    if let Some(cached) = self.cache.get_mut(&path) {
                        cached.lsp_symbols = symbols;
                        refreshed += 1;
                    }
                }
                Ok(_) => {
                    // 空符号列表 — 保留现有(可能是 server 还在索引)
                }
                Err(_) => {
                    // 获取失败 — 保留现有(不阻断其他文件)
                }
            }
        }

        refreshed
    }

    /// Step 4.3:通过 LSP `textDocument/references` 解析跨模块引用计数。
    ///
    /// 对每个已 spawn server 的文件:
    /// 1. 用 [`crate::lsp_client::LspRegistry::get_references`] 查询每个 symbol
    ///    的引用位置(跨文件语义引用,替代 regex 子串匹配)。
    /// 2. 统计"引用位置所在文件 ≠ 定义所在文件"的条目,计入该文件的重要性。
    /// 3. 结果写入 `self.lsp_importance`,`calculate_importance` 优先使用它。
    ///
    /// # 说明
    /// - 单文件最多查询 [`MAX_REF_SYMBOLS_PER_FILE`] 个 symbol,防止 LSP 调用爆炸。
    /// - 这是 best-effort 操作:单文件失败不阻断其他文件。
    /// - 所有 cache 文件先初始化为 0,保证排名列表完整
    ///   (否则零引用文件会从 map 中消失)。
    ///
    /// # 前置条件
    /// 应优先在 [`refresh_lsp_symbols`](Self::refresh_lsp_symbols) 之后调用,
    /// 复用已注入的 lsp_symbols,避免重复 documentSymbol 请求。
    ///
    /// # 返回
    /// 成功完成引用解析的文件数量
    pub fn refresh_lsp_references(&mut self, registry: &crate::lsp_client::LspRegistry) -> usize {
        // 先确认是否有文件对应**已 spawn** 的 LSP server。
        // 仅 registered 而未 spawn 的 server 不视为可用 —— 避免 prompt 组装时
        // 意外触发 lazy auto-start(首次 rust-analyzer 索引可能耗时 30-60s)。
        // 若完全没有,保持 lsp_importance 为空,calculate_importance 继续走 regex 路径。
        let paths: Vec<PathBuf> = self.cache.keys().cloned().collect();
        let mut candidate_paths: Vec<PathBuf> = Vec::new();
        for path in &paths {
            if let Some(s) = path.to_str() {
                if let Some(server) = registry.find_server_for_path(s) {
                    if registry.is_server_spawned(&server.language) {
                        candidate_paths.push(path.clone());
                    }
                }
            }
        }
        if candidate_paths.is_empty() {
            self.lsp_importance.clear();
            return 0;
        }

        // 为所有文件初始化 0,保证排名列表完整性。
        self.lsp_importance = paths.iter().map(|p| (p.clone(), 0usize)).collect();

        let mut refreshed = 0usize;
        for path in candidate_paths {
            let path_str = match path.to_str() {
                Some(s) => s.to_owned(),
                None => continue,
            };

            // 复用 refresh_lsp_symbols 注入的 symbols;若为空则现场获取一次。
            let symbols = {
                let cached = self.cache.get(&path);
                match cached.and_then(|c| {
                    if c.lsp_symbols.is_empty() {
                        None
                    } else {
                        Some(c.lsp_symbols.clone())
                    }
                }) {
                    Some(syms) => syms,
                    None => match registry.get_symbols(&path_str) {
                        Ok(syms) => syms,
                        Err(_) => continue,
                    },
                }
            };

            if symbols.is_empty() {
                continue;
            }

            let mut cross_file_count = 0usize;
            for symbol in symbols.iter().take(MAX_REF_SYMBOLS_PER_FILE) {
                match registry.get_references(&path_str, symbol.line, symbol.character) {
                    Ok(locations) => {
                        for loc in locations {
                            // 跨模块:引用位置所在文件 ≠ 定义所在文件
                            // Windows 上大小写不敏感(LSP 返回的 URI 大小写可能
                            // 与本地 walkdir 路径不同)。
                            let loc_norm = normalize_ref_path(&loc.path);
                            let def_norm = normalize_ref_path(&path_str);
                            #[cfg(windows)]
                            let is_cross = !loc_norm.eq_ignore_ascii_case(&def_norm);
                            #[cfg(not(windows))]
                            let is_cross = loc_norm != def_norm;
                            if is_cross {
                                cross_file_count += 1;
                            }
                        }
                    }
                    Err(_) => {
                        // 单 symbol 失败不阻断该文件整体(部分 symbol 可能可用)
                    }
                }
            }

            if let Some(cached) = self.cache.get_mut(&path) {
                cached.lsp_refs_resolved = true;
            }
            self.lsp_importance.insert(path.clone(), cross_file_count);
            refreshed += 1;
        }

        // 若一个文件都没解析成功(如 server 在索引/全部报错),
        // 清空 lsp_importance 让 calculate_importance 回退 regex,避免全零重要性。
        if refreshed == 0 {
            self.lsp_importance.clear();
        }

        refreshed
    }
}

/// RepoMap 磁盘缓存文件结构(`.claw/repomap_cache.json`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepoMapCache {
    schema_version: u32,
    /// 支持的源语言列表(范围变化 → 缓存作废重建)。
    languages: Vec<String>,
    /// 规范化后的 root。
    root: String,
    files: HashMap<PathBuf, CachedFileMap>,
}

/// SystemTime → UNIX_EPOCH 起的毫秒数(与 `serde_mtime_ms` 序列化精度一致)。
/// 用于跨序列化边界的 mtime 比较,避免纳秒/毫秒精度差导致缓存误判过期。
fn mtime_ms(t: &SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 自定义 `SystemTime` 序列化:UNIX_EPOCH 起的毫秒数。
/// 规避 serde 对 `std::time::SystemTime` 的默认 JSON 表示(平台相关的 struct)
/// 在跨进程/跨版本下的不确定性。
mod serde_mtime_ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let ms = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        s.serialize_u64(ms)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + std::time::Duration::from_millis(ms))
    }
}

/// P4(regex 路径):把引用串按标识符段拆分成倒排索引键。
/// `use b::func_b` → `["b", "func_b"]`;`from a import A` → `["a", "A"]`;
/// 纯字面/命名空间串(`std::io`)同样拆分。段精确匹配避免 `Foo` 命中 `FooBar`
/// 注释/字符串误计(参考 LSP 语义分隔语义,仍是 best-effort)。
fn split_identifiers(s: &str) -> Vec<&str> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|seg| !seg.is_empty())
        .collect()
}

/// 遍历仓库返回目标源文件(.rs/.py)及其 mtime。
///
/// 自写 `read_dir` 递归替代 walkdir:`DirEntry::file_type()` 在 Windows 上
/// 每次≈一次 syscall,walkdir 会对**所有条目**(含 node_modules 等)都触发,
/// 大仓库下仅遍历就数百 ms。这里:
/// 1. 目录名(构建产物/依赖/隐藏目录)按字符串过滤,不 stat;
/// 2. 非目标扩展名的文件按字符串过滤,完全不 stat;
/// 3. 仅对目标候选调用一次 `fs::metadata`,同时取类型与 mtime。
fn walk_source_files(root: &Path) -> Vec<(PathBuf, SystemTime)> {
    fn is_ignored_dir(name: &str) -> bool {
        matches!(
            name,
            "target" | "node_modules" | "__pycache__" | "venv" | ".venv"
        ) || name.starts_with('.')
    }
    fn is_target_file(name: &str) -> bool {
        name.rsplit('.')
            .next()
            .is_some_and(|e| matches!(e, "rs" | "py"))
    }

    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if is_target_file(&name) {
                if let Ok(md) = std::fs::metadata(&path) {
                    if md.is_file() {
                        if let Ok(t) = md.modified() {
                            out.push((path, t));
                        }
                    }
                }
                continue;
            }
            // 非目标文件但可能是目录:仅当目录名未被忽略时才下钻。
            // 目录检查也会触发一次 syscall,但数量远小于文件数。
            if is_ignored_dir(&name) {
                continue;
            }
            if let Ok(md) = std::fs::metadata(&path) {
                if md.is_dir() {
                    stack.push(path);
                }
            }
        }
    }
    out
}

/// P1/诊断:Repomap 性能分段时间(门控,`CLAW_REPOMAP_VERBOSE=1` 时输出 stderr)。
/// 用于定位缓存命中路径的耗时组成,默认关闭零开销。
fn cache_verbose_timer() -> Option<std::time::Instant> {
    std::env::var("CLAW_REPOMAP_VERBOSE")
        .map(|v| v == "1")
        .unwrap_or(false)
        .then(std::time::Instant::now)
}

fn cache_verbose_log(label: &str, t0: &Option<std::time::Instant>) {
    if let Some(t) = t0 {
        eprintln!("[repomap-verbose] {label}={:?}", t.elapsed());
    }
}

fn estimate_tokens(s: &str) -> usize {
    s.chars().count() / 2 + 1
}

/// 规范化路径用于引用位置与定义文件的比较。
///
/// LSP 返回的 path 可能是 `file:///C:/workspace/a.rs`、`C:/workspace/a.rs`
/// 或 `/workspace/a.rs`,与本地 walkdir 产出的绝对路径格式不一致。
/// 统一去掉 `file://` 前缀并转换 `/` 与 `\`,仅用于比较,不用于展示。
fn normalize_ref_path(p: &str) -> String {
    let stripped = p.strip_prefix("file://").unwrap_or(p);
    let normalized = stripped.replace('\\', "/");
    // Windows 盘符场景:`file:///C:/workspace/a.rs` → `/C:/workspace/a.rs`,
    // 去掉前导 `/` 与 `C:/workspace/a.rs` 对齐。
    let bytes = normalized.as_bytes();
    if normalized.starts_with('/') && normalized.len() >= 3 && bytes[2] == b':' {
        normalized[1..].to_string()
    } else {
        normalized
    }
}

type DefRegexVec = Vec<(DefinitionKind, Regex)>;
type RefRegexVec = Vec<Regex>;

static DEFINITION_REGEXES: OnceLock<DefRegexVec> = OnceLock::new();
static REFERENCE_REGEXES: OnceLock<RefRegexVec> = OnceLock::new();
/// P2:Python 定义/引用 regex。best-effort:不支持装饰器、多行定义、
/// 嵌套函数/类(嵌套 def 仍会被捕获但归类为平铺符号,可接受)。
static PY_DEFINITION_REGEXES: OnceLock<DefRegexVec> = OnceLock::new();
static PY_REFERENCE_REGEXES: OnceLock<RefRegexVec> = OnceLock::new();

fn init_py_definition_regexes() -> DefRegexVec {
    vec![
        (
            DefinitionKind::Function,
            Regex::new(r"(?:async\s+)?def\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Class,
            Regex::new(r"(?:class\s+(\w+))").expect("valid regex literal"),
        ),
    ]
}

fn init_py_reference_regexes() -> RefRegexVec {
    vec![
        Regex::new(r"from\s+([\w.]+)\s+import").expect("valid regex literal"),
        Regex::new(r"import\s+([\w.]+)").expect("valid regex literal"),
    ]
}

fn init_definition_regexes() -> DefRegexVec {
    vec![
        (
            DefinitionKind::Function,
            Regex::new(r"(?:pub\s+)?(?:async\s+)?fn\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Struct,
            Regex::new(r"(?:pub\s+)?struct\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Enum,
            Regex::new(r"(?:pub\s+)?enum\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Trait,
            Regex::new(r"(?:pub\s+)?trait\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Impl,
            Regex::new(r"impl(?:<[^>]+>)?\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Module,
            Regex::new(r"(?:pub\s+)?mod\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Const,
            Regex::new(r"(?:pub\s+)?const\s+(\w+)").expect("valid regex literal"),
        ),
        (
            DefinitionKind::Type,
            Regex::new(r"(?:pub\s+)?type\s+(\w+)").expect("valid regex literal"),
        ),
    ]
}

fn init_reference_regexes() -> RefRegexVec {
    vec![
        Regex::new(r"use\s+([\w:]+)").expect("valid regex literal"),
        Regex::new(r"mod\s+(\w+)\s*;").expect("valid regex literal"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn test_extract_definitions_finds_functions() {
        let code =
            "pub fn main() {}\nfn helper() {}\npub async fn fetch() -> Result<String, Error> {}";
        let defs = RepoMap::extract_definitions(code);
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, "main");
        assert_eq!(defs[0].kind, DefinitionKind::Function);
    }

    // ========================================================================
    // P2 — Python 符号提取
    // ========================================================================

    #[test]
    fn py_extract_definitions_finds_def_and_class() {
        let code = "def hello():\n    pass\n\nasync def fetch():\n    pass\n\nclass Foo:\n    def bar(self):\n        pass";
        let defs = RepoMap::extract_definitions_lang(code, Lang::Python);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"fetch"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"bar"), "nested method should be captured");
        let foo = defs.iter().find(|d| d.name == "Foo").unwrap();
        assert_eq!(foo.kind, DefinitionKind::Class);
    }

    #[test]
    fn py_extract_references_finds_imports() {
        let code = "import os\nfrom pathlib import Path\nfrom . import sibling";
        let refs = RepoMap::extract_references_lang(code, Lang::Python);
        assert!(refs.contains(&"os".to_string()));
        assert!(refs.contains(&"pathlib".to_string()));
        // `from . import x` 捕获 "."(相对导入为 best-effort,不解析导入对象)。
        assert!(refs.contains(&".".to_string()));
    }

    #[test]
    fn lang_from_path_recognizes_rs_and_py() {
        assert_eq!(Lang::from_path(Path::new("a.rs")), Some(Lang::Rust));
        assert_eq!(Lang::from_path(Path::new("b.py")), Some(Lang::Python));
        assert_eq!(Lang::from_path(Path::new("c.txt")), None);
        assert_eq!(Lang::from_path(Path::new("noext")), None);
    }

    #[test]
    fn render_outputs_class_kind_for_python() {
        let temp = tempdir().unwrap();
        std::fs::write(
            temp.path().join("mod.py"),
            "class Foo:\n    pass\n\ndef bar():\n    pass",
        )
        .unwrap();
        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        let rendered = map.render();
        assert!(rendered.contains("class Foo"), "got: {rendered}");
        assert!(rendered.contains("fn bar"), "got: {rendered}");
    }

    #[test]
    fn test_extract_definitions_finds_structs_and_enums() {
        let code = "pub struct Foo {}\nenum Bar { A, B }";
        let defs = RepoMap::extract_definitions(code);
        assert!(defs
            .iter()
            .any(|d| d.name == "Foo" && d.kind == DefinitionKind::Struct));
        assert!(defs
            .iter()
            .any(|d| d.name == "Bar" && d.kind == DefinitionKind::Enum));
    }

    #[test]
    fn test_extract_references_finds_use_statements() {
        let code = "use std::io;\nuse crate::runtime::conversation;";
        let refs = RepoMap::extract_references(code);
        assert!(refs.contains(&"std::io".to_string()));
        assert!(refs.contains(&"crate::runtime::conversation".to_string()));
    }

    #[test]
    fn test_calculate_importance_counts_incoming_refs() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("b.rs"), "pub fn func_b() {}").unwrap();
        std::fs::write(
            temp.path().join("a.rs"),
            "use b::func_b;\npub fn func_a() {}",
        )
        .unwrap();

        let mut map = RepoMap::new(temp.path());
        map.refresh_cache_if_stale();
        let importance = map.calculate_importance();
        let b_path = temp.path().join("b.rs");
        let b_importance = importance.get(&b_path).copied().unwrap_or(0);
        assert!(b_importance > 0, "b.rs should have positive importance");
    }

    #[test]
    fn segment_index_matches_identifiers_exactly() {
        // P4:段精确匹配(`Foo` 不命中 `FooBar`),且 `use b::func_b` 命中 func_b。
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("d.rs"), "pub fn Foo() {}\n").unwrap();
        std::fs::write(temp.path().join("e.rs"), "use b::FooBar;\npub fn e() {}").unwrap();
        std::fs::write(temp.path().join("f.rs"), "use b::Foo;\npub fn f() {}").unwrap();

        let mut map = RepoMap::new(temp.path());
        map.refresh_cache_if_stale();
        let importance = map.calculate_importance();
        // d.rs 定义 Foo:FooBar 不算引用(段不匹配),只有 f.rs 的 Foo 算 1 次。
        let d_path = temp.path().join("d.rs");
        let d_importance = importance.get(&d_path).copied().unwrap_or(0);
        assert_eq!(
            d_importance, 1,
            "Foo should be referenced once (FooBar must not match): {d_importance}"
        );
    }

    #[test]
    fn split_identifiers_splits_paths_and_spaces() {
        assert_eq!(
            split_identifiers("use b::func_b"),
            vec!["use", "b", "func_b"]
        );
        assert_eq!(
            split_identifiers("std::io::Read"),
            vec!["std", "io", "Read"]
        );
        assert_eq!(split_identifiers("foo.bar_baz"), vec!["foo", "bar_baz"]);
    }

    #[test]
    fn test_render_respects_token_budget() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}").unwrap();
        std::fs::write(temp.path().join("b.rs"), "pub fn func_b() {}").unwrap();
        let mut map = RepoMap::new(temp.path()).with_max_tokens(50);
        let rendered = map.render();
        let tokens = rendered.chars().count() / 2 + 1;
        assert!(
            tokens <= 50,
            "exceeds budget: {} (rendered: {:?})",
            tokens,
            rendered
        );
    }

    #[test]
    fn test_render_includes_high_importance_files_first() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("b.rs"), "pub fn func_b() {}").unwrap();
        std::fs::write(
            temp.path().join("a.rs"),
            "use b::func_b;\npub fn func_a() {}",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("c.rs"),
            "use b::func_b;\npub fn func_c() {}",
        )
        .unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        let rendered = map.render();
        let b_pos = rendered.find("b.rs").expect("b.rs should be in map");
        let a_pos = rendered.find("a.rs").expect("a.rs should be in map");
        assert!(
            b_pos < a_pos,
            "b.rs (higher importance) should appear before a.rs"
        );
    }

    #[test]
    fn test_cache_refreshes_when_ttl_expired() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}").unwrap();

        let mut map = RepoMap::new(temp.path());
        map.refresh_cache_if_stale();
        let initial_count = map.cache.len();
        assert!(
            initial_count > 0,
            "cache should be populated on first refresh"
        );

        // Force the cache to appear stale.
        map.cache_time = Some(SystemTime::now() - Duration::from_secs(CACHE_TTL_SECS + 10));

        // Add a new file after the initial cache was built.
        std::fs::write(temp.path().join("b.rs"), "pub fn func_b() {}").unwrap();

        map.refresh_cache_if_stale();
        assert!(
            map.cache.len() > initial_count,
            "cache should have been refreshed and picked up the new file"
        );
    }

    // ========================================================================
    // Step 4.2 — LSP symbol 注入 repomap 测试
    // ========================================================================

    #[test]
    fn augment_with_lsp_symbols_creates_new_cache_entry() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}").unwrap();

        let mut map = RepoMap::new(temp.path());
        // 初始 cache 为空
        assert!(map.cache.is_empty());

        // 注入 LSP symbols(文件可能不存在于 cache,应创建新条目)
        let symbols = vec![
            crate::lsp_client::LspSymbol {
                name: "main".to_string(),
                kind: "function".to_string(),
                path: "a.rs".to_string(),
                line: 0,
                character: 3,
            },
            crate::lsp_client::LspSymbol {
                name: "MyStruct".to_string(),
                kind: "struct".to_string(),
                path: "a.rs".to_string(),
                line: 5,
                character: 7,
            },
        ];
        map.augment_with_lsp_symbols(Path::new("a.rs"), symbols);

        // 验证 cache 中有该条目
        let abs_path = temp.path().join("a.rs");
        assert!(map.cache.contains_key(&abs_path));
        assert!(map.has_lsp_symbols(Path::new("a.rs")));
        let cached = &map.cache[&abs_path];
        assert_eq!(cached.lsp_symbols.len(), 2);
    }

    #[test]
    fn augment_with_lsp_symbols_replaces_existing_lsp_symbols() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("a.rs");
        std::fs::write(&file_path, "pub fn func_a() {}").unwrap();

        let mut map = RepoMap::new(temp.path());
        map.refresh_cache_if_stale();

        // 初始无 LSP symbols
        assert!(!map.has_lsp_symbols(Path::new("a.rs")));

        // 注入第一批 symbols
        let symbols1 = vec![crate::lsp_client::LspSymbol {
            name: "func_a".to_string(),
            kind: "function".to_string(),
            path: "a.rs".to_string(),
            line: 0,
            character: 7,
        }];
        map.augment_with_lsp_symbols(Path::new("a.rs"), symbols1);
        assert!(map.has_lsp_symbols(Path::new("a.rs")));
        assert_eq!(map.cache[&file_path].lsp_symbols.len(), 1);

        // 注入第二批 symbols(应替换第一批)
        let symbols2 = vec![
            crate::lsp_client::LspSymbol {
                name: "func_a".to_string(),
                kind: "function".to_string(),
                path: "a.rs".to_string(),
                line: 0,
                character: 7,
            },
            crate::lsp_client::LspSymbol {
                name: "helper".to_string(),
                kind: "function".to_string(),
                path: "a.rs".to_string(),
                line: 5,
                character: 7,
            },
        ];
        map.augment_with_lsp_symbols(Path::new("a.rs"), symbols2);
        assert_eq!(map.cache[&file_path].lsp_symbols.len(), 2);
    }

    #[test]
    fn render_uses_lsp_symbols_when_available() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("a.rs");
        std::fs::write(&file_path, "pub fn func_a() {}").unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.refresh_cache_if_stale();

        // 注入 LSP symbols(与 regex 提取的 definitions 不同,以便区分)
        let symbols = vec![crate::lsp_client::LspSymbol {
            name: "lsp_only_symbol".to_string(),
            kind: "function".to_string(),
            path: "a.rs".to_string(),
            line: 10,
            character: 5,
        }];
        map.augment_with_lsp_symbols(Path::new("a.rs"), symbols);

        let rendered = map.render();
        // 应渲染 LSP symbol(包含位置信息 L:10:5)
        assert!(
            rendered.contains("lsp_only_symbol"),
            "rendered should contain LSP symbol: {rendered}"
        );
        assert!(
            rendered.contains("L:10:5"),
            "rendered should contain LSP position: {rendered}"
        );
        // 不应包含 regex 提取的 func_a(因为 LSP symbols 优先)
        assert!(
            !rendered.contains("fn func_a"),
            "rendered should not contain regex definitions when LSP symbols present: {rendered}"
        );
    }

    #[test]
    fn render_falls_back_to_regex_when_no_lsp_symbols() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}").unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.refresh_cache_if_stale();

        // 不注入 LSP symbols
        assert!(!map.has_lsp_symbols(Path::new("a.rs")));

        let rendered = map.render();
        // 应使用 regex 提取的 definitions
        assert!(
            rendered.contains("fn func_a"),
            "rendered should contain regex definitions: {rendered}"
        );
    }

    #[test]
    fn has_lsp_symbols_returns_false_for_unknown_path() {
        let temp = tempdir().unwrap();
        let map = RepoMap::new(temp.path());
        assert!(!map.has_lsp_symbols(Path::new("nonexistent.rs")));
    }

    // ========================================================================
    // Step 4.3 — LSP references 跨模块引用测试
    // ========================================================================

    #[test]
    fn normalize_ref_path_unifies_uri_and_separators() {
        // file:// URI、正斜杠与反斜杠应归一为同一条路径
        assert_eq!(
            normalize_ref_path("file:///C:/workspace/a.rs"),
            normalize_ref_path(r"C:\workspace\a.rs")
        );
        assert_eq!(
            normalize_ref_path("file:///workspace/a.rs"),
            normalize_ref_path("/workspace/a.rs")
        );
    }

    #[test]
    fn refresh_lsp_references_no_spawned_server_falls_back() {
        // 无已 spawn 的 LSP server 时,lsp_importance 应为空,
        // calculate_importance 走 regex 路径(不出现全零重要性)。
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("b.rs"), "pub fn func_b() {}").unwrap();
        std::fs::write(
            temp.path().join("a.rs"),
            "use crate::func_b;\npub fn func_a() {}",
        )
        .unwrap();

        let mut map = RepoMap::new(temp.path());
        map.refresh_cache_if_stale();

        // 空 registry:没有任何 server,refresh 返回 0
        let registry = crate::lsp_client::LspRegistry::new();
        let refreshed = map.refresh_lsp_references(&registry);
        assert_eq!(refreshed, 0);
        assert!(map.lsp_importance.is_empty());

        // calculate_importance 应仍走 regex 路径:b.rs 被 a.rs 引用 > 0
        let importance = map.calculate_importance();
        let b = temp.path().join("b.rs");
        assert!(
            importance.get(&b).copied().unwrap_or(0) > 0,
            "no LSP server → regex importance should apply"
        );
    }

    #[test]
    fn refresh_lsp_references_registered_but_not_spawned_falls_back() {
        // server 已注册但未 spawn:refresh 不应触发 auto-start,
        // lsp_importance 保持为空(避免 prompt 组装阶段慢启动)。
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}").unwrap();

        let mut map = RepoMap::new(temp.path());
        map.refresh_cache_if_stale();

        let registry = crate::lsp_client::LspRegistry::new();
        registry.register_with_command(
            "rust",
            crate::lsp_client::LspServerStatus::Disconnected,
            Some(temp.path().to_str().unwrap()),
            vec![],
            "rust-analyzer",
        );

        let refreshed = map.refresh_lsp_references(&registry);
        assert_eq!(refreshed, 0);
        assert!(map.lsp_importance.is_empty());
    }

    #[test]
    fn refresh_lsp_references_connected_but_not_spawned_falls_back() {
        // server 状态标记为 Connected 但 process_transports 中无真实 transport
        // (如 spawn 失败后状态残留):refresh 应依据 is_server_spawned 跳过,
        // lsp_importance 保持为空,calculate_importance 走 regex。
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}").unwrap();
        std::fs::write(temp.path().join("b.rs"), "pub fn func_b() {}").unwrap();

        let mut map = RepoMap::new(temp.path());
        map.refresh_cache_if_stale();

        // 注入 LSP symbols 模拟 refresh_lsp_symbols 的结果
        map.augment_with_lsp_symbols(
            Path::new("a.rs"),
            vec![crate::lsp_client::LspSymbol {
                name: "func_a".to_string(),
                kind: "function".to_string(),
                path: "a.rs".to_string(),
                line: 0,
                character: 7,
            }],
        );

        // 注册 server 但从未 spawn(模拟 spawn 失败后 status 残留 Connected)
        let registry = crate::lsp_client::LspRegistry::new();
        registry.register_with_command(
            "rust",
            crate::lsp_client::LspServerStatus::Connected,
            Some(temp.path().to_str().unwrap()),
            vec![],
            "rust-analyzer",
        );
        assert!(!registry.is_server_spawned("rust"));

        let refreshed = map.refresh_lsp_references(&registry);
        assert_eq!(refreshed, 0);
        assert!(map.lsp_importance.is_empty());

        // 未 spawn → calculate_importance 走 regex,不会全零
        let importance = map.calculate_importance();
        assert!(!importance.is_empty());
    }

    // ========================================================================
    // P1 — RepoMap 磁盘缓存
    // ========================================================================

    #[test]
    fn disk_cache_round_trips_across_instances() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}\n").unwrap();
        std::fs::write(
            temp.path().join("b.rs"),
            "use a::func_a;\npub fn func_b() {}\n",
        )
        .unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        assert!(!map.load_disk(), "no cache yet");
        let rendered1 = map.render();
        map.save_disk();
        assert!(
            temp.path()
                .join(".claw")
                .join("repomap_cache.json")
                .exists(),
            "cache file should be written"
        );

        // 新实例加载磁盘缓存后 render 结果与首轮一致。
        let mut map2 = RepoMap::new(temp.path()).with_max_tokens(500);
        assert!(map2.load_disk(), "cache should load");
        let rendered2 = map2.render();
        assert_eq!(rendered1, rendered2);
    }

    #[test]
    fn disk_cache_discovers_new_files_incrementally() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}\n").unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.render();
        map.save_disk();

        // save 之后新增文件:加载缓存后 render 应通过 walkdir mtime 校验发现它。
        std::fs::write(temp.path().join("c.rs"), "pub fn func_c() {}\n").unwrap();
        let mut map2 = RepoMap::new(temp.path()).with_max_tokens(500);
        assert!(map2.load_disk());
        let rendered = map2.render();
        assert!(
            rendered.contains("c.rs"),
            "new file should appear: {rendered}"
        );
    }

    #[test]
    fn disk_cache_warm_load_does_not_reparse_unchanged_files() {
        // 回归:save 经 serde_mtime_ms 截断为毫秒,load 后与纳秒级 SystemTime
        // 直接 `!=` 会误判全部过期 → 每次启动全量重解析。毫秒精度比较修复后,
        // 无变化文件不应触发 dirty(不写盘)、mtime 缓存不被替换。
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}\n").unwrap();
        std::fs::write(temp.path().join("b.rs"), "pub fn func_b() {}\n").unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.render();
        map.save_disk();

        let mut map2 = RepoMap::new(temp.path()).with_max_tokens(500);
        assert!(map2.load_disk());
        let before_mtime = map2.cache.values().next().unwrap().mtime;
        map2.render(); // 热启:walk 后所有 mtime 毫秒相等 → 不重解析
        assert!(
            !map2.dirty,
            "unchanged files must not mark cache dirty (ms precision fix)"
        );
        assert_eq!(
            map2.cache.values().next().unwrap().mtime,
            before_mtime,
            "cache entries must not be replaced on warm load"
        );
    }

    #[test]
    fn disk_cache_skips_lsp_symbols() {
        // LSP symbols 不落盘:save 后 load,LSP symbols 应为空 → render 走 regex。
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}\n").unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.render();
        map.augment_with_lsp_symbols(
            Path::new("a.rs"),
            vec![crate::lsp_client::LspSymbol {
                name: "lsp_only".to_string(),
                kind: "function".to_string(),
                path: "a.rs".to_string(),
                line: 0,
                character: 3,
            }],
        );
        assert!(map.has_lsp_symbols(Path::new("a.rs")));
        map.save_disk();

        let mut map2 = RepoMap::new(temp.path()).with_max_tokens(500);
        assert!(map2.load_disk());
        assert!(!map2.has_lsp_symbols(Path::new("a.rs")));
        let rendered = map2.render();
        assert!(
            rendered.contains("fn func_a"),
            "should fall back to regex definitions: {rendered}"
        );
    }

    #[test]
    fn disk_cache_invalidated_when_supported_languages_change() {
        // languages 范围变化 → 缓存作废(直接改写缓存文件模拟旧版本)。
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}\n").unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.render();
        map.save_disk();

        // 用错误的 languages 字段重写缓存。
        let cache_path = temp.path().join(".claw").join("repomap_cache.json");
        let mut cached: RepoMapCache =
            serde_json::from_str(&std::fs::read_to_string(&cache_path).unwrap()).unwrap();
        cached.languages = vec!["python".to_string()];
        std::fs::write(&cache_path, serde_json::to_string(&cached).unwrap()).unwrap();

        let mut map2 = RepoMap::new(temp.path()).with_max_tokens(500);
        assert!(
            !map2.load_disk(),
            "language mismatch should invalidate cache"
        );
    }

    // ========================================================================
    // P4 — 语义重要性
    // ========================================================================

    #[test]
    fn semantic_importance_counts_cross_caller_files_deduped() {
        use crate::project_topology::{CallSite, SymbolDef, SymbolIndex};
        let temp = tempdir().unwrap();
        std::fs::write(
            temp.path().join("a.rs"),
            "pub fn target_fn() {}\npub fn other() {}",
        )
        .unwrap();
        std::fs::write(temp.path().join("b.rs"), "pub fn b() {}\n").unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.render(); // 填充 cache

        let si = SymbolIndex {
            definitions: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "target_fn".to_string(),
                    vec![SymbolDef {
                        name: "target_fn".to_string(),
                        kind: "fn".to_string(),
                        file: PathBuf::from("a.rs"),
                        line: 1,
                        crate_name: None,
                        visibility: Some("pub".to_string()),
                    }],
                );
                m
            },
            callers: {
                let mut m: std::collections::HashMap<String, Vec<CallSite>> =
                    std::collections::HashMap::new();
                m.insert(
                    "target_fn".to_string(),
                    vec![
                        CallSite {
                            file: PathBuf::from("b.rs"),
                            line: 1,
                            context: None,
                        },
                        CallSite {
                            file: PathBuf::from("b.rs"),
                            line: 2,
                            context: None,
                        },
                        CallSite {
                            file: PathBuf::from("c.rs"),
                            line: 1,
                            context: None,
                        },
                    ],
                );
                m
            },
            file_symbols: std::collections::HashMap::new(),
        };
        map.augment_with_symbol_index(&si);
        let importance = map.calculate_importance();
        // 同一 caller 文件 b.rs 的两处调用去重为 1,加上 c.rs → 共 2。
        let a = temp.path().join("a.rs");
        let got = importance.get(&a).copied().unwrap_or(0);
        assert_eq!(got, 2, "should count 2 distinct caller files, got {got}");
    }

    #[test]
    fn semantic_importance_falls_back_to_regex_when_empty() {
        use crate::project_topology::SymbolIndex;
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "pub fn func_a() {}\n").unwrap();
        std::fs::write(
            temp.path().join("b.rs"),
            "use a::func_a;\npub fn func_b() {}",
        )
        .unwrap();

        let mut map = RepoMap::new(temp.path()).with_max_tokens(500);
        map.render();
        // 空 SymbolIndex → augment 不改变状态 → calculate 走 regex 路径。
        map.augment_with_symbol_index(&SymbolIndex {
            definitions: std::collections::HashMap::new(),
            callers: std::collections::HashMap::new(),
            file_symbols: std::collections::HashMap::new(),
        });
        let importance = map.calculate_importance();
        // regex 路径:a.rs 的 func_a 被 b.rs 的 `use a::func_a` 引用 → a.rs importance > 0。
        let a = temp.path().join("a.rs");
        assert!(
            importance.get(&a).copied().unwrap_or(0) > 0,
            "regex importance should apply when semantic empty"
        );
    }
}
