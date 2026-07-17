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

            self.cache.insert(
                path.to_path_buf(),
                CachedFileMap {
                    definitions,
                    references,
                    mtime,
                },
            );
        }

        // Drop cache entries for files that no longer exist on disk.
        self.cache.retain(|p, _| p.exists());

        self.cache_time = Some(now);
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
        (DefinitionKind::Function, Regex::new(r"(?:pub\s+)?(?:async\s+)?fn\s+(\w+)").unwrap()),
        (DefinitionKind::Struct, Regex::new(r"(?:pub\s+)?struct\s+(\w+)").unwrap()),
        (DefinitionKind::Enum, Regex::new(r"(?:pub\s+)?enum\s+(\w+)").unwrap()),
        (DefinitionKind::Trait, Regex::new(r"(?:pub\s+)?trait\s+(\w+)").unwrap()),
        (DefinitionKind::Impl, Regex::new(r"impl(?:<[^>]+>)?\s+(\w+)").unwrap()),
        (DefinitionKind::Module, Regex::new(r"(?:pub\s+)?mod\s+(\w+)").unwrap()),
        (DefinitionKind::Const, Regex::new(r"(?:pub\s+)?const\s+(\w+)").unwrap()),
        (DefinitionKind::Type, Regex::new(r"(?:pub\s+)?type\s+(\w+)").unwrap()),
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
        let code = "pub fn main() {}\nfn helper() {}\npub async fn fetch() -> Result<String, Error> {}";
        let defs = RepoMap::extract_definitions(code);
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, "main");
        assert_eq!(defs[0].kind, DefinitionKind::Function);
    }

    #[test]
    fn test_extract_definitions_finds_structs_and_enums() {
        let code = "pub struct Foo {}\nenum Bar { A, B }";
        let defs = RepoMap::extract_definitions(code);
        assert!(defs.iter().any(|d| d.name == "Foo" && d.kind == DefinitionKind::Struct));
        assert!(defs.iter().any(|d| d.name == "Bar" && d.kind == DefinitionKind::Enum));
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
        assert!(tokens <= 50, "exceeds budget: {} (rendered: {:?})", tokens, rendered);
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
        assert!(initial_count > 0, "cache should be populated on first refresh");

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
}
