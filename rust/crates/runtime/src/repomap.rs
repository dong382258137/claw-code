//! Simplified repository code map.
//!
//! Generates a token-budgeted overview of Rust definitions across a project
//! using regex-based extraction and reference counting. Intended for system
//! prompt injection so the model has a high-level view of the codebase
//! without consuming the full file contents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use regex::Regex;

const DEFAULT_MAX_TOKENS: usize = 1024;
const CACHE_TTL_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct RepoMap {
    root: PathBuf,
    max_tokens: usize,
    cache: HashMap<PathBuf, CachedFileMap>,
    cache_time: Option<SystemTime>,
}

#[derive(Debug, Clone)]
struct CachedFileMap {
    definitions: Vec<Definition>,
    references: Vec<String>,
    mtime: SystemTime,
    /// Step 4.2:从 LSP 获取的 symbol 信息(可选)。
    /// 若存在,render 时优先使用 LSP symbols(语义准确),
    /// 否则 fallback 到 regex 提取的 definitions。
    lsp_symbols: Vec<crate::lsp_client::LspSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    pub kind: DefinitionKind,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Type,
}

impl RepoMap {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            max_tokens: DEFAULT_MAX_TOKENS,
            cache: HashMap::new(),
            cache_time: None,
        }
    }

    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn render(&mut self) -> String {
        self.refresh_cache_if_stale();
        let importance = self.calculate_importance();
        let mut ranked: Vec<(PathBuf, usize)> = importance.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.build_map_with_budget(&ranked)
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
        let importance = self.calculate_importance();
        let mut ranked: Vec<(PathBuf, usize)> = importance.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.build_map_with_budget(&ranked)
    }

    #[must_use]
    pub fn extract_definitions(content: &str) -> Vec<Definition> {
        let mut defs = Vec::new();
        for (kind, re) in DEFINITION_REGEXES.get_or_init(init_definition_regexes) {
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
        let mut refs = Vec::new();
        for re in REFERENCE_REGEXES.get_or_init(init_reference_regexes) {
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
        for (path, cached) in &self.cache {
            let mut count = 0usize;
            for def in &cached.definitions {
                for (other_path, other_cached) in &self.cache {
                    if other_path == path {
                        continue;
                    }
                    for ref_str in &other_cached.references {
                        if ref_str.contains(def.name.as_str()) {
                            count += 1;
                        }
                    }
                }
            }
            importance.insert(path.clone(), count);
        }
        importance
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
                    for symbol in &cached.lsp_symbols {
                        out.push_str(&format!(
                            "  {} {} (L:{}:{})\n",
                            symbol.kind, symbol.name, symbol.line, symbol.character
                        ));
                    }
                } else {
                    // Fallback:regex 提取的 definitions
                    for def in &cached.definitions {
                        let kind_str = match def.kind {
                            DefinitionKind::Function => "fn",
                            DefinitionKind::Struct => "struct",
                            DefinitionKind::Enum => "enum",
                            DefinitionKind::Trait => "trait",
                            DefinitionKind::Impl => "impl",
                            DefinitionKind::Module => "mod",
                            DefinitionKind::Const => "const",
                            DefinitionKind::Type => "type",
                        };
                        out.push_str(&format!("  {} {}\n", kind_str, def.name));
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

        for entry in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    name != "target" && !name.starts_with('.')
                } else {
                    true
                }
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }

            let mtime = match std::fs::metadata(path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };

            let needs_reparse = match self.cache.get(path) {
                None => true,
                Some(cached) => cached.mtime != mtime,
            };
            if !needs_reparse {
                continue;
            }

            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let definitions = Self::extract_definitions(&content);
            let references = Self::extract_references(&content);

            // SP4.2-B5:re-parse 时保留已有的 LSP symbols(避免 refresh 清空)
            // LSP symbols 由 augment_with_lsp_symbols 单独注入,refresh_cache_if_stale
            // 不负责重新获取 LSP symbols(那需要调用 LSP server,开销大且可能阻塞)。
            // 保留策略:文件 mtime 变化时,regex 提取结果更新,但 LSP symbols 保留
            // (略有过期,但比清空好;下次 augment_with_lsp_symbols 调用会刷新)。
            let existing_lsp_symbols = self
                .cache
                .get(path)
                .map(|c| c.lsp_symbols.clone())
                .unwrap_or_default();

            self.cache.insert(
                path.to_path_buf(),
                CachedFileMap {
                    definitions,
                    references,
                    mtime,
                    lsp_symbols: existing_lsp_symbols,
                },
            );
        }

        // Drop cache entries for files that no longer exist on disk.
        self.cache.retain(|p, _| p.exists());

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
}

fn estimate_tokens(s: &str) -> usize {
    s.chars().count() / 2 + 1
}

type DefRegexVec = Vec<(DefinitionKind, Regex)>;
type RefRegexVec = Vec<Regex>;

static DEFINITION_REGEXES: OnceLock<DefRegexVec> = OnceLock::new();
static REFERENCE_REGEXES: OnceLock<RefRegexVec> = OnceLock::new();

fn init_definition_regexes() -> DefRegexVec {
    vec![
        (
            DefinitionKind::Function,
            Regex::new(r"(?:pub\s+)?(?:async\s+)?fn\s+(\w+)").unwrap(),
        ),
        (
            DefinitionKind::Struct,
            Regex::new(r"(?:pub\s+)?struct\s+(\w+)").unwrap(),
        ),
        (
            DefinitionKind::Enum,
            Regex::new(r"(?:pub\s+)?enum\s+(\w+)").unwrap(),
        ),
        (
            DefinitionKind::Trait,
            Regex::new(r"(?:pub\s+)?trait\s+(\w+)").unwrap(),
        ),
        (
            DefinitionKind::Impl,
            Regex::new(r"impl(?:<[^>]+>)?\s+(\w+)").unwrap(),
        ),
        (
            DefinitionKind::Module,
            Regex::new(r"(?:pub\s+)?mod\s+(\w+)").unwrap(),
        ),
        (
            DefinitionKind::Const,
            Regex::new(r"(?:pub\s+)?const\s+(\w+)").unwrap(),
        ),
        (
            DefinitionKind::Type,
            Regex::new(r"(?:pub\s+)?type\s+(\w+)").unwrap(),
        ),
    ]
}

fn init_reference_regexes() -> RefRegexVec {
    vec![
        Regex::new(r"use\s+([\w:]+)").unwrap(),
        Regex::new(r"mod\s+(\w+)\s*;").unwrap(),
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
}
